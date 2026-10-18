use ahash::AHashSet;
use kytrix_proto::types::{
    Tweet, XProfile as ProtoXProfile, XSelectionStatus as ProtoSelectionStatus,
};
use reqwest::StatusCode;
use serde::Deserialize;
use smol_str::SmolStr;
use thiserror::Error;

use crate::service::{
    engine::{ErrorCode, KytrixServiceError, TweetBatch, TweetEvent, TweetReference},
    protocol::{decode_tweet, decode_x_profile, valid_https_url, valid_x_id, valid_x_username},
};

pub const MAX_X_SELECTIONS: usize = 5_000;
pub const MAX_X_SEARCH_PROFILES: usize = 10;
const MAX_X_SEARCH_QUERY_BYTES: usize = 100;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct XProfile {
    pub x_user_id: SmolStr,
    pub username: SmolStr,
    pub display_name: SmolStr,
    pub avatar_url: SmolStr,
    pub description: SmolStr,
    pub followers_count: u64,
    pub following_count: u64,
    pub protected: bool,
    pub verified: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum XSelectionStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "paused")]
    Paused,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct XSelection {
    pub x_user_id: SmolStr,
    pub status: XSelectionStatus,
    pub created_at: i64,
    pub updated_at: i64,
    pub profile: Option<XProfile>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct XSelectionsResponse {
    pub selections: Vec<XSelection>,
    pub x_account_limit: usize,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum XApiError {
    #[error("the X search query is invalid")]
    InvalidSearchQuery,
    #[error("the X username is invalid")]
    InvalidUsername,
    #[error("the X account was not found")]
    AccountNotFound,
    #[error("the X account quota is full")]
    QuotaExceeded,
    #[error("protected X accounts are not supported")]
    ProtectedAccount,
    #[error("X API authentication failed")]
    Authentication,
    #[error("the X account session changed")]
    AccountChanged,
    #[error("the X monitor is not ready")]
    NotReady,
    #[error("X API service error: {0}")]
    Service(KytrixServiceError),
    #[error("X API protocol error: {0}")]
    Protocol(SmolStr),
    #[error("X API returned HTTP {0}")]
    Server(StatusCode),
    #[error("X API request timed out")]
    Timeout,
    #[error("the X account session is closed")]
    Closed,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum XMutationUnknown {
    #[error("the X mutation timed out")]
    Timeout,
    #[error("the X mutation transport result is unknown")]
    Transport,
    #[error("the X mutation response was lost")]
    ResponseLost,
    #[error("the X mutation response was invalid")]
    InvalidResponse,
    #[error("the X mutation returned HTTP {0}")]
    Server(StatusCode),
    #[error("the X mutation account session changed")]
    AccountChanged,
    #[error("the X mutation service closed")]
    ServiceClosed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XMutationOutcome<T> {
    Confirmed(T),
    Rejected(XApiError),
    Unknown(XMutationUnknown),
}

#[derive(Deserialize)]
pub(super) struct TweetsResponse {
    pub tweets: Vec<Tweet>,
    pub profiles: Vec<ProtoXProfile>,
}

pub(super) fn decode_search_profile(profile: ProtoXProfile) -> Result<XProfile, XApiError> {
    let event = decode_x_profile(profile.clone())
        .map_err(|error| XApiError::Protocol(smol_str::format_smolstr!("{}", error)))?;
    Ok(XProfile {
        x_user_id: event.x_user_id,
        username: event.username,
        display_name: event.display_name,
        avatar_url: event.avatar_url,
        description: profile.description.into(),
        followers_count: profile.followers_count,
        following_count: profile.following_count,
        protected: profile.protected,
        verified: event.verified,
    })
}

pub(super) fn decode_x_selection(
    selection: kytrix_proto::types::XSelection,
) -> Result<XSelection, XApiError> {
    let status = match ProtoSelectionStatus::try_from(selection.status)
        .map_err(|_| XApiError::Protocol("X selection status is invalid".into()))?
    {
        ProtoSelectionStatus::XPending => XSelectionStatus::Pending,
        ProtoSelectionStatus::XActive => XSelectionStatus::Active,
        ProtoSelectionStatus::XPaused => XSelectionStatus::Paused,
        ProtoSelectionStatus::Unspecified => {
            return Err(XApiError::Protocol("X selection status is invalid".into()));
        }
    };
    let profile = selection.profile.map(decode_search_profile).transpose()?;
    let value = XSelection {
        x_user_id: selection.x_user_id.into(),
        status,
        created_at: selection.created_at,
        updated_at: selection.updated_at,
        profile,
    };
    validate_selection(&value)?;
    Ok(value)
}

pub(super) fn map_rpc_error(error: KytrixServiceError) -> XApiError {
    match error {
        KytrixServiceError::Server(ErrorCode::RateLimit) => {
            XApiError::Server(StatusCode::TOO_MANY_REQUESTS)
        }
        KytrixServiceError::Server(ErrorCode::NotReady) => XApiError::NotReady,
        KytrixServiceError::Server(ErrorCode::InvalidArgument) => {
            XApiError::Protocol("invalid X request".into())
        }
        KytrixServiceError::Server(ErrorCode::NotFound) => XApiError::AccountNotFound,
        KytrixServiceError::Server(ErrorCode::QuotaExceeded) => XApiError::QuotaExceeded,
        KytrixServiceError::Server(ErrorCode::ProtectedAccount) => XApiError::ProtectedAccount,
        KytrixServiceError::Server(ErrorCode::Timeout) | KytrixServiceError::Timeout => {
            XApiError::Timeout
        }
        KytrixServiceError::Server(code) => XApiError::Service(KytrixServiceError::Server(code)),
        KytrixServiceError::Authentication(_) | KytrixServiceError::AccountChanged => {
            XApiError::Authentication
        }
        KytrixServiceError::Closed => XApiError::Closed,
        other => XApiError::Service(other),
    }
}

pub(super) fn mutation_rpc_error<T>(
    error: KytrixServiceError,
    possibly_submitted: bool,
) -> XMutationOutcome<T> {
    if !possibly_submitted {
        return XMutationOutcome::Rejected(map_rpc_error(error));
    }
    match error {
        KytrixServiceError::Server(ErrorCode::Timeout) | KytrixServiceError::Timeout => {
            XMutationOutcome::Unknown(XMutationUnknown::Timeout)
        }
        KytrixServiceError::Closed => XMutationOutcome::Unknown(XMutationUnknown::ServiceClosed),
        KytrixServiceError::AccountChanged | KytrixServiceError::Authentication(_) => {
            XMutationOutcome::Unknown(XMutationUnknown::AccountChanged)
        }
        KytrixServiceError::Server(ErrorCode::MutationUnknown) => {
            XMutationOutcome::Unknown(XMutationUnknown::ResponseLost)
        }
        KytrixServiceError::Disconnected
        | KytrixServiceError::Read(_)
        | KytrixServiceError::Protocol(_)
        | KytrixServiceError::RouteChanged => {
            XMutationOutcome::Unknown(XMutationUnknown::ResponseLost)
        }
        KytrixServiceError::Write(_) => XMutationOutcome::Unknown(XMutationUnknown::Transport),
        error => XMutationOutcome::Rejected(map_rpc_error(error)),
    }
}

pub(super) fn normalize_search_query(value: &str) -> Option<&str> {
    let query = value.trim();
    (!query.is_empty()
        && query.len() <= MAX_X_SEARCH_QUERY_BYTES
        && !query.chars().any(char::is_control))
    .then_some(query)
}

pub(super) fn decode_tweets_response(response: TweetsResponse) -> Result<TweetBatch, XApiError> {
    if response.tweets.len() > 50 || response.profiles.len() > 150 {
        return Err(XApiError::Protocol(
            "X API returned too much timeline data".into(),
        ));
    }
    let profiles = response
        .profiles
        .into_iter()
        .map(|profile| {
            decode_x_profile(profile)
                .map_err(|error| XApiError::Protocol(smol_str::format_smolstr!("{}", error)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut profile_ids = AHashSet::with_capacity(profiles.len());
    profile_ids.extend(profiles.iter().map(|profile| profile.x_user_id.as_str()));
    if profile_ids.len() != profiles.len() {
        return Err(XApiError::Protocol(
            "X API returned duplicate profiles".into(),
        ));
    }
    let mut ids = AHashSet::with_capacity(response.tweets.len());
    let tweets = response
        .tweets
        .into_iter()
        .map(|tweet| {
            if !ids.insert(tweet.tweet_id.clone()) {
                return Err(XApiError::Protocol(
                    "X API returned duplicate Tweets".into(),
                ));
            }
            decode_tweet(tweet)
                .map_err(|error| XApiError::Protocol(smol_str::format_smolstr!("{}", error)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut author_ids = AHashSet::with_capacity(tweets.len());
    for tweet in &tweets {
        collect_author_ids(tweet, &mut author_ids);
    }
    if author_ids != profile_ids || profiles.iter().any(|profile| profile.revision == 0) {
        return Err(XApiError::Protocol(
            "X API profiles did not cover Tweet authors".into(),
        ));
    }
    Ok(TweetBatch { tweets, profiles })
}

fn collect_author_ids<'a>(tweet: &'a TweetEvent, ids: &mut AHashSet<&'a str>) {
    ids.insert(tweet.x_user_id.as_str());
    let referenced = match tweet.reference.as_ref() {
        Some(TweetReference::Repost(tweet) | TweetReference::Quote(tweet)) => tweet,
        None => return,
    };
    collect_author_ids(referenced, ids);
}

pub(super) fn validate_selections(response: &XSelectionsResponse) -> Result<(), XApiError> {
    if !(0..=MAX_X_SELECTIONS).contains(&response.x_account_limit)
        || response.selections.len() > MAX_X_SELECTIONS
    {
        return Err(XApiError::Protocol(
            "X API returned an invalid account limit".into(),
        ));
    }
    let mut ids = AHashSet::with_capacity(response.selections.len());
    for selection in &response.selections {
        validate_selection(selection)?;
        if !ids.insert(selection.x_user_id.as_str()) {
            return Err(XApiError::Protocol(
                "X API returned duplicate selections".into(),
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_search_profiles(profiles: &[XProfile]) -> Result<(), XApiError> {
    if profiles.len() > MAX_X_SEARCH_PROFILES {
        return Err(XApiError::Protocol(
            "X API returned too many search profiles".into(),
        ));
    }
    let mut ids = AHashSet::with_capacity(profiles.len());
    for profile in profiles {
        validate_profile(profile, &profile.x_user_id)?;
        if !ids.insert(profile.x_user_id.as_str()) {
            return Err(XApiError::Protocol(
                "X API returned duplicate search profiles".into(),
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_selection(selection: &XSelection) -> Result<(), XApiError> {
    if !valid_x_id(&selection.x_user_id)
        || selection.created_at <= 0
        || selection.updated_at < selection.created_at
    {
        return Err(XApiError::Protocol(
            "X API returned an invalid selection".into(),
        ));
    }
    match (selection.status, selection.profile.as_ref()) {
        (XSelectionStatus::Active, Some(profile)) => {
            validate_profile(profile, &selection.x_user_id)
        }
        (XSelectionStatus::Pending | XSelectionStatus::Paused, None) => Ok(()),
        _ => Err(XApiError::Protocol(
            "X API returned a selection with an invalid profile".into(),
        )),
    }
}

fn validate_profile(profile: &XProfile, x_user_id: &str) -> Result<(), XApiError> {
    if profile.x_user_id != x_user_id
        || !valid_x_id(&profile.x_user_id)
        || !valid_x_username(&profile.username)
        || profile.display_name.is_empty()
        || profile.display_name.len() > 200
        || profile.display_name.chars().count() > 50
        || profile.description.len() > 640
        || profile.description.chars().count() > 160
        || (!profile.avatar_url.is_empty() && !valid_https_url(&profile.avatar_url))
    {
        return Err(XApiError::Protocol(
            "X API returned an invalid profile".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use kytrix_proto::types::{Tweet, XProfile as ProtoXProfile};

    use super::{
        super::engine::{ErrorCode, KytrixServiceError},
        *,
    };

    #[test]
    fn rpc_errors_preserve_quota_and_unknown_mutation_semantics() {
        assert_eq!(
            map_rpc_error(KytrixServiceError::Server(ErrorCode::QuotaExceeded)),
            XApiError::QuotaExceeded
        );
        assert_eq!(
            mutation_rpc_error::<()>(KytrixServiceError::Server(ErrorCode::MutationUnknown), true,),
            XMutationOutcome::Unknown(XMutationUnknown::ResponseLost)
        );
        assert_eq!(
            mutation_rpc_error::<()>(KytrixServiceError::Timeout, false),
            XMutationOutcome::Rejected(XApiError::Timeout)
        );
    }

    fn tweet(id: &str, author: &str) -> Tweet {
        Tweet {
            tweet_id: id.to_owned(),
            timestamp: 1,
            x_user_id: author.to_owned(),
            text_segments: Vec::new(),
            reference: None,
            media: Vec::new(),
        }
    }

    fn wire_profile(author: &str) -> ProtoXProfile {
        ProtoXProfile {
            x_user_id: author.to_owned(),
            username: "kytrix".to_owned(),
            display_name: "Kytrix".to_owned(),
            avatar_url: String::new(),
            description: String::new(),
            followers_count: 0,
            following_count: 0,
            protected: false,
            verified: true,
            revision: 1,
        }
    }

    fn profile() -> XProfile {
        XProfile {
            x_user_id: "42".into(),
            username: "kytrix".into(),
            display_name: "Kytrix".into(),
            avatar_url: "https://example.com/avatar.jpg".into(),
            description: "profile".into(),
            followers_count: 10,
            following_count: 2,
            protected: false,
            verified: true,
        }
    }

    fn active_selection() -> XSelection {
        XSelection {
            x_user_id: "42".into(),
            status: XSelectionStatus::Active,
            created_at: 1,
            updated_at: 2,
            profile: Some(profile()),
        }
    }

    #[test]
    fn selection_contract_requires_profiles_only_for_active_rows() {
        let mut active = active_selection();
        active.profile = None;
        assert!(matches!(
            validate_selection(&active),
            Err(XApiError::Protocol(_))
        ));

        let mut pending = active_selection();
        pending.status = XSelectionStatus::Pending;
        pending.profile = None;
        assert_eq!(validate_selection(&pending), Ok(()));
    }

    #[test]
    fn selections_contract_rejects_duplicates() {
        let selection = active_selection();
        let duplicate = XSelectionsResponse {
            selections: vec![selection.clone(), selection],
            x_account_limit: 1,
        };
        assert!(matches!(
            validate_selections(&duplicate),
            Err(XApiError::Protocol(_))
        ));
    }

    #[test]
    fn selections_contract_accepts_a_zero_limit() {
        assert_eq!(
            validate_selections(&XSelectionsResponse {
                selections: Vec::new(),
                x_account_limit: 0,
            }),
            Ok(())
        );
    }

    #[test]
    fn selections_contract_rejects_a_limit_above_the_process_bound() {
        assert!(matches!(
            validate_selections(&XSelectionsResponse {
                selections: Vec::new(),
                x_account_limit: MAX_X_SELECTIONS + 1,
            }),
            Err(XApiError::Protocol(_))
        ));
    }

    #[test]
    fn search_contract_rejects_invalid_queries_duplicates_and_overflow() {
        assert_eq!(
            normalize_search_query("  Kytrix Labs  "),
            Some("Kytrix Labs")
        );
        assert_eq!(normalize_search_query("\n"), None);
        assert_eq!(normalize_search_query(&"中".repeat(34)), None);

        let duplicate = profile();
        assert!(matches!(
            validate_search_profiles(&[duplicate.clone(), duplicate]),
            Err(XApiError::Protocol(_))
        ));
        assert!(matches!(
            validate_search_profiles(&vec![profile(); MAX_X_SEARCH_PROFILES + 1]),
            Err(XApiError::Protocol(_))
        ));
    }

    #[test]
    fn tweet_history_contract_rejects_duplicates_deep_references_and_missing_profiles() {
        let duplicate = TweetsResponse {
            tweets: vec![tweet("1", "42"), tweet("1", "42")],
            profiles: vec![wire_profile("42")],
        };
        assert!(matches!(
            decode_tweets_response(duplicate),
            Err(XApiError::Protocol(_))
        ));

        let mut deep = tweet("1", "42");
        let mut nested = tweet("2", "43");
        nested.reference = Some(kytrix_proto::types::tweet::Reference::Quote(Box::new(
            tweet("3", "44"),
        )));
        deep.reference = Some(kytrix_proto::types::tweet::Reference::Repost(Box::new(
            nested,
        )));
        assert!(
            decode_tweets_response(TweetsResponse {
                tweets: vec![deep.clone()],
                profiles: vec![wire_profile("42"), wire_profile("43"), wire_profile("44")],
            })
            .is_ok()
        );
        let kytrix_proto::types::tweet::Reference::Repost(nested) =
            deep.reference.as_mut().unwrap()
        else {
            unreachable!()
        };
        let kytrix_proto::types::tweet::Reference::Quote(leaf) = nested.reference.as_mut().unwrap()
        else {
            unreachable!()
        };
        leaf.reference = Some(kytrix_proto::types::tweet::Reference::Quote(Box::new(
            tweet("4", "45"),
        )));
        assert!(matches!(
            decode_tweets_response(TweetsResponse {
                tweets: vec![deep],
                profiles: vec![
                    wire_profile("42"),
                    wire_profile("43"),
                    wire_profile("44"),
                    wire_profile("45"),
                ],
            }),
            Err(XApiError::Protocol(_))
        ));

        assert!(matches!(
            decode_tweets_response(TweetsResponse {
                tweets: vec![tweet("1", "42")],
                profiles: Vec::new(),
            }),
            Err(XApiError::Protocol(_))
        ));

        let mut root = tweet("1", "42");
        root.reference = Some(kytrix_proto::types::tweet::Reference::Quote(Box::new(
            tweet("2", "43"),
        )));
        let mut unversioned = wire_profile("43");
        unversioned.revision = 0;
        assert!(matches!(
            decode_tweets_response(TweetsResponse {
                tweets: vec![root],
                profiles: vec![wire_profile("42"), unversioned],
            }),
            Err(XApiError::Protocol(_))
        ));
    }

    #[test]
    fn tweet_history_contract_keeps_profiles_separate_from_tweets() {
        let batch = decode_tweets_response(TweetsResponse {
            tweets: vec![tweet("1", "42")],
            profiles: vec![wire_profile("42")],
        })
        .unwrap();

        assert_eq!(batch.tweets[0].x_user_id, "42");
        assert_eq!(batch.profiles[0].username, "kytrix");
        assert!(batch.profiles[0].verified);
    }
}

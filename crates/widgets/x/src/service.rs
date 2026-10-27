//! The timeline protocol, its background service, and the seams a test drives it through.
//!
//! # One account, and everything a pane does with it
//!
//! A pane showing this widget follows the timeline of the accounts one X account monitors, and
//! it manages the list of accounts being monitored. Both belong to the same account and both
//! are answered with the same session — the timeline is subscribed with it and every change to
//! the list is sent with it — so one service owns both and one run loop serializes them.
//!
//! # The session is the service's, the list is the pane's
//!
//! Nothing about the monitored accounts is kept here as a list. What the pane shows is the
//! last answer it was given, and the projection of a confirmed change is [`SelectionData`]'s.
//! What this service does own is the *session*: a change is only made through the session the
//! stream was opened with, which is what keeps a dialog acting on an account the pane has left
//! from changing anything — such a request is refused with the session it disagrees with.
//!
//! # Pictures are fetched here, and belong to the stream
//!
//! The service streams tweets and it also fetches the pictures they want, because the two are
//! one question — what is on screen — asked of two places. The pictures a stream is fetching
//! live and die with that stream: a new stream starts from what the run loop knows the pane
//! wants, and a stream that is replaced takes its downloads with it. A picture's state travels
//! in the same message as the tweets it belongs to, so a frame never has to reconcile a tweet
//! against a picture from another moment.
//!
//! # Readiness
//!
//! `Ready` means the subscription is live, and the tweets are published before it: being live
//! is what makes the timeline already on the wire the pane's content, including none at all. A
//! subscription that never came up publishes why instead, and the pane reads the monitored
//! accounts through the session the service still holds.

use std::{collections::VecDeque, future::Future, path::PathBuf, sync::Arc};

use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use kytrix_service_provider::service::{
    connector::KytrixServiceConnector,
    engine::{
        KytrixAccountSession, KytrixServiceError, XAccountMonitoringSubscriptionEvent,
        XAccountMonitoringTweetBatch,
    },
    protocol::{valid_x_id, valid_x_username},
    x::{
        MAX_X_SELECTIONS, XAccountMonitoringApiError, XAccountMonitoringMutationOutcome,
        XAccountMonitoringProfile, XAccountMonitoringSelection,
        XAccountMonitoringSelectionsResponse,
    },
};
use network::transport::{
    error::TransportError,
    subscription::{Subscription, SubscriptionStatus},
};
use smol_str::SmolStr;
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use widget::{
    error::LocalizedError,
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
};

use super::{
    media::{
        MEDIA_OPERATION_TIMEOUT, MediaDownloader, MediaError, MediaTransport, SharedTransport,
    },
    model::{MediaState, MediaStates, SelectionData, TweetSnapshot, TweetState},
};

/// How many pictures one stream fetches at once.
///
/// A timeline is read by a person, and a picture that is not on screen yet is not late: four at
/// a time fills a viewport quickly without turning a pane into a burst of requests at X.
const MEDIA_CONCURRENCY: usize = 4;

/// How many "fetch that picture again" requests may be waiting for the stream at once.
const MEDIA_RETRY_CAPACITY: usize = 16;

/// What a pane asks its service to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TweetRequest {
    /// Follows an account's timeline, keeping the stream that already follows it.
    Load(TweetScope),
    /// Reopens an account's timeline, for one whose stream has failed.
    Retry(TweetScope),
    /// Follows nothing, because nobody is signed in.
    Stop,
    /// Reads the accounts the account monitors.
    FetchSelections(TweetScope),
    /// Says whether a dialog showing those accounts is open, which is what decides whether
    /// their avatars are fetched at all.
    WatchSelections { scope: TweetScope, watched: bool },
    /// Searches X for an account to monitor.
    SearchUsers { scope: TweetScope, query: SmolStr },
    /// Monitors one more account, by username.
    AddSelection {
        scope: TweetScope,
        username: SmolStr,
    },
    /// Stops monitoring one account.
    DeleteSelection {
        scope: TweetScope,
        x_user_id: SmolStr,
    },
    /// Fetches one picture again, for one that failed.
    RetryMedia { scope: TweetScope, url: SmolStr },
}

/// What the service answered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TweetResponse {
    /// The account's timeline is running.
    StreamStarted,
    /// Nothing is followed.
    Stopped,
    /// The accounts the account monitors.
    Selections(SelectionData),
    /// The dialog's avatars are, or are no longer, being fetched.
    SelectionsWatched,
    /// What the search found.
    SearchResults(Arc<[XAccountMonitoringProfile]>),
    /// What became of an account the pane asked to monitor.
    Added(XAccountMonitoringMutationOutcome<XAccountMonitoringSelection>),
    /// What became of an account the pane asked to stop monitoring.
    Deleted(XAccountMonitoringMutationOutcome<()>),
    /// One picture is being fetched again.
    MediaRetried,
}

/// The account whose timeline a stream carries.
///
/// Every request names one, including the ones that change the monitored list: the pane's
/// account can change while a dialog is still open, and an answer about the account the pane
/// has left is not one the pane may act on.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct TweetScope {
    account_id: SmolStr,
}

impl TweetScope {
    /// The scope an account follows, or `None` when nobody is signed in.
    pub(super) fn new(account_id: Option<&SmolStr>) -> Option<Self> {
        let account_id = account_id.filter(|account_id| !account_id.is_empty())?;
        Some(Self {
            account_id: account_id.clone(),
        })
    }

    pub(super) fn account_id(&self) -> &str {
        &self.account_id
    }
}

/// What a subscription the pane holds is doing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TweetStatus {
    /// The subscription is being established.
    Connecting,
    /// The subscription is live.
    Ready,
    /// The subscription ended; the pane is finished until the user recovers it.
    Failed(TweetStreamError),
}

/// Why a subscription stopped, or why it never started.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TweetStreamError {
    /// The account's session could not be taken, or the subscription was refused.
    Service(KytrixServiceError),
    /// The timeline already published could not be read.
    Api(XAccountMonitoringApiError),
    Transport(TransportError),
}

impl LocalizedError for TweetStreamError {
    fn localized_message(&self, cx: &gpui_kit::App) -> gpui_kit::SharedString {
        match self {
            Self::Service(error) => error.localized_message(cx),
            Self::Api(error) => error.localized_message(cx),
            Self::Transport(error) => error.localized_message(cx),
        }
    }
}

/// One message of an account's timeline.
#[derive(Clone, Debug)]
pub(super) enum TweetStreamMessage {
    Status(TweetStatus),
    /// The tweets the pane is showing, with the pictures they want.
    Update(TweetUpdate),
}

/// What one frame draws: the timeline, and what is known about its pictures.
#[derive(Clone, Debug)]
pub(super) struct TweetUpdate {
    revision: u64,
    feed: TweetSnapshot,
    media: MediaStates,
}

impl TweetUpdate {
    /// How many updates this stream has published, which is what makes an older one stale.
    pub(super) const fn revision(&self) -> u64 {
        self.revision
    }

    pub(super) fn feed(&self) -> &TweetSnapshot {
        &self.feed
    }

    pub(super) fn media(&self) -> &MediaStates {
        &self.media
    }

    /// One update, as a stream would have published it, for a test that reports one by hand.
    #[cfg(test)]
    pub(super) const fn published(revision: u64, feed: TweetSnapshot, media: MediaStates) -> Self {
        Self {
            revision,
            feed,
            media,
        }
    }
}

pub(super) struct TweetProtocol;

impl WidgetProtocol for TweetProtocol {
    type Request = TweetRequest;
    type Response = TweetResponse;
    type StreamKey = TweetScope;
    type StreamMessage = TweetStreamMessage;
    type Error = XAccountMonitoringApiError;
}

/// The X account a pane reads through.
///
/// The service is written against this seam rather than against the connector, so a test can
/// drive every state a session and a subscription can be in without one.
pub(super) trait TweetSource: Clone + Send + Sync + 'static {
    type Session: TweetSession;

    /// Takes the account's session, which answers both the stream and every request the pane
    /// makes about the accounts it monitors.
    fn acquire_session(
        &self,
        account_id: &str,
    ) -> impl Future<Output = Result<Self::Session, KytrixServiceError>> + Send;
}

/// One account's session.
pub(super) trait TweetSession: Clone + Send + Sync + 'static {
    type Subscription: TweetEventSubscription;

    /// The account this session is for, which is what a stream keyed by another one is checked
    /// against.
    fn account_id(&self) -> &str;

    fn subscribe_x(
        &self,
    ) -> impl Future<Output = Result<Self::Subscription, KytrixServiceError>> + Send;

    /// The timeline already published for the accounts being monitored.
    fn fetch_tweets(
        &self,
    ) -> impl Future<Output = Result<XAccountMonitoringTweetBatch, XAccountMonitoringApiError>> + Send;

    fn fetch_selections(
        &self,
    ) -> impl Future<
        Output = Result<XAccountMonitoringSelectionsResponse, XAccountMonitoringApiError>,
    > + Send;

    fn search_users(
        &self,
        query: &str,
    ) -> impl Future<Output = Result<Vec<XAccountMonitoringProfile>, XAccountMonitoringApiError>> + Send;

    fn add_selection(
        &self,
        username: &str,
    ) -> impl Future<Output = XAccountMonitoringMutationOutcome<XAccountMonitoringSelection>> + Send;

    fn delete_selection(
        &self,
        x_user_id: &str,
    ) -> impl Future<Output = XAccountMonitoringMutationOutcome<()>> + Send;
}

/// One live event subscription.
pub(super) trait TweetEventSubscription: Send + 'static {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus>;

    fn recv(
        &mut self,
    ) -> impl Future<Output = Result<XAccountMonitoringSubscriptionEvent, TransportError>> + Send;
}

impl TweetSource for KytrixServiceConnector {
    type Session = KytrixAccountSession;

    async fn acquire_session(&self, account_id: &str) -> Result<Self::Session, KytrixServiceError> {
        self.acquire_account_session(account_id).await
    }
}

impl TweetSession for KytrixAccountSession {
    type Subscription = Subscription<XAccountMonitoringSubscriptionEvent>;

    fn account_id(&self) -> &str {
        self.profile().account_id()
    }

    async fn subscribe_x(&self) -> Result<Self::Subscription, KytrixServiceError> {
        KytrixAccountSession::subscribe_x(self).await
    }

    async fn fetch_tweets(
        &self,
    ) -> Result<XAccountMonitoringTweetBatch, XAccountMonitoringApiError> {
        self.fetch_x_tweets().await
    }

    async fn fetch_selections(
        &self,
    ) -> Result<XAccountMonitoringSelectionsResponse, XAccountMonitoringApiError> {
        self.fetch_x_selections().await
    }

    async fn search_users(
        &self,
        query: &str,
    ) -> Result<Vec<XAccountMonitoringProfile>, XAccountMonitoringApiError> {
        self.search_x_users(query).await
    }

    async fn add_selection(
        &self,
        username: &str,
    ) -> XAccountMonitoringMutationOutcome<XAccountMonitoringSelection> {
        KytrixAccountSession::add_x_selection(self, username).await
    }

    async fn delete_selection(&self, x_user_id: &str) -> XAccountMonitoringMutationOutcome<()> {
        KytrixAccountSession::delete_x_selection(self, x_user_id).await
    }
}

impl TweetEventSubscription for Subscription<XAccountMonitoringSubscriptionEvent> {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus> {
        Subscription::observe_status(self)
    }

    async fn recv(&mut self) -> Result<XAccountMonitoringSubscriptionEvent, TransportError> {
        Subscription::recv(self).await
    }
}

/// The service that follows one pane's timeline.
pub(super) struct TweetService<S = KytrixServiceConnector, T = SharedTransport> {
    source: S,
    downloader: MediaDownloader<T>,
}

impl<S, T> TweetService<S, T> {
    pub(super) const fn new(source: S, downloader: MediaDownloader<T>) -> Self {
        Self { source, downloader }
    }
}

/// The session the account a stream belongs to is answered with.
struct ActiveSession<S> {
    scope: TweetScope,
    session: S,
    /// The pictures beyond the timeline this stream should be fetching, as the run loop last
    /// knew them.
    auxiliary: watch::Sender<Arc<[SmolStr]>>,
    /// "Fetch that picture again" requests on their way to the stream that owns it.
    retries: mpsc::Sender<SmolStr>,
}

impl<S> ActiveSession<S> {
    fn session(&self) -> &S {
        &self.session
    }
}

impl<S, T> WidgetService<TweetProtocol> for TweetService<S, T>
where
    S: TweetSource,
    T: MediaTransport,
{
    async fn run(self, mut context: ServiceContext<TweetProtocol>) {
        let mut active: Option<ActiveSession<S::Session>> = None;
        // What the pane wants pictures of beyond the timeline: the avatars of the monitored
        // accounts, while a dialog is showing them, and those of its search results. The
        // avatars are keyed by account so a confirmed removal can take its own avatar out.
        let mut selection_avatars: HashMap<SmolStr, SmolStr> =
            HashMap::with_capacity(MAX_X_SELECTIONS);
        let mut search_avatars: Vec<SmolStr> = Vec::new();
        let mut selections_watched = false;

        while let Some(request) = context.next_rpc().await {
            let (mut reply, request) = request.into_parts();
            match request {
                TweetRequest::Stop => {
                    if let Some(active) = active.take() {
                        context.cancel_stream(&active.scope);
                    }
                    let _ = reply.respond(Ok(TweetResponse::Stopped));
                }
                request @ (TweetRequest::Load(_) | TweetRequest::Retry(_)) => {
                    let (scope, retry) = match request {
                        TweetRequest::Load(scope) => (scope, false),
                        TweetRequest::Retry(scope) => (scope, true),
                        TweetRequest::Stop
                        | TweetRequest::FetchSelections(_)
                        | TweetRequest::WatchSelections { .. }
                        | TweetRequest::SearchUsers { .. }
                        | TweetRequest::AddSelection { .. }
                        | TweetRequest::DeleteSelection { .. }
                        | TweetRequest::RetryMedia { .. } => continue,
                    };
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }

                    // A stream the service is already holding for this account is the one that
                    // was asked for: what the pane is waiting for is the account, and nothing
                    // about the stream changes. A retry is the caller saying the stream it has
                    // is not one it wants, so it is replaced even when the account behind it is
                    // the same.
                    if !retry
                        && active.as_ref().is_some_and(|active| active.scope == scope)
                        && context.stream_generation(&scope).is_some()
                    {
                        let _ = reply.respond(Ok(TweetResponse::StreamStarted));
                        continue;
                    }

                    let acquire = self.source.acquire_session(scope.account_id());
                    tokio::pin!(acquire);
                    let session = tokio::select! {
                        biased;
                        () = reply.cancelled() => continue,
                        result = &mut acquire => match result {
                            Ok(session) => session,
                            Err(error) => {
                                let _ = reply.respond(Err(session_failure(error)));
                                continue;
                            }
                        },
                    };
                    // The stream is keyed by the account the pane asked for, so a session that
                    // turned out to be for another one is answered rather than streamed: a
                    // tweet is not data a pane may show under a name it does not belong to.
                    if session.account_id() != scope.account_id() {
                        let _ = reply.respond(Err(XAccountMonitoringApiError::AccountChanged));
                        continue;
                    }

                    if let Some(previous) = active.take() {
                        context.cancel_stream(&previous.scope);
                    }
                    let streaming = session.clone();
                    let downloader = self.downloader.clone();
                    let wanted =
                        auxiliary_urls(&selection_avatars, &search_avatars, selections_watched);
                    let (auxiliary, auxiliary_changes) = watch::channel(wanted);
                    let (retries, retry_requests) = mpsc::channel(MEDIA_RETRY_CAPACITY);
                    context.start_stream(scope.clone(), move |publisher| async move {
                        run_stream(
                            streaming,
                            downloader,
                            publisher,
                            auxiliary_changes,
                            retry_requests,
                        )
                        .await;
                    });
                    active = Some(ActiveSession {
                        scope: scope.clone(),
                        session,
                        auxiliary,
                        retries,
                    });
                    if reply.respond(Ok(TweetResponse::StreamStarted)).is_err() {
                        // Nobody is waiting for this stream any more, so it is nobody's stream:
                        // a pane that went away must not leave a subscription holding an
                        // account it is not reading.
                        context.cancel_stream(&scope);
                        active = None;
                    }
                }
                TweetRequest::FetchSelections(scope) => {
                    let session = match matching_session(&active, &scope) {
                        Ok(active) => active.session(),
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    let fetch = session.fetch_selections();
                    tokio::pin!(fetch);
                    let response = tokio::select! {
                        biased;
                        () = reply.cancelled() => continue,
                        response = &mut fetch => match response {
                            Ok(response) => response,
                            Err(error) => {
                                let _ = reply.respond(Err(error));
                                continue;
                            }
                        },
                    };
                    let data = SelectionData::new(response);
                    selection_avatars = data
                        .selections()
                        .iter()
                        .filter_map(|selection| {
                            let profile = selection.profile.as_ref()?;
                            (!profile.avatar_url.is_empty())
                                .then(|| (selection.x_user_id.clone(), profile.avatar_url.clone()))
                        })
                        .collect();
                    if let Ok(active) = matching_session(&active, &scope) {
                        let _ = active;
                    }
                    if let Some(active) = active.as_ref().filter(|active| active.scope == scope) {
                        active.auxiliary.send_replace(auxiliary_urls(
                            &selection_avatars,
                            &search_avatars,
                            selections_watched,
                        ));
                    }
                    let _ = reply.respond(Ok(TweetResponse::Selections(data)));
                }
                TweetRequest::WatchSelections { scope, watched } => {
                    let Some(active) = active.as_ref().filter(|active| active.scope == scope)
                    else {
                        let _ = reply.respond(Err(XAccountMonitoringApiError::NotReady));
                        continue;
                    };
                    selections_watched = watched;
                    active.auxiliary.send_replace(auxiliary_urls(
                        &selection_avatars,
                        &search_avatars,
                        selections_watched,
                    ));
                    let _ = reply.respond(Ok(TweetResponse::SelectionsWatched));
                }
                TweetRequest::SearchUsers { scope, query } => {
                    let session = match matching_session(&active, &scope) {
                        Ok(active) => active.session(),
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    let search = session.search_users(&query);
                    tokio::pin!(search);
                    let profiles = tokio::select! {
                        biased;
                        () = reply.cancelled() => continue,
                        result = &mut search => match result {
                            Ok(profiles) => profiles,
                            Err(error) => {
                                let _ = reply.respond(Err(error));
                                continue;
                            }
                        },
                    };
                    search_avatars = profiles
                        .iter()
                        .filter(|profile| !profile.avatar_url.is_empty())
                        .map(|profile| profile.avatar_url.clone())
                        .collect();
                    if let Some(active) = active.as_ref().filter(|active| active.scope == scope) {
                        active.auxiliary.send_replace(auxiliary_urls(
                            &selection_avatars,
                            &search_avatars,
                            selections_watched,
                        ));
                    }
                    let _ = reply.respond(Ok(TweetResponse::SearchResults(profiles.into())));
                }
                TweetRequest::AddSelection { scope, username } => {
                    let session = match matching_session(&active, &scope) {
                        Ok(active) => active.session(),
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    if !valid_x_username(&username) {
                        let _ = reply.respond(Ok(TweetResponse::Added(
                            XAccountMonitoringMutationOutcome::Rejected(
                                XAccountMonitoringApiError::InvalidUsername,
                            ),
                        )));
                        continue;
                    }
                    let add = session.add_selection(&username);
                    tokio::pin!(add);
                    let outcome = tokio::select! {
                        biased;
                        // A caller that went away takes its question with it. The change may or
                        // may not have been made, and nothing here decides which: the pane
                        // reads the list again instead of guessing.
                        () = reply.cancelled() => continue,
                        outcome = &mut add => outcome,
                    };
                    if let XAccountMonitoringMutationOutcome::Confirmed(selection) = &outcome
                        && let Some(profile) = selection.profile.as_ref()
                        && !profile.avatar_url.is_empty()
                    {
                        selection_avatars
                            .insert(selection.x_user_id.clone(), profile.avatar_url.clone());
                        if let Some(active) = active.as_ref().filter(|active| active.scope == scope)
                        {
                            active.auxiliary.send_replace(auxiliary_urls(
                                &selection_avatars,
                                &search_avatars,
                                selections_watched,
                            ));
                        }
                    }
                    let _ = reply.respond(Ok(TweetResponse::Added(outcome)));
                }
                TweetRequest::DeleteSelection { scope, x_user_id } => {
                    let session = match matching_session(&active, &scope) {
                        Ok(active) => active.session(),
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    if !valid_x_id(&x_user_id) {
                        let _ = reply.respond(Ok(TweetResponse::Deleted(
                            XAccountMonitoringMutationOutcome::Rejected(
                                XAccountMonitoringApiError::Protocol("X user ID is invalid".into()),
                            ),
                        )));
                        continue;
                    }
                    let delete = session.delete_selection(&x_user_id);
                    tokio::pin!(delete);
                    let outcome = tokio::select! {
                        biased;
                        () = reply.cancelled() => continue,
                        outcome = &mut delete => outcome,
                    };
                    if matches!(&outcome, XAccountMonitoringMutationOutcome::Confirmed(())) {
                        selection_avatars.remove(&x_user_id);
                        if let Some(active) = active.as_ref().filter(|active| active.scope == scope)
                        {
                            active.auxiliary.send_replace(auxiliary_urls(
                                &selection_avatars,
                                &search_avatars,
                                selections_watched,
                            ));
                        }
                    }
                    let _ = reply.respond(Ok(TweetResponse::Deleted(outcome)));
                }
                TweetRequest::RetryMedia { scope, url } => {
                    let active = match matching_session(&active, &scope) {
                        Ok(active) => active,
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    if !super::media::plausible_media_url(&url) {
                        let _ = reply.respond(Err(XAccountMonitoringApiError::Protocol(
                            "X media URL is invalid".into(),
                        )));
                        continue;
                    }
                    // What is on screen is the stream's to say, so a retry is handed to the
                    // stream that owns the picture: it decides whether the picture is one it
                    // is still fetching, and publishes what comes of it.
                    if active.retries.try_send(url).is_err() {
                        let _ = reply.respond(Err(XAccountMonitoringApiError::NotReady));
                        continue;
                    }
                    let _ = reply.respond(Ok(TweetResponse::MediaRetried));
                }
            }
        }
    }
}

/// The pictures a pane wants beyond its timeline.
fn auxiliary_urls(
    selections: &HashMap<SmolStr, SmolStr>,
    search: &[SmolStr],
    watched: bool,
) -> Arc<[SmolStr]> {
    let mut urls = Vec::with_capacity(selections.len() + search.len());
    if watched {
        urls.extend(selections.values().cloned());
    }
    urls.extend(search.iter().cloned());
    urls.into()
}

/// The session a request about the monitored accounts is answered with.
///
/// A request that names an account the service is not holding is refused with what is wrong
/// rather than answered from another account's session: not holding one at all is a pane that
/// asked too early, and holding a different one is a pane whose account changed under it.
fn matching_session<'a, S>(
    active: &'a Option<ActiveSession<S>>,
    scope: &TweetScope,
) -> Result<&'a ActiveSession<S>, XAccountMonitoringApiError> {
    let Some(active) = active.as_ref() else {
        return Err(XAccountMonitoringApiError::NotReady);
    };
    if &active.scope != scope {
        return Err(XAccountMonitoringApiError::AccountChanged);
    }
    Ok(active)
}

/// What a session that could not be taken means to the request that asked for it.
///
/// The four that name a condition of their own are named as themselves: an expired login, a
/// changed account, a timeout and a closed service all read differently to a user than a
/// service error does.
fn session_failure(error: KytrixServiceError) -> XAccountMonitoringApiError {
    match error {
        KytrixServiceError::Authentication(_) => XAccountMonitoringApiError::Authentication,
        KytrixServiceError::AccountChanged => XAccountMonitoringApiError::AccountChanged,
        KytrixServiceError::Timeout => XAccountMonitoringApiError::Timeout,
        KytrixServiceError::Closed => XAccountMonitoringApiError::Closed,
        error => XAccountMonitoringApiError::Service(error),
    }
}

/// Runs the account's subscription and everything the pane wants pictures of.
async fn run_stream<S, T>(
    session: S,
    downloader: MediaDownloader<T>,
    publisher: StreamPublisher<TweetProtocol>,
    wanted: watch::Receiver<Arc<[SmolStr]>>,
    retries: mpsc::Receiver<SmolStr>,
) where
    S: TweetSession,
    T: MediaTransport,
{
    if publish_status(&publisher, TweetStatus::Connecting)
        .await
        .is_err()
    {
        return;
    }

    // The pictures belong to this stream, so their downloads are cancelled with it: a stream
    // that is replaced stops fetching what it was fetching.
    let mut media = MediaSet::new(downloader, CancellationToken::new());
    let exit = serve(session, &publisher, wanted, retries, &mut media).await;
    media.shutdown().await;
    if let StreamExit::Failed(error) = exit {
        publish_failure(&publisher, error).await;
    }
}

/// Serves one subscription until it ends, holding the stream's pictures as it goes.
async fn serve<S, T>(
    session: S,
    publisher: &StreamPublisher<TweetProtocol>,
    mut wanted: watch::Receiver<Arc<[SmolStr]>>,
    mut retries: mpsc::Receiver<SmolStr>,
    media: &mut MediaSet<T>,
) -> StreamExit
where
    S: TweetSession,
    T: MediaTransport,
{
    let mut published = TweetStatus::Connecting;
    let subscription = tokio::select! {
        biased;
        () = publisher.cancelled() => return StreamExit::Cancelled,
        result = session.subscribe_x() => result,
    };
    let mut subscription = match subscription {
        Ok(subscription) => subscription,
        Err(error) => {
            return StreamExit::Failed(TweetStreamError::Service(error));
        }
    };

    let mut status = subscription.observe_status();
    let mut state = TweetState::new();
    let mut revision = 0;

    // The timeline already on the wire is what the subscription will add to, so it is read
    // first and published before any status: a pane that becomes ready holds the history, and
    // a history that could not be read is why it never becomes ready at all.
    let history = tokio::select! {
        biased;
        () = publisher.cancelled() => return StreamExit::Cancelled,
        result = session.fetch_tweets() => result,
    };
    let batch = match history {
        Ok(batch) => batch,
        Err(error) => {
            return StreamExit::Failed(TweetStreamError::Api(error));
        }
    };
    let current = status.borrow_and_update().clone();
    if let Some(error) = ended_subscription(&current) {
        return StreamExit::Failed(error);
    }
    accept_history(&mut state, batch);
    media.reconcile(state.media_urls());
    media.fill();
    if publish_update(publisher, &mut state, media, &mut revision).is_err() {
        return StreamExit::Cancelled;
    }
    if !apply_subscription_status(publisher, &current, &mut published).await {
        return StreamExit::Cancelled;
    }

    run_subscription(
        &mut subscription,
        &mut status,
        &mut state,
        publisher,
        &mut wanted,
        &mut retries,
        media,
        &mut revision,
        &mut published,
    )
    .await
}

/// Files everything one history read holds, in the order that keeps authors resolvable.
fn accept_history(state: &mut TweetState, batch: XAccountMonitoringTweetBatch) {
    for tweet in batch.tweets {
        state.push(tweet);
    }
    state.prune_profiles();
    for profile in batch.profiles {
        state.upsert_profile(profile);
    }
    state.prune_profiles();
}

/// Serves one live subscription until it ends.
///
/// The workspace allows `too_many_arguments`, so the long parameter list is a
/// deliberate part of this loop's shape: each receiver is a different seam the
/// subscription turns on, and bundling them would hide which one woke it.
async fn run_subscription<Sub, T>(
    subscription: &mut Sub,
    status: &mut watch::Receiver<SubscriptionStatus>,
    state: &mut TweetState,
    publisher: &StreamPublisher<TweetProtocol>,
    wanted: &mut watch::Receiver<Arc<[SmolStr]>>,
    retries: &mut mpsc::Receiver<SmolStr>,
    media: &mut MediaSet<T>,
    revision: &mut u64,
    published: &mut TweetStatus,
) -> StreamExit
where
    Sub: TweetEventSubscription,
    T: MediaTransport,
{
    let mut retries_open = true;
    loop {
        media.fill();
        let input = tokio::select! {
            biased;
            () = publisher.cancelled() => StreamInput::Cancelled,
            changed = status.changed() => StreamInput::Status(changed.is_err()),
            event = subscription.recv() => StreamInput::Event(event),
            changed = wanted.changed() => StreamInput::Wanted(changed.is_ok()),
            retry = retries.recv(), if retries_open => match retry {
                Some(url) => StreamInput::Retry(url),
                None => {
                    retries_open = false;
                    continue;
                }
            },
            finished = media.active.join_next_with_id(), if !media.active.is_empty() => {
                match finished {
                    Some(finished) => StreamInput::Media(finished),
                    // The guard is what says a download is running, so an empty join set here
                    // is a task that settled between the two reads.
                    None => continue,
                }
            }
        };
        match input {
            StreamInput::Cancelled => return StreamExit::Cancelled,
            StreamInput::Status(closed) => {
                let next = if closed {
                    SubscriptionStatus::Closed
                } else {
                    status.borrow_and_update().clone()
                };
                if !apply_subscription_status(publisher, &next, published).await {
                    return StreamExit::Cancelled;
                }
            }
            // A run loop that dropped the sender is a stream nobody is holding any more.
            StreamInput::Wanted(false) => return StreamExit::Cancelled,
            StreamInput::Wanted(true) => {
                let urls = wanted.borrow_and_update().clone();
                if media.replace_auxiliary(urls)
                    && publish_update(publisher, state, media, revision).is_err()
                {
                    return StreamExit::Cancelled;
                }
            }
            StreamInput::Retry(url) => {
                if media.retry(&url) && publish_update(publisher, state, media, revision).is_err() {
                    return StreamExit::Cancelled;
                }
            }
            StreamInput::Media(finished) => {
                if media.finish(finished)
                    && publish_update(publisher, state, media, revision).is_err()
                {
                    return StreamExit::Cancelled;
                }
            }
            StreamInput::Event(event) => {
                // A replaced stream is a stream whose account nobody is reading any more, so
                // what it delivers is not converted either.
                if publisher.is_cancelled() {
                    return StreamExit::Cancelled;
                }
                // A tweet already on the wire belongs to the credentials the subscription was
                // opened with. If those are gone, so is the pane's standing to read it, and the
                // subscription's own status is what says so.
                let current = status.borrow_and_update().clone();
                if let Some(error) = ended_subscription(&current) {
                    return StreamExit::Failed(error);
                }
                let event = match event {
                    Ok(event) => event,
                    Err(error) => return StreamExit::Failed(TweetStreamError::Transport(error)),
                };
                let changed = match event {
                    XAccountMonitoringSubscriptionEvent::Profile(profile) => {
                        state.upsert_profile(profile)
                    }
                    XAccountMonitoringSubscriptionEvent::XAccountMonitoringTweet(tweet) => {
                        state.push(tweet)
                    }
                };
                let pruned = state.prune_profiles();
                if !changed && !pruned {
                    continue;
                }
                // Only a new tweet can want a picture nothing wanted before: a profile that was
                // corrected is the same URL, and one that was pruned takes its picture with it
                // as the timeline it belonged to moves on.
                if changed {
                    media.reconcile(state.media_urls());
                }
                if publish_update(publisher, state, media, revision).is_err() {
                    return StreamExit::Cancelled;
                }
            }
        }
    }
}

/// One thing a subscription loop can wake up for.
enum StreamInput {
    Cancelled,
    /// The pane's pictures changed, or the run loop let go of them.
    Wanted(bool),
    /// The subscription's status changed, or its sender was dropped.
    Status(bool),
    Event(Result<XAccountMonitoringSubscriptionEvent, TransportError>),
    Retry(SmolStr),
    Media(MediaTaskResult),
}

/// One finished download, as the join set reports it.
type MediaTaskResult =
    Result<(tokio::task::Id, (SmolStr, Result<PathBuf, MediaError>)), tokio::task::JoinError>;

/// Why a stream stopped.
enum StreamExit {
    Cancelled,
    Failed(TweetStreamError),
}

/// Publishes the timeline and the pictures it wants, one revision newer.
fn publish_update<T>(
    publisher: &StreamPublisher<TweetProtocol>,
    state: &mut TweetState,
    media: &MediaSet<T>,
    revision: &mut u64,
) -> Result<(), StreamPublishError>
where
    T: MediaTransport,
{
    *revision = revision.saturating_add(1);
    publisher.publish_latest(TweetStreamMessage::Update(update(*revision, state, media)))
}

/// The message one frame draws.
fn update<T>(revision: u64, state: &mut TweetState, media: &MediaSet<T>) -> TweetUpdate
where
    T: MediaTransport,
{
    TweetUpdate {
        revision,
        feed: state.snapshot(),
        media: media.states(),
    }
}

/// Applies one subscription status, publishing what it means to the pane.
///
/// Returns whether the stream may keep running: a status that ended it has already published
/// why, and the task settles on the pane that will replace it.
async fn apply_subscription_status(
    publisher: &StreamPublisher<TweetProtocol>,
    status: &SubscriptionStatus,
    published: &mut TweetStatus,
) -> bool {
    if let Some(error) = ended_subscription(status) {
        publish_failure(publisher, error).await;
        return false;
    }
    match status {
        // A closing subscription is on its way to a terminal status of its own, and that is
        // where the failure is reported from.
        SubscriptionStatus::Closing => true,
        SubscriptionStatus::Connecting | SubscriptionStatus::Reconnecting => {
            publish_change(publisher, TweetStatus::Connecting, published)
                .await
                .is_ok()
        }
        // The timeline was published with the history it was read from, so being live is all
        // that is left to say: a reconnect does not make the tweets a pane already has new.
        SubscriptionStatus::Active => publish_change(publisher, TweetStatus::Ready, published)
            .await
            .is_ok(),
        SubscriptionStatus::Closed | SubscriptionStatus::Failed(_) => false,
    }
}

/// Why a subscription is no longer one a tweet may be read from.
fn ended_subscription(status: &SubscriptionStatus) -> Option<TweetStreamError> {
    match status {
        SubscriptionStatus::Closed => Some(TweetStreamError::Transport(TransportError::Closed)),
        SubscriptionStatus::Failed(error) => Some(TweetStreamError::Transport(error.clone())),
        SubscriptionStatus::Connecting
        | SubscriptionStatus::Active
        | SubscriptionStatus::Reconnecting
        | SubscriptionStatus::Closing => None,
    }
}

/// Publishes a status the pane has not been told yet.
async fn publish_status(
    publisher: &StreamPublisher<TweetProtocol>,
    status: TweetStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(TweetStreamMessage::Status(status))
        .await
}

/// Publishes a status change, if it is one.
async fn publish_change(
    publisher: &StreamPublisher<TweetProtocol>,
    status: TweetStatus,
    published: &mut TweetStatus,
) -> Result<(), StreamPublishError> {
    if *published == status {
        return Ok(());
    }
    publish_status(publisher, status.clone()).await?;
    *published = status;
    Ok(())
}

/// Publishes the failure a stream ended with, and holds the account it took.
///
/// A terminal failure is not retried: the pane asks for a new stream when the user takes the
/// recovery, and until then this task stays where it is so the framework still sees the
/// account as taken — and so the accounts it monitors can still be read and changed, which is
/// the other half of what the session was taken for.
async fn publish_failure(publisher: &StreamPublisher<TweetProtocol>, error: TweetStreamError) {
    let _ = publish_status(publisher, TweetStatus::Failed(error)).await;
    publisher.cancelled().await;
}

/// The pictures one stream is fetching, and what it knows about each.
struct MediaSet<T> {
    downloader: MediaDownloader<T>,
    cancellation: CancellationToken,
    states: HashMap<SmolStr, MediaState>,
    /// The pictures the tweets on screen want.
    timeline: HashSet<SmolStr>,
    /// The pictures the pane asked for on a dialog's behalf.
    auxiliary: HashSet<SmolStr>,
    pending: VecDeque<SmolStr>,
    active: JoinSet<(SmolStr, Result<PathBuf, MediaError>)>,
    jobs: HashMap<tokio::task::Id, SmolStr>,
}

impl<T> MediaSet<T>
where
    T: MediaTransport,
{
    fn new(downloader: MediaDownloader<T>, cancellation: CancellationToken) -> Self {
        Self {
            downloader,
            cancellation,
            states: HashMap::new(),
            timeline: HashSet::new(),
            auxiliary: HashSet::new(),
            pending: VecDeque::new(),
            active: JoinSet::new(),
            jobs: HashMap::new(),
        }
    }

    /// Replaces the timeline's own pictures, returning whether anything changed.
    fn reconcile(&mut self, urls: Vec<SmolStr>) -> bool {
        self.timeline = urls.into_iter().collect();
        self.refresh()
    }

    /// Replaces the pictures a dialog asked for, returning whether anything changed.
    fn replace_auxiliary(&mut self, urls: Arc<[SmolStr]>) -> bool {
        self.auxiliary = urls.iter().cloned().collect();
        self.refresh()
    }

    /// Forgets what nothing wants any more and queues what is new.
    fn refresh(&mut self) -> bool {
        let mut wanted = HashSet::with_capacity(self.timeline.len() + self.auxiliary.len());
        wanted.extend(self.timeline.iter().cloned());
        wanted.extend(self.auxiliary.iter().cloned());

        let mut changed = false;
        let known = self.states.len();
        self.states.retain(|url, _| wanted.contains(url));
        changed |= self.states.len() != known;
        self.pending.retain(|url| wanted.contains(url));

        // A queued picture is one whose state is loading, so nothing is fetched twice: what a
        // picture that is already on its way needs is nothing.
        for url in wanted {
            if !self.states.contains_key(&url) {
                self.states.insert(url.clone(), MediaState::Loading);
                self.pending.push_back(url);
                changed = true;
            }
        }
        changed
    }

    /// Queues one failed picture again.
    fn retry(&mut self, url: &str) -> bool {
        if self.states.get(url) != Some(&MediaState::Failed) {
            return false;
        }
        self.states.insert(SmolStr::new(url), MediaState::Loading);
        self.pending.push_back(SmolStr::new(url));
        true
    }

    /// Starts as many downloads as the concurrency allows.
    fn fill(&mut self) {
        while self.active.len() < MEDIA_CONCURRENCY {
            let Some(url) = self.pending.pop_front() else {
                break;
            };
            if !self.states.contains_key(&url) {
                continue;
            }
            let downloader = self.downloader.clone();
            let cancellation = self.cancellation.clone();
            let job = url.clone();
            let task = self.active.spawn(async move {
                let deadline = Instant::now() + MEDIA_OPERATION_TIMEOUT;
                let result = downloader.fetch(&job, deadline, &cancellation).await;
                (job, result)
            });
            self.jobs.insert(task.id(), url);
        }
    }

    /// Files one finished download, returning whether it changed what a frame draws.
    ///
    /// A download nobody wants any more is dropped: the timeline moved on while it was in
    /// flight, and what it fetched is not a picture any tweet on screen names.
    fn finish(&mut self, result: MediaTaskResult) -> bool {
        let (id, outcome) = match result {
            Ok((id, outcome)) => (id, Some(outcome)),
            Err(error) => (error.id(), None),
        };
        let Some(known) = self.jobs.get(&id) else {
            return false;
        };
        let url = match outcome.as_ref() {
            Some((url, _)) => url.clone(),
            None => {
                // A task that panicked left no picture behind, and the URL it was for is the
                // one this stream recorded when it started it.
                log::debug!("a picture of {known} did not finish");
                known.clone()
            }
        };
        self.jobs.remove(&id);
        if !self.states.contains_key(&url) {
            return false;
        }
        let state = match outcome {
            Some((_, Ok(path))) => MediaState::Ready(path),
            Some((_, Err(error))) => {
                log::debug!("the picture of {url} could not be fetched: {error}");
                MediaState::Failed
            }
            None => MediaState::Failed,
        };
        self.states.insert(url, state);
        true
    }

    /// Every picture state, as one shared map for the frame that draws them.
    fn states(&self) -> MediaStates {
        Arc::new(self.states.clone())
    }

    /// Gives up on everything still running.
    async fn shutdown(&mut self) {
        self.cancellation.cancel();
        while self.active.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {

    /// The handle of the runtime this test runs on.
    ///
    /// A test drives its subject on its own runtime rather than on a shared one. The
    /// clock these tests read is the runtime's: `#[tokio::test(start_paused = true)]`
    /// pauses it and `tokio::time::advance` moves it, and both only affect the runtime
    /// whose clock was paused. Work started on another runtime keeps waiting on real
    /// time while the test advances a clock it never reads.
    #[allow(
        clippy::disallowed_methods,
        reason = "a test drives its subject on its own runtime so the paused clock it advances is the one the subject reads"
    )]
    fn current_test_handle() -> &'static tokio::runtime::Handle {
        Box::leak(Box::new(tokio::runtime::Handle::current()))
    }
    use std::time::Duration;

    use widget::{
        rpc::RpcCallError,
        runtime::WidgetRuntime,
        stream::{LatestStream, ReliableStream},
    };

    use super::*;
    use crate::{
        media::MediaCache,
        model::MAX_TWEETS,
        test_support::{
            ACCOUNT, FakeSession, FakeSource, FakeTransport, media_cache, media_url, profile,
            search_profile, seed_picture, selection, selections, tweet, tweet_with_media,
        },
    };

    fn account(name: &str) -> TweetScope {
        TweetScope::new(Some(&SmolStr::new(name))).expect("an account should be a scope")
    }

    fn scope() -> TweetScope {
        account(ACCOUNT)
    }

    fn runtime(
        source: &FakeSource,
        transport: FakeTransport,
        cache: MediaCache,
    ) -> WidgetRuntime<TweetProtocol> {
        WidgetRuntime::spawn(
            current_test_handle(),
            TweetService::new(
                source.clone(),
                MediaDownloader::with_cache(transport, cache),
            ),
        )
    }

    async fn load(runtime: &WidgetRuntime<TweetProtocol>) {
        let response = runtime
            .client()
            .call(TweetRequest::Load(scope()))
            .await
            .expect("the stream should start")
            .into_value();
        assert_eq!(response, TweetResponse::StreamStarted);
    }

    async fn fetch(
        runtime: &WidgetRuntime<TweetProtocol>,
        scope: TweetScope,
    ) -> Result<TweetResponse, RpcCallError<XAccountMonitoringApiError>> {
        runtime
            .client()
            .call(TweetRequest::FetchSelections(scope))
            .await
            .map(widget::rpc::RpcResponse::into_value)
    }

    async fn call(
        runtime: &WidgetRuntime<TweetProtocol>,
        request: TweetRequest,
    ) -> Result<TweetResponse, RpcCallError<XAccountMonitoringApiError>> {
        runtime
            .client()
            .call(request)
            .await
            .map(widget::rpc::RpcResponse::into_value)
    }

    fn usernames(response: &TweetResponse) -> Vec<String> {
        let TweetResponse::Selections(data) = response else {
            panic!("the answer should be the monitored accounts");
        };
        data.selections()
            .iter()
            .map(|selection| {
                selection.profile.as_ref().map_or_else(
                    || selection.x_user_id.to_string(),
                    |p| p.username.to_string(),
                )
            })
            .collect()
    }

    async fn next_reliable(
        reliable: &mut ReliableStream<TweetProtocol>,
        predicate: impl Fn(&TweetStreamMessage) -> bool,
    ) -> TweetStreamMessage {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let envelope = reliable
                    .recv()
                    .await
                    .expect("the reliable stream should stay open");
                if predicate(envelope.message().as_ref()) {
                    return envelope.message().as_ref().clone();
                }
            }
        })
        .await
        .expect("the expected reliable state should arrive")
    }

    async fn receive_status(reliable: &mut ReliableStream<TweetProtocol>, expected: TweetStatus) {
        let status = next_reliable(
            reliable,
            |message| matches!(message, TweetStreamMessage::Status(status) if *status == expected),
        )
        .await;
        assert!(matches!(status, TweetStreamMessage::Status(_)));
    }

    async fn receive_failure(reliable: &mut ReliableStream<TweetProtocol>) -> TweetStreamError {
        let failure = next_reliable(reliable, |message| {
            matches!(message, TweetStreamMessage::Status(TweetStatus::Failed(_)))
        })
        .await;
        let TweetStreamMessage::Status(TweetStatus::Failed(error)) = failure else {
            panic!("the stream should have reported a failure");
        };
        error
    }

    async fn next_update(
        latest: &mut LatestStream<TweetProtocol>,
        predicate: impl Fn(&TweetUpdate) -> bool,
    ) -> TweetUpdate {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                latest
                    .changed()
                    .await
                    .expect("the latest stream should stay open");
                while let Some(envelope) = latest.next_unseen() {
                    if let TweetStreamMessage::Update(update) = envelope.message().as_ref()
                        && predicate(update)
                    {
                        return update.clone();
                    }
                }
            }
        })
        .await
        .expect("the expected update should arrive")
    }

    fn contents(update: &TweetUpdate) -> Vec<String> {
        update
            .feed()
            .items()
            .iter()
            .map(|item| item.tweet_id().to_owned())
            .collect()
    }

    /// The picture state one URL is in, if it is one the stream is tracking.
    fn picture(update: &TweetUpdate, url: &str) -> Option<MediaState> {
        update.media().get(url).cloned()
    }

    /// A running service with one account taken and its subscription live.
    async fn live(
        source: &FakeSource,
        transport: FakeTransport,
        cache: MediaCache,
    ) -> (
        WidgetRuntime<TweetProtocol>,
        ReliableStream<TweetProtocol>,
        LatestStream<TweetProtocol>,
    ) {
        let mut runtime = runtime(source, transport, cache);
        let reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let latest = runtime.latest_stream();
        load(&runtime).await;
        (runtime, reliable, latest)
    }

    /// A session, its subscription and a live service over both.
    async fn subscribed(
        transport: FakeTransport,
        cache: MediaCache,
    ) -> (
        FakeSource,
        FakeSession,
        crate::test_support::FakeSubscriptionControl,
        WidgetRuntime<TweetProtocol>,
        ReliableStream<TweetProtocol>,
        LatestStream<TweetProtocol>,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let (runtime, reliable, latest) = live(&source, transport, cache).await;
        (source, session, control, runtime, reliable, latest)
    }

    #[tokio::test]
    async fn the_stream_should_publish_history_and_call_itself_live() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(
            vec![tweet("1", 1), tweet("2", 2)],
            vec![profile("42", "kytrix")],
        );
        let (_runtime, mut reliable, mut latest) =
            live(&source, FakeTransport::default(), media_cache().0).await;

        receive_status(&mut reliable, TweetStatus::Ready).await;
        let update = latest
            .next_unseen()
            .expect("a live timeline is published with the readiness it comes with");
        let TweetStreamMessage::Update(update) = update.message().as_ref() else {
            panic!("the latest stream should hold the timeline");
        };
        assert_eq!(contents(update), ["2", "1"]);

        control
            .events
            .send(Ok(
                XAccountMonitoringSubscriptionEvent::XAccountMonitoringTweet(tweet("3", 3)),
            ))
            .await
            .expect("the live subscription should accept a tweet");
        let update = next_update(&mut latest, |update| contents(update).len() == 3).await;
        assert_eq!(contents(&update), ["3", "2", "1"]);
    }

    #[tokio::test]
    async fn a_history_that_could_not_be_read_should_fail_the_stream() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        session.subscribe();
        session.refuse_history(XAccountMonitoringApiError::Closed);

        let (_runtime, mut reliable, _) =
            live(&source, FakeTransport::default(), media_cache().0).await;

        assert_eq!(
            receive_failure(&mut reliable).await,
            TweetStreamError::Api(XAccountMonitoringApiError::Closed)
        );
    }

    #[tokio::test]
    async fn a_subscription_that_could_not_be_taken_should_fail_the_stream() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        session.refuse_subscription(KytrixServiceError::Timeout);

        let (_runtime, mut reliable, _) =
            live(&source, FakeTransport::default(), media_cache().0).await;

        assert_eq!(
            receive_failure(&mut reliable).await,
            TweetStreamError::Service(KytrixServiceError::Timeout)
        );
    }

    #[tokio::test]
    async fn an_account_the_pane_has_left_should_be_refused_rather_than_answered() {
        let (_, session, _control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::default(), media_cache().0).await;
        session.answer_selections(selections(vec![selection("1", "one")], 3));

        let refused = fetch(&runtime, account("another-account")).await;

        match refused.expect_err("another account is not this one") {
            RpcCallError::Service { source, .. } => {
                assert_eq!(source, XAccountMonitoringApiError::AccountChanged);
            }
            other => panic!("the refusal should be the account, not {other:?}"),
        }
        // The history was read by the stream; what the other account's request did not do is
        // reach the session at all.
        let counts = session.state();
        assert_eq!(counts.history_reads, 1);
        assert_eq!(counts.fetches, 0);
    }

    #[tokio::test]
    async fn a_read_should_be_answered_with_the_accounts_the_service_was_given() {
        let (_, session, _control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::default(), media_cache().0).await;
        session.answer_selections(selections(
            vec![selection("1", "one"), selection("2", "two")],
            5,
        ));

        let response = fetch(&runtime, scope())
            .await
            .expect("the read should answer");

        assert_eq!(usernames(&response), ["one", "two"]);
        let TweetResponse::Selections(data) = response else {
            panic!("the answer should be the monitored accounts");
        };
        assert_eq!(data.limit(), 5);
        assert!(!data.quota_full());
    }

    #[tokio::test]
    async fn a_change_should_be_answered_with_what_became_of_it() {
        let (_, session, _control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::default(), media_cache().0).await;
        session.answer_add(XAccountMonitoringMutationOutcome::Confirmed(selection(
            "1", "one",
        )));
        session.answer_delete(XAccountMonitoringMutationOutcome::Confirmed(()));

        let added = call(
            &runtime,
            TweetRequest::AddSelection {
                scope: scope(),
                username: SmolStr::new_static("one"),
            },
        )
        .await
        .expect("the change should be answered");
        assert!(matches!(
            added,
            TweetResponse::Added(XAccountMonitoringMutationOutcome::Confirmed(_))
        ));

        let deleted = call(
            &runtime,
            TweetRequest::DeleteSelection {
                scope: scope(),
                x_user_id: SmolStr::new_static("1"),
            },
        )
        .await
        .expect("the change should be answered");
        assert_eq!(
            deleted,
            TweetResponse::Deleted(XAccountMonitoringMutationOutcome::Confirmed(()))
        );
    }

    #[tokio::test]
    async fn a_change_that_is_not_one_should_be_refused_without_asking() {
        let (_, session, _control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::default(), media_cache().0).await;

        let added = call(
            &runtime,
            TweetRequest::AddSelection {
                scope: scope(),
                username: SmolStr::new_static("not a username"),
            },
        )
        .await
        .expect("the refusal should be an answer");
        assert_eq!(
            added,
            TweetResponse::Added(XAccountMonitoringMutationOutcome::Rejected(
                XAccountMonitoringApiError::InvalidUsername
            ))
        );

        let deleted = call(
            &runtime,
            TweetRequest::DeleteSelection {
                scope: scope(),
                x_user_id: SmolStr::new_static("not an id"),
            },
        )
        .await
        .expect("the refusal should be an answer");
        assert!(matches!(
            deleted,
            TweetResponse::Deleted(XAccountMonitoringMutationOutcome::Rejected(
                XAccountMonitoringApiError::Protocol(_)
            ))
        ));
        assert_eq!(session.state().add_calls, 0);
        assert_eq!(session.state().delete_calls, 0);
    }

    #[tokio::test]
    async fn a_search_should_be_answered_and_its_results_be_the_pictures_to_fetch() {
        let (_, session, _control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::failing(), media_cache().0).await;
        session.answer_search(vec![search_profile("7", "seven")]);

        let response = call(
            &runtime,
            TweetRequest::SearchUsers {
                scope: scope(),
                query: SmolStr::new_static("seven"),
            },
        )
        .await
        .expect("the search should be answered");

        let TweetResponse::SearchResults(profiles) = response else {
            panic!("the answer should be the search results");
        };
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].username, "seven");
    }

    #[tokio::test]
    async fn the_monitored_avatars_should_be_fetched_only_while_a_dialog_shows_them() {
        let (cache, _root) = media_cache();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(vec![tweet("1", 1)], vec![profile("42", "kytrix")]);
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        let (runtime, _reliable, mut latest) =
            live(&source, FakeTransport::blocking(), cache).await;
        let avatar = "https://pbs.twimg.com/seven.jpg";

        // The timeline wants the author's face, and its own update is where that shows.
        next_update(&mut latest, |update| {
            picture(update, "https://pbs.twimg.com/kytrix.jpg").is_some()
        })
        .await;

        // Reading the monitored accounts does not make their faces pictures any frame wants:
        // nothing is showing them.
        fetch(&runtime, scope())
            .await
            .expect("the read should answer");
        control
            .events
            .send(Ok(
                XAccountMonitoringSubscriptionEvent::XAccountMonitoringTweet(tweet("2", 2)),
            ))
            .await
            .expect("the subscription should accept a tweet");
        let update = next_update(&mut latest, |update| contents(update).len() == 2).await;
        assert_eq!(picture(&update, avatar), None);

        call(
            &runtime,
            TweetRequest::WatchSelections {
                scope: scope(),
                watched: true,
            },
        )
        .await
        .expect("watching should be acknowledged");
        let update = next_update(&mut latest, |update| picture(update, avatar).is_some()).await;
        assert!(
            matches!(picture(&update, avatar), Some(MediaState::Loading)),
            "a watched avatar is a picture the stream is fetching"
        );

        call(
            &runtime,
            TweetRequest::WatchSelections {
                scope: scope(),
                watched: false,
            },
        )
        .await
        .expect("watching should be acknowledged");
        let update = next_update(&mut latest, |update| {
            picture(update, avatar).is_none()
                && picture(update, "https://pbs.twimg.com/kytrix.jpg").is_some()
        })
        .await;
        assert!(contents(&update).contains(&"1".to_owned()));
    }

    #[tokio::test]
    async fn a_picture_on_disk_should_be_published_as_ready_without_a_fetch() {
        let (cache, _root) = media_cache();
        let url = media_url("one");
        let path = seed_picture(&cache, &url, b"image").await;
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(
            vec![tweet_with_media("1", 1, url.as_str())],
            vec![profile("42", "kytrix")],
        );
        let (_runtime, _reliable, mut latest) =
            live(&source, FakeTransport::default(), cache).await;

        let update = next_update(&mut latest, |update| {
            picture(update, url.as_str()) == Some(MediaState::Ready(path.clone()))
        })
        .await;

        assert_eq!(contents(&update), ["1"]);
        let _ = control;
    }

    #[tokio::test]
    async fn a_picture_that_could_not_be_fetched_should_be_published_as_failed() {
        let (cache, _root) = media_cache();
        let url = media_url("two");
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(
            vec![tweet_with_media("1", 1, url.as_str())],
            vec![profile("42", "kytrix")],
        );
        let (_runtime, _reliable, mut latest) =
            live(&source, FakeTransport::failing(), cache).await;

        let update = next_update(&mut latest, |update| {
            picture(update, url.as_str()) == Some(MediaState::Failed)
        })
        .await;

        assert_eq!(contents(&update), ["1"]);
        let _ = control;
    }

    #[tokio::test]
    async fn a_retry_should_be_refused_for_a_picture_no_tweet_wants() {
        let (_, _session, _control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::default(), media_cache().0).await;

        let refused = call(
            &runtime,
            TweetRequest::RetryMedia {
                scope: scope(),
                url: SmolStr::new_static("https://example.com/not-a-picture.jpg"),
            },
        )
        .await;

        match refused.expect_err("a URL this pane may not fetch is not a picture") {
            RpcCallError::Service { source, .. } => {
                assert!(matches!(source, XAccountMonitoringApiError::Protocol(_)));
            }
            other => panic!("the refusal should be a protocol error, not {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failed_picture_should_be_fetched_again_when_the_pane_asks() {
        let (cache, _root) = media_cache();
        let url = media_url("three");
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(
            vec![tweet_with_media("1", 1, url.as_str())],
            vec![profile("42", "kytrix")],
        );
        // The transport refuses and the cache is empty, so the first fetch fails. The picture
        // is put on disk afterwards, which is what a retry then finds.
        let (runtime, _reliable, mut latest) =
            live(&source, FakeTransport::failing(), cache.clone()).await;
        next_update(&mut latest, |update| {
            picture(update, url.as_str()) == Some(MediaState::Failed)
        })
        .await;

        seed_picture(&cache, &url, b"image").await;
        call(
            &runtime,
            TweetRequest::RetryMedia {
                scope: scope(),
                url: SmolStr::new(url.as_str()),
            },
        )
        .await
        .expect("the retry should be accepted");

        next_update(&mut latest, |update| {
            matches!(picture(update, url.as_str()), Some(MediaState::Ready(_)))
        })
        .await;
        let _ = control;
    }

    #[tokio::test]
    async fn a_download_nobody_wants_any_more_should_be_forgotten() {
        let (cache, _root) = media_cache();
        let url = media_url("four");
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(
            vec![tweet_with_media("1", 1, url.as_str())],
            vec![profile("42", "kytrix")],
        );
        let (_runtime, _reliable, mut latest) =
            live(&source, FakeTransport::blocking(), cache).await;
        // A picture that is still being fetched is one the stream is tracking.
        next_update(&mut latest, |update| {
            picture(update, url.as_str()) == Some(MediaState::Loading)
        })
        .await;

        // A timeline that fills up and keeps going drops its oldest tweet, which takes that
        // tweet's picture with it.
        for index in 2..MAX_TWEETS + 2 {
            control
                .events
                .send(Ok(
                    XAccountMonitoringSubscriptionEvent::XAccountMonitoringTweet(tweet(
                        &index.to_string(),
                        index as i64,
                    )),
                ))
                .await
                .expect("the subscription should accept a tweet");
        }
        let update = next_update(&mut latest, |update| {
            !contents(update).contains(&"1".to_owned())
        })
        .await;

        assert_eq!(picture(&update, url.as_str()), None);
    }

    #[tokio::test]
    async fn a_replaced_stream_should_let_go_of_its_subscription() {
        let (source, session, control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::default(), media_cache().0).await;

        session.answer_history(Vec::new(), vec![profile("42", "kytrix")]);
        session.subscribe();
        // The account is taken again for the replacement, which is a second session of it.
        let replacement = source.session(ACCOUNT);
        replacement.answer_history(Vec::new(), vec![profile("42", "kytrix")]);
        replacement.subscribe();
        call(&runtime, TweetRequest::Retry(scope()))
            .await
            .expect("the retry should start a stream");

        tokio::time::timeout(Duration::from_secs(2), async {
            while control.drops.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the replaced subscription should be dropped");
    }

    #[tokio::test]
    async fn stopping_should_let_go_of_the_account_and_answer() {
        let (_, _session, control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::default(), media_cache().0).await;

        let stopped = call(&runtime, TweetRequest::Stop)
            .await
            .expect("stopping should be answered");
        assert_eq!(stopped, TweetResponse::Stopped);

        tokio::time::timeout(Duration::from_secs(2), async {
            while control.drops.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the stopped subscription should be dropped");
    }

    #[tokio::test]
    async fn a_subscription_that_reconnects_should_report_itself_connecting_again() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(Vec::new(), vec![profile("42", "kytrix")]);
        let (_runtime, mut reliable, _latest) =
            live(&source, FakeTransport::default(), media_cache().0).await;
        receive_status(&mut reliable, TweetStatus::Ready).await;

        control
            .status
            .send_replace(SubscriptionStatus::Reconnecting);
        receive_status(&mut reliable, TweetStatus::Connecting).await;

        control.status.send_replace(SubscriptionStatus::Active);
        receive_status(&mut reliable, TweetStatus::Ready).await;
    }

    #[tokio::test]
    async fn a_credential_change_already_on_the_wire_should_end_the_stream() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(Vec::new(), vec![profile("42", "kytrix")]);
        let (_runtime, mut reliable, _latest) =
            live(&source, FakeTransport::default(), media_cache().0).await;
        receive_status(&mut reliable, TweetStatus::Ready).await;

        control.fail_on_delivery(TransportError::CredentialChanged);
        control
            .events
            .send(Ok(
                XAccountMonitoringSubscriptionEvent::XAccountMonitoringTweet(tweet("1", 1)),
            ))
            .await
            .expect("the subscription should accept a tweet");

        assert_eq!(
            receive_failure(&mut reliable).await,
            TweetStreamError::Transport(TransportError::CredentialChanged)
        );
    }

    #[tokio::test]
    async fn an_acquisition_nobody_waits_for_any_more_should_be_dropped() {
        let source = FakeSource::default();
        let block = source.block();
        let runtime = runtime(&source, FakeTransport::default(), media_cache().0);
        let client = runtime.client();
        let call = tokio::spawn(async move { client.call(TweetRequest::Load(scope())).await });

        block.wait_started().await;
        call.abort();
        block.wait_dropped().await;
    }
}

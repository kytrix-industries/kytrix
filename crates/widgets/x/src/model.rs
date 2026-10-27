//! The tweets a pane holds, the authors they are read with, and how a newer snapshot changes
//! the rows a list already holds.
//!
//! # A feed is ordered, bounded and shared
//!
//! The service publishes tweets in the order a timeline reads them: newest first, and for two
//! tweets stamped in the same second the larger id first, which is the order X itself numbers
//! them in. A row is an [`Arc`] shared with every snapshot that holds it, so a snapshot is a
//! slice of pointers and two snapshots that differ by one tweet share every other row. What
//! bounds the feed is [`MAX_TWEETS`]: a timeline is what a reader is looking at, not an
//! archive, and the service publishes history rather than a growing log.
//!
//! # Authors are resolved separately from tweets
//!
//! A tweet names its author by id, and the profile that id stands for arrives as its own
//! event with a revision: the same author can be corrected while a tweet is on screen, and a
//! tweet whose author is not known yet still has to be drawn. So the snapshot carries both
//! and the view resolves one against the other.
//!
//! # Display text is derived where it is needed
//!
//! Nothing here formats anything. A timestamp is turned into an *age* ([`TweetAge`]) and the
//! view names that age in the reader's language, because the same feed is read in every
//! language the application ships and an age is only true for the moment it is asked.

use std::{collections::VecDeque, ops::Range, path::PathBuf, sync::Arc};

use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use gpui_kit::SharedString;
use kytrix_service_provider::service::{
    engine::{TweetReference, XProfileEvent, XTweetEvent, XTweetMedia, XTweetTextSegment},
    x::{MAX_X_SEARCH_PROFILES, MAX_X_SELECTIONS, XSelection, XSelectionsResponse},
};
use smol_str::{SmolStr, format_smolstr};
use time::{OffsetDateTime, UtcOffset};

/// The newest tweets a pane keeps.
pub(super) const MAX_TWEETS: usize = 50;

/// How many authors one tweet can name: itself, and the tweet it reposts or quotes with that
/// tweet's own reference.
const MAX_TWEET_AUTHORS: usize = 3;

/// Every URL one stream can be asked for media of: the timeline's own images and avatars, the
/// avatars of the monitored accounts, and those of the search results a dialog shows.
pub(super) const MAX_MEDIA_URLS: usize =
    MAX_TWEETS * MAX_TWEET_AUTHORS * 5 + MAX_X_SELECTIONS + MAX_X_SEARCH_PROFILES;

/// What is known about one URL the pane wants a picture of.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MediaState {
    /// The picture is being fetched.
    Loading,
    /// The picture is on disk, at this path.
    Ready(PathBuf),
    /// The picture could not be fetched.
    Failed,
}

/// Every picture state the pane holds, shared with the frame that draws them.
pub(super) type MediaStates = Arc<HashMap<SmolStr, MediaState>>;

/// One author a tweet is read with.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TweetAuthor {
    pub username: SharedString,
    pub display_name: SharedString,
    pub avatar_url: SharedString,
    pub verified: bool,
    revision: u64,
}

impl From<XProfileEvent> for TweetAuthor {
    fn from(profile: XProfileEvent) -> Self {
        Self {
            username: SharedString::new(profile.username.as_str()),
            display_name: SharedString::new(profile.display_name.as_str()),
            avatar_url: SharedString::new(profile.avatar_url.as_str()),
            verified: profile.verified,
            revision: profile.revision,
        }
    }
}

impl TweetAuthor {
    /// The author a tweet is drawn with before its profile has arrived.
    ///
    /// The id is all that is known, so it is what the row says: an author that has not been
    /// read yet is still an author, and a row that waited for one would be a hole in the feed.
    pub(super) fn fallback(x_user_id: &str) -> Self {
        Self {
            username: SharedString::new(x_user_id),
            display_name: SharedString::new(format_smolstr!("@{x_user_id}").as_str()),
            avatar_url: SharedString::new_static(""),
            verified: false,
            revision: 0,
        }
    }
}

/// The tweet a repost or a quote carries.
#[derive(Debug)]
pub(super) enum TweetReferenceItem {
    Repost(Arc<TweetItem>),
    Quote(Arc<TweetItem>),
}

/// One picture attached to a tweet.
#[derive(Debug)]
pub(super) enum TweetMediaItem {
    Image {
        image_url: SharedString,
        width: u32,
        height: u32,
    },
    /// A video, of which only the still frame is ever fetched.
    Video {
        thumbnail_url: SharedString,
        width: u32,
        height: u32,
    },
    Gif {
        image_url: SharedString,
        width: u32,
        height: u32,
        /// Whether the picture is a still frame standing in for an animation that is not
        /// fetched, which is what the tile is labelled for.
        poster_only: bool,
    },
}

impl TweetMediaItem {
    /// The shape of the picture, which is what a gallery lays its tiles out from.
    pub(super) fn aspect_ratio(&self) -> f32 {
        let (width, height) = match self {
            Self::Image { width, height, .. }
            | Self::Video { width, height, .. }
            | Self::Gif { width, height, .. } => (*width, *height),
        };
        if width == 0 || height == 0 {
            16.0 / 9.0
        } else {
            width as f32 / height as f32
        }
    }

    /// The URL this item is fetched from, which for a video is its still frame.
    pub(super) fn url(&self) -> &SharedString {
        match self {
            Self::Image { image_url, .. } | Self::Gif { image_url, .. } => image_url,
            Self::Video { thumbnail_url, .. } => thumbnail_url,
        }
    }
}

impl From<XTweetMedia> for TweetMediaItem {
    fn from(media: XTweetMedia) -> Self {
        match media {
            XTweetMedia::Image {
                image_url,
                width,
                height,
            } => Self::Image {
                image_url: SharedString::new(image_url.as_str()),
                width,
                height,
            },
            XTweetMedia::Video {
                thumbnail_url,
                width,
                height,
                ..
            } => Self::Video {
                thumbnail_url: SharedString::new(thumbnail_url.as_str()),
                width,
                height,
            },
            XTweetMedia::Gif {
                animation_url,
                image_url,
                width,
                height,
            } => Self::Gif {
                poster_only: animation_url != image_url && !is_animated_image_url(&image_url),
                image_url: SharedString::new(image_url.as_str()),
                width,
                height,
            },
        }
    }
}

/// One run of a tweet's text that stands for a link.
#[derive(Debug)]
pub(super) struct TweetTextLink {
    /// The byte range of the link within the tweet's whole text.
    pub range: Range<usize>,
    pub url: SharedString,
}

/// The text of a tweet, joined into one string with the ranges that are links.
///
/// The segments arrive as the service decoded them — a mention, a link and plain prose are
/// each their own segment — and are joined here because a reader reads one paragraph: a row
/// built from one element per segment cannot wrap a line in the middle of a sentence.
#[derive(Debug)]
pub(super) struct TweetText {
    text: SharedString,
    links: Box<[TweetTextLink]>,
}

impl TweetText {
    fn from_segments(segments: Vec<XTweetTextSegment>) -> Self {
        let mut text =
            String::with_capacity(segments.iter().map(|segment| segment.text.len()).sum());
        let mut links = Vec::with_capacity(segments.len());
        for segment in segments {
            let start = text.len();
            text.push_str(&segment.text);
            if let Some(url) = segment.url {
                links.push(TweetTextLink {
                    range: start..text.len(),
                    url: SharedString::new(url.as_str()),
                });
            }
        }
        Self {
            text: SharedString::new(text),
            links: links.into_boxed_slice(),
        }
    }

    /// The text itself, for a row that draws it as one paragraph.
    pub(super) fn shared(&self) -> SharedString {
        self.text.clone()
    }

    pub(super) fn links(&self) -> &[TweetTextLink] {
        &self.links
    }

    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// One tweet, as the pane holds it.
#[derive(Debug)]
pub(super) struct TweetItem {
    tweet_id: SmolStr,
    timestamp: i64,
    pub x_user_id: SmolStr,
    pub text: TweetText,
    pub reference: Option<TweetReferenceItem>,
    pub media: Box<[TweetMediaItem]>,
}

impl TweetItem {
    pub(super) fn tweet_id(&self) -> &str {
        &self.tweet_id
    }

    /// The tweet this row shows, and whether it is a repost of it.
    ///
    /// A repost is drawn as the tweet that was reposted — its text, its author, its pictures —
    /// under a line naming who reposted it, so the row's content is the inner tweet and the
    /// outer one only says that it happened.
    pub(super) fn displayed(&self) -> (&Self, bool) {
        match self.reference.as_ref() {
            Some(TweetReferenceItem::Repost(tweet)) => (tweet, true),
            Some(TweetReferenceItem::Quote(_)) | None => (self, false),
        }
    }

    /// How long ago this tweet was published, read in `zone`.
    pub(super) fn age(&self, now: OffsetDateTime, zone: UtcOffset) -> TweetAge {
        tweet_age(self.timestamp, now, zone)
    }

    /// When this tweet was published, read in `zone`, to the second.
    pub(super) fn published_at(&self, zone: UtcOffset) -> SharedString {
        absolute_time(self.timestamp, zone)
    }
}

impl From<XTweetEvent> for TweetItem {
    fn from(tweet: XTweetEvent) -> Self {
        Self {
            tweet_id: tweet.tweet_id,
            timestamp: tweet.timestamp,
            x_user_id: tweet.x_user_id,
            text: TweetText::from_segments(tweet.text_segments),
            media: tweet.media.into_iter().map(TweetMediaItem::from).collect(),
            reference: tweet.reference.map(|reference| match reference {
                TweetReference::Repost(tweet) => {
                    TweetReferenceItem::Repost(Arc::new(TweetItem::from(*tweet)))
                }
                TweetReference::Quote(tweet) => {
                    TweetReferenceItem::Quote(Arc::new(TweetItem::from(*tweet)))
                }
            }),
        }
    }
}

/// Every tweet a pane is showing, with the authors they are read with.
#[derive(Clone, Debug)]
pub(super) struct TweetSnapshot {
    items: Arc<[Arc<TweetItem>]>,
    profiles: Arc<HashMap<SmolStr, TweetAuthor>>,
}

impl TweetSnapshot {
    fn new(items: Arc<[Arc<TweetItem>]>, profiles: Arc<HashMap<SmolStr, TweetAuthor>>) -> Self {
        Self { items, profiles }
    }

    pub(super) fn items(&self) -> &[Arc<TweetItem>] {
        &self.items
    }

    pub(super) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The author of one id, or the stand-in a row uses until the profile arrives.
    pub(super) fn author(&self, x_user_id: &str) -> TweetAuthor {
        self.profiles
            .get(x_user_id)
            .cloned()
            .unwrap_or_else(|| TweetAuthor::fallback(x_user_id))
    }
}

/// The tweets a stream holds, at the order and bound a feed reads at.
pub(super) struct TweetState {
    items: VecDeque<Arc<TweetItem>>,
    profiles: HashMap<SmolStr, TweetAuthor>,
    /// The authors whose profile arrived before a tweet named them, newest last.
    ///
    /// A profile is only kept for an author a tweet names, and a profile that arrives first
    /// has no tweet to be kept for yet — so it is remembered here until one does, bounded to
    /// the authors a single tweet can name so a stream of profiles nothing refers to cannot
    /// grow.
    pending_profiles: VecDeque<SmolStr>,
    snapshot: Arc<[Arc<TweetItem>]>,
    snapshot_profiles: Arc<HashMap<SmolStr, TweetAuthor>>,
    dirty: bool,
}

impl TweetState {
    pub(super) fn new() -> Self {
        Self {
            items: VecDeque::with_capacity(MAX_TWEETS),
            profiles: HashMap::with_capacity(MAX_TWEETS * MAX_TWEET_AUTHORS),
            pending_profiles: VecDeque::with_capacity(MAX_TWEET_AUTHORS),
            snapshot: Arc::from([]),
            snapshot_profiles: Arc::new(HashMap::with_capacity(0)),
            dirty: false,
        }
    }

    /// Files one tweet, returning whether it is one the feed did not have.
    ///
    /// A tweet the feed already holds is dropped by id, and a tweet older than the oldest one
    /// a full feed holds is dropped as well: what a full feed keeps is the newest
    /// [`MAX_TWEETS`] of them, and one that belongs past the end is not one of those.
    pub(super) fn push(&mut self, tweet: XTweetEvent) -> bool {
        if self
            .items
            .iter()
            .any(|current| current.tweet_id == tweet.tweet_id)
        {
            return false;
        }
        let item = Arc::new(TweetItem::from(tweet));
        let insertion = self.items.iter().position(|current| {
            current.timestamp < item.timestamp
                || (current.timestamp == item.timestamp
                    && numeric_id_gt(item.tweet_id.as_str(), current.tweet_id.as_str()))
        });
        if self.items.len() == MAX_TWEETS && insertion.is_none() {
            return false;
        }
        if self.items.len() == MAX_TWEETS {
            self.items.pop_back();
        }
        match insertion {
            Some(index) => self.items.insert(index, item),
            None => self.items.push_back(item),
        }
        self.dirty = true;
        true
    }

    /// Files one author's profile, returning whether it is one the feed did not have.
    ///
    /// Profiles carry a revision, and a revision that is not newer than the one held is not
    /// an update: a stale correction arriving late must not replace a newer name. A revision
    /// of zero is the service saying it has no revision for this profile, which is what a
    /// profile read outside the timeline stream looks like, and one of those replaces
    /// whatever is held.
    pub(super) fn upsert_profile(&mut self, profile: XProfileEvent) -> bool {
        let x_user_id = profile.x_user_id.clone();
        let profile = TweetAuthor::from(profile);
        let evicted = if self
            .items
            .iter()
            .any(|tweet| contains_author(tweet, &x_user_id))
        {
            if let Some(index) = self
                .pending_profiles
                .iter()
                .position(|pending| pending == &x_user_id)
            {
                self.pending_profiles.remove(index);
            }
            false
        } else {
            self.track_pending_profile(&x_user_id)
        };
        if let Some(current) = self.profiles.get(&x_user_id)
            && (current == &profile
                || profile.revision < current.revision
                || (profile.revision != 0 && profile.revision == current.revision))
        {
            return evicted;
        }
        self.profiles.insert(x_user_id, profile);
        self.dirty = true;
        true
    }

    /// Keeps a profile nothing refers to yet, at the bound of one tweet's authors.
    fn track_pending_profile(&mut self, x_user_id: &SmolStr) -> bool {
        if let Some(index) = self
            .pending_profiles
            .iter()
            .position(|pending| pending == x_user_id)
        {
            self.pending_profiles.remove(index);
        }
        self.pending_profiles.push_back(x_user_id.clone());
        if self.pending_profiles.len() <= MAX_TWEET_AUTHORS {
            return false;
        }
        let Some(evicted) = self.pending_profiles.pop_front() else {
            return false;
        };
        if self.profiles.remove(&evicted).is_none() {
            return false;
        }
        self.dirty = true;
        true
    }

    /// Drops the authors no tweet on screen names any more.
    ///
    /// A timeline is bounded, so the authors of tweets that fell off its end are authors
    /// nothing reads, and keeping them would let a long session grow with profiles whose
    /// tweets are long gone.
    pub(super) fn prune_profiles(&mut self) -> bool {
        let mut visible = HashSet::with_capacity(MAX_TWEETS * MAX_TWEET_AUTHORS);
        for item in &self.items {
            collect_author_ids(item, &mut visible);
        }
        let previous_len = self.profiles.len();
        self.profiles
            .retain(|x_user_id, _| visible.contains(x_user_id.as_str()));
        self.pending_profiles.clear();
        if self.profiles.len() == previous_len {
            return false;
        }
        self.dirty = true;
        true
    }

    /// The tweets as one shared slice, which is only rebuilt when something changed.
    pub(super) fn snapshot(&mut self) -> TweetSnapshot {
        if self.dirty {
            let mut items = Vec::with_capacity(self.items.len());
            items.extend(self.items.iter().cloned());
            self.snapshot = items.into();
            self.snapshot_profiles = Arc::new(self.profiles.clone());
            self.dirty = false;
        }
        TweetSnapshot::new(self.snapshot.clone(), self.snapshot_profiles.clone())
    }

    /// The media URLs the tweets on screen want: every author's avatar and every picture.
    pub(super) fn media_urls(&self) -> Vec<SmolStr> {
        let mut urls = Vec::with_capacity(MAX_MEDIA_URLS);
        let mut seen = HashSet::with_capacity(MAX_MEDIA_URLS);
        for tweet in &self.items {
            collect_media_urls(tweet, &self.profiles, &mut urls, &mut seen);
            if urls.len() == MAX_MEDIA_URLS {
                break;
            }
        }
        urls
    }
}

/// Every author id one tweet names, including the tweet it carries.
fn collect_author_ids<'a>(tweet: &'a TweetItem, ids: &mut HashSet<&'a str>) {
    ids.insert(tweet.x_user_id.as_str());
    if let Some(TweetReferenceItem::Repost(tweet) | TweetReferenceItem::Quote(tweet)) =
        tweet.reference.as_ref()
    {
        collect_author_ids(tweet, ids);
    }
}

/// Whether one tweet names an author, directly or through the tweet it carries.
fn contains_author(tweet: &TweetItem, x_user_id: &str) -> bool {
    if tweet.x_user_id == x_user_id {
        return true;
    }
    match tweet.reference.as_ref() {
        Some(TweetReferenceItem::Repost(tweet) | TweetReferenceItem::Quote(tweet)) => {
            contains_author(tweet, x_user_id)
        }
        None => false,
    }
}

/// Every picture one tweet wants, resolving its authors against the profiles held.
fn collect_media_urls(
    tweet: &TweetItem,
    profiles: &HashMap<SmolStr, TweetAuthor>,
    urls: &mut Vec<SmolStr>,
    seen: &mut HashSet<SmolStr>,
) {
    if let Some(author) = profiles.get(&tweet.x_user_id) {
        push_media_url(author.avatar_url.as_ref(), urls, seen);
    }
    for media in &tweet.media {
        if urls.len() == MAX_MEDIA_URLS {
            return;
        }
        push_media_url(media.url().as_ref(), urls, seen);
    }
    if let Some(TweetReferenceItem::Repost(tweet) | TweetReferenceItem::Quote(tweet)) =
        tweet.reference.as_ref()
    {
        collect_media_urls(tweet, profiles, urls, seen);
    }
}

fn push_media_url(url: &str, urls: &mut Vec<SmolStr>, seen: &mut HashSet<SmolStr>) {
    if url.is_empty() || urls.len() == MAX_MEDIA_URLS || !crate::media::plausible_media_url(url) {
        return;
    }
    let url = SmolStr::new(url);
    if seen.insert(url.clone()) {
        urls.push(url);
    }
}

/// Two ids whose tweets share a second, ordered the way X numbers them.
///
/// Ids are decimal and grow with time, so the longer one is the larger one, and two of the
/// same length compare as text.
fn numeric_id_gt(left: &str, right: &str) -> bool {
    left.len() > right.len() || (left.len() == right.len() && left > right)
}

/// Whether a GIF's own URL is the animation rather than a still frame of it.
fn is_animated_image_url(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    let path = value.split(['?', '#']).next().unwrap_or_default();
    path.ends_with(".gif")
        || path.ends_with(".webp")
        || value.split_once('?').is_some_and(|(_, query)| {
            query
                .split('&')
                .any(|field| matches!(field.split_once('='), Some(("format", "gif" | "webp"))))
        })
}

/// How long ago a tweet was published, in the buckets a reader reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TweetAge {
    Seconds(u64),
    Minutes(u64),
    Hours(u64),
    /// Older than a day, within the same year: a date without a year.
    Date {
        month: u8,
        day: u8,
    },
    /// Older than a year: a date with one.
    FullDate {
        year: i32,
        month: u8,
        day: u8,
    },
    /// The service sent a timestamp that is not a time.
    Unknown,
}

/// How long ago a tweet stamped `timestamp` was published, as of `now`.
///
/// Both instants are UTC and the difference between them is what an age is, so the reader's
/// own clock never enters into it: a tweet is the same number of minutes old wherever it is
/// read. The date an age older than a day reads as is the day it was published *in `zone`*,
/// because that is the day the reader saw on their own clock.
pub(super) fn tweet_age(timestamp: i64, now: OffsetDateTime, zone: UtcOffset) -> TweetAge {
    let Ok(time) = OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp) * 1_000_000)
    else {
        return TweetAge::Unknown;
    };
    // A timestamp in the future is a clock that disagrees with the service rather than a
    // tweet from tomorrow, and the newest thing it can be is now.
    let seconds = (now - time).whole_seconds().max(0);
    // The offset can move an instant past the last date the calendar holds. Nothing an
    // age says would describe a date that is not one, so a dated age reads as the one
    // answer for a timestamp that is not a time. An age short enough to be a duration is
    // not read in any zone at all, so the offset never touches it.
    let published = || time.checked_to_offset(zone);
    match seconds {
        0..60 => TweetAge::Seconds(seconds.unsigned_abs()),
        60..3_600 => TweetAge::Minutes(seconds.unsigned_abs() / 60),
        3_600..86_400 => TweetAge::Hours(seconds.unsigned_abs() / 3_600),
        86_400..31_536_000 => published().map_or(TweetAge::Unknown, |published| TweetAge::Date {
            month: u8::from(published.month()),
            day: published.day(),
        }),
        _ => published().map_or(TweetAge::Unknown, |published| TweetAge::FullDate {
            year: published.year(),
            month: u8::from(published.month()),
            day: published.day(),
        }),
    }
}

/// When a tweet was published, read in `zone`, to the second.
pub(super) fn absolute_time(timestamp: i64, zone: UtcOffset) -> SharedString {
    // The offset can move an instant past the last date the calendar holds — a
    // timestamp at the far end of the range is one the service cannot publish, and
    // it is written as a placeholder rather than taken to a date that is not one.
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp) * 1_000_000)
        .ok()
        .and_then(|time| time.checked_to_offset(zone))
        .map_or_else(
            || SharedString::new_static("--"),
            |time| {
                SharedString::new(
                    format_smolstr!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                        time.year(),
                        u8::from(time.month()),
                        time.day(),
                        time.hour(),
                        time.minute(),
                        time.second()
                    )
                    .as_str(),
                )
            },
        )
}

/// The name one monitored account is known by, or `None` when nothing names it.
///
/// A row carries a profile only once the account behind it is active: one that is still waiting
/// to be activated has none, and the only name it has is the username the pane asked for it
/// under, remembered against its id. Every question about what a row is called — what it is
/// drawn as, whether an account is already monitored, which rows a filter matches — is asked
/// here, so no two answers can disagree about one row.
pub(super) fn selection_name<'a>(
    selection: &'a XSelection,
    hints: &'a HashMap<SmolStr, SharedString>,
) -> Option<&'a str> {
    match selection.profile.as_ref() {
        Some(profile) => Some(profile.username.as_str()),
        None => hints.get(&selection.x_user_id).map(SharedString::as_str),
    }
}

/// The hints that still name a row of `selections`.
///
/// A hint names a row the account monitors that has no profile. What the pane remembers is
/// pruned against the set as it is read rather than against each change to it, because a row
/// can leave the set by more than one road — it was deleted, the read no longer carries it, or
/// it came back with a profile that names it better than the username it was asked for under.
/// What is left is a memo beside the rows rather than a second list of accounts, which is what
/// keeps a name from outliving the row it belongs to.
pub(super) fn retained_hints(
    hints: &HashMap<SmolStr, SharedString>,
    selections: &[XSelection],
) -> HashMap<SmolStr, SharedString> {
    let mut retained = HashMap::with_capacity(hints.len());
    retained.extend(hints.iter().filter_map(|(x_user_id, name)| {
        let unnamed = selections
            .iter()
            .any(|selection| selection.profile.is_none() && selection.x_user_id == *x_user_id);
        unnamed.then(|| (x_user_id.clone(), name.clone()))
    }));
    retained
}

/// The accounts one account monitors, as the service last answered them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SelectionData {
    selections: Arc<[XSelection]>,
    limit: usize,
}

impl SelectionData {
    /// The accounts one service answer holds.
    ///
    /// The answer has already been checked by the service client, so this is what it said
    /// rather than a second opinion about it.
    pub(super) fn new(response: XSelectionsResponse) -> Self {
        Self {
            selections: response.selections.into(),
            limit: response.x_account_limit,
        }
    }

    pub(super) fn selections(&self) -> &[XSelection] {
        &self.selections
    }

    /// The rows themselves, for a list that is told which of them to draw: what the list was
    /// given is what it draws, so a set that is the same set is not given to it again.
    pub(super) fn shared_selections(&self) -> Arc<[XSelection]> {
        self.selections.clone()
    }

    pub(super) const fn limit(&self) -> usize {
        self.limit
    }

    /// Whether the account may monitor nothing further.
    ///
    /// Every account counts, including one that is still waiting to be activated: the limit
    /// is what the account is allowed to have selected, not what it is currently following.
    pub(super) fn quota_full(&self) -> bool {
        self.selections.len() >= self.limit
    }

    /// Whether one account is already monitored, by name.
    ///
    /// This is [`selection_name`] asked of every row, which is the same question the rows on
    /// screen answer: a row with a profile is named by it, and a row waiting to be activated
    /// by the username it was asked for under.
    pub(super) fn contains_username(
        &self,
        username: &str,
        hints: &HashMap<SmolStr, SharedString>,
    ) -> bool {
        self.selections.iter().any(|selection| {
            selection_name(selection, hints).is_some_and(|name| name.eq_ignore_ascii_case(username))
        })
    }

    /// The accounts this set becomes once the service confirms one was added.
    ///
    /// A confirmation for an account already in the set replaces that row: an account that
    /// was waiting to be activated has a profile now, and the confirmed row is the newer of
    /// the two.
    pub(super) fn with_added(&self, selection: XSelection) -> Self {
        let mut selections = Vec::with_capacity(self.selections.len() + 1);
        selections.extend(
            self.selections
                .iter()
                .filter(|current| current.x_user_id != selection.x_user_id)
                .cloned(),
        );
        selections.push(selection);
        Self {
            selections: selections.into(),
            limit: self.limit,
        }
    }

    /// The accounts this set becomes once the service confirms one was removed.
    pub(super) fn with_deleted(&self, x_user_id: &str) -> Self {
        let selections = self
            .selections
            .iter()
            .filter(|current| current.x_user_id != x_user_id)
            .cloned()
            .collect::<Vec<_>>();
        Self {
            selections: selections.into(),
            limit: self.limit,
        }
    }
}

/// How many of a newer feed's tweets a list holding `held` has not been shown.
///
/// A feed and the list that draws it are both newest first and both bounded to the same number
/// of tweets, so a tweet the list has not been shown is one at the head of the feed, and what
/// follows it is the run the list already holds. `None` is a feed the list cannot be extended
/// into — a tweet that arrived late enough to belong in the middle of it, or one that shares no
/// tweet with what the list holds — and the list a pane draws is built again for it instead.
pub(super) fn unshown(held: &[Arc<TweetItem>], next: &[Arc<TweetItem>]) -> Option<usize> {
    // A list that holds nothing has been shown nothing.
    if held.is_empty() {
        return Some(next.len());
    }

    // Everything the new feed added is at its head, so the first tweet of it the list already
    // holds is the first tweet the two have in common.
    let unshown = next
        .iter()
        .position(|row| held.iter().any(|held| Arc::ptr_eq(held, row)))?;
    let shared = next.len() - unshown;
    if shared > held.len() {
        return None;
    }
    next[unshown..]
        .iter()
        .zip(held)
        .all(|(next, held)| Arc::ptr_eq(next, held))
        .then_some(unshown)
}

#[cfg(test)]
mod tests {
    use kytrix_service_provider::service::x::{XProfile, XSelectionStatus};

    use super::*;

    fn tweet(id: &str, timestamp: i64) -> XTweetEvent {
        XTweetEvent {
            tweet_id: id.into(),
            timestamp,
            x_user_id: "42".into(),
            text_segments: vec![XTweetTextSegment {
                text: "hello".into(),
                url: None,
            }],
            reference: None,
            media: Vec::new(),
        }
    }

    fn profile(display_name: &str, verified: bool, revision: u64) -> XProfileEvent {
        XProfileEvent {
            x_user_id: "42".into(),
            username: "kytrix".into(),
            display_name: display_name.into(),
            avatar_url: "https://pbs.twimg.com/avatar.jpg".into(),
            verified,
            revision,
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("the test clock is a time")
    }

    #[test]
    fn state_should_deduplicate_sort_and_keep_only_fifty_tweets() {
        let mut state = TweetState::new();
        for timestamp in 0..55 {
            assert!(state.push(tweet(&timestamp.to_string(), timestamp)));
        }
        assert!(!state.push(tweet("54", 999)));

        let snapshot = state.snapshot();
        assert_eq!(snapshot.items().len(), MAX_TWEETS);
        assert_eq!(snapshot.items()[0].tweet_id(), "54");
        assert_eq!(snapshot.items()[MAX_TWEETS - 1].tweet_id(), "5");
    }

    #[test]
    fn state_should_insert_late_tweets_in_timestamp_order() {
        let mut state = TweetState::new();
        state.push(tweet("3", 3));
        state.push(tweet("1", 1));
        state.push(tweet("2", 2));

        assert_eq!(
            state
                .snapshot()
                .items()
                .iter()
                .map(|item| item.tweet_id())
                .collect::<Vec<_>>(),
            ["3", "2", "1"]
        );
    }

    #[test]
    fn state_should_sort_same_second_tweet_ids_numerically() {
        let mut state = TweetState::new();
        state.push(tweet("99", 1));
        state.push(tweet("100", 1));

        assert_eq!(state.snapshot().items()[0].tweet_id(), "100");
    }

    #[test]
    fn profile_upserts_should_refresh_existing_tweets() {
        let mut state = TweetState::new();
        state.push(tweet("1", 1));
        assert_eq!(state.snapshot().author("42").display_name, "@42");

        assert!(state.upsert_profile(profile("Kytrix", false, 1)));
        assert!(!state.upsert_profile(profile("Kytrix", false, 1)));
        assert_eq!(state.snapshot().author("42").display_name, "Kytrix");

        assert!(state.upsert_profile(profile("Kytrix Labs", true, 2)));
        let snapshot = state.snapshot();
        let author = snapshot.author("42");
        assert_eq!(author.display_name, "Kytrix Labs");
        assert!(author.verified);
        assert_eq!(snapshot.items()[0].tweet_id(), "1");
    }

    #[test]
    fn profile_upserts_reject_stale_and_conflicting_versioned_values() {
        let mut state = TweetState::new();
        assert!(state.upsert_profile(profile("Current", true, 2)));
        assert!(!state.upsert_profile(profile("Stale", false, 1)));
        assert!(!state.upsert_profile(profile("Conflict", false, 2)));
        assert_eq!(state.snapshot().author("42").display_name, "Current");

        let mut unversioned = TweetState::new();
        assert!(unversioned.upsert_profile(profile("First", false, 0)));
        assert!(unversioned.upsert_profile(profile("Second", false, 0)));
        assert_eq!(unversioned.snapshot().author("42").display_name, "Second");
    }

    #[test]
    fn profiles_should_be_pruned_to_current_tweet_authors() {
        let mut state = TweetState::new();
        for index in 0..MAX_TWEETS + 10 {
            let root_id = (index * 2).to_string();
            let quote_id = (index * 2 + 1).to_string();
            let mut root = tweet(&root_id, i64::try_from(index).unwrap());
            root.x_user_id = root_id.clone().into();
            let mut quote = tweet(&quote_id, i64::try_from(index).unwrap());
            quote.x_user_id = quote_id.clone().into();
            root.reference = Some(TweetReference::Quote(Box::new(quote)));

            let mut root_profile = profile("root", false, 1);
            root_profile.x_user_id = root_id.into();
            let mut quote_profile = profile("quote", false, 1);
            quote_profile.x_user_id = quote_id.into();
            state.upsert_profile(root_profile);
            state.upsert_profile(quote_profile);
            state.push(root);
            state.prune_profiles();
        }

        let snapshot = state.snapshot();
        assert_eq!(snapshot.profiles.len(), MAX_TWEETS * 2);
        for item in snapshot.items() {
            assert_ne!(snapshot.author(&item.x_user_id).revision, 0);
            let Some(TweetReferenceItem::Quote(quote)) = item.reference.as_ref() else {
                panic!("test Tweet should quote another Tweet");
            };
            assert_ne!(snapshot.author(&quote.x_user_id).revision, 0);
        }
    }

    #[test]
    fn pending_profiles_should_be_bounded_to_one_live_tweet() {
        let mut state = TweetState::new();
        for id in 0..10 {
            let id = id.to_string();
            let mut author = profile(&id, false, 1);
            author.x_user_id = id.into();
            state.upsert_profile(author);
        }
        assert_eq!(state.profiles.len(), MAX_TWEET_AUTHORS);

        let mut leaf = tweet("12", 1);
        leaf.x_user_id = "9".into();
        let mut middle = tweet("11", 1);
        middle.x_user_id = "8".into();
        middle.reference = Some(TweetReference::Quote(Box::new(leaf)));
        let mut root = tweet("10", 1);
        root.x_user_id = "7".into();
        root.reference = Some(TweetReference::Quote(Box::new(middle)));
        state.push(root);
        state.prune_profiles();

        let snapshot = state.snapshot();
        assert_eq!(snapshot.profiles.len(), MAX_TWEET_AUTHORS);
        for id in ["7", "8", "9"] {
            assert_ne!(snapshot.author(id).revision, 0);
        }
    }

    #[test]
    fn tweet_references_should_keep_repost_and_quote_semantics() {
        let mut repost = tweet("2", 2);
        repost.reference = Some(TweetReference::Repost(Box::new(tweet("1", 1))));
        let mut quote = tweet("4", 4);
        quote.reference = Some(TweetReference::Quote(Box::new(tweet("3", 3))));

        let repost = TweetItem::from(repost);
        let quote = TweetItem::from(quote);
        assert!(matches!(
            repost.reference,
            Some(TweetReferenceItem::Repost(_))
        ));
        assert!(matches!(
            quote.reference,
            Some(TweetReferenceItem::Quote(_))
        ));
        assert_eq!(repost.displayed().0.tweet_id(), "1");
        assert!(repost.displayed().1);
        assert_eq!(quote.displayed().0.tweet_id(), "4");
        assert!(!quote.displayed().1);
    }

    #[test]
    fn tweet_text_should_join_segments_and_keep_link_byte_ranges() {
        let item = TweetItem::from(XTweetEvent {
            tweet_id: "1".into(),
            timestamp: 1,
            x_user_id: "42".into(),
            text_segments: vec![
                XTweetTextSegment {
                    text: "😀 ".into(),
                    url: None,
                },
                XTweetTextSegment {
                    text: "@a".into(),
                    url: Some("https://x.com/a".into()),
                },
                XTweetTextSegment {
                    text: " ".into(),
                    url: None,
                },
                XTweetTextSegment {
                    text: "x.test".into(),
                    url: Some("https://t.co/a".into()),
                },
            ],
            reference: None,
            media: Vec::new(),
        });

        assert_eq!(item.text.shared().as_ref(), "😀 @a x.test");
        assert_eq!(
            item.text
                .links()
                .iter()
                .map(|link| (link.range.clone(), link.url.as_ref()))
                .collect::<Vec<_>>(),
            [(5..7, "https://x.com/a"), (8..14, "https://t.co/a")]
        );
    }

    #[test]
    fn media_urls_should_cover_authors_pictures_and_references_once() {
        let mut state = TweetState::new();
        let mut root = tweet("1", 1);
        root.media = vec![
            XTweetMedia::Image {
                image_url: "https://pbs.twimg.com/media/one.jpg".into(),
                width: 10,
                height: 10,
            },
            XTweetMedia::Video {
                playback_url: "https://video.twimg.com/playback.mp4".into(),
                thumbnail_url: "https://pbs.twimg.com/media/thumb.jpg".into(),
                width: 10,
                height: 10,
            },
            XTweetMedia::Gif {
                animation_url: "https://video.twimg.com/animation.mp4".into(),
                image_url: "https://pbs.twimg.com/media/poster.jpg".into(),
                width: 10,
                height: 10,
            },
        ];
        let mut quoted = tweet("2", 1);
        quoted.media = vec![XTweetMedia::Image {
            image_url: "https://pbs.twimg.com/media/one.jpg".into(),
            width: 10,
            height: 10,
        }];
        root.reference = Some(TweetReference::Quote(Box::new(quoted)));
        state.push(root);
        state.upsert_profile(profile("Kytrix", true, 1));

        assert_eq!(
            state.media_urls(),
            [
                "https://pbs.twimg.com/avatar.jpg",
                "https://pbs.twimg.com/media/one.jpg",
                "https://pbs.twimg.com/media/thumb.jpg",
                "https://pbs.twimg.com/media/poster.jpg",
            ]
            .map(SmolStr::new)
        );
    }

    #[test]
    fn media_urls_should_refuse_urls_this_pane_may_not_fetch() {
        let mut state = TweetState::new();
        let mut root = tweet("1", 1);
        root.media = vec![
            XTweetMedia::Image {
                image_url: "https://example.com/one.jpg".into(),
                width: 10,
                height: 10,
            },
            XTweetMedia::Image {
                image_url: "http://pbs.twimg.com/media/insecure.jpg".into(),
                width: 10,
                height: 10,
            },
            XTweetMedia::Image {
                image_url: "".into(),
                width: 10,
                height: 10,
            },
        ];
        state.push(root);

        assert!(state.media_urls().is_empty());
    }

    #[test]
    fn ages_should_cover_every_boundary() {
        let now = now();
        // A timestamp on this wire is in milliseconds, which is what the boundaries are
        // measured in.
        let ago = |seconds: i64| now.unix_timestamp() * 1_000 - seconds * 1_000;

        assert_eq!(
            tweet_age(ago(-1), now, UtcOffset::UTC),
            TweetAge::Seconds(0)
        );
        assert_eq!(
            tweet_age(ago(59), now, UtcOffset::UTC),
            TweetAge::Seconds(59)
        );
        assert_eq!(
            tweet_age(ago(60), now, UtcOffset::UTC),
            TweetAge::Minutes(1)
        );
        assert_eq!(
            tweet_age(ago(3_599), now, UtcOffset::UTC),
            TweetAge::Minutes(59)
        );
        assert_eq!(
            tweet_age(ago(3_600), now, UtcOffset::UTC),
            TweetAge::Hours(1)
        );
        assert_eq!(
            tweet_age(ago(86_399), now, UtcOffset::UTC),
            TweetAge::Hours(23)
        );

        let day = OffsetDateTime::from_unix_timestamp_nanos(i128::from(ago(86_400)) * 1_000_000)
            .expect("a day ago is a time");
        assert_eq!(
            tweet_age(ago(86_400), now, UtcOffset::UTC),
            TweetAge::Date {
                month: u8::from(day.month()),
                day: day.day(),
            }
        );
        let year = OffsetDateTime::from_unix_timestamp_nanos(
            i128::from(ago(365 * 24 * 60 * 60)) * 1_000_000,
        )
        .expect("a year ago is a time");
        assert_eq!(
            tweet_age(ago(365 * 24 * 60 * 60), now, UtcOffset::UTC),
            TweetAge::FullDate {
                year: year.year(),
                month: u8::from(year.month()),
                day: year.day(),
            }
        );
        assert_eq!(tweet_age(i64::MAX, now, UtcOffset::UTC), TweetAge::Unknown);
    }

    /// A date an age reads as is the day the tweet was published in the display zone,
    /// which is the day the reader saw on their own clock.
    #[test]
    fn a_dated_age_should_read_the_day_in_the_display_zone() {
        // 2024-01-01T23:00:00Z: the same instant is already the 2nd at UTC+14.
        let timestamp = 1_704_150_000_000;
        let now = OffsetDateTime::from_unix_timestamp(1_704_150_000 + 86_400)
            .expect("the fixture is a time");

        assert_eq!(
            tweet_age(timestamp, now, UtcOffset::UTC),
            TweetAge::Date { month: 1, day: 1 }
        );
        assert_eq!(
            tweet_age(
                timestamp,
                now,
                UtcOffset::from_hms(14, 0, 0).expect("UTC+14 is a valid offset")
            ),
            TweetAge::Date { month: 1, day: 2 }
        );
    }

    #[test]
    fn absolute_time_should_be_read_in_the_display_zone_to_the_second() {
        assert_eq!(
            absolute_time(1_800_000_000_000, UtcOffset::UTC).as_ref(),
            "2027-01-15 08:00:00"
        );
        assert_eq!(
            absolute_time(
                1_800_000_000_000,
                UtcOffset::from_hms(8, 0, 0).expect("UTC+8 is a valid offset")
            )
            .as_ref(),
            "2027-01-15 16:00:00"
        );
        assert_eq!(absolute_time(i64::MAX, UtcOffset::UTC).as_ref(), "--");
    }

    /// The display offset can move an instant past the last day the calendar holds, so
    /// no date can be written for it: the absolute time is the placeholder. What an age
    /// says is still the difference between two UTC instants — a stamp at the far end of
    /// the calendar is a future one, which reads as now, and an ancient one needs a date
    /// the calendar cannot give it, which reads as unknown.
    #[test]
    fn a_tweet_the_display_zone_cannot_hold_should_read_without_a_date() {
        let boundary = time::Date::MAX
            .with_hms(23, 0, 0)
            .expect("the last day has an evening")
            .assume_utc();
        let timestamp = i64::try_from(boundary.unix_timestamp_nanos() / 1_000_000)
            .expect("the last day is a millisecond timestamp");
        let zone = UtcOffset::from_hms(14, 0, 0).expect("UTC+14 is a valid offset");

        assert_eq!(absolute_time(timestamp, zone).as_ref(), "--");
        assert_eq!(
            tweet_age(timestamp, OffsetDateTime::now_utc(), zone),
            TweetAge::Seconds(0),
            "a stamp in the future is now, and that is a duration rather than a date"
        );

        let boundary = time::Date::MIN
            .with_hms(0, 0, 0)
            .expect("the first day has a midnight")
            .assume_utc();
        let timestamp = i64::try_from(boundary.unix_timestamp_nanos() / 1_000_000)
            .expect("the first day is a millisecond timestamp");
        let zone = UtcOffset::from_hms(-12, 0, 0).expect("UTC-12 is a valid offset");

        assert_eq!(
            absolute_time(timestamp, zone).as_ref(),
            "--",
            "a western offset runs off the other end of the calendar"
        );
        assert_eq!(
            tweet_age(timestamp, OffsetDateTime::now_utc(), zone),
            TweetAge::Unknown,
            "an age that needs a date the calendar cannot give it has no answer"
        );
    }

    /// An age short enough to be a duration is the difference between two UTC instants,
    /// so it is the same answer whatever the display zone is — even one whose local
    /// reading of the instant has no date at all.
    #[test]
    fn a_relative_age_should_not_depend_on_the_display_zone() {
        let now = time::Date::MAX
            .with_hms(23, 59, 59)
            .expect("the last day has a last second")
            .assume_utc();
        let timestamp = i64::try_from(now.unix_timestamp_nanos() / 1_000_000)
            .expect("the last second is a millisecond timestamp")
            - 30_000;

        assert_eq!(
            tweet_age(
                timestamp,
                now,
                UtcOffset::from_hms(14, 0, 0).expect("UTC+14 is a valid offset")
            ),
            TweetAge::Seconds(30),
            "the offset has no date to give this instant, and a duration needs none"
        );
    }

    #[test]
    fn selection_projection_should_add_replace_and_remove_rows() {
        let data = SelectionData::new(XSelectionsResponse {
            selections: vec![selection("1", Some("one")), selection("2", None)],
            x_account_limit: 3,
        });
        assert_eq!(data.limit(), 3);
        assert!(!data.quota_full());
        assert!(data.contains_username("one", &HashMap::new()));
        assert!(!data.contains_username("two", &HashMap::new()));

        let added = data.with_added(selection("3", Some("three")));
        assert_eq!(
            added
                .selections()
                .iter()
                .map(|selection| selection.x_user_id.as_str())
                .collect::<Vec<_>>(),
            ["1", "2", "3"]
        );
        assert!(added.quota_full());

        let replaced = added.with_added(selection("2", Some("two")));
        assert_eq!(
            replaced
                .selections()
                .iter()
                .map(|selection| selection.x_user_id.as_str())
                .collect::<Vec<_>>(),
            ["1", "3", "2"]
        );
        assert_eq!(replaced.selections().len(), 3);

        let removed = replaced.with_deleted("3");
        assert_eq!(
            removed
                .selections()
                .iter()
                .map(|selection| selection.x_user_id.as_str())
                .collect::<Vec<_>>(),
            ["1", "2"]
        );
        assert!(!removed.quota_full());
    }

    /// Whether an account is already monitored has to be the question the rows on screen
    /// answer: a row with a profile is named by it, and a row that is still waiting to be
    /// activated by the name the pane remembers under that id.
    #[test]
    fn contains_username_should_name_pending_rows_by_the_remembered_hint() {
        let data = SelectionData::new(XSelectionsResponse {
            selections: vec![
                selection("1", Some("one")),
                pending_selection("2"),
                pending_selection("3"),
            ],
            x_account_limit: 3,
        });
        let hints = HashMap::from([
            (SmolStr::new_static("1"), SharedString::new_static("stale")),
            (SmolStr::new_static("2"), SharedString::new_static("two")),
            (SmolStr::new_static("9"), SharedString::new_static("nine")),
        ]);

        assert!(
            data.contains_username("ONE", &hints),
            "a row with a profile should be named by it whatever case it is asked in"
        );
        assert!(
            data.contains_username("two", &hints),
            "a row with no profile should be named by the hint the pane remembers for its id"
        );
        assert!(
            !data.contains_username("three", &hints),
            "a row with no profile and no hint should be named by nothing"
        );
        assert!(
            !data.contains_username("nine", &hints),
            "a hint for an account that is not monitored should name nothing"
        );
        assert!(
            !data.contains_username("stale", &hints),
            "a row with a profile should be named by it rather than by the hint for its id"
        );
    }

    /// What a row is drawn as and what it answers to are one question, asked once: a row with
    /// a profile is named by it, and one waiting to be activated by what is remembered for its
    /// id — and by nothing at all when nothing is.
    #[test]
    fn a_row_should_be_named_by_its_profile_or_by_what_is_remembered_for_its_id() {
        let hints = HashMap::from([(SmolStr::new_static("2"), SharedString::new_static("two"))]);

        assert_eq!(
            selection_name(&selection("1", Some("one")), &hints),
            Some("one")
        );
        assert_eq!(
            selection_name(&selection("1", Some("one")), &HashMap::new()),
            Some("one")
        );
        assert_eq!(selection_name(&pending_selection("2"), &hints), Some("two"));
        assert_eq!(selection_name(&pending_selection("3"), &hints), None);
        assert_eq!(
            selection_name(&pending_selection("2"), &HashMap::new()),
            None,
            "nothing is known about a row that has no profile and no remembered name"
        );
    }

    /// What the pane remembers is pruned against the set as it is read: a hint survives while
    /// it names a row that is monitored and has no profile, and no longer than that.
    #[test]
    fn remembered_hints_should_be_pruned_to_the_rows_they_still_name() {
        let hints = HashMap::from([
            (SmolStr::new_static("1"), SharedString::new_static("stale")),
            (SmolStr::new_static("2"), SharedString::new_static("two")),
            (SmolStr::new_static("3"), SharedString::new_static("gone")),
        ]);

        let read = [
            selection("1", Some("one")),
            pending_selection("2"),
            selection("3", Some("three")),
        ];
        let retained = retained_hints(&hints, &read);

        assert_eq!(
            retained.get("2").map(SharedString::as_str),
            Some("two"),
            "a hint for a row that is still waiting to be activated should be kept"
        );
        assert!(
            !retained.contains_key("1"),
            "a row a profile names should not keep the name it was remembered by"
        );
        assert!(
            !retained.contains_key("3"),
            "a row whose profile replaced the remembered name should not keep it either"
        );
        assert!(
            retained_hints(&hints, &[pending_selection("9")]).is_empty(),
            "a hint for a row the read no longer holds should be dropped"
        );
    }

    /// A feed that grew at its head is the tweets at the head of it a list has not been shown,
    /// and what the list holds is what follows them however many left at its tail.
    #[test]
    fn unshown_should_count_the_tweets_at_the_head_of_a_feed() {
        let mut state = TweetState::new();
        let mut timestamp = 0;
        let mut push = |state: &mut TweetState, id: &str| {
            timestamp += 10;
            state.push(tweet(id, timestamp));
        };
        for id in ["a", "b", "c"] {
            push(&mut state, id);
        }
        let first = state.snapshot().items().to_vec();

        // One more tweet at the head, nothing dropped.
        push(&mut state, "d");
        let second = state.snapshot().items().to_vec();
        assert_eq!(
            unshown(&first, &second),
            Some(1),
            "every tweet the list has not been shown is one of them"
        );

        // A full feed drops its oldest tweet as the newest arrives, which moves what it holds
        // rather than what it has not been shown.
        let mut full = TweetState::new();
        for index in 0..MAX_TWEETS as i64 {
            full.push(tweet(&index.to_string(), index));
        }
        let before = full.snapshot().items().to_vec();
        full.push(tweet("new", MAX_TWEETS as i64));
        let after = full.snapshot().items().to_vec();

        assert_eq!(
            unshown(&before, &after),
            Some(1),
            "the tweet that arrived at the head is the one the list has not been shown"
        );
    }

    /// A list that holds nothing has been shown nothing, so every tweet of a feed is one it has
    /// not been shown.
    #[test]
    fn unshown_should_ask_for_every_tweet_of_a_feed_a_list_holds_nothing_of() {
        let mut state = TweetState::new();
        state.push(tweet("2", 2));
        state.push(tweet("1", 1));
        let next = state.snapshot().items().to_vec();

        assert_eq!(unshown(&[], &next), Some(2));
    }

    #[test]
    fn unshown_should_report_nothing_for_a_feed_that_did_not_move() {
        let mut state = TweetState::new();
        state.push(tweet("2", 2));
        state.push(tweet("1", 1));
        let snapshot = state.snapshot().items().to_vec();

        assert_eq!(unshown(&snapshot, &snapshot), Some(0));
    }

    #[test]
    fn unshown_should_ask_for_a_replacement_when_nothing_survives() {
        let mut state = TweetState::new();
        state.push(tweet("1", 1));
        let held = state.snapshot().items().to_vec();
        let next = vec![Arc::new(TweetItem::from(tweet("other", 99)))];

        assert_eq!(unshown(&held, &next), None);
    }

    /// A tweet that belongs in the middle of a feed is not a feed a list can be extended into:
    /// what it holds is replaced rather than rearranged.
    #[test]
    fn unshown_should_ask_for_a_replacement_when_a_tweet_belongs_in_the_middle() {
        let mut state = TweetState::new();
        state.push(tweet("3", 3));
        state.push(tweet("1", 1));
        let held = state.snapshot().items().to_vec();

        state.push(tweet("2", 2));
        let next = state.snapshot().items().to_vec();
        assert_eq!(next.len(), 3);

        assert_eq!(unshown(&held, &next), None);
    }

    fn selection(x_user_id: &str, username: Option<&str>) -> XSelection {
        XSelection {
            x_user_id: x_user_id.into(),
            status: XSelectionStatus::Active,
            created_at: 1,
            updated_at: 2,
            profile: username.map(|username| XProfile {
                x_user_id: x_user_id.into(),
                username: username.into(),
                display_name: username.into(),
                avatar_url: "https://pbs.twimg.com/42.jpg".into(),
                description: SmolStr::new_static(""),
                followers_count: 0,
                following_count: 0,
                protected: false,
                verified: false,
            }),
        }
    }

    /// A row the service has not activated yet, which by contract carries no profile.
    fn pending_selection(x_user_id: &str) -> XSelection {
        XSelection {
            status: XSelectionStatus::Pending,
            ..selection(x_user_id, None)
        }
    }
}

//! The news a subscription retains, and how much of a newer snapshot a list has not been
//! shown.
//!
//! # Order
//!
//! News is held newest first, ordered by the timestamp the service stamped it with. A
//! report that arrives late still belongs where its timestamp says rather than at the
//! head, and two reports of one timestamp keep the order they arrived in: news is
//! ordered by the time it happened, not by the order this process happened to see it.
//!
//! # Identity
//!
//! An item's identity is the allocation that holds it, which a snapshot shares with the
//! snapshot it inherited it from. That is what makes the difference between two
//! snapshots the difference between two sets of pointers, and it is why a report that
//! repeats another one exactly is still its own row — the service reports news, not a
//! set of strings.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError},
};

use gpui_kit::SharedString;
use kytrix_service_provider::service::engine::{NewsEvent, NewsKind};
use smol_str::format_smolstr;
use time::{OffsetDateTime, UtcOffset};

/// How many items one subscription retains.
///
/// A feed is read from the top, so what a pane keeps is what a reader can reach: older
/// news is dropped rather than kept behind a bound nobody scrolls past.
pub(super) const MAX_NEWS_ITEMS: usize = 50;

/// The kind of news an item is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NewsCategory {
    General,
    Crypto,
    Political,
}

impl From<NewsKind> for NewsCategory {
    fn from(kind: NewsKind) -> Self {
        match kind {
            NewsKind::General => Self::General,
            NewsKind::Crypto => Self::Crypto,
            NewsKind::Political => Self::Political,
        }
    }
}

/// One retained item, and the text its row reads.
#[derive(Debug)]
pub(super) struct NewsItem {
    timestamp: i64,
    title: Option<SharedString>,
    content: SharedString,
    category: NewsCategory,
    display: Mutex<Option<(UtcOffset, Arc<NewsItemDisplay>)>>,
}

impl NewsItem {
    fn new(event: NewsEvent) -> Self {
        Self {
            timestamp: event.timestamp,
            title: event.title.map(|title| SharedString::new(title.as_str())),
            content: SharedString::new(event.content.as_str()),
            category: NewsCategory::from(event.category),
            display: Mutex::new(None),
        }
    }

    pub(super) fn title(&self) -> Option<&SharedString> {
        self.title.as_ref()
    }

    pub(super) const fn content(&self) -> &SharedString {
        &self.content
    }

    pub(super) const fn category(&self) -> NewsCategory {
        self.category
    }

    /// The text of an item that is produced only once something draws it.
    ///
    /// A report is retained the moment it arrives, while most of what a feed holds is
    /// never on screen: formatting a timestamp is the one part of a row that costs
    /// anything, so it happens here rather than when the item is retained. The clock
    /// it was formatted in is remembered beside the text, so a frame that draws the
    /// same zone reuses it and a frame after the setting moved formats it again.
    pub(super) fn display(&self, zone: UtcOffset) -> Arc<NewsItemDisplay> {
        let mut cached = self.display.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((cached_zone, display)) = cached.as_ref()
            && *cached_zone == zone
        {
            return Arc::clone(display);
        }

        let display = Arc::new(NewsItemDisplay::new(self, zone));
        *cached = Some((zone, Arc::clone(&display)));
        display
    }
}

/// The part of an item's row that is built rather than borrowed.
#[derive(Debug)]
pub(super) struct NewsItemDisplay {
    pub time: SharedString,
}

impl NewsItemDisplay {
    fn new(item: &NewsItem, zone: UtcOffset) -> Self {
        Self {
            time: format_time(item.timestamp, zone),
        }
    }
}

/// An immutable, cheap-to-clone view of the retained news.
#[derive(Clone, Debug, Default)]
pub(super) struct NewsSnapshot {
    items: Arc<[Arc<NewsItem>]>,
}

impl NewsSnapshot {
    /// The news of this snapshot, newest first.
    pub(super) fn items(&self) -> &[Arc<NewsItem>] {
        &self.items
    }
}

/// The news one subscription retains.
pub(super) struct NewsState {
    items: VecDeque<Arc<NewsItem>>,
    maximum_items: usize,
}

impl NewsState {
    /// Retains up to `maximum_items` items, never more than a feed may hold.
    pub(super) fn new(maximum_items: usize) -> Self {
        let maximum_items = maximum_items.clamp(1, MAX_NEWS_ITEMS);
        Self {
            items: VecDeque::with_capacity(maximum_items),
            maximum_items,
        }
    }

    /// Retains one reported item.
    ///
    /// Returns whether the feed changed: a report older than everything a full feed
    /// holds is refused rather than displacing news that is newer than it.
    pub(super) fn push(&mut self, event: NewsEvent) -> bool {
        if self.items.len() == self.maximum_items
            && self
                .items
                .back()
                .is_some_and(|oldest| event.timestamp < oldest.timestamp)
        {
            return false;
        }

        let item = Arc::new(NewsItem::new(event));
        // A report that repeats the timestamp of one already held goes in front of it, so
        // the newest arrival of that instant is the one read first.
        let insertion = self
            .items
            .iter()
            .position(|current| current.timestamp <= item.timestamp);
        if self.items.len() == self.maximum_items {
            self.items.pop_back();
        }
        match insertion {
            Some(index) => self.items.insert(index, item),
            None => self.items.push_back(item),
        }
        true
    }

    /// The retained news, newest first.
    pub(super) fn snapshot(&self) -> NewsSnapshot {
        let mut items = Vec::with_capacity(self.items.len());
        items.extend(self.items.iter().cloned());
        NewsSnapshot {
            items: items.into(),
        }
    }
}

/// Whether a feed a pane is showing is a feed a reader has news to hear in.
///
/// A pane that has just opened is not being told news — it is being told what the feed is,
/// and everything in it happened before the reader was looking. So the first feed a pane
/// holds is not an arrival, and neither is the feed a reconnect publishes, because a
/// reconnect publishes the same reports it published before. A feed leading with nothing is
/// not one a pane was told anything by either: a live subscription that holds no news yet is
/// what a pane starts from, and the first report to fill it is the first news that pane has.
///
/// After that, news is a feed whose lead is not the report it was leading with: the feed is
/// ordered by the instant its reports happened, so its lead is the newest report the service
/// has published, and a feed led by somewhere else is a feed the service has published
/// something new to. The instant is never what is compared — two reports can share one — and
/// neither is a report that arrived late enough to belong *below* the lead: that report is
/// news the feed is carrying now, but the feed is quiet about when it got there, and a pane
/// that announced every reordering of a feed it is showing would be announcing news it has
/// already shown. What is compared is the allocation holding the report, which a newer feed
/// shares with the feed it grew from, so news the pane already held is never news twice.
///
/// A feed led by a report the pane holds is answered the same way whether or not the rest of
/// it moved, and a feed rebuilt from scratch leads somewhere else by definition: two feeds
/// that share reports share the ones they have in common, which is what makes the comparison
/// mean anything at all.
pub(super) fn has_new_news(shown: Option<&NewsSnapshot>, published: &NewsSnapshot) -> bool {
    let Some(shown) = shown else {
        return false;
    };
    let Some(head) = published.items().first() else {
        return false;
    };

    !shown
        .items()
        .first()
        .is_some_and(|held| Arc::ptr_eq(held, head))
}

/// How many of a newer feed's newest reports a list holding `held` has not been shown.
///
/// A feed and the list that draws it are both newest first and both bounded to the same
/// number of reports, so a report the list has not been shown is one at the head of the feed,
/// and what follows it is the run the list already holds. `None` is a feed the list cannot be
/// extended into — a report that arrived late enough to belong in the middle of it, or one
/// that shares no report with what the list holds — and the list a pane draws is built again
/// for it instead.
pub(super) fn unshown(held: &[Arc<NewsItem>], next: &[Arc<NewsItem>]) -> Option<usize> {
    // A list that holds nothing has been shown nothing.
    if held.is_empty() {
        return Some(next.len());
    }

    // Everything the new feed added is at its head, so the first report of it the list
    // already holds is the first report the two have in common.
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

/// The time of an item, read in the zone the settings display data in.
///
/// The service stamps news in UTC, so what a reader sees is that instant converted
/// by the offset the caller read from the settings.
fn format_time(timestamp: i64, zone: UtcOffset) -> SharedString {
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

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;

    use super::*;

    fn event(timestamp: i64) -> NewsEvent {
        NewsEvent {
            timestamp,
            title: Some(SmolStr::new_static("Title")),
            content: SmolStr::new_static("Content"),
            category: NewsKind::General,
        }
    }

    fn state(maximum_items: usize, timestamps: &[i64]) -> NewsState {
        let mut state = NewsState::new(maximum_items);
        for &timestamp in timestamps {
            state.push(event(timestamp));
        }
        state
    }

    fn timestamps(snapshot: &NewsSnapshot) -> Vec<i64> {
        snapshot.items().iter().map(|item| item.timestamp).collect()
    }

    #[test]
    fn time_should_read_as_the_instant_the_service_stamped() {
        // 2023-11-14T22:13:20Z, so a formatter that reads seconds as milliseconds, or
        // that drops the date, does not pass.
        assert_eq!(
            format_time(1_700_000_000_000, UtcOffset::UTC).as_ref(),
            "2023-11-14 22:13:20"
        );
        assert_eq!(
            format_time(
                1_700_000_000_000,
                UtcOffset::from_hms(8, 0, 0).expect("UTC+8 is a valid offset")
            )
            .as_ref(),
            "2023-11-15 06:13:20",
            "the instant is read in the zone the settings display"
        );
    }

    #[test]
    fn time_should_read_as_a_placeholder_when_it_is_not_an_instant() {
        assert_eq!(format_time(i64::MAX, UtcOffset::UTC).as_ref(), "--");
    }

    /// The display offset can move an instant past the last day the calendar holds.
    /// That is not a date this can write, so it is a placeholder rather than a panic.
    #[test]
    fn time_the_display_zone_cannot_hold_should_read_as_a_placeholder() {
        let boundary = time::Date::MAX
            .with_hms(23, 0, 0)
            .expect("the last day has an evening")
            .assume_utc();
        let timestamp = i64::try_from(boundary.unix_timestamp_nanos() / 1_000_000)
            .expect("the last day is a millisecond timestamp");

        assert_eq!(
            format_time(
                timestamp,
                UtcOffset::from_hms(14, 0, 0).expect("UTC+14 is a valid offset")
            )
            .as_ref(),
            "--"
        );

        let first = time::Date::MIN
            .with_hms(0, 0, 0)
            .expect("the first day has a midnight")
            .assume_utc();
        let timestamp = i64::try_from(first.unix_timestamp_nanos() / 1_000_000)
            .expect("the first day is a millisecond timestamp");

        assert_eq!(
            format_time(
                timestamp,
                UtcOffset::from_hms(-12, 0, 0).expect("UTC-12 is a valid offset")
            )
            .as_ref(),
            "--",
            "a western offset can run off the other end of the calendar"
        );
    }

    #[test]
    fn state_should_keep_newest_items_first() {
        let mut state = NewsState::new(3);
        for timestamp in 0..5 {
            state.push(event(timestamp));
        }

        assert_eq!(timestamps(&state.snapshot()), [4, 3, 2]);
    }

    #[test]
    fn state_should_insert_late_older_news_by_timestamp() {
        let state = state(3, &[3, 1, 2]);

        assert_eq!(timestamps(&state.snapshot()), [3, 2, 1]);
    }

    #[test]
    fn state_should_clamp_capacity_to_fifty_items() {
        let mut state = NewsState::new(usize::MAX);
        for timestamp in 0..75 {
            state.push(event(timestamp));
        }

        assert_eq!(state.snapshot().items().len(), MAX_NEWS_ITEMS);
    }

    #[test]
    fn identical_receipts_should_keep_distinct_rows() {
        let state = state(3, &[1, 1]);

        assert_eq!(state.snapshot().items().len(), 2);
    }

    #[test]
    fn state_should_keep_distinct_news_with_the_same_timestamp() {
        let mut state = NewsState::new(3);
        state.push(event(1));
        state.push(NewsEvent {
            content: SmolStr::new_static("Content updated"),
            ..event(1)
        });

        let snapshot = state.snapshot();
        assert_eq!(snapshot.items().len(), 2);
        assert_eq!(
            snapshot.items()[0].content().as_ref(),
            "Content updated",
            "the newest arrival of one instant is read first"
        );
    }

    #[test]
    fn a_full_state_should_refuse_older_news_without_changing_the_feed() {
        let mut state = state(2, &[2, 1]);
        let before = state.snapshot();

        assert!(!state.push(event(0)));
        let after = state.snapshot();

        assert!(Arc::ptr_eq(&before.items[0], &after.items[0]));
        assert!(Arc::ptr_eq(&before.items[1], &after.items[1]));
    }

    #[test]
    fn full_state_should_evict_the_tail_without_growing_storage() {
        let mut state = NewsState::new(3);
        let capacity = state.items.capacity();
        for timestamp in 0..10 {
            state.push(event(timestamp));
        }

        assert_eq!(state.items.capacity(), capacity);
    }

    #[test]
    fn display_should_be_built_once_per_zone() {
        let state = state(1, &[1_700_000_000_000]);
        let snapshot = state.snapshot();
        let item = &snapshot.items()[0];
        assert!(
            item.display
                .lock()
                .expect("the cache is not poisoned")
                .is_none()
        );

        let first = item.display(UtcOffset::UTC);
        let second = item.display(UtcOffset::UTC);
        let other = item.display(UtcOffset::from_hms(8, 0, 0).expect("UTC+8 is a valid offset"));

        assert!(
            Arc::ptr_eq(&first, &second),
            "the same zone reuses the text"
        );
        assert_ne!(
            first.time, other.time,
            "another zone is another reading of the same instant"
        );
    }

    /// A feed that grew at its head is the reports at the head of it a list has not been
    /// shown, and what the list holds is what follows them however many left at its tail.
    #[test]
    fn unshown_should_count_the_reports_at_the_head_of_a_feed() {
        let mut full = state(
            MAX_NEWS_ITEMS,
            &(0..MAX_NEWS_ITEMS as i64).collect::<Vec<_>>(),
        );
        let current = full.snapshot();
        full.push(event(MAX_NEWS_ITEMS as i64));
        let next = full.snapshot();

        assert_eq!(unshown(current.items(), next.items()), Some(1));

        let mut grown = state(5, &[0, 1, 2, 3, 4]);
        let current = grown.snapshot();
        grown.push(event(5));
        grown.push(event(6));
        let next = grown.snapshot();

        assert_eq!(
            unshown(current.items(), next.items()),
            Some(2),
            "every report the list has not been shown is one of them"
        );
    }

    /// A list that holds nothing has been shown nothing, so every report of a feed is one it
    /// has not been shown.
    #[test]
    fn unshown_should_ask_for_every_report_of_a_feed_a_list_holds_nothing_of() {
        let next = state(3, &[2, 1]).snapshot();

        assert_eq!(unshown(&[], next.items()), Some(2));
    }

    #[test]
    fn unshown_should_report_nothing_for_a_feed_that_did_not_move() {
        let snapshot = state(2, &[2, 1]).snapshot();

        assert_eq!(unshown(snapshot.items(), snapshot.items()), Some(0));
    }

    #[test]
    fn unshown_should_ask_for_a_replacement_when_nothing_survives() {
        let current = state(1, &[1]).snapshot();
        let next = state(1, &[2]).snapshot();

        assert_eq!(unshown(current.items(), next.items()), None);
    }

    /// A report that belongs in the middle of a feed is not a feed a list can be extended
    /// into: what it holds is replaced rather than rearranged.
    #[test]
    fn unshown_should_ask_for_a_replacement_when_a_report_belongs_in_the_middle() {
        let current = state(MAX_NEWS_ITEMS, &[3, 1]).snapshot();
        let mut grown = state(MAX_NEWS_ITEMS, &[3, 1]);
        grown.push(event(2));
        let next = grown.snapshot();

        assert_eq!(next.items().len(), 3);
        assert_eq!(unshown(current.items(), next.items()), None);
    }

    /// A feed that lost the report a list holds at its head is not one the list can be
    /// extended into either: what it holds no longer starts where the feed does.
    #[test]
    fn unshown_should_ask_for_a_replacement_when_a_feed_lost_its_head() {
        let current = state(MAX_NEWS_ITEMS, &[3, 2]).snapshot();
        let next = state(MAX_NEWS_ITEMS, &[2]).snapshot();

        assert_eq!(unshown(current.items(), next.items()), None);
    }

    #[test]
    fn unshown_should_ask_for_a_replacement_when_the_order_changed() {
        let current = state(2, &[2, 1]).snapshot();
        let mut reversed = current.items().to_vec();
        reversed.reverse();
        let next = NewsSnapshot {
            items: reversed.into(),
        };

        assert_eq!(unshown(current.items(), next.items()), None);
    }

    /// A pane that has just opened is being told what the feed is, not what arrived.
    #[test]
    fn has_new_news_should_say_nothing_arrived_before_a_pane_had_a_feed() {
        assert!(!has_new_news(None, &state(1, &[1]).snapshot()));
        assert!(!has_new_news(None, &NewsSnapshot::default()));
    }

    /// A live subscription that holds no news yet is what a pane starts from, so the first
    /// report to fill it is the first news that pane ever has: a pane that said nothing had
    /// arrived here would be a pane that stayed silent through the arrival it exists for.
    #[test]
    fn has_new_news_should_say_the_first_report_a_pane_ever_has_arrived() {
        let empty = NewsSnapshot::default();

        assert!(has_new_news(Some(&empty), &state(1, &[1]).snapshot()));
        assert!(
            !has_new_news(Some(&empty), &empty),
            "a feed that is still empty has brought nothing"
        );
    }

    #[test]
    fn has_new_news_should_say_nothing_arrived_when_a_feed_did_not_move() {
        let snapshot = state(3, &[2, 1]).snapshot();

        assert!(!has_new_news(Some(&snapshot), &snapshot));
    }

    /// A report older than the whole feed is refused by the feed itself, and the one that
    /// belongs in the middle of it is not the report a feed leads with: a feed says it has
    /// news by leading somewhere else, and news that arrived late enough to belong below the
    /// lead is news the pane has already been shown.
    #[test]
    fn has_new_news_should_say_the_lead_is_what_arrived() {
        let mut feed = state(3, &[2, 1]);
        let current = feed.snapshot();

        feed.push(event(3));
        let arrived = feed.snapshot();
        assert!(has_new_news(Some(&current), &arrived));

        let mut late = state(3, &[2, 1]);
        let current = late.snapshot();
        late.push(event(0));
        assert!(
            !has_new_news(Some(&current), &late.snapshot()),
            "news the feed refuses never reached the pane either"
        );

        let mut between = state(3, &[3, 1]);
        let current = between.snapshot();
        between.push(event(2));
        assert!(
            !has_new_news(Some(&current), &between.snapshot()),
            "a report that belongs in the middle is not the report the feed leads with"
        );
    }

    /// A feed is ordered by the instant its reports happened, so a report that reached the
    /// service late can be inserted below the lead of a feed a pane is already showing. The
    /// row is drawn — the list is built again for it — but the feed is quiet about when that
    /// report got there, and what the sound answers is "the feed has published something new
    /// to lead with", so this is not an arrival.
    #[test]
    fn has_new_news_should_say_nothing_arrived_for_a_report_below_the_lead() {
        let mut feed = state(MAX_NEWS_ITEMS, &[3, 1]);
        let current = feed.snapshot();
        assert_eq!(timestamps(&current), [3, 1]);

        feed.push(event(2));
        let published = feed.snapshot();

        assert_eq!(timestamps(&published), [3, 2, 1]);
        assert_eq!(
            unshown(current.items(), published.items()),
            None,
            "the list is built again for a report that belongs in the middle of it"
        );
        assert!(!has_new_news(Some(&current), &published));
    }

    /// A feed that shares nothing with what a pane holds is a list built again, not a list
    /// extended: what it dropped is not news, so whether it is an arrival is whether it leads
    /// somewhere else. A feed whose newest report is one the pane was already reading is a
    /// pane being told about the same news in a new set of allocations.
    #[test]
    fn has_new_news_should_compare_a_replaced_feed_by_its_lead() {
        let all = state(
            MAX_NEWS_ITEMS,
            &(0..MAX_NEWS_ITEMS as i64).collect::<Vec<_>>(),
        );
        let current = all.snapshot();

        let mut rolled = state(
            MAX_NEWS_ITEMS,
            &(1..=MAX_NEWS_ITEMS as i64).collect::<Vec<_>>(),
        );
        rolled.push(event(MAX_NEWS_ITEMS as i64));
        assert!(has_new_news(Some(&current), &rolled.snapshot()));

        let same_news = NewsSnapshot {
            items: current.items().to_vec().into(),
        };
        assert!(
            !has_new_news(Some(&current), &same_news),
            "the same reports in new allocations are not news"
        );
    }

    /// What makes two feeds comparable is that they share the reports they have in common:
    /// a feed rebuilt from scratch describes the same news as a set of allocations the pane
    /// has never been shown, which is why a pane that is handed one starts over.
    #[test]
    fn has_new_news_should_not_be_fooled_by_a_feed_that_repeats_itself() {
        let current = state(2, &[2, 1]).snapshot();
        let rebuilt = state(2, &[2, 1]).snapshot();

        assert!(has_new_news(Some(&current), &rebuilt));
    }
}

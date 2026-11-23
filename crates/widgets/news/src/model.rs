use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError},
};

use kytrix_service_provider::service::engine::{NewsEvent, NewsKind};
use smol_str::{SmolStr, format_smolstr};
use time::{OffsetDateTime, UtcOffset};

pub(super) const MAX_NEWS_ITEMS: usize = 50;

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

#[derive(Debug)]
pub(super) struct NewsItem {
    timestamp: i64,
    title: Option<SmolStr>,
    content: SmolStr,
    category: NewsCategory,
    display: Mutex<Option<(UtcOffset, Arc<NewsItemDisplay>)>>,
}

impl NewsItem {
    fn new(event: NewsEvent) -> Self {
        Self {
            timestamp: event.timestamp,
            title: event.title,
            content: event.content,
            category: NewsCategory::from(event.category),
            display: Mutex::new(None),
        }
    }

    pub(super) fn title(&self) -> Option<&SmolStr> {
        self.title.as_ref()
    }

    pub(super) const fn content(&self) -> &SmolStr {
        &self.content
    }

    pub(super) const fn category(&self) -> NewsCategory {
        self.category
    }

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

#[derive(Debug)]
pub(super) struct NewsItemDisplay {
    pub time: SmolStr,
}

impl NewsItemDisplay {
    fn new(item: &NewsItem, zone: UtcOffset) -> Self {
        Self {
            time: format_time(item.timestamp, zone),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct NewsSnapshot {
    items: Arc<[Arc<NewsItem>]>,
}

impl PartialEq for NewsSnapshot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.items, &other.items)
    }
}

impl NewsSnapshot {
    pub(super) fn items(&self) -> &[Arc<NewsItem>] {
        &self.items
    }
}

pub(super) struct NewsState {
    items: VecDeque<Arc<NewsItem>>,
    maximum_items: usize,
}

impl NewsState {
    pub(super) fn new(maximum_items: usize) -> Self {
        let maximum_items = maximum_items.clamp(1, MAX_NEWS_ITEMS);
        Self {
            items: VecDeque::with_capacity(maximum_items),
            maximum_items,
        }
    }

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

    pub(super) fn snapshot(&self) -> NewsSnapshot {
        let mut items = Vec::with_capacity(self.items.len());
        items.extend(self.items.iter().cloned());
        NewsSnapshot {
            items: items.into(),
        }
    }
}

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

pub(super) fn unshown(held: &[Arc<NewsItem>], next: &[Arc<NewsItem>]) -> Option<usize> {
    if held.is_empty() {
        return Some(next.len());
    }

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

fn format_time(timestamp: i64, zone: UtcOffset) -> SmolStr {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp) * 1_000_000)
        .ok()
        .and_then(|time| time.checked_to_offset(zone))
        .map_or_else(
            || SmolStr::new_static("--"),
            |time| {
                format_smolstr!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    time.year(),
                    u8::from(time.month()),
                    time.day(),
                    time.hour(),
                    time.minute(),
                    time.second()
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
        assert_eq!(
            format_time(1_700_000_000_000, UtcOffset::UTC).as_str(),
            "2023-11-14 22:13:20"
        );
        assert_eq!(
            format_time(
                1_700_000_000_000,
                UtcOffset::from_hms(8, 0, 0).expect("UTC+8 is a valid offset")
            )
            .as_str(),
            "2023-11-15 06:13:20",
            "the instant is read in the zone the settings display"
        );
    }

    #[test]
    fn time_should_read_as_a_placeholder_when_it_is_not_an_instant() {
        assert_eq!(format_time(i64::MAX, UtcOffset::UTC).as_str(), "--");
    }

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
            .as_str(),
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
            .as_str(),
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
            snapshot.items()[0].content().as_str(),
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

    #[test]
    fn unshown_should_ask_for_a_replacement_when_a_report_belongs_in_the_middle() {
        let current = state(MAX_NEWS_ITEMS, &[3, 1]).snapshot();
        let mut grown = state(MAX_NEWS_ITEMS, &[3, 1]);
        grown.push(event(2));
        let next = grown.snapshot();

        assert_eq!(next.items().len(), 3);
        assert_eq!(unshown(current.items(), next.items()), None);
    }

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

    #[test]
    fn has_new_news_should_say_nothing_arrived_before_a_pane_had_a_feed() {
        assert!(!has_new_news(None, &state(1, &[1]).snapshot()));
        assert!(!has_new_news(None, &NewsSnapshot::default()));
    }

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

    #[test]
    fn has_new_news_should_not_be_fooled_by_a_feed_that_repeats_itself() {
        let current = state(2, &[2, 1]).snapshot();
        let rebuilt = state(2, &[2, 1]).snapshot();

        assert!(has_new_news(Some(&current), &rebuilt));
    }
}

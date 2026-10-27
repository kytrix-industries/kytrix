//! The transfers one subscription retains, and the addresses one account monitors.
//!
//! # Order
//!
//! Transfers are held newest first, ordered by the time the service stamped them with.
//! One that arrives late still belongs where its time says rather than at the head, and
//! two of one instant are ordered by their identifiers, newest first, so a feed reads
//! the same however the arrivals happened to be interleaved.
//!
//! # Identity
//!
//! A transfer is identified by its event, which is what the service deduplicates on: the
//! same transfer reported twice is one row, because a transfer is an event that happened
//! rather than a value that was observed.
//!
//! # The monitored addresses
//!
//! The addresses belong to the account rather than to a pane, and the service client
//! already checks and orders every answer it returns. What is kept here is therefore what
//! the service said, plus the one projection the client cannot make for us: what the list
//! becomes once a change is confirmed, which arrives as the single row it changed rather
//! than as the list.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError},
};

use gpui_kit::{App, SharedString};
use kytrix_service_provider::service::{
    cryptocurrency_wallet_monitoring::{
        CryptocurrencyWalletMonitoringSelection, CryptocurrencyWalletMonitoringSelectionsResponse,
        compare_cryptocurrency_wallet_monitoring_selections,
    },
    engine::CryptocurrencyWalletMonitoringTransfer,
};
use smol_str::{SmolStr, format_smolstr};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};
use widget::error::LocalizedError;

/// How many transfers one subscription retains.
///
/// A feed is read from the top, so what a pane keeps is what a reader can reach: older
/// transfers are dropped rather than kept behind a bound nobody scrolls past.
pub(super) const MAX_TRANSFERS: usize = 50;

/// One retained transfer, and the text its row reads.
#[derive(Debug)]
pub(super) struct TransferItem {
    event_id: SmolStr,
    timestamp: i64,
    from_address: SharedString,
    to_address: SharedString,
    amount: SharedString,
    token_symbol: SharedString,
    display: Mutex<Option<(UtcOffset, Arc<TransferDisplay>)>>,
}

impl TransferItem {
    fn new(transfer: CryptocurrencyWalletMonitoringTransfer) -> Self {
        Self {
            event_id: transfer.event_id,
            timestamp: transfer.timestamp,
            from_address: SharedString::new(transfer.from_address.as_str()),
            to_address: SharedString::new(transfer.to_address.as_str()),
            amount: SharedString::new(transfer.amount.as_str()),
            token_symbol: SharedString::new(transfer.token_symbol.as_str()),
            display: Mutex::new(None),
        }
    }

    /// The event a transfer is, which is what a feed deduplicates on.
    ///
    /// A row is drawn by the index of the list rather than by its event — the list is what
    /// remembers where a reader is — so the event is only what a test tells two rows apart
    /// by.
    #[cfg(test)]
    pub(super) fn event_id(&self) -> &str {
        &self.event_id
    }

    pub(super) const fn origin(&self) -> &SharedString {
        &self.from_address
    }

    pub(super) const fn destination(&self) -> &SharedString {
        &self.to_address
    }

    /// The text of a transfer that is produced only once something draws it.
    ///
    /// A transfer is retained the moment it arrives, while most of what a feed holds is
    /// never on screen: joining and formatting are the one part of a row that costs
    /// anything, so they happen here rather than when the transfer is retained. The zone
    /// the time was read in is remembered beside the text, so a frame that draws the same
    /// zone reuses it and a frame after the setting moved reads it again.
    pub(super) fn display(&self, zone: UtcOffset) -> Arc<TransferDisplay> {
        let mut cached = self.display.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((cached_zone, display)) = cached.as_ref()
            && *cached_zone == zone
        {
            return Arc::clone(display);
        }

        let display = Arc::new(TransferDisplay::new(self, zone));
        *cached = Some((zone, Arc::clone(&display)));
        display
    }
}

/// The part of a transfer's row that is built rather than borrowed.
#[derive(Debug)]
pub(super) struct TransferDisplay {
    /// The amount and what it is denominated in, which a row reads as one value.
    pub amount: SharedString,
    pub time: SharedString,
}

impl TransferDisplay {
    fn new(item: &TransferItem, zone: UtcOffset) -> Self {
        Self {
            amount: SharedString::new(
                format_smolstr!("{} {}", item.amount, item.token_symbol).as_str(),
            ),
            time: format_time(item.timestamp, zone),
        }
    }
}

/// An immutable, cheap-to-clone view of the retained transfers.
#[derive(Clone, Debug)]
pub(super) struct TransferSnapshot {
    items: Arc<[Arc<TransferItem>]>,
}

impl TransferSnapshot {
    /// The transfers of this snapshot, newest first.
    pub(super) fn items(&self) -> &[Arc<TransferItem>] {
        &self.items
    }
}

/// The transfers one subscription retains.
pub(super) struct TransferState {
    items: VecDeque<Arc<TransferItem>>,
}

impl TransferState {
    pub(super) fn new() -> Self {
        Self {
            items: VecDeque::with_capacity(MAX_TRANSFERS),
        }
    }

    /// Retains one reported transfer.
    ///
    /// Returns whether the feed changed: a transfer the feed already holds is the same
    /// event reported twice, and one older than everything a full feed holds is refused
    /// rather than displacing transfers that are newer than it.
    pub(super) fn push(&mut self, transfer: CryptocurrencyWalletMonitoringTransfer) -> bool {
        if self
            .items
            .iter()
            .any(|current| current.event_id == transfer.event_id)
        {
            return false;
        }
        let insertion = self.items.iter().position(|current| {
            current.timestamp < transfer.timestamp
                || (current.timestamp == transfer.timestamp
                    && current.event_id.as_str() < transfer.event_id.as_str())
        });
        if self.items.len() == MAX_TRANSFERS && insertion.is_none() {
            return false;
        }

        let item = Arc::new(TransferItem::new(transfer));
        if self.items.len() == MAX_TRANSFERS {
            self.items.pop_back();
        }
        match insertion {
            Some(index) => self.items.insert(index, item),
            None => self.items.push_back(item),
        }
        true
    }

    /// The retained transfers, newest first.
    pub(super) fn snapshot(&self) -> TransferSnapshot {
        let mut items = Vec::with_capacity(self.items.len());
        items.extend(self.items.iter().cloned());
        TransferSnapshot {
            items: items.into(),
        }
    }
}

/// How many of a newer feed's newest transfers a list holding `held` has not been shown.
///
/// A feed and the list that draws it are both newest first and both bounded to the same
/// number of transfers, so a transfer the list has not been shown is one at the head of the
/// feed, and what follows it is the run the list already holds. `None` is a feed the list
/// cannot be extended into — a transfer that arrived late enough to belong in the middle of
/// it, or one that shares no transfer with what the list holds — and the list a pane draws is
/// built again for it instead.
pub(super) fn unshown(held: &[Arc<TransferItem>], next: &[Arc<TransferItem>]) -> Option<usize> {
    // A list that holds nothing has been shown nothing.
    if held.is_empty() {
        return Some(next.len());
    }

    // Everything the new feed added is at its head, so the first transfer of it the list
    // already holds is the first transfer the two have in common.
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

/// The addresses one account monitors, as the service last answered them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SelectionData {
    selections: Arc<[CryptocurrencyWalletMonitoringSelection]>,
    limit: usize,
}

impl SelectionData {
    /// The addresses one service answer holds.
    ///
    /// The answer has already been checked and ordered by the service client, so this is
    /// what it said rather than a second opinion about it.
    pub(super) fn new(response: CryptocurrencyWalletMonitoringSelectionsResponse) -> Self {
        Self {
            selections: response.selections.into(),
            limit: response.cryptocurrency_wallet_monitoring_address_limit,
        }
    }

    pub(super) fn selections(&self) -> &[CryptocurrencyWalletMonitoringSelection] {
        &self.selections
    }

    /// The rows themselves, for a list that is built once and read on every frame.
    pub(super) fn shared_selections(&self) -> Arc<[CryptocurrencyWalletMonitoringSelection]> {
        self.selections.clone()
    }

    pub(super) const fn limit(&self) -> usize {
        self.limit
    }

    /// Whether the account may monitor nothing further.
    ///
    /// Every address counts, including the ones a paused history holds: the limit is what
    /// the account is allowed to have selected, not what it is currently watching.
    pub(super) fn quota_full(&self) -> bool {
        self.selections.len() >= self.limit
    }

    /// The addresses this set becomes once the service confirms one was added.
    ///
    /// A confirmation the list cannot hold means the list and the account have gone out
    /// of step — the service accepted an address the pane believes is beyond the quota,
    /// already there, or not the one it asked about — and the next read is what settles
    /// which of the two is right.
    pub(super) fn with_added(
        &self,
        requested: &str,
        selection: CryptocurrencyWalletMonitoringSelection,
    ) -> Result<Self, CryptocurrencyWalletMonitoringDataError> {
        if selection.address != requested
            || self.quota_full()
            || self
                .selections
                .iter()
                .any(|current| current.address == selection.address)
        {
            return Err(CryptocurrencyWalletMonitoringDataError::InconsistentSelections);
        }

        let mut selections = Vec::with_capacity(self.selections.len() + 1);
        selections.extend(self.selections.iter().cloned());
        selections.push(selection);
        // The order is the account's recency order, which is the one a fresh read
        // returns: a list the pane projects itself must read like the one it fetches.
        selections.sort_by(compare_cryptocurrency_wallet_monitoring_selections);
        Ok(Self {
            selections: selections.into(),
            limit: self.limit,
        })
    }

    /// The addresses this set becomes once the service confirms one was removed.
    pub(super) fn with_deleted(&self, address: &str) -> Self {
        let selections = self
            .selections
            .iter()
            .filter(|current| current.address != address)
            .cloned()
            .collect::<Vec<_>>();
        if selections.len() == self.selections.len() {
            return self.clone();
        }
        Self {
            selections: selections.into(),
            limit: self.limit,
        }
    }
}

/// Why a confirmed change cannot be applied to the addresses a pane is holding.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum CryptocurrencyWalletMonitoringDataError {
    #[error(
        "the confirmed cryptocurrency_wallet_monitoring address does not fit the list it was added to"
    )]
    InconsistentSelections,
}

impl LocalizedError for CryptocurrencyWalletMonitoringDataError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::InconsistentSelections => {
                i18n::t!(
                    cx,
                    "widget.cryptocurrency_wallet_monitoring.inconsistent_selections"
                )
            }
        }
    }
}

/// The time of a transfer, read in the zone the settings display data in.
///
/// The service stamps transfers in UTC, so what a reader sees is that instant
/// converted by the offset the caller read from the settings.
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
    use kytrix_service_provider::service::cryptocurrency_wallet_monitoring::{
        CryptocurrencyWalletMonitoringNetwork, CryptocurrencyWalletMonitoringSelectionStatus,
        compare_cryptocurrency_wallet_monitoring_selections,
    };
    use smol_str::SmolStr;

    use super::*;

    const FROM: &str = "0x0123456789abcdef0123456789abcdef01234567";
    const TO: &str = "0x89abcdef0123456789abcdef0123456789abcdef";
    const FIRST: &str = "0x1111111111111111111111111111111111111111";
    const SECOND: &str = "0x2222222222222222222222222222222222222222";
    const THIRD: &str = "0x3333333333333333333333333333333333333333";
    const FOURTH: &str = "0x4444444444444444444444444444444444444444";

    fn transfer(event_id: &str, timestamp: i64) -> CryptocurrencyWalletMonitoringTransfer {
        CryptocurrencyWalletMonitoringTransfer {
            event_id: SmolStr::new(event_id),
            from_address: SmolStr::new_static(FROM),
            to_address: SmolStr::new_static(TO),
            amount: SmolStr::new_static("1.25"),
            token_symbol: SmolStr::new_static("ETH"),
            timestamp,
        }
    }

    fn selection(address: &str, updated_at: i64) -> CryptocurrencyWalletMonitoringSelection {
        CryptocurrencyWalletMonitoringSelection {
            network: CryptocurrencyWalletMonitoringNetwork::Ethereum,
            address: SmolStr::new(address),
            status: CryptocurrencyWalletMonitoringSelectionStatus::Active,
            created_at: updated_at,
            updated_at,
        }
    }

    fn paused(address: &str, updated_at: i64) -> CryptocurrencyWalletMonitoringSelection {
        CryptocurrencyWalletMonitoringSelection {
            status: CryptocurrencyWalletMonitoringSelectionStatus::Paused,
            ..selection(address, updated_at)
        }
    }

    fn response(
        selections: Vec<CryptocurrencyWalletMonitoringSelection>,
        limit: usize,
    ) -> CryptocurrencyWalletMonitoringSelectionsResponse {
        CryptocurrencyWalletMonitoringSelectionsResponse {
            selections,
            cryptocurrency_wallet_monitoring_address_limit: limit,
        }
    }

    fn identifiers(snapshot: &TransferSnapshot) -> Vec<&str> {
        snapshot
            .items()
            .iter()
            .map(|item| item.event_id())
            .collect()
    }

    fn addresses(data: &SelectionData) -> Vec<&str> {
        data.selections()
            .iter()
            .map(|selection| selection.address.as_str())
            .collect()
    }

    #[test]
    fn the_feed_should_keep_only_the_newest_transfers() {
        let mut state = TransferState::new();
        for timestamp in 0..55 {
            assert!(state.push(transfer(&timestamp.to_string(), timestamp)));
        }
        assert!(
            !state.push(transfer("54", 999)),
            "a transfer the feed already holds is the same event reported twice"
        );

        let snapshot = state.snapshot();
        assert_eq!(snapshot.items().len(), MAX_TRANSFERS);
        assert_eq!(identifiers(&snapshot)[0], "54");
        assert_eq!(
            identifiers(&snapshot)[MAX_TRANSFERS - 1],
            "5",
            "the feed keeps the newest transfers and drops the oldest"
        );
    }

    #[test]
    fn a_transfer_older_than_a_full_feed_should_be_refused() {
        let mut state = TransferState::new();
        for timestamp in 0..(MAX_TRANSFERS as i64) {
            assert!(state.push(transfer(&timestamp.to_string(), timestamp)));
        }

        assert!(
            !state.push(transfer("older", -1)),
            "a transfer older than everything the feed holds displaces nothing"
        );
        assert_eq!(state.snapshot().items().len(), MAX_TRANSFERS);
    }

    #[test]
    fn a_transfer_that_arrived_late_should_be_placed_by_its_time() {
        let mut state = TransferState::new();
        state.push(transfer("third", 3));
        state.push(transfer("first", 1));
        state.push(transfer("second", 2));

        assert_eq!(identifiers(&state.snapshot()), ["third", "second", "first"]);
    }

    #[test]
    fn two_transfers_of_one_instant_should_read_the_same_however_they_arrived() {
        let mut first_then_second = TransferState::new();
        first_then_second.push(transfer("a", 7));
        first_then_second.push(transfer("b", 7));

        let mut second_then_first = TransferState::new();
        second_then_first.push(transfer("b", 7));
        second_then_first.push(transfer("a", 7));

        let first = first_then_second.snapshot();
        let second = second_then_first.snapshot();
        let reading = identifiers(&first);
        assert_eq!(
            reading,
            identifiers(&second),
            "one instant is one moment of the feed, not two orderings of it"
        );
        assert_eq!(reading.len(), 2);
    }

    #[test]
    fn a_zero_amount_should_still_be_displayed() {
        let mut state = TransferState::new();
        let mut zero = transfer("zero", 1);
        zero.amount = SmolStr::new_static("0");
        state.push(zero);

        assert_eq!(
            state.snapshot().items()[0].display(UtcOffset::UTC).amount,
            "0 ETH"
        );
    }

    #[test]
    fn the_time_of_a_transfer_should_be_read_in_the_display_zone() {
        let mut state = TransferState::new();
        state.push(transfer("instant", 1_700_000_000_000));
        let snapshot = state.snapshot();
        let item = &snapshot.items()[0];

        assert_eq!(
            item.display(UtcOffset::UTC).time,
            "2023-11-14 22:13:20",
            "the instant is the service's rather than the reader's clock"
        );
        assert_eq!(
            item.display(UtcOffset::from_hms(8, 0, 0).expect("UTC+8 is a valid offset"))
                .time,
            "2023-11-15 06:13:20",
            "and it is drawn in the zone the settings display data in"
        );
    }

    #[test]
    fn a_transfer_the_service_stamped_out_of_range_should_read_without_a_time() {
        let mut state = TransferState::new();
        state.push(transfer("beyond", i64::MAX));
        let snapshot = state.snapshot();
        let item = &snapshot.items()[0];

        assert_eq!(item.display(UtcOffset::UTC).time, "--");
    }

    /// The display offset can move an instant past the last day the calendar holds.
    /// That is not a date this can write, so it is a placeholder rather than a panic.
    #[test]
    fn a_transfer_the_display_zone_cannot_hold_should_read_without_a_time() {
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
            ),
            "--"
        );

        let boundary = time::Date::MIN
            .with_hms(0, 0, 0)
            .expect("the first day has a midnight")
            .assume_utc();
        let timestamp = i64::try_from(boundary.unix_timestamp_nanos() / 1_000_000)
            .expect("the first day is a millisecond timestamp");

        assert_eq!(
            format_time(
                timestamp,
                UtcOffset::from_hms(-12, 0, 0).expect("UTC-12 is a valid offset")
            ),
            "--",
            "a western offset runs off the other end of the calendar"
        );
    }

    #[test]
    fn the_addresses_should_be_kept_as_the_answer_listed_them() {
        let data = SelectionData::new(response(
            vec![
                selection(THIRD, 3),
                selection(FIRST, 1),
                selection(SECOND, 2),
            ],
            5,
        ));

        assert_eq!(
            addresses(&data),
            [THIRD, FIRST, SECOND],
            "an answer arrives ordered by the service client and is not reordered here"
        );
    }

    #[test]
    fn a_zero_quota_should_be_full() {
        let data = SelectionData::new(response(Vec::new(), 0));

        assert!(data.quota_full());
        assert_eq!(data.limit(), 0);
    }

    #[test]
    fn an_add_should_be_refused_when_the_quota_is_full() {
        let data = SelectionData::new(response(vec![paused(FIRST, 1)], 1));

        assert_eq!(
            data.with_added(SECOND, selection(SECOND, 2)),
            Err(CryptocurrencyWalletMonitoringDataError::InconsistentSelections),
            "a paused address still counts against the quota"
        );
    }

    #[test]
    fn an_add_the_list_already_holds_should_be_refused() {
        let data = SelectionData::new(response(vec![selection(FIRST, 1)], 2));

        assert_eq!(
            data.with_added(FIRST, selection(FIRST, 2)),
            Err(CryptocurrencyWalletMonitoringDataError::InconsistentSelections)
        );
    }

    #[test]
    fn an_add_of_another_address_should_be_refused() {
        let data = SelectionData::new(response(vec![selection(FIRST, 1)], 2));

        assert_eq!(
            data.with_added(SECOND, selection(THIRD, 2)),
            Err(CryptocurrencyWalletMonitoringDataError::InconsistentSelections),
            "a confirmation about a different address is not the one that was asked for"
        );
    }

    #[test]
    fn an_add_should_be_placed_by_recency() {
        let data = SelectionData::new(response(vec![selection(SECOND, 2), selection(FIRST, 1)], 3));

        let grown = data
            .with_added(THIRD, selection(THIRD, 3))
            .expect("an address within the quota should be accepted");

        assert_eq!(addresses(&grown), [THIRD, SECOND, FIRST]);
        assert_eq!(grown.limit(), 3, "adding does not change the quota");
    }

    #[test]
    fn a_delete_should_remove_the_address_and_keep_the_order() {
        let data = SelectionData::new(response(
            vec![
                selection(THIRD, 3),
                selection(SECOND, 2),
                selection(FIRST, 1),
            ],
            3,
        ));

        let shrunk = data.with_deleted(SECOND);

        assert_eq!(addresses(&shrunk), [THIRD, FIRST]);
    }

    #[test]
    fn a_delete_of_an_address_the_list_does_not_hold_should_change_nothing() {
        let data = SelectionData::new(response(vec![selection(FIRST, 1)], 2));

        assert_eq!(data.with_deleted(SECOND), data);
    }

    #[test]
    fn a_delete_should_bring_a_full_list_back_under_its_quota() {
        let data = SelectionData::new(response(vec![paused(FIRST, 1), paused(SECOND, 2)], 2));
        assert!(data.quota_full());

        let shrunk = data.with_deleted(FIRST);

        assert!(!shrunk.quota_full());
        assert!(
            shrunk.with_added(THIRD, selection(THIRD, 3)).is_ok(),
            "removing history is what makes room for a new address"
        );
    }

    /// The projection a pane makes has to read like the list a service answer returns,
    /// which is the order the service client sorts by.
    #[test]
    fn a_projected_list_should_read_like_a_fetched_one() {
        let data = SelectionData::new(response(vec![selection(FIRST, 1)], 4));
        let projected = data
            .with_added(FOURTH, selection(FOURTH, 9))
            .expect("an address within the quota should be accepted");
        let fetched =
            SelectionData::new(response(vec![selection(FOURTH, 9), selection(FIRST, 1)], 4));

        assert_eq!(addresses(&projected), addresses(&fetched));
    }

    /// The order a projection ends in is the service's own comparison, so a change of a
    /// tie is a change of that comparison rather than of this projection alone.
    #[test]
    fn a_projection_should_order_ties_the_way_the_service_does() {
        let mut expected = [selection(SECOND, 5), selection(FIRST, 5)];
        expected.sort_by(compare_cryptocurrency_wallet_monitoring_selections);
        let data = SelectionData::new(response(vec![selection(SECOND, 5)], 3));

        let projected = data
            .with_added(FIRST, selection(FIRST, 5))
            .expect("an address within the quota should be accepted");

        assert_eq!(
            addresses(&projected),
            expected
                .iter()
                .map(|selection| selection.address.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// A feed whose rows read in the order they are given, and which a test then grows.
    ///
    /// The rows are ordered by the time they were stamped with, so the first identifier is
    /// the newest and each following one is ten milliseconds older — which leaves room for
    /// a transfer stamped between two of them.
    fn feed(ids: &[&str]) -> TransferState {
        let mut state = TransferState::new();
        for (offset, id) in ids.iter().enumerate() {
            state.push(transfer(id, 1_000 - offset as i64 * 10));
        }
        state
    }

    /// A feed that grew at its head is the transfers at the head of it a list has not been
    /// shown, and what the list holds is what follows them however many left at its tail.
    #[test]
    fn unshown_should_count_the_transfers_at_the_head_of_a_feed() {
        let mut full = TransferState::new();
        for timestamp in 0..MAX_TRANSFERS {
            full.push(transfer(&timestamp.to_string(), timestamp as i64));
        }
        let current = full.snapshot();
        full.push(transfer("new", MAX_TRANSFERS as i64));
        let next = full.snapshot();

        assert_eq!(unshown(current.items(), next.items()), Some(1));

        let mut grown = feed(&["a", "b"]);
        let current = grown.snapshot();
        grown.push(transfer("newer", 2_000));
        grown.push(transfer("newest", 3_000));
        let next = grown.snapshot();

        assert_eq!(
            unshown(current.items(), next.items()),
            Some(2),
            "every transfer the list has not been shown is one of them"
        );
    }

    #[test]
    fn unshown_should_report_nothing_for_a_feed_that_did_not_move() {
        let snapshot = feed(&["a", "b"]).snapshot();

        assert_eq!(unshown(snapshot.items(), snapshot.items()), Some(0));
    }

    /// A list that holds nothing has been shown nothing, so every transfer of a feed is one
    /// it has not been shown.
    #[test]
    fn unshown_should_ask_for_every_transfer_of_a_feed_a_list_holds_nothing_of() {
        let next = feed(&["a", "b"]).snapshot();

        assert_eq!(unshown(&[], next.items()), Some(2));
    }

    #[test]
    fn unshown_should_ask_for_a_replacement_when_nothing_survives() {
        let current = feed(&["a"]).snapshot();
        let next = feed(&["b"]).snapshot();

        assert_eq!(unshown(current.items(), next.items()), None);
    }

    /// A feed that lost every transfer a list holds has nothing to extend it with, so what
    /// the list holds is replaced rather than left standing behind an empty feed.
    #[test]
    fn unshown_should_ask_for_a_replacement_when_a_feed_lost_every_transfer() {
        let held = feed(&["a"]).snapshot();
        let next = TransferState::new().snapshot();

        assert_eq!(unshown(held.items(), next.items()), None);
    }

    /// A transfer that belongs in the middle of a feed is not a feed a list can be extended
    /// into: what it holds is replaced rather than rearranged.
    #[test]
    fn unshown_should_ask_for_a_replacement_when_a_transfer_belongs_in_the_middle() {
        let mut state = feed(&["a", "c"]);
        let current = state.snapshot();
        state.push(transfer("b", 995));
        let next = state.snapshot();

        assert_eq!(
            identifiers(&next),
            ["a", "b", "c"],
            "the late transfer belongs between the two the feed already holds"
        );
        assert_eq!(unshown(current.items(), next.items()), None);
    }

    /// A feed that lost the transfer a list holds at its head is not one the list can be
    /// extended into either: what it holds no longer starts where the feed does.
    #[test]
    fn unshown_should_ask_for_a_replacement_when_a_feed_lost_its_head() {
        let current = feed(&["a", "b"]).snapshot();
        let next = &current.items()[1..];

        assert_eq!(unshown(current.items(), next), None);
    }
}

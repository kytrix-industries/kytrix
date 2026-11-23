use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError},
};

use i18n::{t_smol, translate::Translate};
use kytrix_service_provider::service::{
    engine::SolanaWalletTransfer,
    solana_wallet::{
        SolanaWalletSelection, SolanaWalletSelectionsResponse, compare_solana_wallet_selections,
    },
};
use smol_str::{SmolStr, format_smolstr};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};
use widget::error::TranslatedError;

pub(super) const MAX_TRANSFERS: usize = 50;

#[derive(Debug)]
pub(super) struct TransferItem {
    event_id: SmolStr,
    timestamp: i64,
    from_address: SmolStr,
    to_address: SmolStr,
    amount: SmolStr,
    token_symbol: SmolStr,
    display: Mutex<Option<(UtcOffset, Arc<TransferDisplay>)>>,
}

impl TransferItem {
    fn new(transfer: SolanaWalletTransfer) -> Self {
        Self {
            event_id: transfer.event_id,
            timestamp: transfer.timestamp,
            from_address: transfer.from_address,
            to_address: transfer.to_address,
            amount: transfer.amount,
            token_symbol: transfer.token_symbol,
            display: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(super) fn event_id(&self) -> &str {
        &self.event_id
    }

    pub(super) const fn origin(&self) -> &SmolStr {
        &self.from_address
    }

    pub(super) const fn destination(&self) -> &SmolStr {
        &self.to_address
    }

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

#[derive(Debug)]
pub(super) struct TransferDisplay {
    pub amount: SmolStr,
    pub time: SmolStr,
}

impl TransferDisplay {
    fn new(item: &TransferItem, zone: UtcOffset) -> Self {
        Self {
            amount: format_smolstr!("{} {}", item.amount, item.token_symbol),
            time: format_time(item.timestamp, zone),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct TransferSnapshot {
    items: Arc<[Arc<TransferItem>]>,
}

impl TransferSnapshot {
    pub(super) fn items(&self) -> &[Arc<TransferItem>] {
        &self.items
    }
}

impl Default for TransferSnapshot {
    fn default() -> Self {
        Self {
            items: Arc::from([]),
        }
    }
}

impl PartialEq for TransferSnapshot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.items, &other.items)
    }
}

impl Eq for TransferSnapshot {}

pub(super) struct TransferState {
    items: VecDeque<Arc<TransferItem>>,
}

impl TransferState {
    pub(super) fn new() -> Self {
        Self {
            items: VecDeque::with_capacity(MAX_TRANSFERS),
        }
    }

    pub(super) fn push(&mut self, transfer: SolanaWalletTransfer) -> bool {
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

    pub(super) fn snapshot(&self) -> TransferSnapshot {
        let mut items = Vec::with_capacity(self.items.len());
        items.extend(self.items.iter().cloned());
        TransferSnapshot {
            items: items.into(),
        }
    }
}

pub(super) fn unshown(held: &[Arc<TransferItem>], next: &[Arc<TransferItem>]) -> Option<usize> {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SelectionData {
    selections: Arc<[SolanaWalletSelection]>,
    limit: usize,
}

impl SelectionData {
    pub(super) fn new(response: SolanaWalletSelectionsResponse) -> Self {
        Self {
            selections: response.selections.into(),
            limit: response.solana_wallet_monitor_address_limit,
        }
    }

    pub(super) fn selections(&self) -> &[SolanaWalletSelection] {
        &self.selections
    }

    pub(super) fn shared_selections(&self) -> Arc<[SolanaWalletSelection]> {
        self.selections.clone()
    }

    pub(super) const fn limit(&self) -> usize {
        self.limit
    }

    pub(super) fn quota_full(&self) -> bool {
        self.selections.len() >= self.limit
    }

    pub(super) fn with_added(
        &self,
        requested: &str,
        selection: SolanaWalletSelection,
    ) -> Result<Self, SolanaWalletDataError> {
        if selection.address != requested
            || self.quota_full()
            || self
                .selections
                .iter()
                .any(|current| current.address == selection.address)
        {
            return Err(SolanaWalletDataError::InconsistentSelections);
        }

        let mut selections = Vec::with_capacity(self.selections.len() + 1);
        selections.extend(self.selections.iter().cloned());
        selections.push(selection);
        selections.sort_by(compare_solana_wallet_selections);
        Ok(Self {
            selections: selections.into(),
            limit: self.limit,
        })
    }

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

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum SolanaWalletDataError {
    #[error("the confirmed solana_wallet address does not fit the list it was added to")]
    InconsistentSelections,
}

impl TranslatedError for SolanaWalletDataError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::InconsistentSelections => {
                t_smol!(translate, "widget.solana_wallet.inconsistent_selections")
            }
        }
    }
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
    use kytrix_service_provider::service::solana_wallet::compare_solana_wallet_selections;
    use smol_str::SmolStr;

    use super::*;

    const FROM: &str = "So11111111111111111111111111111111111111112";
    const TO: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    const FIRST: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
    const SECOND: &str = "BPFLoaderUpgradeab1e11111111111111111111111";
    const THIRD: &str = "ComputeBudget111111111111111111111111111111";
    const FOURTH: &str = "SysvarC1ock11111111111111111111111111111111";

    fn transfer(event_id: &str, timestamp: i64) -> SolanaWalletTransfer {
        SolanaWalletTransfer {
            event_id: SmolStr::new(event_id),
            from_address: SmolStr::new_static(FROM),
            to_address: SmolStr::new_static(TO),
            amount: SmolStr::new_static("1.25"),
            token_symbol: SmolStr::new_static("SOL"),
            timestamp,
        }
    }

    fn selection(address: &str, created_at: i64) -> SolanaWalletSelection {
        SolanaWalletSelection {
            address: SmolStr::new(address),
            created_at,
        }
    }

    fn response(
        selections: Vec<SolanaWalletSelection>,
        limit: usize,
    ) -> SolanaWalletSelectionsResponse {
        SolanaWalletSelectionsResponse {
            selections,
            solana_wallet_monitor_address_limit: limit,
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
            "0 SOL"
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
        let data = SelectionData::new(response(vec![selection(FIRST, 1)], 1));

        assert_eq!(
            data.with_added(SECOND, selection(SECOND, 2)),
            Err(SolanaWalletDataError::InconsistentSelections),
            "an address still counts against the quota"
        );
    }

    #[test]
    fn an_add_the_list_already_holds_should_be_refused() {
        let data = SelectionData::new(response(vec![selection(FIRST, 1)], 2));

        assert_eq!(
            data.with_added(FIRST, selection(FIRST, 2)),
            Err(SolanaWalletDataError::InconsistentSelections)
        );
    }

    #[test]
    fn an_add_of_another_address_should_be_refused() {
        let data = SelectionData::new(response(vec![selection(FIRST, 1)], 2));

        assert_eq!(
            data.with_added(SECOND, selection(THIRD, 2)),
            Err(SolanaWalletDataError::InconsistentSelections),
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
        let data = SelectionData::new(response(vec![selection(FIRST, 1), selection(SECOND, 2)], 2));
        assert!(data.quota_full());

        let shrunk = data.with_deleted(FIRST);

        assert!(!shrunk.quota_full());
        assert!(
            shrunk.with_added(THIRD, selection(THIRD, 3)).is_ok(),
            "removing an address is what makes room for a new one"
        );
    }

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

    #[test]
    fn a_projection_should_order_ties_the_way_the_service_does() {
        let mut expected = [selection(SECOND, 5), selection(FIRST, 5)];
        expected.sort_by(compare_solana_wallet_selections);
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

    fn feed(ids: &[&str]) -> TransferState {
        let mut state = TransferState::new();
        for (offset, id) in ids.iter().enumerate() {
            state.push(transfer(id, 1_000 - offset as i64 * 10));
        }
        state
    }

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

    #[test]
    fn unshown_should_ask_for_a_replacement_when_a_feed_lost_every_transfer() {
        let held = feed(&["a"]).snapshot();
        let next = TransferState::new().snapshot();

        assert_eq!(unshown(held.items(), next.items()), None);
    }

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

    #[test]
    fn unshown_should_ask_for_a_replacement_when_a_feed_lost_its_head() {
        let current = feed(&["a", "b"]).snapshot();
        let next = &current.items()[1..];

        assert_eq!(unshown(current.items(), next), None);
    }
}

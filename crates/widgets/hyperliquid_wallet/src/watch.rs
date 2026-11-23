use ahash::AHashSet;
use market_participants_model::identifier::WalletAddress;
use serde::{Deserialize, Deserializer};

pub(super) const MAX_WALLET_ADDRESSES: usize = 10;

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub(super) enum WatchListError {
    #[error("at most {MAX_WALLET_ADDRESSES} Hyperliquid wallet addresses are supported")]
    TooMany,
    #[error("wallet {address} is listed more than once")]
    Duplicate { address: WalletAddress },
}

pub(super) fn validate(addresses: &[WalletAddress]) -> Result<(), WatchListError> {
    if addresses.len() > MAX_WALLET_ADDRESSES {
        return Err(WatchListError::TooMany);
    }
    let mut seen = AHashSet::with_capacity(addresses.len());
    for address in addresses {
        if !seen.insert(address) {
            return Err(WatchListError::Duplicate {
                address: address.clone(),
            });
        }
    }
    Ok(())
}

pub(super) fn with(
    addresses: &[WalletAddress],
    address: WalletAddress,
) -> Result<Vec<WalletAddress>, WatchListError> {
    if addresses.contains(&address) {
        return Err(WatchListError::Duplicate { address });
    }
    if addresses.len() >= MAX_WALLET_ADDRESSES {
        return Err(WatchListError::TooMany);
    }
    let mut next = Vec::with_capacity(addresses.len() + 1);
    next.extend_from_slice(addresses);
    next.push(address);
    Ok(next)
}

pub(super) fn without(addresses: &[WalletAddress], address: &WalletAddress) -> Vec<WalletAddress> {
    addresses
        .iter()
        .filter(|current| *current != address)
        .cloned()
        .collect()
}

pub(super) fn deserialize_addresses<'de, D>(deserializer: D) -> Result<Vec<WalletAddress>, D::Error>
where
    D: Deserializer<'de>,
{
    let addresses = Vec::<WalletAddress>::deserialize(deserializer)?;
    validate(&addresses).map_err(serde::de::Error::custom)?;
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::address;

    fn list(length: usize) -> Vec<WalletAddress> {
        (0..length)
            .map(|index| address(&format!("{index}")))
            .collect()
    }

    fn list_of(suffixes: &[&str]) -> Vec<WalletAddress> {
        suffixes.iter().map(|suffix| address(suffix)).collect()
    }

    #[test]
    fn a_list_should_refuse_the_same_address_twice() {
        let first = address("1");

        assert_eq!(
            validate(&[first.clone(), first.clone()]),
            Err(WatchListError::Duplicate { address: first })
        );
    }

    #[test]
    fn a_list_should_refuse_an_address_past_its_limit() {
        let full = list(MAX_WALLET_ADDRESSES);
        let past = address("a");

        assert_eq!(full.len(), MAX_WALLET_ADDRESSES);
        assert_eq!(validate(&full), Ok(()));
        assert_eq!(with(&full, past.clone()), Err(WatchListError::TooMany));

        let mut longer = full;
        longer.push(past);
        assert_eq!(validate(&longer), Err(WatchListError::TooMany));
    }

    #[test]
    fn adding_an_address_should_keep_the_order_it_was_added_in() {
        let first = address("1");
        let second = address("2");

        let added = with(std::slice::from_ref(&first), second.clone()).expect("room for one more");

        assert_eq!(added, vec![first, second]);
    }

    #[test]
    fn adding_an_address_the_list_already_holds_should_be_refused() {
        let first = address("1");

        assert_eq!(
            with(std::slice::from_ref(&first), first.clone()),
            Err(WatchListError::Duplicate { address: first })
        );
    }

    #[test]
    fn removing_an_address_should_keep_the_order_of_the_others() {
        assert_eq!(
            without(&list_of(&["1", "2", "3"]), &address("2")),
            list_of(&["1", "3"])
        );
    }

    #[test]
    fn a_list_that_does_not_hold_an_address_should_be_unchanged_by_removing_it() {
        let before = list_of(&["1", "2"]);

        assert_eq!(without(&before, &address("9")), before);
    }
}

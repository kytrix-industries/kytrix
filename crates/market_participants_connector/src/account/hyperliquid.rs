//! Hyperliquid wallet streams and their mapping into account values.

use super::{
    domain::{HyperliquidUserFill, HyperliquidWalletUpdate},
    error::{AccountError, account_error_from_market_participant},
    mapping::{
        WalletCatalogs, map_hyperliquid_asset_context, map_hyperliquid_user_fill, map_ledger_event,
    },
};
use crate::{adapter::ProtocolEvent, market_participants::model};

/// Stream of wallet-facing Hyperliquid updates mapped to account values.
///
/// The stream holds the proof that its monitored routes' catalogs are published, so a
/// fill that names a coin resolves to the listing of whichever route published it — and
/// to that listing's units — without awaiting a fetch. A wallet stream is venue-wide:
/// one stream carries the coins of every scope its reader monitors.
pub struct HyperliquidSubscription {
    inner: crate::adapter::ProtocolStream,
    catalogs: WalletCatalogs,
}

impl HyperliquidSubscription {
    pub(super) fn new(inner: crate::adapter::ProtocolStream, catalogs: WalletCatalogs) -> Self {
        Self { inner, catalogs }
    }

    pub async fn recv(&mut self) -> Option<Result<HyperliquidWalletUpdate, AccountError>> {
        use futures::StreamExt;
        while let Some(item) = self.inner.next().await {
            let event = match item {
                Ok(ProtocolEvent::HyperliquidWalletUpdate(value)) => value,
                Ok(_) => continue,
                Err(error) => return Some(Err(account_error_from_market_participant(error))),
            };
            let mapped = match event {
                model::NativeHyperliquidWalletUpdate::UserFills { is_snapshot, fills } => {
                    let fills = fills
                        .iter()
                        .map(|fill| {
                            let (_, resolved, assets) = self.catalogs.resolve(&fill.listing)?;
                            map_hyperliquid_user_fill(fill, &resolved, assets)
                        })
                        .collect::<Result<Vec<HyperliquidUserFill>, AccountError>>();
                    fills.map(|fills| HyperliquidWalletUpdate::UserFills { is_snapshot, fills })
                }
                model::NativeHyperliquidWalletUpdate::UserLedger {
                    is_snapshot,
                    events,
                } => {
                    let events = match events
                        .iter()
                        .map(|event| map_ledger_event(event, &self.catalogs))
                        .collect::<Result<Vec<_>, AccountError>>()
                    {
                        Ok(events) => events,
                        Err(error) => return Some(Err(error)),
                    };
                    Ok(HyperliquidWalletUpdate::UserLedger {
                        is_snapshot,
                        events,
                    })
                }
                model::NativeHyperliquidWalletUpdate::FastAssetContexts { contexts } => {
                    let mut mapped = Vec::with_capacity(contexts.len());
                    for context in &contexts {
                        let (_, resolved, _) = match self.catalogs.resolve(&context.listing) {
                            Ok(resolved) => resolved,
                            Err(error) => return Some(Err(error)),
                        };
                        mapped.push(map_hyperliquid_asset_context(context, &resolved));
                    }
                    Ok(HyperliquidWalletUpdate::FastAssetContexts { contexts: mapped })
                }
            };
            return Some(mapped);
        }
        None
    }
}

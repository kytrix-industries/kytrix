use std::sync::Arc;

use ahash::{AHashMap, AHashSet};
use i18n::{t_smol, translate::Translate};
use market_participants_connector::{
    account::{
        domain::{
            HyperliquidAssetContext, HyperliquidHistoricalOrder, HyperliquidLedgerEvent,
            HyperliquidOpenOrder, HyperliquidPerpetualPosition, HyperliquidPerpetualState,
            HyperliquidSpotPair, HyperliquidSpotState, HyperliquidUserFill,
        },
        shared::PositionDirection,
    },
    catalog::{batch::CatalogEntry, display::catalog::DisplaySnapshot},
};
use market_participants_model::{
    asset::AssetId,
    decimal::{Price, ProfitOrLoss, Quantity, SignedQuantity, Tick},
    identifier::VenueId,
    instrument::{InstrumentKind, InstrumentTerms},
    listing::{
        route::{VenueListingKey, VenueRoute},
        rules::ListingPrecision,
        venue::PriceBasis,
    },
    market::{account::AssetAmount, price::MarketPrice},
};
use smol_str::{SmolStr, format_smolstr};
use thiserror::Error;
use widget::{
    error::TranslatedError,
    venue::{VenueDisplay as _, route_display_string},
};

#[derive(Clone, Debug, Eq, PartialEq)]
struct ListingTerms {
    precision: ListingPrecision,
    quote_asset: AssetId,
    basis: PriceBasis,
}

#[derive(Clone, Debug)]
pub(super) struct AssetLabels {
    generation: DisplaySnapshot,
    labels: Arc<AHashMap<AssetId, SmolStr>>,
}

impl AssetLabels {
    pub(super) fn new(generation: DisplaySnapshot) -> Self {
        Self {
            generation,
            labels: Arc::new(AHashMap::new()),
        }
    }

    pub(super) fn label(&self, asset: &AssetId) -> Option<SmolStr> {
        if let Some(label) = self.labels.get(asset) {
            return Some(label.clone());
        }
        let label = self.name(asset)?;
        let mut labels = (*self.labels).clone();
        labels.insert(asset.clone(), label.clone());
        Some(label)
    }

    fn name(&self, asset: &AssetId) -> Option<SmolStr> {
        self.generation
            .resolver()
            .asset_code(asset)
            .ok()
            .map(|code| SmolStr::new(code.as_str()))
    }
}

pub(super) const MAX_HISTORY: usize = 100;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(super) enum WalletDataError {
    #[error("the market catalog of {route} does not describe the market it was fetched for")]
    RouteMismatch { route: VenueRoute },
    #[error("the market catalog of {route} is missing listing {listing:?}")]
    ListingMissing {
        route: VenueRoute,
        listing: Box<VenueListingKey>,
    },
    #[error("invalid Hyperliquid wallet data while attempting to {operation}")]
    Codec { operation: &'static str },
}

impl TranslatedError for WalletDataError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        let market = VenueId::Hyperliquid.display_string(translate);
        match self {
            Self::RouteMismatch { route } => t_smol!(
                translate,
                "widget.hyperliquid_wallet.error_route_mismatch",
                market = market,
                route = route_display_string(route, translate)
            ),
            Self::ListingMissing { route, .. } => t_smol!(
                translate,
                "widget.hyperliquid_wallet.error_listing_missing",
                market = market,
                route = route_display_string(route, translate)
            ),
            Self::Codec { operation } => t_smol!(
                translate,
                "widget.hyperliquid_wallet.error_codec",
                market = market,
                operation = operation
            ),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct WalletRouteKinds {
    spot: Vec<VenueRoute>,
    perpetual: Vec<VenueRoute>,
}

impl WalletRouteKinds {
    pub(super) fn of(catalogs: &[(VenueRoute, Arc<[CatalogEntry]>)]) -> Self {
        let mut kinds = Self::default();
        for (route, catalog) in catalogs {
            let spot = catalog
                .iter()
                .any(|entry| matches!(entry.instrument.terms(), InstrumentTerms::Spot(_)));
            if spot {
                kinds.spot.push(route.clone());
            } else {
                kinds.perpetual.push(route.clone());
            }
        }
        kinds
    }

    pub(super) fn spot(&self) -> &[VenueRoute] {
        &self.spot
    }

    pub(super) fn perpetual(&self) -> &[VenueRoute] {
        &self.perpetual
    }
}

#[derive(Clone, Debug)]
pub(super) struct DisplayPrecision {
    listings: Arc<AHashMap<VenueListingKey, ListingTerms>>,
    assets: Arc<AHashMap<AssetId, Tick>>,
    listing_assets: Arc<AHashMap<VenueListingKey, AssetId>>,
    quote_assets: Arc<[AssetId]>,
    pairs: Arc<AHashMap<(AssetId, AssetId), VenueListingKey>>,
    listing_kinds: Arc<AHashMap<VenueListingKey, InstrumentKind>>,
    settlement_assets: Arc<AHashMap<VenueListingKey, AssetId>>,
    stable_assets: Arc<AHashSet<AssetId>>,
    labels: AssetLabels,
    amount_tick: Tick,
}

impl PartialEq for DisplayPrecision {
    fn eq(&self, other: &Self) -> bool {
        self.listings == other.listings
            && self.assets == other.assets
            && self.listing_assets == other.listing_assets
            && self.quote_assets == other.quote_assets
            && self.pairs == other.pairs
            && self.listing_kinds == other.listing_kinds
            && self.settlement_assets == other.settlement_assets
            && self.stable_assets == other.stable_assets
            && self.amount_tick == other.amount_tick
    }
}

impl Eq for DisplayPrecision {}

impl DisplayPrecision {
    pub(super) fn from_catalogs(
        asset_labels: AssetLabels,
        catalogs: impl IntoIterator<Item = (VenueRoute, Arc<[CatalogEntry]>)>,
    ) -> Result<Self, WalletDataError> {
        let catalogs = catalogs.into_iter().collect::<Vec<_>>();
        let listing_capacity = catalogs.iter().map(|(_, catalog)| catalog.len()).sum();
        let mut listings: AHashMap<VenueListingKey, ListingTerms> =
            AHashMap::with_capacity(listing_capacity);
        let mut assets: AHashMap<AssetId, Tick> = AHashMap::with_capacity(listing_capacity);
        let mut listing_assets = AHashMap::with_capacity(listing_capacity);
        let mut quote_assets: Vec<AssetId> = Vec::with_capacity(listing_capacity);
        let mut pairs = AHashMap::with_capacity(listing_capacity);
        let mut listing_kinds = AHashMap::with_capacity(listing_capacity);
        let mut settlement_assets = AHashMap::with_capacity(listing_capacity);
        let mut stable_assets = AHashSet::with_capacity(listing_capacity);
        for (route, catalog) in catalogs {
            for entry in catalog.iter() {
                let key = &entry.listing.key;
                if key.route != route {
                    return Err(WalletDataError::RouteMismatch { route });
                }
                let rules = &entry.snapshot.rules;
                let terms = ListingTerms {
                    precision: ListingPrecision::of(rules),
                    quote_asset: rules.common.price.quote_asset.clone(),
                    basis: rules.common.price.basis.clone(),
                };
                let quantity_tick = terms.precision.quantity_tick;
                listings.insert(key.clone(), terms);
                listing_kinds.insert(key.clone(), entry.instrument.kind());
                match entry.instrument.terms() {
                    InstrumentTerms::Spot(terms) => {
                        match assets.get_mut(terms.base()) {
                            Some(existing) => *existing = (*existing).min(quantity_tick),
                            None => {
                                assets.insert(terms.base().clone(), quantity_tick);
                            }
                        }
                        stable_assets.insert(terms.quote().clone());
                        listing_assets.insert(key.clone(), terms.base().clone());
                        settlement_assets.insert(key.clone(), terms.quote().clone());
                        pairs.insert((terms.base().clone(), terms.quote().clone()), key.clone());
                        if !quote_assets.contains(terms.quote()) {
                            quote_assets.push(terms.quote().clone());
                        }
                    }
                    InstrumentTerms::Perpetual(terms) => {
                        stable_assets.insert(terms.settlement_asset().clone());
                        settlement_assets.insert(key.clone(), terms.settlement_asset().clone());
                        if let Some(asset) = terms.underlying().reference().asset() {
                            listing_assets.insert(key.clone(), asset.clone());
                        }
                    }
                    InstrumentTerms::Future(_)
                    | InstrumentTerms::Option(_)
                    | InstrumentTerms::Outcome(_) => {}
                }
            }
        }
        Ok(Self {
            listings: Arc::new(listings),
            assets: Arc::new(assets),
            listing_assets: Arc::new(listing_assets),
            quote_assets: quote_assets.into(),
            pairs: Arc::new(pairs),
            listing_kinds: Arc::new(listing_kinds),
            settlement_assets: Arc::new(settlement_assets),
            stable_assets: Arc::new(stable_assets),
            labels: asset_labels,
            amount_tick: amount_tick()?,
        })
    }

    pub(super) fn money_only(asset_labels: AssetLabels) -> Result<Self, WalletDataError> {
        Ok(Self {
            listings: Arc::new(AHashMap::new()),
            assets: Arc::new(AHashMap::new()),
            listing_assets: Arc::new(AHashMap::new()),
            quote_assets: Arc::from(Vec::new()),
            pairs: Arc::new(AHashMap::new()),
            listing_kinds: Arc::new(AHashMap::new()),
            settlement_assets: Arc::new(AHashMap::new()),
            stable_assets: Arc::new(AHashSet::new()),
            labels: asset_labels,
            amount_tick: amount_tick()?,
        })
    }

    fn listing_asset(&self, listing: &VenueListingKey) -> Option<&AssetId> {
        self.listing_assets.get(listing)
    }

    fn listing_quote_asset(&self, listing: &VenueListingKey) -> Option<&AssetId> {
        Some(&self.listings.get(listing)?.quote_asset)
    }

    fn listing_settlement_asset(&self, listing: &VenueListingKey) -> Option<&AssetId> {
        self.settlement_assets.get(listing)
    }

    fn listing_price_basis(&self, listing: &VenueListingKey) -> Option<&PriceBasis> {
        Some(&self.listings.get(listing)?.basis)
    }

    fn pair_listing(&self, base: &AssetId, quote: &AssetId) -> Option<&VenueListingKey> {
        self.pairs.get(&(base.clone(), quote.clone()))
    }

    fn quote_priority(&self, asset: &AssetId) -> Option<usize> {
        self.quote_assets.iter().position(|quote| quote == asset)
    }

    pub(super) fn asset_label(&self, asset: &AssetId, translate: &Translate) -> SmolStr {
        self.asset_name(asset)
            .unwrap_or_else(|| t_smol!(translate, "error.display.unknown_asset"))
    }

    pub(super) fn asset_name(&self, asset: &AssetId) -> Option<SmolStr> {
        self.labels.label(asset)
    }

    pub(super) fn quantity_unit_label(
        &self,
        unit: &market_participants_model::market::units::QuantityUnit,
        translate: &Translate,
    ) -> Option<SmolStr> {
        use market_participants_model::market::units::QuantityUnit;

        match unit {
            QuantityUnit::AssetUnits { asset } | QuantityUnit::Notional { asset } => {
                Some(self.asset_label(asset, translate))
            }
            QuantityUnit::Contracts => Some(SmolStr::new_static("contracts")),
            QuantityUnit::OutcomeTokens { .. } => None,
        }
    }

    pub(super) fn listing_kind(&self, listing: &VenueListingKey) -> InstrumentKind {
        self.listing_kinds
            .get(listing)
            .copied()
            .unwrap_or(InstrumentKind::Spot)
    }

    pub(super) fn listing_label(
        &self,
        listing: &VenueListingKey,
        translate: &Translate,
    ) -> SmolStr {
        self.listing_name(listing)
            .unwrap_or_else(|| t_smol!(translate, "error.display.unknown_listing"))
    }

    pub(super) fn listing_name(&self, listing: &VenueListingKey) -> Option<SmolStr> {
        self.listing_assets
            .get(listing)
            .and_then(|asset| self.labels.label(asset))
    }

    #[cfg(test)]
    pub(super) fn collateral_name(&self, listing: &VenueListingKey) -> Option<SmolStr> {
        self.listing_settlement_asset(listing)
            .and_then(|asset| self.labels.label(asset))
    }

    pub(super) fn listing_collateral_label(
        &self,
        listing: &VenueListingKey,
        translate: &Translate,
    ) -> SmolStr {
        match self.listing_settlement_asset(listing) {
            Some(asset) => self.asset_label(asset, translate),
            None => SmolStr::new_static("-"),
        }
    }

    pub(super) fn format_asset_quantity(
        &self,
        asset: &AssetId,
        value: SignedQuantity,
    ) -> Result<SmolStr, WalletDataError> {
        let tick = if self.stable_assets.contains(asset) {
            self.amount_tick
        } else {
            self.asset_quantity_tick(asset)?
        };
        Ok(format_grouped_decimal(
            value.format_and_round_tick(tick).as_str(),
        ))
    }

    pub(super) fn format_money_quantity(&self, value: Quantity) -> SmolStr {
        format_grouped_decimal(value.format_and_round_tick(self.amount_tick).as_str())
    }

    pub(super) fn format_money_signed_quantity(&self, value: SignedQuantity) -> SmolStr {
        format_grouped_decimal(value.format_and_round_tick(self.amount_tick).as_str())
    }

    pub(super) fn format_money_profit(&self, value: SignedQuantity) -> SmolStr {
        if value.is_zero() {
            return SmolStr::new_static("0");
        }
        let formatted =
            format_grouped_decimal(value.format_and_round_tick(self.amount_tick).as_str());
        if value.inner().is_positive() {
            format_smolstr!("+{formatted}")
        } else {
            formatted
        }
    }

    pub(super) fn format_profit_or_loss(&self, value: ProfitOrLoss) -> SmolStr {
        self.format_money_profit(SignedQuantity::from_inner(value.inner()))
    }

    pub(super) fn format_listing_price(
        &self,
        listing: &VenueListingKey,
        value: Price,
    ) -> Result<SmolStr, WalletDataError> {
        let tick = self.listing_terms(listing)?.precision.price_tick;
        Ok(format_grouped_decimal(
            value.format_and_round_tick(tick).as_str(),
        ))
    }

    pub(super) fn format_listing_quantity(
        &self,
        listing: &VenueListingKey,
        value: Quantity,
    ) -> Result<SmolStr, WalletDataError> {
        let tick = self.listing_terms(listing)?.precision.quantity_tick;
        Ok(format_grouped_decimal(
            value.format_and_round_tick(tick).as_str(),
        ))
    }

    pub(super) fn format_asset_price(
        &self,
        asset: &AssetId,
        value: Price,
    ) -> Result<SmolStr, WalletDataError> {
        let tick = if self.stable_assets.contains(asset) {
            self.amount_tick
        } else {
            self.asset_quantity_tick(asset)?
        };
        Ok(format_grouped_decimal(
            value.format_and_round_tick(tick).as_str(),
        ))
    }

    pub(super) fn format_listing_signed_quantity(
        &self,
        listing: &VenueListingKey,
        value: SignedQuantity,
    ) -> Result<SmolStr, WalletDataError> {
        let tick = self.listing_terms(listing)?.precision.quantity_tick;
        Ok(format_grouped_decimal(
            value.format_and_round_tick(tick).as_str(),
        ))
    }

    fn asset_quantity_tick(&self, asset: &AssetId) -> Result<Tick, WalletDataError> {
        self.assets
            .get(asset)
            .copied()
            .ok_or(WalletDataError::Codec {
                operation: "display a Hyperliquid asset the catalog does not list",
            })
    }

    fn listing_terms(&self, listing: &VenueListingKey) -> Result<ListingTerms, WalletDataError> {
        self.listings
            .get(listing)
            .cloned()
            .ok_or_else(|| WalletDataError::ListingMissing {
                route: listing.route.clone(),
                listing: Box::new(listing.clone()),
            })
    }

    fn validate_detail(&self, data: &DetailData) -> Result<(), WalletDataError> {
        match data {
            DetailData::Spot(snapshot) => {
                for row in snapshot
                    .rows
                    .iter()
                    .filter(|row| !self.stable_assets.contains(&row.asset))
                {
                    self.asset_quantity_tick(&row.asset)?;
                }
            }
            DetailData::Perpetual(state) => {
                for position in &state.positions {
                    self.listing_terms(&position.listing)?;
                }
            }
            DetailData::OpenOrders(orders) => {
                for order in orders {
                    self.listing_terms(&order.listing)?;
                }
            }
            DetailData::Trades(fills) => {
                for fill in fills {
                    self.listing_terms(&fill.listing)?;
                }
            }
            DetailData::HistoricalOrders(orders) => {
                for order in orders {
                    self.listing_terms(&order.listing)?;
                }
            }
            DetailData::Ledger(_) => {}
        }
        Ok(())
    }
}

fn amount_tick() -> Result<Tick, WalletDataError> {
    Tick::from_inner(10_000_000_000_000_000).map_err(|_| WalletDataError::Codec {
        operation: "index Hyperliquid wallet money precision",
    })
}

fn format_grouped_decimal(value: &str) -> SmolStr {
    let (sign, unsigned) = value
        .strip_prefix('-')
        .map_or(("", value), |unsigned| ("-", unsigned));
    let (integer, fraction) = unsigned
        .split_once('.')
        .map_or((unsigned, None), |(integer, fraction)| {
            (integer, Some(fraction))
        });
    let mut formatted = String::with_capacity(value.len() + integer.len() / 3);
    formatted.push_str(sign);
    for (index, character) in integer.chars().enumerate() {
        if index > 0 && (integer.len() - index) % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(character);
    }
    if let Some(fraction) = fraction {
        formatted.push('.');
        formatted.push_str(fraction);
    }
    SmolStr::new(formatted)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AccountSummary {
    pub spot_value: Option<Quantity>,
    pub perpetual_value: Option<Quantity>,
    pub unrealized_pnl: Option<ProfitOrLoss>,
}

impl AccountSummary {
    pub(super) fn from_states(
        spot: &HyperliquidSpotState,
        perpetual: &HyperliquidPerpetualState,
        pairs: &[HyperliquidSpotPair],
        contexts: &[HyperliquidAssetContext],
        precision: &DisplayPrecision,
    ) -> Self {
        let spot_value = spot_value(spot, pairs, contexts, precision);
        let (perpetual_value, unrealized_pnl) = perpetual_values(perpetual, contexts, precision);
        Self {
            spot_value,
            perpetual_value,
            unrealized_pnl,
        }
    }
}

fn spot_value(
    state: &HyperliquidSpotState,
    pairs: &[HyperliquidSpotPair],
    contexts: &[HyperliquidAssetContext],
    precision: &DisplayPrecision,
) -> Option<Quantity> {
    let mut total = SignedQuantity::ZERO;
    let mut saw_priced_balance = false;
    for balance in &state.balances {
        let Some((price, _)) = spot_asset_price(&balance.asset, pairs, contexts, precision) else {
            continue;
        };
        let value = balance.total.checked_mul_price(price.value)?;
        total = total.checked_add(value)?;
        saw_priced_balance = true;
    }
    if !saw_priced_balance && !state.balances.is_empty() {
        return None;
    }
    (total.inner() >= 0).then(|| Quantity::from_inner(total.inner() as u128))
}

fn signed_size(position: &HyperliquidPerpetualPosition) -> SignedQuantity {
    let magnitude = SignedQuantity::from_inner(
        i128::try_from(position.size.value.inner()).unwrap_or(i128::MAX),
    );
    match position.direction {
        PositionDirection::Long => magnitude,
        PositionDirection::Short => magnitude.checked_neg().unwrap_or(magnitude),
    }
}

fn perpetual_values(
    state: &HyperliquidPerpetualState,
    contexts: &[HyperliquidAssetContext],
    precision: &DisplayPrecision,
) -> (Option<Quantity>, Option<ProfitOrLoss>) {
    if state.positions.is_empty() {
        return (Some(Quantity::ZERO), Some(ProfitOrLoss::ZERO));
    }
    let mut value = Quantity::ZERO;
    let mut pnl = ProfitOrLoss::ZERO;
    let mut saw_value = false;
    let mut saw_pnl = false;
    for position in &state.positions {
        let Some((price, _)) = listing_context_price(contexts, precision, &position.listing) else {
            continue;
        };
        let size = position.size.value;
        let Some(notional) = size.checked_mul_price(price.value) else {
            continue;
        };
        let Some(next_value) = value.checked_add(notional) else {
            continue;
        };
        value = next_value;
        saw_value = true;
        if let Some(entry) = position.entry_price.clone() {
            let signed_pnl = if price.value >= entry.value {
                price
                    .value
                    .checked_sub(entry.value)
                    .and_then(|delta| signed_size(position).checked_mul_price(delta))
            } else {
                entry
                    .value
                    .checked_sub(price.value)
                    .and_then(|delta| signed_size(position).checked_mul_price(delta))
                    .and_then(SignedQuantity::checked_neg)
            };
            if let Some(signed_pnl) = signed_pnl {
                let signed = ProfitOrLoss::from_inner(signed_pnl.inner());
                let Some(next_pnl) = pnl.checked_add(signed) else {
                    continue;
                };
                pnl = next_pnl;
                saw_pnl = true;
            }
        }
    }
    (saw_value.then_some(value), saw_pnl.then_some(pnl))
}

fn listing_context_price(
    contexts: &[HyperliquidAssetContext],
    precision: &DisplayPrecision,
    listing: &VenueListingKey,
) -> Option<(MarketPrice, VenueListingKey)> {
    let asset = precision.listing_asset(listing)?;
    let quoted = match contexts.iter().find(|context| &context.listing == listing) {
        Some(context) => context,
        None => {
            let mut same_asset = contexts.iter().filter(|context| {
                precision
                    .listing_asset(&context.listing)
                    .is_some_and(|context_asset| context_asset == asset)
            });
            let only = same_asset.next()?;
            if same_asset.next().is_some() {
                return None;
            }
            only
        }
    };
    let value = quoted
        .mark_price
        .clone()
        .or_else(|| quoted.mid_price.clone())?;
    let price = MarketPrice {
        value: value.value,
        quote_asset: precision.listing_quote_asset(&quoted.listing)?.clone(),
        basis: precision.listing_price_basis(&quoted.listing)?.clone(),
    };
    Some((price, quoted.listing.clone()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WalletOverviewSnapshot {
    pub summary: AccountSummary,
    pub precision: DisplayPrecision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TimelineSnapshot {
    pub window_start: i64,
    pub window_end: i64,
    pub fills: Arc<[HyperliquidUserFill]>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum DetailTab {
    Spot,
    Perpetual,
    OpenOrders,
    Trades,
    HistoricalOrders,
    Ledger,
}

impl DetailTab {
    pub(super) const ALL: [Self; 6] = [
        Self::Spot,
        Self::Perpetual,
        Self::OpenOrders,
        Self::Trades,
        Self::HistoricalOrders,
        Self::Ledger,
    ];

    pub(super) const fn index(self) -> usize {
        match self {
            Self::Spot => 0,
            Self::Perpetual => 1,
            Self::OpenOrders => 2,
            Self::Trades => 3,
            Self::HistoricalOrders => 4,
            Self::Ledger => 5,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SpotBalanceRow {
    pub asset: AssetId,
    pub total: SignedQuantity,
    pub value: Option<SignedQuantity>,
    pub price: Option<MarketPrice>,
    pub price_listing: Option<VenueListingKey>,
    pub asset_share: Option<SmolStr>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SpotSnapshot {
    pub rows: Arc<[SpotBalanceRow]>,
    pub pairs: Arc<[HyperliquidSpotPair]>,
}

impl SpotSnapshot {
    pub(super) fn new(
        mut state: HyperliquidSpotState,
        pairs: Vec<HyperliquidSpotPair>,
        contexts: Vec<HyperliquidAssetContext>,
        precision: &DisplayPrecision,
    ) -> Result<Self, WalletDataError> {
        state.balances.retain(|balance| {
            precision.assets.contains_key(&balance.asset)
                || precision.stable_assets.contains(&balance.asset)
        });
        let rows = state
            .balances
            .into_iter()
            .map(|balance| SpotBalanceRow {
                asset: balance.asset,
                total: balance.total,
                value: None,
                price: None,
                price_listing: None,
                asset_share: None,
            })
            .collect::<Vec<_>>();
        let mut snapshot = Self {
            rows: rows.into(),
            pairs: pairs.into(),
        };
        snapshot.apply_contexts(&contexts, precision)?;
        Ok(snapshot)
    }

    pub(super) fn apply_contexts(
        &mut self,
        contexts: &[HyperliquidAssetContext],
        precision: &DisplayPrecision,
    ) -> Result<(), WalletDataError> {
        let mut rows = self.rows.to_vec();
        let mut total_value = SignedQuantity::ZERO;
        for row in &mut rows {
            let priced = spot_asset_price(&row.asset, &self.pairs, contexts, precision);
            let value = priced
                .as_ref()
                .map(|(price, _)| {
                    row.total
                        .checked_mul_price(price.value)
                        .ok_or(WalletDataError::Codec {
                            operation: "calculate Hyperliquid spot balance value",
                        })
                })
                .transpose()?;
            if let Some(value) = value {
                total_value = total_value
                    .checked_add(value)
                    .ok_or(WalletDataError::Codec {
                        operation: "calculate Hyperliquid spot total value",
                    })?;
            }
            row.value = value;
            row.price_listing = priced.as_ref().and_then(|(_, listing)| listing.clone());
            row.price = priced.map(|(price, _)| price);
            row.asset_share = None;
        }
        if !total_value.is_zero() {
            for row in &mut rows {
                row.asset_share = row
                    .value
                    .map(|value| checked_percentage(value, total_value))
                    .transpose()?;
            }
        }
        self.rows = rows.into();
        Ok(())
    }
}

fn spot_asset_price(
    asset: &AssetId,
    pairs: &[HyperliquidSpotPair],
    contexts: &[HyperliquidAssetContext],
    precision: &DisplayPrecision,
) -> Option<(MarketPrice, Option<VenueListingKey>)> {
    if precision.stable_assets.contains(asset) {
        return Some((
            MarketPrice {
                value: Price::from(1_u8),
                quote_asset: asset.clone(),
                basis: PriceBasis::PerAssetUnit,
            },
            None,
        ));
    }
    let mut candidates = pairs
        .iter()
        .filter(|pair| &pair.base_token == asset)
        .filter_map(|pair| {
            let priority = precision.quote_priority(&pair.quote_token)?;
            let listing = precision.pair_listing(&pair.base_token, &pair.quote_token)?;
            let price = listing_context_price(contexts, precision, listing)?;
            Some((priority, price))
        })
        .collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|(priority, _)| *priority);
    let first = candidates.first()?;
    let priority = first.0;
    if candidates
        .iter()
        .filter(|(candidate, _)| *candidate == priority)
        .count()
        > 1
    {
        return None;
    }
    Some((first.1.0.clone(), Some(first.1.1.clone())))
}

fn checked_percentage(
    value: SignedQuantity,
    total: SignedQuantity,
) -> Result<SmolStr, WalletDataError> {
    let hundredths = value
        .inner()
        .checked_mul(10_000)
        .and_then(|value| value.checked_div(total.inner()))
        .ok_or(WalletDataError::Codec {
            operation: "calculate Hyperliquid spot asset share",
        })?;
    let sign = if hundredths.is_negative() { "-" } else { "" };
    let magnitude = hundredths.unsigned_abs();
    let percentage = format_smolstr!("{sign}{}.{:02}%", magnitude / 100, magnitude % 100);
    let number = percentage.strip_suffix('%').unwrap_or(percentage.as_str());
    Ok(format_smolstr!("{}%", format_grouped_decimal(number)))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum DetailData {
    Spot(SpotSnapshot),
    Perpetual(HyperliquidPerpetualState),
    OpenOrders(Vec<HyperliquidOpenOrder>),
    Trades(Vec<HyperliquidUserFill>),
    HistoricalOrders(Vec<HyperliquidHistoricalOrder>),
    Ledger(Vec<HyperliquidLedgerEvent>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DetailSnapshot {
    pub data: DetailData,
    pub precision: DisplayPrecision,
}

impl DetailSnapshot {
    pub(super) fn new(
        data: DetailData,
        precision: DisplayPrecision,
    ) -> Result<Self, WalletDataError> {
        precision.validate_detail(&data)?;
        Ok(Self { data, precision })
    }

    pub(super) fn apply_contexts(
        &mut self,
        contexts: &[HyperliquidAssetContext],
    ) -> Result<(), WalletDataError> {
        let precision = &self.precision;
        match &mut self.data {
            DetailData::Spot(snapshot) => snapshot.apply_contexts(contexts, precision),
            DetailData::Perpetual(state) => {
                for position in &mut state.positions {
                    let priced = listing_context_price(contexts, precision, &position.listing);
                    let price = priced.map(|(price, _)| price);
                    position.mark_price = price.clone();
                    let size = position.size.value;
                    position.position_value = price.as_ref().and_then(|price| {
                        size.checked_mul_price(price.value).and_then(|value| {
                            i128::try_from(value.inner()).ok().map(|value| AssetAmount {
                                asset: price.quote_asset.clone(),
                                value: SignedQuantity::from_inner(value),
                            })
                        })
                    });
                    if let (Some(entry), Some(price)) = (position.entry_price.clone(), price) {
                        let pnl = if price.value >= entry.value {
                            price
                                .value
                                .checked_sub(entry.value)
                                .and_then(|delta| signed_size(position).checked_mul_price(delta))
                        } else {
                            entry
                                .value
                                .checked_sub(price.value)
                                .and_then(|delta| signed_size(position).checked_mul_price(delta))
                                .and_then(SignedQuantity::checked_neg)
                        };
                        position.unrealized_pnl = AssetAmount {
                            asset: price.quote_asset,
                            value: pnl.unwrap_or(SignedQuantity::ZERO),
                        };
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

pub(super) fn sort_and_limit_fills(
    mut fills: Vec<HyperliquidUserFill>,
) -> Vec<HyperliquidUserFill> {
    fills.sort_unstable_by(|left, right| {
        right
            .timestamp
            .cmp(&left.timestamp)
            .then_with(|| right.tid.cmp(&left.tid))
    });
    fills.truncate(MAX_HISTORY);
    fills
}

pub(super) fn sort_and_limit_open_orders(
    mut orders: Vec<HyperliquidOpenOrder>,
) -> Vec<HyperliquidOpenOrder> {
    orders.sort_unstable_by(|left, right| {
        right
            .timestamp
            .cmp(&left.timestamp)
            .then_with(|| right.order_id.cmp(&left.order_id))
    });
    orders.truncate(MAX_HISTORY);
    orders
}

pub(super) fn sort_and_limit_historical_orders(
    mut orders: Vec<HyperliquidHistoricalOrder>,
) -> Vec<HyperliquidHistoricalOrder> {
    orders.sort_unstable_by(|left, right| {
        right
            .timestamp
            .cmp(&left.timestamp)
            .then_with(|| right.order_id.cmp(&left.order_id))
    });
    orders.truncate(MAX_HISTORY);
    orders
}

pub(super) fn sort_and_limit_ledger(
    mut events: Vec<HyperliquidLedgerEvent>,
) -> Vec<HyperliquidLedgerEvent> {
    events.sort_unstable_by_key(|event| std::cmp::Reverse(event.timestamp));
    events.truncate(MAX_HISTORY);
    events
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AgeBucket {
    Seconds(u64),
    Minutes(u64),
    Hours(u64),
    Days(u64),
}

pub(super) fn age_bucket(now_millis: i64, timestamp: i64) -> AgeBucket {
    let seconds = now_millis.saturating_sub(timestamp).max(0) / 1_000;
    let seconds = u64::try_from(seconds).unwrap_or(u64::MAX);
    if seconds <= 60 {
        AgeBucket::Seconds(seconds)
    } else if seconds <= 3_600 {
        AgeBucket::Minutes(seconds / 60)
    } else if seconds <= 86_400 {
        AgeBucket::Hours(seconds / 3_600)
    } else {
        AgeBucket::Days(seconds / 86_400)
    }
}

pub(super) fn now_millis() -> Option<i64> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let value = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    i64::try_from(value).ok()
}

pub(super) fn absolute_time(timestamp: i64, zone: time::UtcOffset) -> SmolStr {
    use time::OffsetDateTime;

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
    use std::rc::Rc;

    use i18n::{language::codes::LanguageCode, translate::Translate};
    use market_participants_connector::account::domain::HyperliquidOpenOrder;

    use super::*;
    use crate::test_support::{
        context, money, pair_listing, perp_entry, perp_instrument, perpetual_route,
        perpetual_state, position, precision_for, precision_of, published_perpetual,
        published_spot, spot_entry, spot_pair, spot_route, spot_state, token, wallet_balance,
    };

    fn translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    fn perpetual_listing() -> VenueListingKey {
        published_perpetual("ETH")
    }

    fn perpetual(asset: &str, price_tick: &str, quantity_tick: &str) -> CatalogEntry {
        crate::test_support::entry(
            &perpetual_route(),
            perp_instrument(asset),
            asset,
            price_tick,
            quantity_tick,
        )
    }

    fn spot(base: &str, quote: &str) -> CatalogEntry {
        crate::test_support::spot_entry(&spot_route(), base, quote, "0.001", "0.001")
    }

    #[test]
    fn display_precision_should_apply_each_listings_price_and_quantity_ticks() {
        let eth = perpetual("ETH", "0.1", "0.001");
        let btc = perpetual("BTC", "1", "0.00001");
        let eth_key = eth.listing.key.clone();
        let btc_key = btc.listing.key.clone();
        let precision = precision_of(&perpetual_route(), vec![eth, btc]);

        assert_eq!(
            (
                precision
                    .format_listing_price(&eth_key, "123.16".parse().expect("price should parse"))
                    .expect("ETH precision should exist"),
                precision
                    .format_listing_quantity(
                        &btc_key,
                        "0.123456".parse().expect("quantity should parse")
                    )
                    .expect("BTC precision should exist"),
            ),
            (SmolStr::new_static("123.2"), SmolStr::new_static("0.12346"))
        );
    }

    #[test]
    fn display_precision_should_keep_two_listings_of_one_instrument_apart() {
        let first = perpetual("ETH", "0.1", "0.001");
        let other_route = crate::test_support::route("perp-dex/xyz");
        let second =
            crate::test_support::entry(&other_route, perp_instrument("ETH"), "ETH", "1", "0.1");
        assert_eq!(
            first.instrument.id(),
            second.instrument.id(),
            "the two listings are the same canonical instrument"
        );
        let first_key = first.listing.key.clone();
        let second_key = second.listing.key.clone();
        let precision = crate::test_support::precision_for(vec![
            (perpetual_route(), vec![first]),
            (other_route, vec![second]),
        ]);

        assert_eq!(
            (
                precision
                    .format_listing_price(&first_key, "1.234".parse().expect("price should parse"))
                    .expect("the first listing should have precision"),
                precision
                    .format_listing_price(&second_key, "1.234".parse().expect("price should parse"))
                    .expect("the second listing should have precision"),
            ),
            (SmolStr::new_static("1.2"), SmolStr::new_static("1"))
        );
    }

    #[test]
    fn display_precision_should_round_money_negative_and_zero_to_two_places() {
        let precision = money();

        assert_eq!(
            (
                precision.format_profit_or_loss(
                    "1.235"
                        .parse::<ProfitOrLoss>()
                        .expect("pnl should be valid"),
                ),
                precision.format_profit_or_loss(
                    "-1.235"
                        .parse::<ProfitOrLoss>()
                        .expect("pnl should be valid"),
                ),
                precision.format_profit_or_loss(ProfitOrLoss::ZERO),
            ),
            (
                SmolStr::new_static("+1.24"),
                SmolStr::new_static("-1.24"),
                SmolStr::new_static("0")
            )
        );
    }

    #[test]
    fn display_precision_should_group_integer_digits_after_rounding() {
        let precision = money();

        assert_eq!(
            (
                precision.format_money_quantity(
                    "1234567.895"
                        .parse::<Quantity>()
                        .expect("quantity should be valid")
                ),
                precision.format_profit_or_loss(
                    "-1234567.895"
                        .parse::<ProfitOrLoss>()
                        .expect("pnl should be valid")
                ),
                format_grouped_decimal("-123456789.012300"),
            ),
            (
                SmolStr::new_static("1,234,567.90"),
                SmolStr::new_static("-1,234,567.90"),
                SmolStr::new_static("-123,456,789.012300"),
            )
        );
    }

    #[test]
    fn display_precision_should_reject_a_listing_the_catalog_does_not_hold() {
        let precision = precision_of(&perpetual_route(), vec![perpetual("ETH", "0.1", "0.001")]);
        let elsewhere =
            crate::test_support::listing_key(&crate::test_support::route("another"), "ETH");

        let error = precision
            .format_listing_price(&elsewhere, Price::from(1_u8))
            .expect_err("a listing no catalog published should fail");

        assert!(matches!(error, WalletDataError::ListingMissing { .. }));
    }

    #[test]
    fn wallet_route_kinds_should_group_a_route_by_what_it_publishes() {
        let core_spot = spot_route();
        let core_perp = perpetual_route();
        let deployed = crate::test_support::route("perp-dex/xyz");
        let quiet = crate::test_support::route("perp-dex/quiet");

        let catalogs = vec![
            (
                core_spot.clone(),
                Arc::from([spot_entry(&core_spot, "HYPE", "USDC", "0.001", "0.001")]),
            ),
            (
                core_perp.clone(),
                Arc::from([perp_entry(&core_perp, "ETH", "0.1", "0.001")]),
            ),
            (
                deployed.clone(),
                Arc::from([perp_entry(&deployed, "SOL", "0.01", "0.001")]),
            ),
            (quiet.clone(), Arc::<[CatalogEntry]>::from([])),
        ];

        let kinds = WalletRouteKinds::of(&catalogs);

        assert_eq!(kinds.spot(), vec![core_spot]);
        assert_eq!(kinds.perpetual(), vec![core_perp, deployed, quiet]);
    }

    #[test]
    fn display_precision_should_index_every_wallet_route_catalog_at_once() {
        let core_spot = spot_route();
        let core_perp = perpetual_route();
        let deployed = crate::test_support::route("perp-dex/xyz");
        let routes = vec![
            (core_spot, vec![spot("HYPE", "USDC"), spot("SOL", "USDC")]),
            (
                core_perp,
                vec![
                    perpetual("ETH", "0.1", "0.001"),
                    perpetual("BTC", "1", "0.00001"),
                ],
            ),
            (
                deployed.clone(),
                vec![
                    perp_entry(&deployed, "ETH", "1", "0.1"),
                    perp_entry(&deployed, "SOL", "0.01", "0.001"),
                ],
            ),
        ];
        let precision = crate::test_support::precision_for(routes);
        assert_eq!(
            (
                precision
                    .format_listing_price(
                        &published_spot("HYPE", "USDC"),
                        "1.2345".parse().expect("a price should parse")
                    )
                    .expect("the spot listing should carry its own tick"),
                precision
                    .format_listing_quantity(
                        &published_perpetual("ETH"),
                        "0.12345".parse().expect("a quantity should parse")
                    )
                    .expect("the core perpetual listing should carry its own tick"),
                precision
                    .format_listing_price(
                        &crate::test_support::listing_key(&deployed, "ETH"),
                        "1.2345".parse().expect("a price should parse")
                    )
                    .expect("the deployed perpetual listing should carry its own tick"),
            ),
            (
                SmolStr::new_static("1.235"),
                SmolStr::new_static("0.123"),
                SmolStr::new_static("1"),
            )
        );
    }

    #[test]
    fn display_precision_should_reject_a_catalog_for_another_route() {
        let route = perpetual_route();

        assert_eq!(
            crate::test_support::precision_failure(vec![(
                route.clone(),
                vec![spot("HYPE", "USDC")]
            )]),
            WalletDataError::RouteMismatch { route }
        );
    }

    #[test]
    fn an_asset_should_be_named_by_its_own_terms() {
        let precision = precision_of(&spot_route(), vec![spot("HYPE", "USDC")]);
        let hype = token("HYPE");
        let unregistered = market_participants_model::asset::AssetId::CryptoToken(
            market_participants_model::asset::CryptoTokenId::new(
                market_participants_model::asset::CryptoTokenOrigin::VenueIssued {
                    issuer: market_participants_model::identifier::TokenIssuerId::new(
                        "somewhere-else",
                    )
                    .expect("the test issuer should be canonical"),
                    namespace: market_participants_model::identifier::IdentifierNamespace::new(
                        "somewhere-else",
                    )
                    .expect("the test namespace should be canonical"),
                    token: market_participants_model::asset::TokenReference::Native {
                        code: market_participants_model::identifier::IdentifierValue::new("ZZZ")
                            .expect("the test code should be canonical"),
                    },
                },
            )
            .expect("the test token should be valid"),
        );

        assert_eq!(
            precision.asset_name(&hype),
            Some(SmolStr::new_static("HYPE"))
        );
        assert_eq!(
            precision.asset_name(&unregistered),
            None,
            "an asset the published generation does not name has no name at all"
        );
        assert_eq!(
            precision.listing_name(&pair_listing("HYPE", "USDC")),
            Some(SmolStr::new_static("HYPE"))
        );
        assert_eq!(
            precision.collateral_name(&pair_listing("HYPE", "USDC")),
            Some(SmolStr::new_static("USDC")),
            "a spot order is paid for in the listing's quote asset"
        );
    }

    #[test]
    fn a_perpetual_should_be_named_by_its_underlying() {
        let precision = precision_of(&perpetual_route(), vec![perpetual("ETH", "0.1", "0.001")]);
        let listing = perpetual_listing();

        assert_eq!(
            precision.listing_name(&listing),
            Some(SmolStr::new_static("ETH"))
        );
        assert_eq!(
            precision.collateral_name(&listing),
            Some(SmolStr::new_static("USDC"))
        );
        assert_eq!(precision.listing_kind(&listing), InstrumentKind::Perpetual);
        assert_eq!(
            precision.listing_kind(&pair_listing("HYPE", "USDC")),
            InstrumentKind::Spot,
            "a listing no catalog described is not a perpetual"
        );
    }

    #[test]
    fn a_stablecoin_balance_should_be_displayed_as_money() {
        let precision = precision_of(&spot_route(), vec![spot("HYPE", "USDC")]);

        assert_eq!(
            precision
                .format_asset_quantity(
                    &token("USDC"),
                    "1234567.891234567".parse().expect("quantity should parse"),
                )
                .expect("stablecoin precision should exist"),
            SmolStr::new_static("1,234,567.89")
        );
    }

    #[test]
    fn spot_snapshot_should_calculate_signed_values_and_shares() {
        let precision = precision_of(&spot_route(), vec![spot("HYPE", "USDC")]);
        let pair = spot_pair("HYPE", "USDC");
        let snapshot = SpotSnapshot::new(
            spot_state(vec![
                wallet_balance("USDC", "10"),
                wallet_balance("HYPE", "-2"),
            ]),
            vec![pair],
            vec![context(&pair_listing("HYPE", "USDC"), Some("2"), None)],
            &precision,
        )
        .expect("snapshot should calculate");

        assert_eq!(
            snapshot
                .rows
                .iter()
                .map(|row| (
                    row.value.map(|value| value.to_string()),
                    row.asset_share.clone(),
                ))
                .collect::<Vec<_>>(),
            vec![
                (Some("10".to_owned()), Some(SmolStr::new_static("166.66%"))),
                (Some("-4".to_owned()), Some(SmolStr::new_static("-66.66%")))
            ]
        );
    }

    #[test]
    fn spot_asset_share_should_use_grouped_percentage() {
        let precision = precision_of(&spot_route(), vec![spot("HYPE", "USDC")]);
        let pair = spot_pair("HYPE", "USDC");
        let snapshot = SpotSnapshot::new(
            spot_state(vec![
                wallet_balance("USDC", "1234567"),
                wallet_balance("HYPE", "-1234566"),
            ]),
            vec![pair],
            vec![context(&pair_listing("HYPE", "USDC"), Some("1"), None)],
            &precision,
        )
        .expect("snapshot should calculate");

        assert_eq!(
            snapshot.rows[0].asset_share.as_deref(),
            Some("123,456,700.00%")
        );
    }

    #[test]
    fn spot_valuation_should_skip_unpriced_rows_and_fallback_quote_priority() {
        let precision = precision_of(
            &spot_route(),
            vec![spot("HYPE", "USDC"), spot("HYPE", "USDT")],
        );
        let state = spot_state(vec![
            wallet_balance("HYPE", "2"),
            wallet_balance("SOL", "4"),
        ]);
        let pairs = vec![spot_pair("HYPE", "USDC"), spot_pair("HYPE", "USDT")];
        let contexts = vec![
            context(&pair_listing("HYPE", "USDC"), Some("2"), None),
            context(&pair_listing("HYPE", "USDT"), Some("3"), None),
        ];

        assert_eq!(
            spot_value(&state, &pairs, &contexts, &precision)
                .expect("a priced balance should value")
                .to_string(),
            "4",
            "the higher-ranked quote values the base, and the asset no pair prices is skipped"
        );
    }

    #[test]
    fn spot_valuation_should_fall_back_to_the_pair_that_has_a_price() {
        let precision = precision_of(
            &spot_route(),
            vec![spot("HYPE", "USDC"), spot("HYPE", "USDT")],
        );
        let state = spot_state(vec![wallet_balance("HYPE", "2")]);
        let pairs = vec![spot_pair("HYPE", "USDC"), spot_pair("HYPE", "USDT")];
        let contexts = vec![context(&pair_listing("HYPE", "USDT"), Some("3"), None)];

        assert_eq!(
            spot_value(&state, &pairs, &contexts, &precision)
                .expect("a priced balance should value")
                .to_string(),
            "6"
        );
    }

    #[test]
    fn spot_valuation_should_mark_two_pairs_of_one_quote_ambiguous() {
        let precision = precision_of(&spot_route(), vec![spot("HYPE", "USDC")]);
        let pairs = vec![spot_pair("HYPE", "USDC"), spot_pair("HYPE", "USDC")];
        let listing = pair_listing("HYPE", "USDC");
        let contexts = vec![
            context(&listing, Some("2"), None),
            context(&listing, Some("3"), None),
        ];

        assert!(spot_asset_price(&token("HYPE"), &pairs, &contexts, &precision).is_none());
    }

    #[test]
    fn a_spot_balance_the_catalog_does_not_describe_should_not_fail_the_tab() {
        let precision = precision_of(&spot_route(), vec![spot("HYPE", "USDC")]);
        let state = spot_state(vec![
            wallet_balance("HYPE", "2"),
            wallet_balance("+25970", "100"),
        ]);
        let snapshot = SpotSnapshot::new(
            state,
            vec![spot_pair("HYPE", "USDC")],
            Vec::new(),
            &precision,
        )
        .expect("an undescribed balance is left out, not a failure");

        assert_eq!(
            snapshot.rows.len(),
            1,
            "only the described holding is drawn"
        );
        assert_eq!(snapshot.rows[0].asset, token("HYPE"));
        DetailSnapshot::new(DetailData::Spot(snapshot), precision)
            .expect("the reading validates once its undescribed balances are left out");
    }

    #[test]
    fn a_balance_price_should_carry_the_pair_it_was_read_on() {
        let precision = precision_of(
            &spot_route(),
            vec![spot("HYPE", "USDC"), spot("HYPE", "USDH")],
        );
        let usdc = pair_listing("HYPE", "USDC");
        let usdh = pair_listing("HYPE", "USDH");
        let snapshot = SpotSnapshot::new(
            spot_state(vec![wallet_balance("HYPE", "2")]),
            vec![spot_pair("HYPE", "USDC"), spot_pair("HYPE", "USDH")],
            vec![context(&usdh, Some("3"), None)],
            &precision,
        )
        .expect("a balance of a two-quote asset should value");

        assert_eq!(snapshot.rows[0].price_listing.as_ref(), Some(&usdh));
        assert_ne!(snapshot.rows[0].price_listing.as_ref(), Some(&usdc));
        assert_eq!(
            snapshot.rows[0]
                .price
                .as_ref()
                .map(|price| price.quote_asset.clone()),
            Some(token("USDH")),
            "the price says which asset it is quoted in"
        );
    }

    #[test]
    fn a_two_quote_asset_should_be_priced_at_the_pair_that_quotes_it() {
        let precision = precision_of(
            &spot_route(),
            vec![
                spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.01"),
                spot_entry(&spot_route(), "HYPE", "USDH", "0.01", "0.01"),
            ],
        );
        let usdh = pair_listing("HYPE", "USDH");
        let snapshot = SpotSnapshot::new(
            spot_state(vec![wallet_balance("HYPE", "2")]),
            vec![spot_pair("HYPE", "USDC"), spot_pair("HYPE", "USDH")],
            vec![context(&usdh, Some("1.0004"), None)],
            &precision,
        )
        .expect("a balance of a two-quote asset should value");

        assert_eq!(snapshot.rows[0].price_listing.as_ref(), Some(&usdh));
        assert_eq!(
            snapshot.rows[0]
                .price_listing
                .as_ref()
                .map(|listing| precision
                    .format_listing_price(listing, "1.0004".parse().expect("a price should parse"))
                    .expect("the pair that priced the balance should format its price"))
                .as_deref(),
            Some("1.00"),
            "the tick of the pair that priced the balance is the one its price is drawn at, \
             where the other pair's tick would draw 1.000"
        );
    }

    #[test]
    fn a_balance_priced_from_another_market_of_its_asset_should_keep_that_market() {
        let precision = precision_for(vec![
            (
                spot_route(),
                vec![spot_entry(&spot_route(), "HYPE", "USDT", "0.001", "0.01")],
            ),
            (
                perpetual_route(),
                vec![perp_entry(&perpetual_route(), "HYPE", "0.1", "0.001")],
            ),
        ]);
        let perpetual_listing = published_perpetual("HYPE");
        let snapshot = SpotSnapshot::new(
            spot_state(vec![wallet_balance("HYPE", "2")]),
            vec![spot_pair("HYPE", "USDT")],
            vec![context(&perpetual_listing, Some("1.5"), None)],
            &precision,
        )
        .expect("a balance of an asset another market priced should value");

        assert_eq!(
            snapshot.rows[0].price_listing.as_ref(),
            Some(&perpetual_listing)
        );
        assert_ne!(
            snapshot.rows[0].price_listing.as_ref(),
            Some(&pair_listing("HYPE", "USDT"))
        );
        assert_eq!(
            snapshot.rows[0]
                .price
                .as_ref()
                .map(|price| price.value.to_string()),
            Some("1.5".to_owned())
        );
    }

    #[test]
    fn spot_valuation_should_prefer_mark_and_fallback_to_mid_price() {
        let precision = precision_of(&spot_route(), vec![spot("HYPE", "USDC")]);
        let pairs = vec![spot_pair("HYPE", "USDC")];
        let listing = pair_listing("HYPE", "USDC");

        assert_eq!(
            spot_asset_price(
                &token("HYPE"),
                &pairs,
                &[context(&listing, Some("3"), Some("2"))],
                &precision
            )
            .expect("a mark price should win")
            .0
            .value
            .to_string(),
            "3"
        );
        assert_eq!(
            spot_asset_price(
                &token("HYPE"),
                &pairs,
                &[context(&listing, None, Some("2"))],
                &precision
            )
            .expect("a missing mark price should fall back to the mid")
            .0
            .value
            .to_string(),
            "2"
        );
    }

    #[test]
    fn spot_valuation_should_report_nothing_when_no_balance_could_be_priced() {
        let precision = precision_of(&spot_route(), vec![spot("HYPE", "USDC")]);
        let state = spot_state(vec![wallet_balance("SOL", "4")]);

        assert!(spot_value(&state, &[], &[], &precision).is_none());
        assert_eq!(
            spot_value(&spot_state(Vec::new()), &[], &[], &precision)
                .expect("an empty wallet is worth zero")
                .to_string(),
            "0"
        );
    }

    #[test]
    fn perpetual_valuation_should_use_live_notional_and_signed_pnl() {
        let listings = vec![
            perpetual("BTC", "0.1", "0.001"),
            perpetual("ETH", "0.1", "0.001"),
        ];
        let btc = listings[0].listing.key.clone();
        let eth = listings[1].listing.key.clone();
        let precision = precision_of(&perpetual_route(), listings);
        let state = perpetual_state(vec![
            position(&btc, "2", Some("100")),
            position(&eth, "1", Some("90")),
        ]);
        let contexts = vec![
            context(&btc, Some("110"), None),
            context(&eth, Some("90"), None),
        ];
        let (value, pnl) = perpetual_values(&state, &contexts, &precision);
        assert_eq!(value.unwrap().to_string(), "310");
        assert_eq!(pnl.unwrap().to_string(), "20");
    }

    #[test]
    fn perpetual_valuation_should_read_a_short_the_other_way_round() {
        let listings = vec![perpetual("BTC", "0.1", "0.001")];
        let btc = listings[0].listing.key.clone();
        let precision = precision_of(&perpetual_route(), listings);
        let state = perpetual_state(vec![position(&btc, "-2", Some("100"))]);
        let contexts = vec![context(&btc, Some("90"), None)];

        let (value, pnl) = perpetual_values(&state, &contexts, &precision);

        assert_eq!(
            value.unwrap().to_string(),
            "180",
            "what the positions hold is the same magnitude whichever side they are held from"
        );
        assert_eq!(
            pnl.unwrap().to_string(),
            "20",
            "a short gains what the price fell by, not what it rose by"
        );
    }

    #[test]
    fn perpetual_valuation_should_exclude_unpriced_or_entryless_components() {
        let listings = vec![
            perpetual("BTC", "0.1", "0.001"),
            perpetual("ETH", "0.1", "0.001"),
        ];
        let btc = listings[0].listing.key.clone();
        let eth = listings[1].listing.key.clone();
        let precision = precision_of(&perpetual_route(), listings);
        let state = perpetual_state(vec![
            position(&btc, "1", None),
            position(&eth, "1", Some("100")),
        ]);
        let contexts = vec![context(&btc, Some("110"), None)];

        let (value, pnl) = perpetual_values(&state, &contexts, &precision);
        assert_eq!(value.unwrap().to_string(), "110");
        assert!(pnl.is_none());
    }

    #[test]
    fn perpetual_valuation_should_report_a_flat_wallet_as_zero() {
        let state = perpetual_state(Vec::new());

        let (value, pnl) = perpetual_values(&state, &[], &money());
        assert_eq!(value.expect("a flat wallet values").to_string(), "0");
        assert_eq!(pnl.expect("a flat wallet has no pnl").to_string(), "0");
    }

    #[test]
    fn detail_snapshot_should_refuse_an_order_the_catalog_cannot_display() {
        let precision = precision_of(&perpetual_route(), vec![perpetual("ETH", "0.1", "0.001")]);
        let order = HyperliquidOpenOrder {
            listing: crate::test_support::listing_key(&perpetual_route(), "BTC"),
            side: market_participants_model::market::units::Side::Buy,
            quantity: market_participants_model::market::units::MarketQuantity::traded(
                "1".parse().expect("quantity should parse"),
                market_participants_model::market::units::QuantityUnit::Contracts,
            )
            .expect("an order quantity is a traded quantity"),
            price: market_participants_model::market::price::MarketPrice {
                value: "100".parse().expect("price should parse"),
                quote_asset: token("USDC"),
                basis: market_participants_model::listing::venue::PriceBasis::PerAssetUnit,
            },
            status: market_participants_connector::account::shared::OrderStatus::Open,
            kind: market_participants_connector::account::shared::OrderKind::Limit {
                price: "100".parse().expect("price should parse"),
                time_in_force:
                    market_participants_connector::account::shared::TimeInForce::GoodTillCancelled,
            },
            reduce_only: false,
            timestamp: 1,
            order_id: SmolStr::new_static("1"),
        };

        let error = DetailSnapshot::new(DetailData::OpenOrders(vec![order]), precision)
            .expect_err("a listing the catalog does not publish should be refused");

        assert!(matches!(error, WalletDataError::ListingMissing { .. }));
    }

    #[test]
    fn detail_snapshot_should_revalue_a_perpetual_position_at_the_latest_context() {
        let listings = vec![perpetual("ETH", "0.1", "0.001")];
        let eth = listings[0].listing.key.clone();
        let mut snapshot = DetailSnapshot::new(
            DetailData::Perpetual(perpetual_state(vec![position(&eth, "2", Some("100"))])),
            precision_of(&perpetual_route(), listings),
        )
        .expect("a published listing should validate");

        snapshot
            .apply_contexts(&[context(&eth, Some("110"), None)])
            .expect("a context should apply");

        let DetailData::Perpetual(state) = &snapshot.data else {
            panic!("the snapshot should stay perpetual");
        };
        assert_eq!(
            (
                state.positions[0]
                    .position_value
                    .as_ref()
                    .expect("a priced position should have a value")
                    .value
                    .to_string(),
                state.positions[0].unrealized_pnl.value.to_string(),
            ),
            ("220".to_owned(), "20".to_owned())
        );
    }

    #[test]
    fn detail_snapshot_should_not_revalue_history() {
        let mut snapshot = DetailSnapshot::new(DetailData::Ledger(Vec::new()), money())
            .expect("a ledger validates");

        snapshot
            .apply_contexts(&[context(
                &crate::test_support::listing_key(&perpetual_route(), "ETH"),
                Some("110"),
                None,
            )])
            .expect("a context against history should be a no-op");
    }

    #[test]
    fn age_buckets_should_use_the_requested_boundaries() {
        assert_eq!(age_bucket(61_000, 1_000), AgeBucket::Seconds(60));
        assert_eq!(age_bucket(3_661_000, 1_000), AgeBucket::Hours(1));
        assert_eq!(age_bucket(3_600_000 + 1_000, 1_000), AgeBucket::Minutes(60));
        assert_eq!(age_bucket(86_400_000 + 1_000, 1_000), AgeBucket::Hours(24));
        assert_eq!(age_bucket(86_401_000 + 1_000, 1_000), AgeBucket::Days(1));
        assert_eq!(age_bucket(1_000, 2_000), AgeBucket::Seconds(0));
    }

    #[test]
    fn absolute_time_should_be_read_in_the_display_zone_to_the_second() {
        assert_eq!(
            absolute_time(1_700_000_000_000, time::UtcOffset::UTC).as_str(),
            "2023-11-14 22:13:20"
        );
        assert_eq!(
            absolute_time(
                1_700_000_000_000,
                time::UtcOffset::from_hms(8, 0, 0).expect("UTC+8 is a valid offset")
            )
            .as_str(),
            "2023-11-15 06:13:20"
        );
        assert_eq!(absolute_time(i64::MIN, time::UtcOffset::UTC).as_str(), "--");
    }

    #[test]
    fn a_time_the_display_zone_cannot_hold_should_read_as_a_placeholder() {
        let boundary = time::Date::MAX
            .with_hms(23, 0, 0)
            .expect("the last day has an evening")
            .assume_utc();
        let timestamp = i64::try_from(boundary.unix_timestamp_nanos() / 1_000_000)
            .expect("the last day is a millisecond timestamp");

        assert_eq!(
            absolute_time(
                timestamp,
                time::UtcOffset::from_hms(14, 0, 0).expect("UTC+14 is a valid offset")
            )
            .as_str(),
            "--"
        );

        let boundary = time::Date::MIN
            .with_hms(0, 0, 0)
            .expect("the first day has a midnight")
            .assume_utc();
        let timestamp = i64::try_from(boundary.unix_timestamp_nanos() / 1_000_000)
            .expect("the first day is a millisecond timestamp");

        assert_eq!(
            absolute_time(
                timestamp,
                time::UtcOffset::from_hms(-12, 0, 0).expect("UTC-12 is a valid offset")
            )
            .as_str(),
            "--",
            "a western offset runs off the other end of the calendar"
        );
    }

    #[test]
    fn every_tab_should_sit_at_its_own_index() {
        for (index, tab) in DetailTab::ALL.into_iter().enumerate() {
            assert_eq!(tab.index(), index);
        }
    }

    #[test]
    fn an_unpublished_asset_should_be_named_as_unknown() {
        let translate = translate(LanguageCode::En);

        let precision = money();
        let unpublished = token("NOWHERE");
        let label = precision.asset_label(&unpublished, &translate);
        let identity = unpublished.to_string();

        assert_eq!(
            label.as_str(),
            t_smol!(&translate, "error.display.unknown_asset").as_str()
        );
        assert_ne!(
            label.as_str(),
            identity.as_str(),
            "a canonical identity is not a name"
        );
        assert!(
            !label.contains("crypto-token") && !label.contains("v1/"),
            "the label is a name, not an identity: {label}"
        );
    }

    #[test]
    fn a_listing_no_generation_names_should_be_the_localized_unknown() {
        let translate = translate(LanguageCode::En);

        let precision = money();
        let listing = crate::test_support::published_spot("BTC", "USDT");
        let label = precision.listing_label(&listing, &translate);

        assert_eq!(
            label.as_str(),
            t_smol!(&translate, "error.display.unknown_listing").as_str()
        );
        assert!(
            !label.contains("listing-id/v1/"),
            "an identity is not a name: {label}"
        );
    }
}

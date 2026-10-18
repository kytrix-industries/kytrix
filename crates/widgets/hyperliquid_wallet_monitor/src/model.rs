//! What a Hyperliquid wallet monitor holds and what it means.
//!
//! # Prices are only as precise as the catalog says
//!
//! Hyperliquid quotes every instrument on its own tick, and a wallet's rows are read against the
//! catalog of the market they belong to rather than the instrument they name: a spot pair is
//! displayed with its own price tick and quote asset, a spot balance's quantity with its base
//! asset's own tick, and a balance in a stablecoin is money rather than a quantity.
//! [`DisplayPrecision`] is that lookup, built once from the catalogs the venue answered with, so
//! a snapshot carries the precision it was valued at instead of the view re-deriving it per
//! frame.
//!
//! # A valuation that cannot be made is left out, not guessed
//!
//! Spot value, perpetual value and unrealized profit are each a sum over the rows that could
//! be priced. A row whose asset is absent from the catalog, or an ambiguous spot pair, is
//! skipped rather than valued at zero — and when nothing could be valued at all the summary
//! says so with `None` instead of reporting an empty wallet.
//!
//! # Every view of an account is one snapshot
//!
//! The six detail tabs are six shapes of the same account, and they are all read the same way:
//! fetch, validate against the catalog, then keep revaluing from the venue's fast asset
//! contexts. [`DetailSnapshot`] is one such reading, and [`DetailData`] is which of the six it
//! is.

use std::sync::Arc;

use ahash::{AHashMap, AHashSet};
use gpui_kit::{App, SharedString};
use i18n::t;
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
    error::LocalizedError,
    venue::{VenueDisplay as _, route_display_string},
};

/// What one listing publishes its numbers in: the increments they are written at,
/// and the terms its prices are read in.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ListingTerms {
    precision: ListingPrecision,
    quote_asset: AssetId,
    basis: PriceBasis,
}

/// The asset names of one catalog generation.
///
/// A name is the display code the published display catalog decided on for the
/// asset, read from the generation the whole reading is built against. Nothing here
/// restates a naming rule: a venue-issued token, a chain token, a security and a
/// commodity are all named by the code that generation published, and an asset it
/// does not name has no name at all — the reader shows the localized unknown rather
/// than a digest.
#[derive(Clone, Debug)]
pub(super) struct AssetLabels {
    generation: DisplaySnapshot,
    /// The resolved names, so a frame that draws a hundred rows names an asset once.
    labels: Arc<AHashMap<AssetId, SmolStr>>,
}

impl AssetLabels {
    /// Reads names out of one captured catalog generation.
    ///
    /// The capture is the generation the precision of every value is read from too,
    /// so a name and the digits beside it always come from one publication.
    pub(super) fn new(generation: DisplaySnapshot) -> Self {
        Self {
            generation,
            labels: Arc::new(AHashMap::new()),
        }
    }

    /// The published display code of one asset, when the generation names it.
    pub(super) fn label(&self, asset: &AssetId) -> Option<SmolStr> {
        if let Some(label) = self.labels.get(asset) {
            return Some(label.clone());
        }
        let label = self.name(asset)?;
        let mut labels = (*self.labels).clone();
        labels.insert(asset.clone(), label.clone());
        Some(label)
    }

    /// The name one generation gives an asset.
    ///
    /// The catalog names every asset whose own terms state a code, or
    /// whose code a route staged — not only the ones a surviving listing references —
    /// so the catalog is the one answer to what an asset is called here: a balance, a
    /// fee or a collateral asset read from a wallet has its code there whether or not
    /// a listing of this generation trades it. An asset the catalog publishes no code
    /// for — a contract token or a security no route staged, which nothing references
    /// — has no name of its own, and the row is drawn as the localized unknown rather
    /// than under a code this module would derive on its own: a second naming rule
    /// would be free to disagree with the publication about the same asset.
    fn name(&self, asset: &AssetId) -> Option<SmolStr> {
        self.generation
            .resolver()
            .asset_code(asset)
            .ok()
            .map(|code| SmolStr::new(code.as_str()))
    }
}

/// How many rows one detail tab keeps.
///
/// The venue answers with more than this, and the oldest beyond it are dropped: a wallet's
/// history is read from the top, and a tab is a view of what just happened.
pub(super) const MAX_HISTORY: usize = 100;

/// What a reading could not be turned into.
///
/// These are failures of the reading itself rather than of the venue: a catalog that does not
/// describe the market it was fetched for, a listing it does not carry, or an amount outside
/// the fixed-point range. They reach the user, so they are typed and localized, and none of
/// them states an identity: the refusal names what is missing, while the listing a read was
/// about stays a typed field of the error.
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

impl LocalizedError for WalletDataError {
    fn localized_message(&self, cx: &App) -> SharedString {
        // The venue this widget reads is named by the catalogue rather than spelled
        // into each message, so a sentence about it follows a rename of it.
        let market = VenueId::Hyperliquid.display_string(cx);
        match self {
            Self::RouteMismatch { route } => t!(
                cx,
                "widget.hyperliquid_wallet_monitor.error_route_mismatch",
                market = market,
                route = route_display_string(route, cx)
            ),
            // The listing a reading could not be valued for is an identity this
            // message does not print: the row it belongs to was already drawn from
            // the reading, so the failure states what the catalog lacks rather than
            // naming a listing by a key that has no published name here.
            Self::ListingMissing { route, .. } => t!(
                cx,
                "widget.hyperliquid_wallet_monitor.error_listing_missing",
                market = market,
                route = route_display_string(route, cx)
            ),
            Self::Codec { operation } => t!(
                cx,
                "widget.hyperliquid_wallet_monitor.error_codec",
                market = market,
                operation = operation
            ),
        }
    }
}

/// Which of a venue's partitions one address's state is read from, and how.
///
/// # Why the split follows the catalogs rather than the route names
///
/// A venue's partitions are not the same kind of thing. `clearinghouseState` and
/// `frontendOpenOrders` carry a `dex` parameter, so every perpetual partition is read once, while
/// spot pairs are published by the spot partition alone. Which partition a route is, is answered
/// by *what its catalog publishes* and never by what the route is called: which routes exist is
/// the venue's answer, and this widget has no business knowing that one of them is named
/// `core-spot`.
///
/// # Why "everything else is perpetual" is the conservative reading
///
/// A deployed DEX whose catalog is empty lands in `perpetual` and is therefore still read once:
/// if that partition is broken, the address fails, which is the documented behaviour, instead of
/// being quietly skipped. Grouping by "published a perpetual listing" would put an empty route in
/// neither group and silently drop a partition from the reading — a much worse way to fail.
#[derive(Clone, Debug, Default)]
pub(super) struct WalletRouteKinds {
    /// The routes that published at least one spot listing.
    spot: Vec<VenueRoute>,
    /// Every other route, each read once for positions and once for open orders.
    perpetual: Vec<VenueRoute>,
}

impl WalletRouteKinds {
    /// Groups the routes by what their catalogs publish, keeping the given order.
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

    /// The routes that published spot listings, whose pairs a balance is valued through.
    pub(super) fn spot(&self) -> &[VenueRoute] {
        &self.spot
    }

    /// The routes read once each for positions and once each for open orders.
    pub(super) fn perpetual(&self) -> &[VenueRoute] {
        &self.perpetual
    }
}

/// How the listings of a venue's markets are displayed.
///
/// It is built once per reading and then shared by everything that shows a number from it, so
/// two rows valued at the same moment agree about how many digits they have and about what the
/// assets in them are called.
///
/// # Why the index is by listing and by asset
///
/// A listing's ticks belong to the listing: one canonical instrument can be listed by several
/// routes, and a position of one of them is drawn with the rules of the listing it was held on,
/// never with a sibling's. A spot balance names no listing — it is a quantity of one asset — so
/// it is drawn with that asset's own quantity tick, which is why the two indexes are separate
/// and neither falls back to a canonical instrument. A balance's *price*, on the other hand, is
/// read on one pair and is drawn from that pair: two pairs quoting one base against two assets
/// are two markets, not one asset with two precisions.
#[derive(Clone, Debug)]
pub(super) struct DisplayPrecision {
    listings: Arc<AHashMap<VenueListingKey, ListingTerms>>,
    /// The quantity tick of one spot asset, which is a property of the asset itself.
    ///
    /// A spot balance is a quantity of an asset rather than a position on a pair, so what
    /// displays it is the asset's own tick. A price is the one thing of a pair that is *not*
    /// the asset's: the same base is quoted at its own tick against each asset it trades
    /// against, so a price is displayed from the listing it was read on, never from anything
    /// indexed by the asset alone.
    assets: Arc<AHashMap<AssetId, Tick>>,
    /// The asset each listing is valued in, which is what matches a listing's row to the
    /// venue's own mark price for it.
    listing_assets: Arc<AHashMap<VenueListingKey, AssetId>>,
    /// The assets the route's spot listings are quoted in, in the order the catalog ranks
    /// them, which is what orders the pairs a balance may be valued through.
    quote_assets: Arc<[AssetId]>,
    /// The spot listing of one `(base, quote)` pair, which is the listing a balance quoted in
    /// that pair is valued at.
    pairs: Arc<AHashMap<(AssetId, AssetId), VenueListingKey>>,
    /// The product family of every listing whose catalog described it.
    listing_kinds: Arc<AHashMap<VenueListingKey, InstrumentKind>>,
    /// The asset every listing is settled or paid in, when its instrument has one.
    settlement_assets: Arc<AHashMap<VenueListingKey, AssetId>>,
    stable_assets: Arc<AHashSet<AssetId>>,
    labels: AssetLabels,
    /// The tick a money amount is written in: ten decimal places, which is finer than any
    /// amount the venue reports.
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
    /// Indexes the catalogs of several routes at once.
    ///
    /// Every route is passed with its own published entries, and the catalog generation
    /// supplies the asset names, so a reading is displayed with the terms that were published
    /// with it.
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
                // One route describes one listing once: a key carries its route, and a route's
                // catalog is the venue's own answer for it, so a key met twice is a broken
                // catalog rather than two descriptions to reconcile. Nothing is merged on a
                // collision — the entry that arrived is the entry the route stated.
                listings.insert(key.clone(), terms);
                listing_kinds.insert(key.clone(), entry.instrument.kind());
                match entry.instrument.terms() {
                    InstrumentTerms::Spot(terms) => {
                        // A spot balance is held in the listing's base asset, and the base
                        // token's own size precision is the same on every pair it is quoted in.
                        // Its price is not: two pairs of one base are quoted at their own ticks
                        // and against their own assets, which is why nothing pair-shaped is
                        // indexed by the asset here. Each field takes the finer of the two
                        // values where a base really is described twice: a tick only decides how
                        // many digits a value is written with on the reading side, and the finer
                        // one cannot merge two values a coarser one would tell apart, while the
                        // coarser one can. Merging per field also keeps the index independent of
                        // the order the catalogs are visited in.
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

    /// Displays money without a market behind it: the ledger and its amounts.
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

    /// The asset one listing is valued in, when the catalogs describe it.
    fn listing_asset(&self, listing: &VenueListingKey) -> Option<&AssetId> {
        self.listing_assets.get(listing)
    }

    /// The asset one listing's prices are quoted in, when the catalogs describe it.
    fn listing_quote_asset(&self, listing: &VenueListingKey) -> Option<&AssetId> {
        Some(&self.listings.get(listing)?.quote_asset)
    }

    /// The asset one listing is settled or paid in, when the catalogs describe it.
    fn listing_settlement_asset(&self, listing: &VenueListingKey) -> Option<&AssetId> {
        self.settlement_assets.get(listing)
    }

    /// The basis one listing's prices are published on, when the catalogs describe it.
    fn listing_price_basis(&self, listing: &VenueListingKey) -> Option<&PriceBasis> {
        Some(&self.listings.get(listing)?.basis)
    }

    /// The spot listing of one pair, when a route publishes it.
    fn pair_listing(&self, base: &AssetId, quote: &AssetId) -> Option<&VenueListingKey> {
        self.pairs.get(&(base.clone(), quote.clone()))
    }

    /// Where one quote asset sits in the route's own ranking.
    fn quote_priority(&self, asset: &AssetId) -> Option<usize> {
        self.quote_assets.iter().position(|quote| quote == asset)
    }

    /// The name of one asset, as a reader knows it.
    ///
    /// A generation that does not name the asset is drawn as the localized unknown:
    /// a digest is not a name, and a name invented here would disagree with every
    /// other pane about the same asset.
    pub(super) fn asset_label(&self, asset: &AssetId, cx: &App) -> SmolStr {
        self.asset_name(asset)
            .unwrap_or_else(|| SmolStr::new(t!(cx, "error.display.unknown_asset").as_ref()))
    }

    /// The published display name of one asset, when the generation names it.
    pub(super) fn asset_name(&self, asset: &AssetId) -> Option<SmolStr> {
        self.labels.label(asset)
    }

    /// The label of one quantity unit, when the unit is one this build can name.
    ///
    /// A unit that names an asset is labelled by that asset's own short name, and a unit that
    /// counts contracts is the shared "contracts" wording. An outcome unit names the leg it
    /// belongs to and not the group it settles with, so it has no label here: the tab that
    /// knows the group is what names it.
    pub(super) fn quantity_unit_label(
        &self,
        unit: &market_participants_model::market::units::QuantityUnit,
        cx: &App,
    ) -> Option<SmolStr> {
        use market_participants_model::market::units::QuantityUnit;

        match unit {
            QuantityUnit::AssetUnits { asset } | QuantityUnit::Notional { asset } => {
                Some(self.asset_label(asset, cx))
            }
            QuantityUnit::Contracts => Some(SmolStr::new_static("contracts")),
            QuantityUnit::OutcomeTokens { .. } => None,
        }
    }

    /// The product family of one listing the catalogs know.
    pub(super) fn listing_kind(&self, listing: &VenueListingKey) -> InstrumentKind {
        self.listing_kinds
            .get(listing)
            .copied()
            .unwrap_or(InstrumentKind::Spot)
    }

    /// The name of one listing: its canonical instrument's base asset.
    ///
    /// A listing the reading does not describe has no name here, so it is drawn as the
    /// shared localized unknown rather than as its listing identity. The names
    /// themselves come from the generation the reading was taken under — this index is
    /// built from that generation's catalogs — so the fallback is the same shared
    /// message every other pane falls back to, not a second wording of its own.
    pub(super) fn listing_label(&self, listing: &VenueListingKey, cx: &App) -> SmolStr {
        self.listing_name(listing)
            .unwrap_or_else(|| SmolStr::new(t!(cx, "error.display.unknown_listing").as_ref()))
    }

    /// The published display name of one listing, when the reading describes it.
    pub(super) fn listing_name(&self, listing: &VenueListingKey) -> Option<SmolStr> {
        self.listing_assets
            .get(listing)
            .and_then(|asset| self.labels.label(asset))
    }

    /// The published display name of the asset one listing is collateralised in.
    ///
    /// It is the pure read `listing_collateral_label` falls back from, which only a
    /// test asks for directly: the view draws the label, not the name.
    #[cfg(test)]
    pub(super) fn collateral_name(&self, listing: &VenueListingKey) -> Option<SmolStr> {
        self.listing_settlement_asset(listing)
            .and_then(|asset| self.labels.label(asset))
    }

    /// The name of the asset one listing is collateralised in.
    ///
    /// A perpetual position is margined in what its settlement asset is and a spot order is paid
    /// for in its quote asset, so the collateral of a listing is the asset its own terms name.
    /// A listing the reading does not describe has no collateral to name, which is a missing
    /// value rather than an unknown name.
    pub(super) fn listing_collateral_label(&self, listing: &VenueListingKey, cx: &App) -> SmolStr {
        match self.listing_settlement_asset(listing) {
            Some(asset) => self.asset_label(asset, cx),
            None => SmolStr::new_static("-"),
        }
    }

    /// Displays a quantity of one asset in that asset's own tick, or as money for a stablecoin.
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

    /// Displays a money amount: a balance value, a margin, a notional.
    pub(super) fn format_money_quantity(&self, value: Quantity) -> SmolStr {
        format_grouped_decimal(value.format_and_round_tick(self.amount_tick).as_str())
    }

    /// Displays a signed money amount.
    pub(super) fn format_money_signed_quantity(&self, value: SignedQuantity) -> SmolStr {
        format_grouped_decimal(value.format_and_round_tick(self.amount_tick).as_str())
    }

    /// Displays a profit or a loss, with the sign a gain is read by.
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

    /// Displays one reading's own profit or loss, with the sign a gain is read by.
    pub(super) fn format_profit_or_loss(&self, value: ProfitOrLoss) -> SmolStr {
        self.format_money_profit(SignedQuantity::from_inner(value.inner()))
    }

    /// Displays a price of a listing the catalogs know.
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

    /// Displays a quantity of a listing the catalogs know.
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

    /// Displays an asset's own price, in that asset's own tick, or as money for a stablecoin.
    ///
    /// This is for the one price that has no market behind it: a stablecoin is one unit of
    /// itself, and a balance of one is valued at that. A price read on a pair is drawn from the
    /// pair (see [`SpotBalanceRow::price_listing`]).
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

    /// Displays a signed quantity of a listing the catalogs know: a position or a size.
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

    /// The quantity tick of one asset of a spot catalog.
    fn asset_quantity_tick(&self, asset: &AssetId) -> Result<Tick, WalletDataError> {
        self.assets
            .get(asset)
            .copied()
            .ok_or(WalletDataError::Codec {
                operation: "display a Hyperliquid asset the catalog does not list",
            })
    }

    /// The terms of one listing.
    ///
    /// The lookup is by listing key, so two routes that list the same canonical instrument
    /// keep their own ticks: a position held on one of them is drawn with the rules of the
    /// listing it was held on.
    fn listing_terms(&self, listing: &VenueListingKey) -> Result<ListingTerms, WalletDataError> {
        self.listings
            .get(listing)
            .cloned()
            .ok_or_else(|| WalletDataError::ListingMissing {
                route: listing.route.clone(),
                listing: Box::new(listing.clone()),
            })
    }

    /// Refuses a reading the catalog cannot display.
    ///
    /// A row whose listing is absent would be drawn as its raw value, which is exactly the
    /// silent downgrade this refuses: the answer is an error the pane can report instead.
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

/// The tick a money amount is written in.
fn amount_tick() -> Result<Tick, WalletDataError> {
    Tick::from_inner(10_000_000_000_000_000).map_err(|_| WalletDataError::Codec {
        operation: "index Hyperliquid wallet money precision",
    })
}

/// Groups a formatted decimal's integer digits, leaving its fraction as it is.
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

/// What one wallet is worth right now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AccountSummary {
    pub spot_value: Option<Quantity>,
    pub perpetual_value: Option<Quantity>,
    pub unrealized_pnl: Option<ProfitOrLoss>,
}

impl AccountSummary {
    /// Values one wallet's spot balances and perpetual positions.
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

/// The spot balances a wallet holds, valued at the venue's latest contexts.
///
/// A balance names an asset, and an asset has no price of its own on the venue: what is
/// published is a price per listing. A balance is therefore valued through the pairs that
/// quote its asset, which is the same reading [`SpotSnapshot`] draws row by row.
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

/// The signed size of one position, as the one number its profit is read in.
///
/// A position carries its size as a non-negative magnitude and the direction it points, so the
/// sign is put back here: a long is the magnitude itself and a short is the same magnitude read
/// from the other side. Which way a position points is what decides whether a move in its price
/// is a profit or a loss, and that is the whole of what the sign carries.
fn signed_size(position: &HyperliquidPerpetualPosition) -> SignedQuantity {
    let magnitude = SignedQuantity::from_inner(
        i128::try_from(position.size.value.inner()).unwrap_or(i128::MAX),
    );
    match position.direction {
        PositionDirection::Long => magnitude,
        // A magnitude that fits this build is never `i128::MIN`, so the negation always has a
        // value; an impossible one would leave the magnitude standing rather than reporting a
        // position as its opposite.
        PositionDirection::Short => magnitude.checked_neg().unwrap_or(magnitude),
    }
}

/// The notional a wallet's perpetual positions hold, and what they are up or down.
///
/// A position whose listing has no context is left out of both sums, and a position without an
/// entry price has no unrealized profit to report — which is why the two are optional
/// independently of each other.
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
        // What the positions hold is a magnitude: a size times a price is the notional one
        // position is worth, whichever side it is held from. Which way it is held is what the
        // profit below is read in.
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

/// The venue's own mark or mid price of one listing, with the listing it was read on.
///
/// A position of a route is valued at the context the route publishes for the listing it was
/// held on, so two routes that list the same canonical instrument keep their own prices. The
/// listing travels back with the price because a price is drawn with the rules of the market it
/// was read on and those rules are the listing's, never the one that was asked about.
fn listing_context_price(
    contexts: &[HyperliquidAssetContext],
    precision: &DisplayPrecision,
    listing: &VenueListingKey,
) -> Option<(MarketPrice, VenueListingKey)> {
    let asset = precision.listing_asset(listing)?;
    let quoted = match contexts.iter().find(|context| &context.listing == listing) {
        Some(context) => context,
        None => {
            // A route may publish a context for another listing of the same asset, which is a
            // price of that asset when the venue publishes exactly one of them. Two is not a
            // price this can pick, so nothing is valued rather than the wrong listing being.
            //
            // The listing that answered is the one the price belongs to: it names a market of
            // the same asset, and that market's quote asset is what the price is quoted in.
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

/// One wallet's row in the overview: what it is worth, and how to read it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WalletOverviewSnapshot {
    pub summary: AccountSummary,
    pub precision: DisplayPrecision,
}

/// The fills of one wallet over the window a timeline shows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TimelineSnapshot {
    pub window_start: i64,
    pub window_end: i64,
    pub fills: Arc<[HyperliquidUserFill]>,
}

/// The six views of one account.
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
    /// Every tab, in the order the tab bar shows them.
    pub(super) const ALL: [Self; 6] = [
        Self::Spot,
        Self::Perpetual,
        Self::OpenOrders,
        Self::Trades,
        Self::HistoricalOrders,
        Self::Ledger,
    ];

    /// Where this tab sits in [`DetailTab::ALL`].
    ///
    /// Nothing reads a position back into a tab: a control names the tab it opens, so a
    /// position only ever has to be built from one.
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

/// One spot balance, valued against the wallet's other balances.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SpotBalanceRow {
    pub asset: AssetId,
    pub total: SignedQuantity,
    pub value: Option<SignedQuantity>,
    pub price: Option<MarketPrice>,
    /// The pair the price beside it was read on, which is what draws that price: the price
    /// tick, the quote asset and the basis of a spot market are that market's, and a base
    /// quoted against two assets has two of each. A stablecoin's price is money, which no
    /// market quotes, so it has no listing here.
    pub price_listing: Option<VenueListingKey>,
    /// What share of the wallet's spot value this balance is, when the wallet has one.
    pub asset_share: Option<SmolStr>,
}

/// Every spot balance a wallet holds, with the pairs they are quoted against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SpotSnapshot {
    pub rows: Arc<[SpotBalanceRow]>,
    pub pairs: Arc<[HyperliquidSpotPair]>,
}

impl SpotSnapshot {
    /// Takes the displayable balances as read, and values them against the venues' contexts.
    ///
    /// The venue's spot state reports every token the account holds, including tokens its
    /// spot metadata does not describe — outcome shares are the ones seen in practice. A
    /// balance whose asset has neither a published quantity tick nor a stablecoin identity
    /// cannot be drawn as a spot position: it has no digits to round to and no pair to price
    /// it on. It is left out rather than failing the tab, because one such balance would
    /// otherwise hide every holding the tab can draw.
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

    /// Revalues every row at the latest contexts, and re-derives the shares.
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

/// The price of one spot asset, from the pairs it is quoted in, with the listing it was read on.
///
/// A balance is held in an asset and priced through the pairs that quote it: the venue
/// publishes no price for an asset on its own, and the stablecoins a route settles in are the
/// money everything else is read against.
///
/// The listing travels back with the price because a price is drawn with the rules of the market
/// it was read on and a base quoted against two assets has two of those. Money is the one price
/// that has no market behind it, so it answers no listing at all.
///
/// Two pairs of the same rank are not a price this can pick — a balance held in one base and
/// quoted against two different assets has no single price — so it answers `None` rather than
/// guessing which market the balance is valued in.
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
    // The listing that answered, which is the pair asked about unless that pair published no
    // context and exactly one other market of the same asset did.
    Some((first.1.0.clone(), Some(first.1.1.clone())))
}

/// What share of a total one value is, to two places.
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

/// Which of the six views of an account a reading is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum DetailData {
    Spot(SpotSnapshot),
    Perpetual(HyperliquidPerpetualState),
    OpenOrders(Vec<HyperliquidOpenOrder>),
    Trades(Vec<HyperliquidUserFill>),
    HistoricalOrders(Vec<HyperliquidHistoricalOrder>),
    Ledger(Vec<HyperliquidLedgerEvent>),
}

/// One reading of one detail tab, with the precision it is displayed at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DetailSnapshot {
    pub data: DetailData,
    pub precision: DisplayPrecision,
}

impl DetailSnapshot {
    /// Validates a reading against the catalog before anything shows it.
    pub(super) fn new(
        data: DetailData,
        precision: DisplayPrecision,
    ) -> Result<Self, WalletDataError> {
        precision.validate_detail(&data)?;
        Ok(Self { data, precision })
    }

    /// Revalues the reading at the latest contexts.
    ///
    /// Only the two tabs whose rows carry a live valuation revalue; the rest are history, and
    /// history does not move.
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
                        // The size the profit is read in carries the direction: a long gains
                        // what the price rose and a short gains what it fell, which is the same
                        // arithmetic with the opposite sign.
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

/// The newest fills of a reading, newest first.
///
/// The venue answers with more than a tab shows, and the oldest beyond the cap are dropped
/// here rather than by whoever asked: an answer's size is the venue's business, and a tab's
/// size is this widget's.
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

/// The newest open orders of a reading, newest first.
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

/// The newest historical orders of a reading, newest first.
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

/// The newest ledger events of a reading, newest first.
pub(super) fn sort_and_limit_ledger(
    mut events: Vec<HyperliquidLedgerEvent>,
) -> Vec<HyperliquidLedgerEvent> {
    events.sort_unstable_by_key(|event| std::cmp::Reverse(event.timestamp));
    events.truncate(MAX_HISTORY);
    events
}

/// How old a timestamp is, in the coarsest unit that still says something.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AgeBucket {
    Seconds(u64),
    Minutes(u64),
    Hours(u64),
    Days(u64),
}

/// The age of a timestamp, as of `now_millis`.
///
/// A timestamp the clock has not reached yet is "now" rather than a negative age, and the
/// boundaries are inclusive so a value that reads as a whole minute is the minute.
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

/// The wall clock, in the milliseconds the venue timestamps with.
///
/// It answers nothing when the system clock is before the epoch, which is the one thing a
/// caller has to decide about rather than read a plausible-looking zero for.
pub(super) fn now_millis() -> Option<i64> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let value = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    i64::try_from(value).ok()
}

/// When a fill, an order or a transfer happened, in UTC, to the second.
///
/// It is what a relative age is a summary of, so it is written the same way for every row.
pub(super) fn absolute_time(timestamp: i64) -> SharedString {
    use time::OffsetDateTime;

    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp) * 1_000_000).map_or_else(
        |_| SharedString::new_static("--"),
        |time| {
            SharedString::new(
                format_smolstr!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
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
    use gpui_kit::TestAppContext;
    use market_participants_connector::account::domain::HyperliquidOpenOrder;

    use super::*;
    use crate::test_support::{
        context, money, pair_listing, perp_entry, perp_instrument, perpetual_route,
        perpetual_state, position, precision_for, precision_of, published_perpetual,
        published_spot, spot_entry, spot_pair, spot_route, spot_state, token, wallet_balance,
    };

    /// The perpetual listing the tests are about, as the fixture publishes it.
    fn perpetual_listing() -> VenueListingKey {
        published_perpetual("ETH")
    }

    /// A perpetual listing of the test perpetual route.
    fn perpetual(asset: &str, price_tick: &str, quantity_tick: &str) -> CatalogEntry {
        crate::test_support::entry(
            &perpetual_route(),
            perp_instrument(asset),
            asset,
            price_tick,
            quantity_tick,
        )
    }

    /// A spot listing of the test spot route.
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

    /// Two routes may list the same canonical instrument under different ticks, and a value is
    /// drawn with the rules of the listing it belongs to.
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

    /// Gate G1: the catalogs of every wallet route index into one display.
    ///
    /// One monitored wallet is read across the core spot scope, the core perpetual scope and every
    /// deployed perpetual DEX, and all of those readings are displayed through one precision index.
    /// Whether those catalogs can be indexed together is a property to check rather than to
    /// assume, and the gate that checks it is the precision index itself: the two synthetic
    /// perpetuals of one instrument are what a deployed DEX publishes next to the core perpetual
    /// of the same instrument. What the gate does *not* cover is a base asset quoted against two
    /// assets, which is a property of one route's own catalog rather than of the union; the
    /// two-quote tests below are that shape's guards.
    /// A route is read the way its own catalog says: spot pairs come from a route that published
    /// spot listings, and every other route is read for positions and orders.
    ///
    /// The empty catalog is what the last assertion is about: "everything else is perpetual" is
    /// the conservative reading, because a partition that is skipped drops whatever it held
    /// silently, while a partition that is read and fails is a failure the address reports.
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

    /// An asset is named by its own canonical terms, and an asset no catalog registered is
    /// named by the identity that stands in for it.
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

    /// A perpetual is named by the asset its underlying tracks, and margined in what it settles.
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

    /// A balance whose asset no pair prices is left out of the total rather than valued at zero,
    /// and the venue's quote ranking is what picks between two pairs that quote it.
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
        // Both pairs of the base have a price of their own, so the ranking is what decides which
        // one values it: USDC is the quote the catalog states first.
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

    /// The quote ranking still decides when the higher-ranked pair has no price of its own: the
    /// base is valued through the pair that answered rather than left out.
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

    /// Two pairs of the same rank are not a price this can pick: a balance quoted against both
    /// has no single market it is valued in.
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

    /// A balance the spot catalogs do not describe is left out rather than failing the tab.
    ///
    /// Hyperliquid's spot state also reports outcome shares, which its spot metadata does not
    /// describe and whose quantity tick therefore cannot be read. One of them must not hide every
    /// holding the tab can draw.
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

    /// A base asset quoted against two assets is one asset with two markets, and its price
    /// belongs to the market it was read on: the quote travels with the price rather than being
    /// taken from whichever pair the catalog happened to list first.
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
            // Only the second pair has a price, so the balance is valued through it even though
            // the first pair is the one the catalog ranks higher.
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

    /// The two pairs of one base are two markets: their price ticks are their own, so the
    /// balance is displayed at the tick of the pair that priced it rather than at the first
    /// tick the catalog stated.
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

    /// A balance priced through a context of another market of its own asset keeps that market.
    ///
    /// A route may publish a context for another listing of the same asset — a perpetual of the
    /// coin beside the spot pair — and that context's price is the price of the asset, so the
    /// balance is valued at it. What draws it is still the market that answered: its tick, its
    /// quote asset and its basis, never the pair that was asked about first.
    #[test]
    fn a_balance_priced_from_another_market_of_its_asset_should_keep_that_market() {
        // Two routes, because a catalog entry states the route that published it: the spot pair
        // is core spot's and the perpetual of the same coin is core perpetual's.
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
            // The pair has no context of its own; the coin's perpetual is the only market of
            // this asset that published one.
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

    /// What a short is worth and what it is up are read from the side it is held on: the price
    /// falling is a gain to a short exactly where it is a loss to a long.
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
    fn absolute_time_should_be_utc_to_the_second() {
        assert_eq!(
            absolute_time(1_700_000_000_000).as_ref(),
            "2023-11-14 22:13:20Z"
        );
        assert_eq!(absolute_time(i64::MIN).as_ref(), "--");
    }

    /// A tab's index is where it sits in the order the tabs are shown in.
    #[test]
    fn every_tab_should_sit_at_its_own_index() {
        for (index, tab) in DetailTab::ALL.into_iter().enumerate() {
            assert_eq!(tab.index(), index);
        }
    }

    /// An asset no generation names is the localized unknown rather than the
    /// canonical identity the reading carried.
    ///
    /// A wallet reading names balances, fees and collateral of assets the
    /// published generation may not name at all, and the label of one of those is
    /// user-visible text. A digest is not a name, and a code invented here would
    /// disagree with every other pane about the same asset.
    #[test]
    fn an_unpublished_asset_should_be_named_as_unknown() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

            let precision = money();
            let unpublished = token("NOWHERE");
            let label = precision.asset_label(&unpublished, cx);
            let identity = unpublished.to_string();

            assert_eq!(
                label.as_str(),
                t!(cx, "error.display.unknown_asset").as_ref()
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
        });
    }

    /// The same read for a listing: a listing the reading does not describe is the
    /// localized unknown rather than its identity.
    #[test]
    fn a_listing_no_generation_names_should_be_the_localized_unknown() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

            let precision = money();
            let listing = crate::test_support::published_spot("BTC", "USDT");
            let label = precision.listing_label(&listing, cx);

            assert_eq!(
                label.as_str(),
                t!(cx, "error.display.unknown_listing").as_ref()
            );
            assert!(
                !label.contains("listing-id/v1/"),
                "an identity is not a name: {label}"
            );
        });
    }
}

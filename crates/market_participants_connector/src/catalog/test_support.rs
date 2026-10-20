//! Catalogs and registries a caller's unit test drives the directory through.
//!
//! A widget asks its route whether it still lists a listing, and that question
//! has to be answered in a test without a venue behind it. What a route answers
//! is a property of its catalog, so the answer is built here, next to the rules
//! that decide it: a test names the listings a route publishes — or says that the
//! route does not answer at all — and installs the result through
//! [`crate::test_support::install_root`], which wires it into a fixture root.
//!
//! Enabled by the `test-support` feature, which is a dev-dependency feature only
//! and never part of a release build.

use std::sync::Arc;

use futures::future::BoxFuture;
use market_participants_model::{
    asset::{
        AssetId, CryptoTokenId, CryptoTokenOrigin, CryptoTokenTerms, CurrencyId, CurrencyTerms,
        Iso4217Code, SecurityForm, SecurityId, SecurityIssuerId, SecurityTerms, ShareClassId,
        ShareClassTerms, TokenReference, UnderlyingRef,
    },
    decimal::{Quantity, Tick, UnixMillis},
    identifier::{
        IdentifierNamespace, IdentifierValue, NativeSymbol, ReferenceSourceId,
        ReferenceTermsVersion, TokenIssuerId, VenueId, VenueScopeId,
    },
    instrument::{Instrument, InstrumentKind, InstrumentTerms, SpotTerms},
    listing::{
        continuity::{ListingContinuityKey, ListingContinuityTerms},
        identifiers::{BinanceListingIdentifiers, VenueListingIdentifiers},
        listing_impl::{ListingSnapshot, ListingStatus, VenueListing},
        route::{VenueListingKey, VenueRoute},
        rules::{
            CommonTradingRules, LotPolicy, PriceRule, ProductTradingRules, QuantityRule,
            ShortSalePolicy, SpotTradingRules, TradingRules, VenueTradingPolicy,
        },
        venue::{BinanceProduct, PriceBasis, VenueListingId},
    },
    market::units::QuantityUnit,
    reference::{
        PriceReferenceTerms, ReferenceAggregation, ReferenceObservation, ReferenceObservationKind,
        ReferenceSessionConvention, ReferenceSettlementConvention,
    },
    routing::{
        CapabilityId, IdentifierNormalizationProduct, IdentifierNormalizationRule,
        ListingIdentifierKind, SymbolNormalization,
    },
    settlement::DeliveryMode,
};
use smol_str::SmolStr;

use crate::catalog::{
    batch::{CatalogBatch, CatalogEntry},
    error::{CatalogError, CatalogValidationError, SourceError},
    generation::CatalogGenerationId,
    registry::{
        asset::{AssetRegistry, AssetTerms},
        staging::CatalogStagingHandle,
    },
    resolved::EnsuredRoute,
    source::{
        CatalogFetchContext, CatalogRouteRegistration, CatalogRouteRequirements, CatalogRouteSet,
        CatalogRouteUpdate, CatalogSource, CatalogSourceBinding, CatalogSourceId,
        RouteInputRevision,
    },
    store::Catalog,
};

/// The venue of every fixture route.
pub const VENUE: &str = "binance";

/// The scope of the fixture spot route.
pub const SPOT_SCOPE: &str = "spot";

/// The id of the fixture spot source.
pub const SPOT_SOURCE: &str = "binance-spot";

/// A canonical fiat asset identity.
pub fn currency(code: &str) -> AssetId {
    AssetId::Currency(currency_id(code))
}

/// A canonical fiat currency identity.
pub fn currency_id(code: &str) -> CurrencyId {
    CurrencyId::new(Iso4217Code::new(code).expect("the fixture currency code should be valid"))
}

/// The canonical terms of one fixture fiat currency.
pub fn currency_terms(code: &str) -> CurrencyTerms {
    CurrencyTerms {
        code: Iso4217Code::new(code).expect("the fixture currency code should be valid"),
    }
}

/// A canonical asset identity of a venue-issued token.
pub fn token(issuer: &str, code: &str) -> AssetId {
    AssetId::CryptoToken(token_id(issuer, code))
}

/// A canonical crypto token identity of a venue-issued token.
pub fn token_id(issuer: &str, code: &str) -> CryptoTokenId {
    CryptoTokenId::new(venue_issued(issuer, code)).expect("the fixture token should be valid")
}

/// The canonical terms of one fixture venue-issued token.
pub fn token_terms(issuer: &str, code: &str) -> CryptoTokenTerms {
    CryptoTokenTerms {
        origin: venue_issued(issuer, code),
    }
}

fn venue_issued(issuer: &str, code: &str) -> CryptoTokenOrigin {
    CryptoTokenOrigin::VenueIssued {
        issuer: TokenIssuerId::new(issuer).expect("the fixture issuer should be valid"),
        namespace: IdentifierNamespace::new(issuer).expect("the fixture namespace is valid"),
        token: TokenReference::Native {
            code: IdentifierValue::new(code).expect("the fixture token code is valid"),
        },
    }
}

/// A canonical security identity of an issued common stock.
pub fn security(issuer: &str, designation: &str) -> AssetId {
    AssetId::Security(security_id(issuer, designation))
}

/// A canonical issued security identity.
pub fn security_id(issuer: &str, designation: &str) -> SecurityId {
    let terms = security_terms(issuer, designation);
    SecurityId::new(terms.issuer, terms.share_class, terms.form)
}

/// The canonical terms of one fixture issued security.
pub fn security_terms(issuer: &str, designation: &str) -> SecurityTerms {
    let issuer_id =
        SecurityIssuerId::new(issuer).expect("the fixture security issuer should be valid");
    let share_class = ShareClassId::new(&ShareClassTerms {
        issuer: issuer_id.clone(),
        designation: designation.into(),
        form: SecurityForm::CommonStock,
        par_value: None,
        adr_ratio: None,
    })
    .expect("the fixture share class should be valid");
    SecurityTerms {
        issuer: issuer_id,
        share_class,
        form: SecurityForm::CommonStock,
    }
}

/// A venue route of the fixture venue.
pub fn route(venue: &str, scope: &str) -> VenueRoute {
    VenueRoute::new(
        VenueId::from_name(venue).expect("the fixture venue should be valid"),
        VenueScopeId::new(scope).expect("the fixture scope should be valid"),
    )
}

/// The fixture spot route.
pub fn spot_route() -> VenueRoute {
    route(VENUE, SPOT_SCOPE)
}

/// A second fixture route of the same venue, for cross-route isolation.
pub fn perp_route() -> VenueRoute {
    route(VENUE, "perpetual")
}

/// The canonical spot instrument of two fixture assets.
pub fn spot(base: AssetId, quote: AssetId) -> Instrument {
    Instrument::new(InstrumentTerms::Spot(
        SpotTerms::new(base, quote).expect("the fixture spot terms should be valid"),
    ))
    .expect("the fixture instrument should be valid")
}

/// A spot instrument of two venue-issued tokens of one issuer.
///
/// A stablecoin is a token, never a fiat currency: the model only accepts a real
/// ISO 4217 code as a currency, so `USDT` can only be a `CryptoTokenId`.
pub fn spot_token(issuer: &str, base: &str, quote: &str) -> Instrument {
    spot(token(issuer, base), token(issuer, quote))
}

/// The continuity key of one Binance fixture listing.
pub fn continuity_key(
    route: &VenueRoute,
    symbol: &str,
    product: BinanceProduct,
) -> VenueListingKey {
    ListingContinuityKey {
        route: route.clone(),
        terms: ListingContinuityTerms::Binance {
            product,
            instrument_id: NativeSymbol::new(symbol).expect("the fixture symbol should be valid"),
        },
    }
    .listing_key()
    .expect("the fixture continuity key should be valid")
}

/// The listing key of one Binance spot fixture listing.
pub fn listing_key(route: &VenueRoute, symbol: &str) -> VenueListingKey {
    continuity_key(route, symbol, BinanceProduct::Spot)
}

/// One validated spot trading rule set.
pub fn rules(quote: AssetId, base: AssetId) -> TradingRules {
    TradingRules {
        common: CommonTradingRules {
            price: PriceRule {
                quote_asset: quote,
                basis: PriceBasis::PerAssetUnit,
                tick: Tick::try_from(Quantity::from(1_u8))
                    .expect("the fixture tick should be valid"),
                minimum: None,
                maximum: None,
            },
            quantity: QuantityRule {
                unit: QuantityUnit::AssetUnits { asset: base },
                step: Quantity::from(1_u8),
                minimum: None,
                maximum: None,
            },
            notional: None,
            venue_policy: Arc::new(VenueTradingPolicy::continuous()),
        },
        product: Arc::new(ProductTradingRules::Spot(
            SpotTradingRules::new(
                LotPolicy::FractionalAllowed {
                    lot_size: Quantity::from(1_u8),
                    minimum_fraction: None,
                },
                ShortSalePolicy::Allowed,
            )
            .expect("the fixture spot rules should be valid"),
        )),
    }
}

/// The quote and base asset of a fixture spot instrument.
pub fn spot_assets(instrument: &Instrument) -> (AssetId, AssetId) {
    match instrument.terms() {
        InstrumentTerms::Spot(terms) => (terms.quote().clone(), terms.base().clone()),
        _ => (currency("USD"), currency("USD")),
    }
}

/// One catalog entry a route publishes under a Binance fixture listing.
pub fn entry(route: &VenueRoute, instrument: Instrument, symbol: &str) -> CatalogEntry {
    entry_of(
        route,
        instrument,
        symbol,
        BinanceProduct::Spot,
        BinanceListingIdentifiers {
            product: BinanceProduct::Spot,
            future_series: None,
            option_series: None,
        },
    )
}

/// The same, under the identifier family of one named product.
///
/// A Binance listing declares the product its symbol belongs to, and the model
/// refuses a listing whose family does not match the kind of its instrument, so a
/// fixture that publishes a derivative names the derivative product and is keyed
/// by it.
pub fn entry_of(
    route: &VenueRoute,
    instrument: Instrument,
    symbol: &str,
    product: BinanceProduct,
    identifiers: BinanceListingIdentifiers,
) -> CatalogEntry {
    let key = continuity_key(route, symbol, product);
    let (quote, base) = spot_assets(&instrument);
    let listing = VenueListing::new(
        key.clone(),
        *instrument.id(),
        Some(NativeSymbol::new(symbol).expect("the fixture symbol should be valid")),
        Vec::new(),
        VenueListingIdentifiers::Binance(identifiers),
    )
    .expect("the fixture listing should be valid");
    let snapshot = ListingSnapshot::new(
        key,
        rules(quote, base),
        ListingStatus::Trading,
        None,
        None,
        0,
    )
    .expect("the fixture snapshot should be valid");
    CatalogEntry {
        instrument,
        listing,
        snapshot,
    }
}

/// The same entry with extra display aliases.
pub fn entry_with_aliases(
    route: &VenueRoute,
    instrument: Instrument,
    symbol: &str,
    aliases: &[&str],
) -> CatalogEntry {
    let mut entry = entry(route, instrument, symbol);
    entry.listing.aliases = aliases
        .iter()
        .map(|alias| NativeSymbol::new(alias).expect("the fixture alias should be valid"))
        .collect();
    entry
}

/// The same entry under another snapshot status.
pub fn with_status(mut entry: CatalogEntry, status: ListingStatus) -> CatalogEntry {
    entry.snapshot.status = status;
    entry
}

/// The identifier normalization rules that fold ASCII case for symbols.
///
/// The rules are sorted by `(identifier, product)` and unique, which is what a
/// route descriptor requires.
pub fn case_insensitive_symbols() -> Vec<IdentifierNormalizationRule> {
    let mut rules = vec![
        IdentifierNormalizationRule::new(
            ListingIdentifierKind::BinanceInstrument,
            None,
            SymbolNormalization::AsciiCaseInsensitive,
        ),
        IdentifierNormalizationRule::new(
            ListingIdentifierKind::Symbol,
            None,
            SymbolNormalization::AsciiCaseInsensitive,
        ),
    ];
    rules.sort_by_key(|rule| (rule.identifier, rule.product));
    rules
}

/// The normalization rules a venue route declares, at typed identifier and
/// product granularity, with `mode` applied to both.
///
/// This is the shape the built-in Binance and OKX descriptors declare: the
/// instrument identifier is folded per product and the display symbol per
/// instrument kind.
pub fn instrument_normalization(mode: SymbolNormalization) -> Vec<IdentifierNormalizationRule> {
    let mut rules = vec![
        IdentifierNormalizationRule::new(
            ListingIdentifierKind::BinanceInstrument,
            Some(IdentifierNormalizationProduct::Binance(
                BinanceProduct::Spot,
            )),
            mode,
        ),
        IdentifierNormalizationRule::new(
            ListingIdentifierKind::Symbol,
            Some(IdentifierNormalizationProduct::Instrument(
                InstrumentKind::Spot,
            )),
            mode,
        ),
    ];
    rules.sort_by_key(|rule| (rule.identifier, rule.product));
    rules
}

/// The requirements of a route set that provides a catalog on every given route.
pub fn requirements(routes: &[VenueRoute]) -> CatalogRouteRequirements {
    CatalogRouteRequirements::new(
        routes
            .iter()
            .cloned()
            .map(|route| (route, CapabilityId::MarketCatalog)),
    )
}

/// The requirements of a route set in which no route provides a catalog.
pub fn non_catalog_requirements(routes: &[VenueRoute]) -> CatalogRouteRequirements {
    CatalogRouteRequirements::new(
        routes
            .iter()
            .cloned()
            .map(|route| (route, CapabilityId::Tickers)),
    )
}

/// One registered catalog source binding.
pub fn binding(id: &str, source: Arc<dyn CatalogSource>) -> CatalogSourceBinding {
    CatalogSourceBinding::new(
        CatalogSourceId::new(id).expect("the fixture source id should be valid"),
        source,
    )
}

/// One registered catalog route.
pub fn registration(route: &VenueRoute, source: &str) -> CatalogRouteRegistration {
    let source = CatalogSourceId::new(source).expect("the fixture source id should be valid");
    let normalization = case_insensitive_symbols();
    CatalogRouteRegistration::catalog(
        route.clone(),
        source.clone(),
        normalization.clone(),
        vec![CapabilityId::MarketCatalog],
        RouteInputRevision::of_declaration(route, Some(&source), &normalization),
    )
}

/// One registered route that provides no catalog.
pub fn non_catalog_registration(route: &VenueRoute) -> CatalogRouteRegistration {
    let normalization = case_insensitive_symbols();
    CatalogRouteRegistration::without_catalog(
        route.clone(),
        normalization.clone(),
        vec![CapabilityId::Tickers],
        RouteInputRevision::of_declaration(route, None, &normalization),
    )
}

/// A source that publishes the entries whose listing belongs to the fetched
/// route, and an empty catalog for any other route.
pub fn listing(entries: Vec<CatalogEntry>) -> Arc<dyn CatalogSource> {
    Arc::new(Listed(entries))
}

/// A source that publishes its entries verbatim, whatever route is fetched.
///
/// Used by the tests that assert what the validator does with a batch that does
/// not belong to the fetched route.
pub fn raw(entries: Vec<CatalogEntry>) -> Arc<dyn CatalogSource> {
    Arc::new(Raw(entries))
}

/// A source that never answers.
pub fn pending() -> Arc<dyn CatalogSource> {
    Arc::new(Unanswered)
}

/// A source whose every fetch fails.
pub fn failing() -> Arc<dyn CatalogSource> {
    Arc::new(Failed)
}

/// A source that publishes its batch only once the returned gate is released.
pub fn gated(entries: Vec<CatalogEntry>) -> (Arc<Gate>, Arc<dyn CatalogSource>) {
    let gate = Arc::new(Gate::default());
    (
        Arc::clone(&gate),
        Arc::new(Gated {
            entries,
            gate: Arc::clone(&gate),
        }),
    )
}

/// The batch one fixture route publishes.
pub fn batch(entries: Vec<CatalogEntry>) -> CatalogBatch {
    CatalogBatch {
        listings: entries,
        ..CatalogBatch::default()
    }
}

/// A release gate a test drives a fetch with.
///
/// The gate records that a fetch started and then waits for a latched release,
/// so a release that happens before the fetch reaches the gate is never lost and
/// a test can drive one fetch at a time.
pub struct Gate {
    started: tokio::sync::Semaphore,
    released: std::sync::atomic::AtomicBool,
    calls: std::sync::atomic::AtomicUsize,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            started: tokio::sync::Semaphore::new(0),
            released: std::sync::atomic::AtomicBool::new(false),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl Gate {
    /// Waits until the source has started a fetch.
    pub async fn started(&self) {
        let permit = self
            .started
            .acquire()
            .await
            .expect("the fixture gate is never closed");
        permit.forget();
    }

    /// Releases every waiting fetch.
    pub fn release(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Returns how many fetches have started.
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Acquire)
    }

    fn begin(&self) {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.started.add_permits(1);
    }

    async fn wait(&self) {
        while !self.released.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    }
}

struct Gated {
    entries: Vec<CatalogEntry>,
    gate: Arc<Gate>,
}

impl CatalogSource for Gated {
    fn fetch_catalog(
        &self,
        context: CatalogFetchContext,
    ) -> BoxFuture<'static, Result<CatalogBatch, SourceError>> {
        let entries = entries_of(&self.entries, &context);
        let gate = Arc::clone(&self.gate);
        Box::pin(async move {
            gate.begin();
            gate.wait().await;
            Ok(batch(entries))
        })
    }
}

struct Listed(Vec<CatalogEntry>);

impl CatalogSource for Listed {
    fn fetch_catalog(
        &self,
        context: CatalogFetchContext,
    ) -> BoxFuture<'static, Result<CatalogBatch, SourceError>> {
        let entries = entries_of(&self.0, &context);
        // The display contract renders the canonical terms of every asset a
        // listing is denominated in, so the fixture source stages them the way a
        // real adapter stages the terms it translated out of a venue response.
        let mut batch = batch(entries);
        let published = Arc::clone(context.staging().base().assets());
        stage_fixture_terms(&mut batch, context.staging(), &published);
        Box::pin(async move { Ok(batch) })
    }
}

/// Stages the canonical terms of every fixture asset a batch names.
///
/// This is the one call a test source makes before it returns its batch, so the
/// batch a fixture publishes carries the same terms a real adapter's batch does.
/// The batch is returned unchanged, so the call reads as a statement in a source
/// that builds one inline.
pub fn fixture_batch(mut batch: CatalogBatch, context: &CatalogFetchContext) -> CatalogBatch {
    let published = Arc::clone(context.staging().base().assets());
    stage_fixture_terms(&mut batch, context.staging(), &published);
    batch
}

/// Carries the canonical terms of every fixture asset a batch names.
///
/// A canonical identity is a digest of its terms, so a batch that carries only
/// identities cannot be displayed. A real source stages the terms while it
/// translates a venue response; this helper does the same for the fixture
/// vocabulary, so a test batch carries the terms its listings are read with. A
/// term set the published generation already carries is preferred, so a fixture
/// that built its own terms — a security, whose class terms an identity cannot be
/// read back from — keeps them.
pub fn stage_fixture_terms(
    batch: &mut CatalogBatch,
    staging: &CatalogStagingHandle,
    published: &AssetRegistry,
) {
    let mut terms: Vec<AssetTerms> = Vec::with_capacity(batch.listings.len() * 2 + 2);
    // A batch that already states the terms of its assets keeps them: a fixture
    // whose vocabulary the recovery below does not know — a reserved token, a
    // security with its own class terms — states them itself.
    for stated in &batch.asset_terms {
        terms_push(&mut terms, stated.clone());
    }
    for entry in &batch.listings {
        for candidate in fixture_asset_terms(&entry.instrument) {
            terms_push(&mut terms, candidate);
        }
    }
    // An outcome group settles in an asset its listings are not denominated in,
    // and the display renders that settlement asset, so its terms are carried too.
    for group in &batch.outcome_groups {
        if let Some(candidate) = published
            .terms(&group.settlement_asset)
            .or_else(|| fixture_asset_term(&group.settlement_asset))
        {
            terms_push(&mut terms, candidate);
        }
    }
    batch.asset_terms = terms;
    staging.with(|staging| {
        let pending = staging.pending_references_mut();
        for entry in &batch.listings {
            if let Some(terms) = fixture_price_reference_terms(&entry.instrument) {
                let _ = pending.register_price_reference(terms);
            }
        }
    });
}

struct Raw(Vec<CatalogEntry>);

impl CatalogSource for Raw {
    fn fetch_catalog(
        &self,
        _: CatalogFetchContext,
    ) -> BoxFuture<'static, Result<CatalogBatch, SourceError>> {
        let entries = self.0.clone();
        Box::pin(async move { Ok(batch(entries)) })
    }
}

struct Unanswered;

impl CatalogSource for Unanswered {
    fn fetch_catalog(
        &self,
        _: CatalogFetchContext,
    ) -> BoxFuture<'static, Result<CatalogBatch, SourceError>> {
        Box::pin(std::future::pending())
    }
}

struct Failed;

impl CatalogSource for Failed {
    fn fetch_catalog(
        &self,
        _: CatalogFetchContext,
    ) -> BoxFuture<'static, Result<CatalogBatch, SourceError>> {
        Box::pin(async {
            Err(SourceError::Network {
                reason: SmolStr::new_static("the test catalog is unreachable"),
            })
        })
    }
}

fn entries_of(entries: &[CatalogEntry], context: &CatalogFetchContext) -> Vec<CatalogEntry> {
    entries
        .iter()
        .filter(|entry| &entry.listing.key.route == context.venue_route())
        .cloned()
        .collect()
}

/// The canonical terms of every asset one fixture instrument is denominated in.
///
/// A security identity is a digest of its class terms, so a fixture security's
/// terms cannot be recovered from the `AssetId` a listing names; the fixture
/// vocabulary only builds currencies and venue-issued tokens, whose terms are
/// recoverable, and a security is reported as the missing terms the display gate
/// would refuse.
pub fn fixture_asset_terms(instrument: &Instrument) -> Vec<AssetTerms> {
    let mut terms: Vec<AssetTerms> = Vec::with_capacity(4);
    for asset in entry_asset_ids(instrument) {
        if let Some(candidate) = fixture_asset_term(&asset) {
            terms_push(&mut terms, candidate);
        }
    }
    terms
}

/// The price reference terms a fixture batch's derivatives are quoted through.
///
/// The reference identity is a digest of a reference's own terms, and the display
/// contract renders the reference and its scope, so the fixture stages the one
/// canonical reference shape the fixture vocabulary uses: a direct mark
/// observation of the underlying in the price quote, on a continuous session. The
/// observation source has to be the one the instrument's own reference was built
/// with, which a digest cannot be read back from, so every fixture source is tried
/// and only the shape that derives the identity the instrument names is staged.
///
/// The source stages the reference through the fetch context, because a registry
/// identity only reaches the publication when the source wrote it into the staging
/// view or a published generation already carries it.
pub fn fixture_price_reference_terms(instrument: &Instrument) -> Option<PriceReferenceTerms> {
    let terms = instrument.terms();
    let (tracked, quote_asset, wanted) = match &terms {
        InstrumentTerms::Perpetual(terms) => (
            terms.underlying().reference().clone(),
            terms.price_quote().clone(),
            *terms.underlying().price_reference(),
        ),
        InstrumentTerms::Future(terms) => (
            terms.underlying().reference().clone(),
            terms.price_quote().clone(),
            *terms.underlying().price_reference(),
        ),
        InstrumentTerms::Option(terms) => (
            terms.underlying().reference().clone(),
            terms.price_quote().clone(),
            *terms.underlying().price_reference(),
        ),
        InstrumentTerms::Spot(_) | InstrumentTerms::Outcome(_) => return None,
    };
    for source in FIXTURE_ISSUERS {
        let candidate = PriceReferenceTerms {
            underlying: tracked.clone(),
            quote_asset: quote_asset.clone(),
            basis: PriceBasis::PerAssetUnit,
            observation: ReferenceObservation {
                source: ReferenceSourceId::new(source)
                    .expect("a fixture reference source is canonical"),
                kind: ReferenceObservationKind::Mark,
            },
            aggregation: ReferenceAggregation::Direct,
            session: ReferenceSessionConvention::Continuous,
            settlement: ReferenceSettlementConvention::Continuous,
            schema_version: ReferenceTermsVersion::new("v1")
                .expect("the fixture reference version is valid"),
        };
        if candidate.reference_id().ok() == Some(wanted) {
            return Some(candidate);
        }
    }
    None
}

/// Appends one term set unless its asset is already collected.
fn terms_push(terms: &mut Vec<AssetTerms>, candidate: AssetTerms) {
    if !terms
        .iter()
        .any(|known| known.asset_id() == candidate.asset_id())
    {
        terms.push(candidate);
    }
}

/// The issuers the fixture vocabulary recovers a token from.
///
/// A canonical token identity is a digest of its origin, so the terms of the
/// assets a listing is denominated in cannot be read back from its `AssetId`. The
/// fixture source stages the terms of the assets its own vocabulary builds — the
/// venue-issued native tokens of the fixture venues — and reports every other
/// asset as the missing terms the display gate would refuse.
pub const FIXTURE_ISSUERS: &[&str] = &[VENUE, "hyperliquid"];

/// Returns the canonical terms of one fixture asset, when the fixture vocabulary
/// can build them back from its identity.
fn fixture_asset_term(asset: &AssetId) -> Option<AssetTerms> {
    match asset {
        AssetId::Currency(id) => {
            // The ISO code is the whole currency terms, so the fixture currency
            // vocabulary is the one the tests built the identity from.
            let code = FIXTURE_CODES
                .iter()
                .find(|code| &currency_id(code) == id)
                .copied()?;
            Some(AssetTerms::Currency(currency_terms(code)))
        }
        AssetId::CryptoToken(id) => {
            for issuer in FIXTURE_ISSUERS {
                if let Some(code) = FIXTURE_TOKENS
                    .iter()
                    .find(|code| &token_id(issuer, code) == id)
                {
                    return Some(AssetTerms::CryptoToken(token_terms(issuer, code)));
                }
            }
            None
        }
        AssetId::Security(_) | AssetId::Commodity(_) => None,
    }
}

/// Returns every canonical asset one fixture instrument is denominated in.
fn entry_asset_ids(instrument: &Instrument) -> Vec<AssetId> {
    let mut assets: Vec<AssetId> = Vec::with_capacity(6);
    match instrument.terms() {
        InstrumentTerms::Spot(terms) => {
            assets.push(terms.base().clone());
            assets.push(terms.quote().clone());
        }
        InstrumentTerms::Perpetual(terms) => {
            if let UnderlyingRef::Asset(asset) = terms.underlying().reference() {
                assets.push(asset.clone());
            }
            assets.push(terms.price_quote().clone());
            assets.push(terms.settlement_asset().clone());
        }
        InstrumentTerms::Future(terms) => {
            if let UnderlyingRef::Asset(asset) = terms.underlying().reference() {
                assets.push(asset.clone());
            }
            assets.push(terms.price_quote().clone());
            collect_delivery_assets(terms.delivery(), &mut assets);
        }
        InstrumentTerms::Option(terms) => {
            if let UnderlyingRef::Asset(asset) = terms.underlying().reference() {
                assets.push(asset.clone());
            }
            assets.push(terms.price_quote().clone());
            assets.push(terms.strike().quote_asset().clone());
            collect_delivery_assets(terms.delivery(), &mut assets);
        }
        InstrumentTerms::Outcome(_) => {}
    }
    assets
}

/// Appends the assets one delivery hands over.
fn collect_delivery_assets(delivery: &DeliveryMode, assets: &mut Vec<AssetId>) {
    if let Some(asset) = delivery.cash_asset() {
        assets.push(asset.clone());
    }
    for leg in delivery.deliveries() {
        assets.push(leg.asset().clone());
    }
}

/// Commits one fixture's whole candidate route set.
///
/// This is the fixture form of a registration: the candidate is the complete
/// state — a route absent from it is revoked — because that is the shape a test
/// expresses ("these are the routes, and nothing else"). Production registers a
/// venue's routes through the load transaction alone.
pub fn commit_fixture_routes(
    catalog: &Catalog,
    update: CatalogRouteUpdate,
    requirements: &CatalogRouteRequirements,
) -> Result<CatalogGenerationId, CatalogError> {
    catalog.commit_fixture_routes(update, requirements)
}

/// Binds one source instance to every catalog source id a candidate set names.
pub fn bindings(
    registrations: &[CatalogRouteRegistration],
    source: Arc<dyn CatalogSource>,
) -> Vec<CatalogSourceBinding> {
    crate::catalog::source::bindings_of(registrations, |_| Arc::clone(&source))
}

/// Plans one venue's candidate route set, with `source` bound to every id it
/// names.
///
/// This is the fixture form of the planning step of a load: the candidate is
/// validated against the catalogue's own rules and nothing changes.
pub fn plan_venue_commit(
    catalog: &Catalog,
    venue: &market_participants_model::identifier::VenueId,
    registrations: Vec<CatalogRouteRegistration>,
    requirements: &CatalogRouteRequirements,
    source: Arc<dyn CatalogSource>,
) -> Result<CatalogRouteSet, CatalogError> {
    let bindings = bindings(&registrations, source);
    catalog.plan_venue_commit(venue, Some(registrations), requirements, bindings)
}

/// Publishes one venue's candidate commit through the catalogue's own write
/// path.
///
/// This is the fixture form of the commit step of a load: the registrations, the
/// staged batches and the new generation become visible together, exactly as the
/// connection action publishes them.
pub fn publish_venue_commit(
    catalog: &Catalog,
    venue: &market_participants_model::identifier::VenueId,
    registrations: Vec<CatalogRouteRegistration>,
    requirements: &CatalogRouteRequirements,
    pending: Vec<crate::catalog::batch::CatalogPending>,
    source: Arc<dyn CatalogSource>,
    visible: impl FnOnce(&Arc<crate::catalog::generation::CatalogGeneration>),
) -> Result<CatalogGenerationId, CatalogError> {
    let bindings = bindings(&registrations, source);
    catalog.commit_venue_with_visibility(
        venue,
        Some(registrations),
        requirements,
        pending,
        bindings,
        visible,
    )
}

/// A catalogue a widget test installs without a venue behind it.
///
/// Its route registration can be republished while the catalogue stays shared,
/// so a test can drive an input-revision change or a revocation without
/// rebuilding the fixture.
pub struct Fixture {
    catalog: Arc<Catalog>,
    route: VenueRoute,
    /// The listings this fixture's source publishes.
    ///
    /// A test that publishes the fixture's route itself — because it cannot await
    /// a load where it builds the fixture — takes them from here rather than
    /// keeping a second copy of the fixture's vocabulary.
    entries: Vec<CatalogEntry>,
    source_id: std::cell::RefCell<SmolStr>,
    source: std::cell::RefCell<Arc<dyn CatalogSource>>,
}

impl Fixture {
    /// Builds a catalog whose fixture spot route publishes `entries`, seeding
    /// the canonical fiat currencies the fixture assets are built from.
    pub fn new(entries: Vec<CatalogEntry>) -> Self {
        Self::with_entries(
            spot_route(),
            SPOT_SOURCE,
            listing(entries.clone()),
            &entries,
            FIXTURE_CODES,
        )
    }

    /// Builds a catalog whose fixture spot route never answers.
    pub fn unanswering() -> Self {
        Self::with_source(spot_route(), SPOT_SOURCE, pending())
    }

    /// Builds a catalog whose fixture spot route always fails.
    pub fn failing() -> Self {
        Self::with_source(spot_route(), SPOT_SOURCE, failing())
    }

    /// Builds a catalog with one registered route and one source, without
    /// seeding any canonical asset.
    pub fn with_source(route: VenueRoute, source_id: &str, source: Arc<dyn CatalogSource>) -> Self {
        Self::with_entries(route, source_id, source, &[], &[])
    }

    /// Builds a catalog whose source publishes `entries`, seeding the given
    /// canonical fiat currencies.
    pub fn with_entries(
        route: VenueRoute,
        source_id: &str,
        source: Arc<dyn CatalogSource>,
        entries: &[CatalogEntry],
        codes: &[&str],
    ) -> Self {
        Self::with_normalization(
            route,
            source_id,
            source,
            entries,
            codes,
            case_insensitive_symbols(),
        )
    }

    /// Builds a catalog whose fixture spot route publishes `entries` under
    /// `normalization`.
    ///
    /// This is how a test drives a route that declares a rule of its own — one
    /// that matches exactly, or one declared per typed identifier and product.
    pub fn with_normalization(
        route: VenueRoute,
        source_id: &str,
        source: Arc<dyn CatalogSource>,
        entries: &[CatalogEntry],
        codes: &[&str],
        normalization: Vec<IdentifierNormalizationRule>,
    ) -> Self {
        Self::with_registration(
            route,
            source_id,
            source,
            entries,
            codes,
            normalization,
            vec![CapabilityId::MarketCatalog],
        )
    }

    /// Builds a catalog from every registered input of one fixture route.
    fn with_registration(
        route: VenueRoute,
        source_id: &str,
        source: Arc<dyn CatalogSource>,
        entries: &[CatalogEntry],
        codes: &[&str],
        normalization: Vec<IdentifierNormalizationRule>,
        capabilities: Vec<CapabilityId>,
    ) -> Self {
        let catalog = Arc::new(Catalog::new());
        seed_currencies(&catalog, codes, entries);
        let source_binding =
            CatalogSourceId::new(source_id).expect("the fixture source id is valid");
        catalog
            .commit_fixture_routes(
                CatalogRouteUpdate::new(
                    [binding(source_id, Arc::clone(&source))],
                    [CatalogRouteRegistration::catalog(
                        route.clone(),
                        source_binding.clone(),
                        normalization.clone(),
                        capabilities,
                        RouteInputRevision::of_declaration(
                            &route,
                            Some(&source_binding),
                            &normalization,
                        ),
                    )],
                ),
                &requirements(std::slice::from_ref(&route)),
            )
            .expect("the fixture route set should be accepted");
        Self {
            catalog,
            route,
            entries: entries.to_vec(),
            source_id: std::cell::RefCell::new(SmolStr::new(source_id)),
            source: std::cell::RefCell::new(source),
        }
    }

    /// Returns the catalog of the fixture.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Returns the shared catalog of the fixture, for installing it as a global.
    pub fn catalog_arc(&self) -> &Arc<Catalog> {
        &self.catalog
    }

    /// Returns the route of the fixture.
    pub fn route(&self) -> &VenueRoute {
        &self.route
    }

    /// Asserts that one route's catalogue is published and answers the proof.
    ///
    /// This is the approval a caller gets from a connected market, for a test
    /// that drives a per-message path without a market participants root.
    pub fn ensured(&self, route: &VenueRoute) -> Result<EnsuredRoute, CatalogError> {
        ensured(&self.catalog, route)
    }

    /// Returns the listings this fixture's source publishes.
    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    /// Publishes the fixture route's catalog and answers the proof that it is
    /// present.
    ///
    /// A fixture is registered but not loaded when it is built, which is the state
    /// a test about "this market is not loaded yet" needs. A test about a
    /// published route drives the load the connection action drives — the one
    /// write entry a catalog has — and reads the proof a caller gets from a
    /// connected market.
    pub async fn ensure(&self) -> Result<crate::catalog::resolved::EnsuredRoute, CatalogError> {
        self.catalog.refresh_for_test(&self.route).await?;
        self.catalog.ensure(&self.route)?;
        Ok(crate::catalog::resolved::EnsuredRoute::new(
            Arc::clone(&self.catalog),
            self.route.clone(),
        ))
    }

    /// Returns the id of the published generation.
    pub fn generation(&self) -> CatalogGenerationId {
        self.catalog.generation_id()
    }

    /// Returns the registration of the fixture route.
    pub fn registered(&self) -> crate::catalog::source::RegisteredCatalogRoute {
        self.catalog()
            .registered_route(&self.route)
            .expect("the fixture route should be registered")
    }

    /// Revokes every registered route.
    pub fn revoke_all(&self) {
        self.catalog()
            .commit_fixture_routes(CatalogRouteUpdate::default(), &requirements(&[]))
            .expect("revoking every fixture route should be accepted");
    }

    /// Rebinds the fixture source to another implementation.
    pub fn republish_source(&self, source_id: &str, source: Arc<dyn CatalogSource>) {
        *self.source.borrow_mut() = source;
        *self.source_id.borrow_mut() = SmolStr::new(source_id);
        self.publish();
    }

    /// Republishes the fixture route under another declared normalization rule.
    ///
    /// This is how a test moves a route's registration to a declaration of its
    /// own: the rule is part of what the registration is keyed by, so the route's
    /// revision moves with it and a fetch staged under the old declaration belongs
    /// to a registration the route no longer carries.
    pub fn republish_declared_inputs(&self, normalization: Vec<IdentifierNormalizationRule>) {
        self.publish_inputs(normalization);
    }

    /// Republishes the fixture route with its current source and declaration.
    fn publish(&self) {
        self.publish_inputs(case_insensitive_symbols());
    }

    /// Republishes the fixture route with `normalization` as its declaration.
    fn publish_inputs(&self, normalization: Vec<IdentifierNormalizationRule>) {
        let source_id = self.source_id.borrow().clone();
        let source = Arc::clone(&self.source.borrow());
        let source_binding = CatalogSourceId::new(source_id.as_str())
            .expect("the fixture source id should be valid");
        let update = CatalogRouteUpdate::new(
            [binding(source_id.as_str(), source)],
            [CatalogRouteRegistration::catalog(
                self.route.clone(),
                source_binding.clone(),
                normalization.clone(),
                vec![CapabilityId::MarketCatalog],
                RouteInputRevision::of_declaration(
                    &self.route,
                    Some(&source_binding),
                    &normalization,
                ),
            )],
        );
        self.catalog()
            .commit_fixture_routes(update, &requirements(std::slice::from_ref(&self.route)))
            .expect("the fixture route update should be accepted");
    }

    /// Registers one more catalog route with its own source.
    pub fn register_route(&self, route: &VenueRoute, source_id: &str, entries: &[CatalogEntry]) {
        let source_id_owned = self.source_id.borrow().clone();
        let source = Arc::clone(&self.source.borrow());
        self.catalog()
            .commit_fixture_routes(
                CatalogRouteUpdate::new(
                    [
                        binding(source_id_owned.as_str(), source),
                        binding(source_id, listing(entries.to_vec())),
                    ],
                    [
                        registration(&self.route, source_id_owned.as_str()),
                        registration(route, source_id),
                    ],
                ),
                &requirements(&[self.route.clone(), route.clone()]),
            )
            .expect("the extra fixture route should be accepted");
    }

    /// Loads one route the fixture registered beside its own.
    ///
    /// A route registered through [`Self::register_route`] has nothing published
    /// until a load asks for it, exactly as it does in production.
    pub async fn load_route(&self, route: &VenueRoute) -> Arc<[CatalogEntry]> {
        self.catalog()
            .refresh_for_test(route)
            .await
            .expect("the extra fixture route should publish its catalog")
    }
}

/// Asserts that one route's catalogue is published and answers the proof.
///
/// This is the approval a caller gets from a connected market, for a test that
/// drives a per-message path over a catalogue it built itself.
pub fn ensured(catalog: &Arc<Catalog>, route: &VenueRoute) -> Result<EnsuredRoute, CatalogError> {
    catalog.ensure(route)?;
    Ok(EnsuredRoute::new(Arc::clone(catalog), route.clone()))
}

/// Seeds the canonical assets a fixture catalog publishes.
///
/// The staging view is opened on the current generation of the catalogue and is
/// committed through the same batch path a source would use, so a fixture asset
/// really does go through the registry boundary. The fiat currencies are the
/// declared ISO 4217 codes and the tokens are the venue-issued tokens of the
/// fixture issuer and its declared codes; an entry that needs anything else
/// stages it itself.
pub fn seed_currencies(catalog: &Catalog, codes: &[&str], entries: &[CatalogEntry]) {
    let publication = catalog.generation();
    let staging = crate::catalog::registry::staging::CatalogStagingHandle::new(
        crate::catalog::registry::staging::StagedRegistry::new(
            Arc::clone(publication.assets()),
            Arc::clone(publication.references()),
            Arc::clone(publication.outcomes()),
        ),
    );
    staging.with(|staging| {
        let pending = staging.pending_assets_mut();
        for code in codes {
            pending.register_currency(currency_terms(code));
        }
        for code in FIXTURE_TOKENS {
            let terms = token_terms(VENUE, code);
            let id = market_participants_model::asset::CryptoTokenId::of_terms(&terms);
            pending.crypto_tokens.insert(id, terms);
        }
    });
    let pending_assets = staging.with(|staging| staging.pending_assets().clone());
    let referenced = crate::catalog::batch::ReferencedTerms {
        assets: codes
            .iter()
            .map(|code| currency(code))
            .chain(FIXTURE_TOKENS.iter().map(|code| token(VENUE, code)))
            .chain(entries.iter().flat_map(entry_assets))
            .collect(),
        ..crate::catalog::batch::ReferencedTerms::default()
    };
    let assets =
        crate::catalog::batch::publish_assets(publication.assets(), &pending_assets, &referenced);
    catalog
        .install_registries(Arc::new(assets))
        .expect("the seeded registries publish their display catalog");
}

/// Publishes a registry carrying exactly `terms` into `catalog`, and answers the
/// generation it published.
///
/// A fixture whose assets its own vocabulary cannot recover — a chain-issued
/// token, a contract token, a security — states their terms here instead of
/// naming them by code. The registry is built and published through the same
/// staging boundary a source's batch is published through, so what a test reads
/// back is the publication that boundary produced.
pub fn install_asset_terms(
    catalog: &Catalog,
    terms: &[AssetTerms],
) -> Arc<crate::catalog::generation::CatalogGeneration> {
    let mut pending = crate::catalog::registry::asset::AssetRegistryPending::new();
    let mut referenced = crate::catalog::batch::ReferencedTerms::default();
    for term in terms {
        referenced.assets.insert(term.asset_id());
        pending.stage(term.clone());
    }
    let registry = crate::catalog::batch::publish_assets(
        &Arc::new(AssetRegistry::new()),
        &pending,
        &referenced,
    );
    catalog
        .install_registries(Arc::new(registry))
        .expect("the fixture registries publish their display catalog");
    catalog.generation()
}

/// Returns every canonical asset one fixture entry is denominated in.
pub fn entry_assets(entry: &CatalogEntry) -> Vec<AssetId> {
    let mut assets: Vec<AssetId> = Vec::with_capacity(4);
    if let InstrumentTerms::Spot(terms) = entry.instrument.terms() {
        assets.push(terms.base().clone());
        assets.push(terms.quote().clone());
    }
    assets.push(entry.snapshot.rules.common.price.quote_asset.clone());
    if let Some(asset) = entry.snapshot.rules.common.quantity.unit.asset() {
        assets.push(asset.clone());
    }
    if let Some(notional) = &entry.snapshot.rules.common.notional {
        assets.push(notional.asset.clone());
    }
    assets
}

/// The fiat codes a fixture catalog publishes by default.
pub const FIXTURE_CODES: &[&str] = &["USD", "EUR", "JPY", "GBP"];

/// The venue-issued token codes a fixture catalog publishes by default.
pub const FIXTURE_TOKENS: &[&str] = &["BTC", "ETH", "USDT", "USDC", "SOL"];

/// A listing identity rendered from a hexadecimal value.
pub fn listing_id(value: &str) -> VenueListingId {
    let hex: SmolStr = format!("{value:0>64}").chars().take(64).collect();
    VenueListingId::from_identity_value(&format!("listing-id/v1/{hex}"))
        .expect("the fixture listing id should be well formed")
}

/// A fixture timestamp.
pub fn at(millis: i64) -> UnixMillis {
    millis
}

/// The catalog error a refused fixture batch produces.
pub fn refused(route: &VenueRoute, error: CatalogValidationError) -> CatalogError {
    CatalogError::invalid_catalog(route.clone(), error)
}
/// Installs a fixture root as the global, publishing `entries` on the fixture
/// spot route.
///
/// A widget test installs this once and then drives the widget with the route,
/// the listing keys and the catalogue the fixture hands back, so no widget suite
/// builds a second fake of the directory. The catalogue is published here the way
/// the connection action publishes it, so the widget starts against a connected
/// market rather than against one nothing has loaded.
pub fn install(cx: &mut gpui_kit::App, entries: Vec<CatalogEntry>) -> Fixture {
    let fixture = Fixture::new(entries);
    crate::test_support::install_root(cx, Arc::clone(fixture.catalog_arc()));
    widget_tokio_runtime::global::handle(cx)
        .handle()
        .block_on(fixture.catalog().refresh_for_test(fixture.route()))
        .expect("the fixture route should publish its catalog");
    fixture
}

/// Installs a fixture root whose route always fails.
pub fn install_failing(cx: &mut gpui_kit::App) -> Fixture {
    let fixture = Fixture::failing();
    crate::test_support::install_root(cx, Arc::clone(fixture.catalog_arc()));
    fixture
}

/// Publishes one route's fixtures into the catalogue the installed root owns.
///
/// A widget test that publishes its own route set — its own descriptors, its own
/// fixture vocabulary — connects them the way the connection action does: the
/// registration is followed by a load of every catalog route. The batch goes
/// through the catalogue's own publication path, so a fixture snapshot is the
/// same kind of value a fetch produces, and the terms of every asset the fixture
/// names travel with it: a canonical identity is a digest of its terms, so a
/// batch that carried only identities could not be displayed. Nothing here
/// awaits, because a fixture is built from inside the test's own context, which
/// has no thread to block on.
pub fn publish_fixture_catalogs(cx: &gpui_kit::App, routes: &[(VenueRoute, Vec<CatalogEntry>)]) {
    let catalog = cx
        .global::<crate::connector::MarketParticipantsConnector>()
        .catalog();
    for (route, listings) in routes {
        publish_fixture_route(catalog, route, listings.clone())
            .expect("the fixture route should publish its listings");
    }
}

/// Publishes one fixture's listings as the route's snapshot, without a fetch.
///
/// This is the fixture form of a load: the listings travel the same validation
/// and staging as a fetched batch, so a fixture snapshot is the same kind of
/// value a connection action produces.
pub fn publish_fixture_route(
    catalog: &Catalog,
    route: &VenueRoute,
    listings: Vec<CatalogEntry>,
) -> Result<Arc<[CatalogEntry]>, CatalogError> {
    let context = catalog.fetch_context_for_test(route)?;
    let batch = fixture_batch(
        CatalogBatch {
            listings,
            ..CatalogBatch::default()
        },
        &context,
    );
    catalog.publish_batch_for_test(route, batch)
}

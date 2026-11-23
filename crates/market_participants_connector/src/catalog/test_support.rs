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

pub const VENUE: &str = "binance";

pub const SPOT_SCOPE: &str = "spot";

pub const SPOT_SOURCE: &str = "binance-spot";

pub fn currency(code: &str) -> AssetId {
    AssetId::Currency(currency_id(code))
}

pub fn currency_id(code: &str) -> CurrencyId {
    CurrencyId::new(Iso4217Code::new(code).expect("the fixture currency code should be valid"))
}

pub fn currency_terms(code: &str) -> CurrencyTerms {
    CurrencyTerms {
        code: Iso4217Code::new(code).expect("the fixture currency code should be valid"),
    }
}

pub fn token(issuer: &str, code: &str) -> AssetId {
    AssetId::CryptoToken(token_id(issuer, code))
}

pub fn token_id(issuer: &str, code: &str) -> CryptoTokenId {
    CryptoTokenId::new(venue_issued(issuer, code)).expect("the fixture token should be valid")
}

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

pub fn security(issuer: &str, designation: &str) -> AssetId {
    AssetId::Security(security_id(issuer, designation))
}

pub fn security_id(issuer: &str, designation: &str) -> SecurityId {
    let terms = security_terms(issuer, designation);
    SecurityId::new(terms.issuer, terms.share_class, terms.form)
}

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

pub fn route(venue: &str, scope: &str) -> VenueRoute {
    VenueRoute::new(
        VenueId::from_name(venue).expect("the fixture venue should be valid"),
        VenueScopeId::new(scope).expect("the fixture scope should be valid"),
    )
}

pub fn spot_route() -> VenueRoute {
    route(VENUE, SPOT_SCOPE)
}

pub fn perp_route() -> VenueRoute {
    route(VENUE, "perpetual")
}

pub fn spot(base: AssetId, quote: AssetId) -> Instrument {
    Instrument::new(InstrumentTerms::Spot(
        SpotTerms::new(base, quote).expect("the fixture spot terms should be valid"),
    ))
    .expect("the fixture instrument should be valid")
}

pub fn spot_token(issuer: &str, base: &str, quote: &str) -> Instrument {
    spot(token(issuer, base), token(issuer, quote))
}

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

pub fn listing_key(route: &VenueRoute, symbol: &str) -> VenueListingKey {
    continuity_key(route, symbol, BinanceProduct::Spot)
}

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

pub fn spot_assets(instrument: &Instrument) -> (AssetId, AssetId) {
    match instrument.terms() {
        InstrumentTerms::Spot(terms) => (terms.quote().clone(), terms.base().clone()),
        _ => (currency("USD"), currency("USD")),
    }
}

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

pub fn with_status(mut entry: CatalogEntry, status: ListingStatus) -> CatalogEntry {
    entry.snapshot.status = status;
    entry
}

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

pub fn requirements(routes: &[VenueRoute]) -> CatalogRouteRequirements {
    CatalogRouteRequirements::new(
        routes
            .iter()
            .cloned()
            .map(|route| (route, CapabilityId::MarketCatalog)),
    )
}

pub fn non_catalog_requirements(routes: &[VenueRoute]) -> CatalogRouteRequirements {
    CatalogRouteRequirements::new(
        routes
            .iter()
            .cloned()
            .map(|route| (route, CapabilityId::Tickers)),
    )
}

pub fn binding(id: &str, source: Arc<dyn CatalogSource>) -> CatalogSourceBinding {
    CatalogSourceBinding::new(
        CatalogSourceId::new(id).expect("the fixture source id should be valid"),
        source,
    )
}

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

pub fn non_catalog_registration(route: &VenueRoute) -> CatalogRouteRegistration {
    let normalization = case_insensitive_symbols();
    CatalogRouteRegistration::without_catalog(
        route.clone(),
        normalization.clone(),
        vec![CapabilityId::Tickers],
        RouteInputRevision::of_declaration(route, None, &normalization),
    )
}

pub fn listing(entries: Vec<CatalogEntry>) -> Arc<dyn CatalogSource> {
    Arc::new(Listed(entries))
}

pub fn raw(entries: Vec<CatalogEntry>) -> Arc<dyn CatalogSource> {
    Arc::new(Raw(entries))
}

pub fn pending() -> Arc<dyn CatalogSource> {
    Arc::new(Unanswered)
}

pub fn failing() -> Arc<dyn CatalogSource> {
    Arc::new(Failed)
}

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

pub fn batch(entries: Vec<CatalogEntry>) -> CatalogBatch {
    CatalogBatch {
        listings: entries,
        ..CatalogBatch::default()
    }
}

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
    pub async fn started(&self) {
        let permit = self
            .started
            .acquire()
            .await
            .expect("the fixture gate is never closed");
        permit.forget();
    }

    pub fn release(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::Release);
    }

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
        let mut batch = batch(entries);
        let published = Arc::clone(context.staging().base().assets());
        stage_fixture_terms(&mut batch, context.staging(), &published);
        Box::pin(async move { Ok(batch) })
    }
}

pub fn fixture_batch(mut batch: CatalogBatch, context: &CatalogFetchContext) -> CatalogBatch {
    let published = Arc::clone(context.staging().base().assets());
    stage_fixture_terms(&mut batch, context.staging(), &published);
    batch
}

pub fn stage_fixture_terms(
    batch: &mut CatalogBatch,
    staging: &CatalogStagingHandle,
    published: &AssetRegistry,
) {
    let mut terms: Vec<AssetTerms> = Vec::with_capacity(batch.listings.len() * 2 + 2);
    for stated in &batch.asset_terms {
        terms_push(&mut terms, stated.clone());
    }
    for entry in &batch.listings {
        for candidate in fixture_asset_terms(&entry.instrument) {
            terms_push(&mut terms, candidate);
        }
    }
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

pub fn fixture_asset_terms(instrument: &Instrument) -> Vec<AssetTerms> {
    let mut terms: Vec<AssetTerms> = Vec::with_capacity(4);
    for asset in entry_asset_ids(instrument) {
        if let Some(candidate) = fixture_asset_term(&asset) {
            terms_push(&mut terms, candidate);
        }
    }
    terms
}

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

fn terms_push(terms: &mut Vec<AssetTerms>, candidate: AssetTerms) {
    if !terms
        .iter()
        .any(|known| known.asset_id() == candidate.asset_id())
    {
        terms.push(candidate);
    }
}

pub const FIXTURE_ISSUERS: &[&str] = &[VENUE, "hyperliquid"];

fn fixture_asset_term(asset: &AssetId) -> Option<AssetTerms> {
    match asset {
        AssetId::Currency(id) => {
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

fn collect_delivery_assets(delivery: &DeliveryMode, assets: &mut Vec<AssetId>) {
    if let Some(asset) = delivery.cash_asset() {
        assets.push(asset.clone());
    }
    for leg in delivery.deliveries() {
        assets.push(leg.asset().clone());
    }
}

pub fn commit_fixture_routes(
    catalog: &Catalog,
    update: CatalogRouteUpdate,
    requirements: &CatalogRouteRequirements,
) -> Result<CatalogGenerationId, CatalogError> {
    catalog.commit_fixture_routes(update, requirements)
}

pub fn bindings(
    registrations: &[CatalogRouteRegistration],
    source: Arc<dyn CatalogSource>,
) -> Vec<CatalogSourceBinding> {
    crate::catalog::source::bindings_of(registrations, |_| Arc::clone(&source))
}

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

pub struct Fixture {
    catalog: Arc<Catalog>,
    route: VenueRoute,
    entries: Vec<CatalogEntry>,
    source_id: std::cell::RefCell<SmolStr>,
    source: std::cell::RefCell<Arc<dyn CatalogSource>>,
}

impl Fixture {
    pub fn new(entries: Vec<CatalogEntry>) -> Self {
        Self::with_entries(
            spot_route(),
            SPOT_SOURCE,
            listing(entries.clone()),
            &entries,
            FIXTURE_CODES,
        )
    }

    pub fn unanswering() -> Self {
        Self::with_source(spot_route(), SPOT_SOURCE, pending())
    }

    pub fn failing() -> Self {
        Self::with_source(spot_route(), SPOT_SOURCE, failing())
    }

    pub fn with_source(route: VenueRoute, source_id: &str, source: Arc<dyn CatalogSource>) -> Self {
        Self::with_entries(route, source_id, source, &[], &[])
    }

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

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn catalog_arc(&self) -> &Arc<Catalog> {
        &self.catalog
    }

    pub fn route(&self) -> &VenueRoute {
        &self.route
    }

    pub fn ensured(&self, route: &VenueRoute) -> Result<EnsuredRoute, CatalogError> {
        ensured(&self.catalog, route)
    }

    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    pub async fn ensure(&self) -> Result<crate::catalog::resolved::EnsuredRoute, CatalogError> {
        self.catalog.refresh_for_test(&self.route).await?;
        self.catalog.ensure(&self.route)?;
        Ok(crate::catalog::resolved::EnsuredRoute::new(
            Arc::clone(&self.catalog),
            self.route.clone(),
        ))
    }

    pub fn generation(&self) -> CatalogGenerationId {
        self.catalog.generation_id()
    }

    pub fn registered(&self) -> crate::catalog::source::RegisteredCatalogRoute {
        self.catalog()
            .registered_route(&self.route)
            .expect("the fixture route should be registered")
    }

    pub fn revoke_all(&self) {
        self.catalog()
            .commit_fixture_routes(CatalogRouteUpdate::default(), &requirements(&[]))
            .expect("revoking every fixture route should be accepted");
    }

    pub fn republish_source(&self, source_id: &str, source: Arc<dyn CatalogSource>) {
        *self.source.borrow_mut() = source;
        *self.source_id.borrow_mut() = SmolStr::new(source_id);
        self.publish();
    }

    pub fn republish_declared_inputs(&self, normalization: Vec<IdentifierNormalizationRule>) {
        self.publish_inputs(normalization);
    }

    fn publish(&self) {
        self.publish_inputs(case_insensitive_symbols());
    }

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

    pub async fn load_route(&self, route: &VenueRoute) -> Arc<[CatalogEntry]> {
        self.catalog()
            .refresh_for_test(route)
            .await
            .expect("the extra fixture route should publish its catalog")
    }
}

pub fn ensured(catalog: &Arc<Catalog>, route: &VenueRoute) -> Result<EnsuredRoute, CatalogError> {
    catalog.ensure(route)?;
    Ok(EnsuredRoute::new(Arc::clone(catalog), route.clone()))
}

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

pub const FIXTURE_CODES: &[&str] = &["USD", "EUR", "JPY", "GBP"];

pub const FIXTURE_TOKENS: &[&str] = &["BTC", "ETH", "USDT", "USDC", "SOL"];

pub fn listing_id(value: &str) -> VenueListingId {
    let hex: SmolStr = format!("{value:0>64}").chars().take(64).collect();
    VenueListingId::from_identity_value(&format!("listing-id/v1/{hex}"))
        .expect("the fixture listing id should be well formed")
}

pub fn at(millis: i64) -> UnixMillis {
    millis
}

pub fn refused(route: &VenueRoute, error: CatalogValidationError) -> CatalogError {
    CatalogError::invalid_catalog(route.clone(), error)
}
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

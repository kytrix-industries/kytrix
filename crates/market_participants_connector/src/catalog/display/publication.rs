//! The one publication pass that turns surviving listings and registries into a
//! [`DisplayCatalog`].
//!
//! Everything catalogue-global is decided here exactly once per publication: the
//! code of every canonical asset, the name of every instrument and the name of
//! every listing, together with the one invariant that ties a name to the details
//! it was rendered from. A publication that cannot make one of those decisions is
//! refused as a whole, and the generation that was published before it stays
//! exactly as it was.
//!
//! The pass reads exactly the material the new generation will carry — the
//! surviving route snapshots, the codes they staged and the asset, reference and
//! outcome registries — so the catalog can never describe one publication while
//! the registries describe another.

use std::sync::Arc;

use ahash::{AHashMap, AHashSet};
use market_participants_model::{
    asset::{AssetId, IndexId, UnderlyingRef},
    display::{
        asset::{AssetDisplayCode, AssetDisplayTerms},
        details::InstrumentDisplayDetails,
        grammar::format_instrument,
        values::InstrumentDisplayString,
    },
    instrument::{Instrument, InstrumentTerms},
    listing::{
        listing_impl::VenueListing,
        route::{VenueListingKey, VenueRoute},
    },
    outcome::{OutcomeGroup, OutcomeGroupId},
    reference::ExternalReferenceId,
    settlement::{DeliveryMode, InstrumentId},
};
use smol_str::SmolStr;

use crate::catalog::{
    batch::CatalogEntry,
    display::{catalog::DisplayCatalog, types::DisplayError},
    generation::RouteSnapshot,
    registry::{asset::AssetRegistry, outcome::OutcomeRegistry, reference::ReferenceRegistry},
};

/// The verified display code one route staged for one canonical asset.
///
/// The staging key is the canonical asset itself, so one asset states one code
/// whatever route staged it: a contract token has no code of its own, and the
/// ticker of a security is a listing fact rather than a canonical term.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedAssetCode {
    /// The canonical asset the code was verified for.
    pub asset: AssetId,
    /// The verified display code of that asset.
    pub code: AssetDisplayCode,
}

/// The display material one route staged together with its listings.
///
/// The material is kept with the route snapshot rather than with the batch, so a
/// later publication that only revokes another route — or only replaces the
/// canonical registries — rebuilds the same display from the same facts instead
/// of losing them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteDisplayMaterial {
    /// The display code of every asset whose own terms state none.
    pub asset_codes: Arc<[StagedAssetCode]>,
}

/// Every staged code of the surviving routes, indexed by canonical asset.
#[derive(Clone, Debug, Default)]
struct StagedCodes {
    entries: AHashMap<AssetId, AssetDisplayCode>,
}

impl StagedCodes {
    /// Returns the code one route staged for an asset.
    fn get(&self, asset: &AssetId) -> Option<&AssetDisplayCode> {
        self.entries.get(asset)
    }
}

/// Indexes the staged codes of every surviving route by canonical asset.
///
/// Every entry belongs to a route the generation still holds — a revoked route
/// leaves no snapshot, so its staged codes are dropped with it — and one asset
/// states exactly one code, so two routes that disagree refuse the batch instead
/// of being resolved by iteration order.
fn index_staged_codes(
    snapshots: &AHashMap<VenueRoute, Arc<RouteSnapshot>>,
) -> Result<StagedCodes, DisplayError> {
    let mut entries: AHashMap<AssetId, AssetDisplayCode> = AHashMap::new();
    for snapshot in snapshots.values() {
        for staged in snapshot.display_material().asset_codes.iter() {
            match entries.get(&staged.asset) {
                Some(existing) if existing != &staged.code => {
                    return Err(DisplayError::StagedCodeMismatch {
                        asset: Box::new(staged.asset.clone()),
                    });
                }
                _ => {
                    entries.insert(staged.asset.clone(), staged.code.clone());
                }
            }
        }
    }
    Ok(StagedCodes { entries })
}

/// Everything the publication has to look up in the registries.
#[derive(Default)]
struct RequestedTerms {
    /// Every asset the catalog names: the ones a listing states and every
    /// registered one.
    assets: AHashSet<AssetId>,
    /// The assets a published name renders, which must have a code.
    ///
    /// A contract unit, a strike quote and a delivery leg are deliberately not
    /// part of it: no name states them, so an asset only they mention may stay
    /// without a code.
    named_assets: AHashSet<AssetId>,
    /// Every index the catalog names.
    indices: AHashSet<IndexId>,
    /// Every external reference the catalog names.
    external_references: AHashSet<ExternalReferenceId>,
    /// Every outcome group an outcome listing names.
    outcomes: AHashSet<OutcomeGroupId>,
}

impl RequestedTerms {
    /// Collects every registry identity the publication must resolve.
    ///
    /// Only the positions a name renders are collected as *named*: the base and
    /// quote of a spot pair, the underlying of a derivative, the asset a contract
    /// settles in cash and the settlement asset of an outcome group. Everything
    /// else a listing states is a listing fact and never a name.
    fn collect(
        candidates: &[&CatalogEntry],
        outcomes: &OutcomeRegistry,
    ) -> Result<Self, DisplayError> {
        let mut requested = Self::default();
        for candidate in candidates {
            requested.collect_instrument(&candidate.instrument, outcomes)?;
        }
        Ok(requested)
    }

    fn collect_instrument(
        &mut self,
        instrument: &Instrument,
        outcomes: &OutcomeRegistry,
    ) -> Result<(), DisplayError> {
        match instrument.terms() {
            InstrumentTerms::Spot(terms) => {
                self.named_asset(terms.base());
                self.named_asset(terms.quote());
            }
            InstrumentTerms::Perpetual(terms) => {
                self.named_underlying(terms.underlying().reference());
                self.named_asset(terms.price_quote());
                self.named_asset(terms.settlement_asset());
            }
            InstrumentTerms::Future(terms) => {
                self.named_underlying(terms.underlying().reference());
                self.named_asset(terms.price_quote());
                if let Some(asset) = terms.delivery().cash_asset() {
                    self.named_asset(asset);
                }
            }
            InstrumentTerms::Option(terms) => {
                self.named_underlying(terms.underlying().reference());
                self.named_asset(terms.price_quote());
                if let Some(asset) = terms.delivery().cash_asset() {
                    self.named_asset(asset);
                }
            }
            InstrumentTerms::Outcome(terms) => {
                self.outcomes.insert(*terms.group());
                // An outcome instrument names no asset of its own: the group's
                // settlement asset is the one its name is denominated in, so it is
                // the asset this publication resolves.
                let group = outcome_group(outcomes, terms.group())?;
                self.named_asset(&group.settlement_asset);
            }
        }
        Ok(())
    }

    fn named_underlying(&mut self, underlying: &UnderlyingRef) {
        match underlying {
            UnderlyingRef::Asset(asset) => self.named_asset(asset),
            UnderlyingRef::Index(index) => {
                self.indices.insert(*index);
            }
            UnderlyingRef::External(reference) => {
                self.external_references.insert(*reference);
            }
        }
    }

    fn named_asset(&mut self, asset: &AssetId) {
        self.named_assets.insert(asset.clone());
        self.assets.insert(asset.clone());
    }
}

/// Adds the registry identities no listing of this publication references.
///
/// The catalog names one asset, index and external reference per *registered*
/// identity, not one per identity a surviving listing happens to state, because a
/// reader that names an asset no listing of its generation references — a wallet
/// balance, a fee asset, a collateral asset — has to find that code in the same
/// catalog every other name comes from. The identity is added without being named:
/// an asset nothing renders occupies no position of a name, so an asset whose own
/// terms state no code and which no route staged may stay without one.
fn collect_registered_terms(
    requested: &mut RequestedTerms,
    assets: &AssetRegistry,
    references: &ReferenceRegistry,
    staged: &StagedCodes,
) {
    for asset in assets.assets() {
        // A security states no code of its own, so a registered one is named only
        // when a route staged a code for it: without one the resolution below could
        // only ever skip it, and an asset no name renders stays without a code
        // either way.
        if matches!(asset, AssetId::Security(_)) && staged.get(&asset).is_none() {
            continue;
        }
        requested.assets.insert(asset);
    }
    for (index, _) in references.indices() {
        requested.indices.insert(*index);
    }
    for (reference, _) in references.external_references() {
        requested.external_references.insert(*reference);
    }
}

/// The display material of one publication, before any name is assembled.
pub struct PublicationInput<'a> {
    /// The route snapshots the new generation will carry.
    pub snapshots: &'a AHashMap<VenueRoute, Arc<RouteSnapshot>>,
    /// The asset registry the new generation will carry.
    pub assets: &'a AssetRegistry,
    /// The reference registry the new generation will carry.
    pub references: &'a ReferenceRegistry,
    /// The outcome registry the new generation will carry.
    pub outcomes: &'a OutcomeRegistry,
}

/// The resolved display codes every instrument component is assembled from.
struct NameLinks<'a> {
    assets: &'a AHashMap<AssetId, AssetDisplayCode>,
    indices: &'a AHashMap<IndexId, AssetDisplayCode>,
    external_references: &'a AHashMap<ExternalReferenceId, AssetDisplayCode>,
}

impl NameLinks<'_> {
    /// Returns the display code of one canonical asset.
    fn asset_code(&self, asset: &AssetId) -> Result<AssetDisplayCode, DisplayError> {
        self.assets
            .get(asset)
            .cloned()
            .ok_or_else(|| DisplayError::AssetDisplayCodeMissing {
                asset: Box::new(asset.clone()),
            })
    }

    /// Returns the display code of one canonical index.
    fn index_code(&self, index: &IndexId) -> Result<AssetDisplayCode, DisplayError> {
        self.indices
            .get(index)
            .cloned()
            .ok_or_else(|| DisplayError::MissingIndexTerms {
                index: Box::new(*index),
            })
    }

    /// Returns the display code of one external reference.
    fn external_reference_code(
        &self,
        reference: &ExternalReferenceId,
    ) -> Result<AssetDisplayCode, DisplayError> {
        self.external_references
            .get(reference)
            .cloned()
            .ok_or_else(|| DisplayError::MissingExternalReferenceTerms {
                reference: Box::new(*reference),
            })
    }
}

/// Builds the display catalog of one publication.
///
/// A revocation drops the routes it removes and rebuilds the catalog from the
/// survivors with the same rules, so the display of a surviving listing never
/// changes because another route left, and a registry-only publication rebuilds
/// against the registries it installs.
pub fn build_display_catalog(input: PublicationInput<'_>) -> Result<DisplayCatalog, DisplayError> {
    let PublicationInput {
        snapshots,
        assets,
        references,
        outcomes,
    } = input;
    let staged = index_staged_codes(snapshots)?;
    let candidates = collect_candidates(snapshots);
    let mut requested = RequestedTerms::collect(&candidates, outcomes)?;
    collect_registered_terms(&mut requested, assets, references, &staged);
    let asset_codes = resolve_asset_codes(&requested, assets, &staged)?;
    let index_codes = resolve_index_codes(&requested.indices, references)?;
    let external_reference_codes =
        resolve_external_reference_codes(&requested.external_references, references)?;
    let links = NameLinks {
        assets: &asset_codes,
        indices: &index_codes,
        external_references: &external_reference_codes,
    };
    let (instrument_names, instrument_details) =
        resolve_instruments(&candidates, &links, outcomes)?;
    verify_instrument_equality(&instrument_names, &instrument_details)?;
    let (listings, listing_search_aliases) = resolve_listings(&candidates, &instrument_names)?;
    Ok(DisplayCatalog::from_parts(
        asset_codes,
        index_codes,
        external_reference_codes,
        instrument_names,
        instrument_details,
        listings,
        listing_search_aliases,
    ))
}

/// Every surviving listing of the generation, in canonical route/key order.
fn collect_candidates(snapshots: &AHashMap<VenueRoute, Arc<RouteSnapshot>>) -> Vec<&CatalogEntry> {
    let mut routes: Vec<&VenueRoute> = snapshots.keys().collect();
    routes.sort();
    let mut candidates: Vec<&CatalogEntry> = Vec::new();
    for route in routes {
        candidates.extend(snapshots[route].entries().iter());
    }
    candidates
}

/// Resolves the display code of every canonical asset the catalog names.
///
/// The code of an asset is a pure function of its own terms or of the value a
/// route staged for it, so no decision is taken here beyond the one refusal the
/// contract states: an asset a published name renders must have a code, and an
/// asset that is only registered may stay without one.
fn resolve_asset_codes(
    requested: &RequestedTerms,
    assets: &AssetRegistry,
    staged: &StagedCodes,
) -> Result<AHashMap<AssetId, AssetDisplayCode>, DisplayError> {
    let mut codes: AHashMap<AssetId, AssetDisplayCode> =
        AHashMap::with_capacity(requested.assets.len());
    for asset in &requested.assets {
        let code = match (staged.get(asset), term_code(asset, assets)?) {
            // One asset states exactly one code, whatever stated it.
            (Some(staged), Some(term)) if staged != &term => {
                return Err(DisplayError::StagedCodeMismatch {
                    asset: Box::new(asset.clone()),
                });
            }
            (Some(staged), _) => staged.clone(),
            (None, Some(term)) => term,
            (None, None) => {
                if requested.named_assets.contains(asset) {
                    return Err(DisplayError::AssetDisplayCodeMissing {
                        asset: Box::new(asset.clone()),
                    });
                }
                continue;
            }
        };
        codes.insert(asset.clone(), code);
    }
    Ok(codes)
}

/// Returns the code the canonical terms of one asset state on their own.
///
/// A currency, a token and a commodity state theirs; a contract token keeps its
/// chain and its address and states none, and a security states none at all —
/// its ticker is the code a route stages — so both are named by the staging of
/// the route that publishes them.
fn term_code(
    asset: &AssetId,
    assets: &AssetRegistry,
) -> Result<Option<AssetDisplayCode>, DisplayError> {
    let missing = || DisplayError::MissingAssetTerms {
        asset: Box::new(asset.clone()),
    };
    let terms = match asset {
        AssetId::Currency(id) => {
            AssetDisplayTerms::Currency(assets.currency(id).ok_or_else(missing)?)
        }
        AssetId::CryptoToken(id) => {
            AssetDisplayTerms::CryptoToken(assets.crypto_token(id).ok_or_else(missing)?)
        }
        AssetId::Commodity(id) => {
            AssetDisplayTerms::Commodity(assets.commodity(id).ok_or_else(missing)?)
        }
        // The issuer, designation and form of a security are not a name and state
        // no code, so no registered term is read for one.
        AssetId::Security(_) => return Ok(None),
    };
    AssetDisplayCode::from_terms(terms).map_err(|error| DisplayError::Format { error })
}

/// Resolves the display code of every index the catalog names.
fn resolve_index_codes(
    requested: &AHashSet<IndexId>,
    references: &ReferenceRegistry,
) -> Result<AHashMap<IndexId, AssetDisplayCode>, DisplayError> {
    let mut codes: AHashMap<IndexId, AssetDisplayCode> = AHashMap::with_capacity(requested.len());
    for index in requested {
        let missing = || DisplayError::MissingIndexTerms {
            index: Box::new(*index),
        };
        let terms = references.index(index).ok_or_else(missing)?;
        let code = AssetDisplayCode::from_terms(AssetDisplayTerms::Index(terms))
            .map_err(|error| DisplayError::Format { error })?
            .ok_or_else(missing)?;
        codes.insert(*index, code);
    }
    Ok(codes)
}

/// Resolves the display code of every external reference the catalog names.
fn resolve_external_reference_codes(
    requested: &AHashSet<ExternalReferenceId>,
    references: &ReferenceRegistry,
) -> Result<AHashMap<ExternalReferenceId, AssetDisplayCode>, DisplayError> {
    let mut codes: AHashMap<ExternalReferenceId, AssetDisplayCode> =
        AHashMap::with_capacity(requested.len());
    for reference in requested {
        let missing = || DisplayError::MissingExternalReferenceTerms {
            reference: Box::new(*reference),
        };
        let terms = references
            .external_reference(reference)
            .ok_or_else(missing)?;
        let code = AssetDisplayCode::from_terms(AssetDisplayTerms::External(terms))
            .map_err(|error| DisplayError::Format { error })?
            .ok_or_else(missing)?;
        codes.insert(*reference, code);
    }
    Ok(codes)
}

/// Resolves the economic name and the details of every published instrument.
fn resolve_instruments(
    candidates: &[&CatalogEntry],
    links: &NameLinks<'_>,
    outcomes: &OutcomeRegistry,
) -> Result<
    (
        AHashMap<InstrumentId, InstrumentDisplayString>,
        AHashMap<InstrumentId, InstrumentDisplayDetails>,
    ),
    DisplayError,
> {
    let mut names: AHashMap<InstrumentId, InstrumentDisplayString> =
        AHashMap::with_capacity(candidates.len());
    let mut details: AHashMap<InstrumentId, InstrumentDisplayDetails> =
        AHashMap::with_capacity(candidates.len());
    for candidate in candidates {
        let instrument = &candidate.instrument;
        if names.contains_key(instrument.id()) {
            continue;
        }
        let rendered = instrument_details(instrument, links, outcomes)?;
        let name = format_instrument(&rendered).map_err(|error| DisplayError::Format { error })?;
        names.insert(*instrument.id(), name);
        details.insert(*instrument.id(), rendered);
    }
    Ok((names, details))
}

/// Renders the typed display details of one instrument.
///
/// This is the one place the identifying fields of an instrument become the
/// details the grammar renders, so a field a name does not state is never read
/// here and a field a name states is never filled in twice.
fn instrument_details(
    instrument: &Instrument,
    links: &NameLinks<'_>,
    outcomes: &OutcomeRegistry,
) -> Result<InstrumentDisplayDetails, DisplayError> {
    Ok(match instrument.terms() {
        InstrumentTerms::Spot(terms) => InstrumentDisplayDetails::Spot {
            base: links.asset_code(terms.base())?,
            quote: links.asset_code(terms.quote())?,
        },
        InstrumentTerms::Perpetual(terms) => InstrumentDisplayDetails::Perpetual {
            underlying: underlying_code(terms.underlying().reference(), links)?,
            price_quote: links.asset_code(terms.price_quote())?,
            settlement: links.asset_code(terms.settlement_asset())?,
        },
        InstrumentTerms::Future(terms) => InstrumentDisplayDetails::Future {
            underlying: underlying_code(terms.underlying().reference(), links)?,
            price_quote: links.asset_code(terms.price_quote())?,
            settlement: cash_settlement_code(terms.delivery(), links)?,
            expiration_at: terms.expiry().expiration_at(),
        },
        InstrumentTerms::Option(terms) => InstrumentDisplayDetails::Option {
            underlying: underlying_code(terms.underlying().reference(), links)?,
            price_quote: links.asset_code(terms.price_quote())?,
            settlement: cash_settlement_code(terms.delivery(), links)?,
            right: terms.right(),
            strike: terms.strike().value(),
            expiration_at: terms.expiry().expiration_at(),
        },
        InstrumentTerms::Outcome(terms) => {
            let group = outcome_group(outcomes, terms.group())?;
            let (member, side) = outcome_leg(group, terms.member(), terms.side())?;
            InstrumentDisplayDetails::Outcome {
                question: group.question.clone(),
                member: member.clone(),
                side: side.clone(),
                settlement: links.asset_code(&group.settlement_asset)?,
                expiration_at: group.expiry.expiration_at(),
            }
        }
    })
}

/// Returns the display code of the settlement of a dated contract.
///
/// Only a cash delivery states one: a contract that hands over real assets states
/// no settlement asset, and its name renders the `PHYS` value of that position.
/// The settlement asset is deliberately not inferred from the price quote — a
/// physical contract has none, and the model's own settlement accessor answers
/// with the cash asset alone.
fn cash_settlement_code(
    delivery: &DeliveryMode,
    links: &NameLinks<'_>,
) -> Result<Option<AssetDisplayCode>, DisplayError> {
    delivery
        .cash_asset()
        .map(|asset| links.asset_code(asset))
        .transpose()
}

/// Renders the underlying of one derivative.
fn underlying_code(
    underlying: &UnderlyingRef,
    links: &NameLinks<'_>,
) -> Result<AssetDisplayCode, DisplayError> {
    match underlying {
        UnderlyingRef::Asset(asset) => links.asset_code(asset),
        UnderlyingRef::Index(index) => links.index_code(index),
        UnderlyingRef::External(reference) => links.external_reference_code(reference),
    }
}

/// The outcome group one outcome listing is displayed from.
fn outcome_group<'a>(
    outcomes: &'a OutcomeRegistry,
    group: &OutcomeGroupId,
) -> Result<&'a OutcomeGroup, DisplayError> {
    outcomes
        .group(group)
        .ok_or(DisplayError::MissingOutcomeTerms { group: *group })
}

/// Returns the registry label of the member and side one outcome listing trades.
fn outcome_leg<'a>(
    group: &'a OutcomeGroup,
    member: &market_participants_model::outcome::OutcomeMemberId,
    side: &market_participants_model::outcome::OutcomeSideId,
) -> Result<(&'a SmolStr, &'a SmolStr), DisplayError> {
    let member_label = group
        .member(member)
        .map(|found| &found.label)
        .ok_or(DisplayError::MissingOutcomeTerms { group: group.id })?;
    let side_label = group
        .side(side)
        .map(|found| &found.label)
        .ok_or(DisplayError::MissingOutcomeTerms { group: group.id })?;
    Ok((member_label, side_label))
}

/// Requires that every published name is the rendering of its own details.
///
/// The publication gate asserts the invariant: for every instrument, formatting
/// the published details again must produce the published name. A generation that
/// fails it has two renderings of one instrument and is refused rather than
/// published.
fn verify_instrument_equality(
    names: &AHashMap<InstrumentId, InstrumentDisplayString>,
    details: &AHashMap<InstrumentId, InstrumentDisplayDetails>,
) -> Result<(), DisplayError> {
    for (id, rendered) in details {
        let formatted =
            format_instrument(rendered).map_err(|error| DisplayError::Format { error })?;
        if names.get(id) != Some(&formatted) {
            return Err(DisplayError::InstrumentNameMismatch {
                instrument: Box::new(*id),
            });
        }
    }
    Ok(())
}

/// Names every surviving listing after its own instrument.
///
/// A listing never re-renders an economic name: it copies the one the catalog
/// published for its instrument, which is what makes the two unable to drift.
/// The search aliases are the listing's own native symbols, uppercased and
/// deduplicated, because a reader that searches by the spelling a venue publishes
/// is searching for a listing and not for an economic name.
fn resolve_listings(
    candidates: &[&CatalogEntry],
    names: &AHashMap<InstrumentId, InstrumentDisplayString>,
) -> Result<
    (
        AHashMap<VenueListingKey, InstrumentDisplayString>,
        AHashMap<VenueListingKey, Arc<[SmolStr]>>,
    ),
    DisplayError,
> {
    let mut listings: AHashMap<VenueListingKey, InstrumentDisplayString> =
        AHashMap::with_capacity(candidates.len());
    let mut aliases: AHashMap<VenueListingKey, Arc<[SmolStr]>> =
        AHashMap::with_capacity(candidates.len());
    for candidate in candidates {
        let listing = &candidate.listing;
        let name = names.get(&listing.instrument_id).cloned().ok_or_else(|| {
            DisplayError::InstrumentNameMismatch {
                instrument: Box::new(listing.instrument_id),
            }
        })?;
        listings.insert(listing.key.clone(), name);
        aliases.insert(listing.key.clone(), search_aliases(listing));
    }
    Ok((listings, aliases))
}

/// Returns the normalized search aliases of one listing.
fn search_aliases(listing: &VenueListing) -> Arc<[SmolStr]> {
    let mut aliases: Vec<SmolStr> = listing
        .native_symbol
        .iter()
        .chain(listing.aliases.iter())
        .map(|symbol| SmolStr::new(symbol.as_str().to_ascii_uppercase()))
        .collect();
    aliases.sort();
    aliases.dedup();
    aliases.into()
}

#[cfg(test)]
mod tests {
    //! The publication pass's own guards.
    //!
    //! Each guard refuses a state the pass must never publish: a name that is not the
    //! rendering of its details, a listing whose instrument the pass did not name, and
    //! an outcome group no registry carries. The pass is driven end to end by
    //! `display_catalog_tests`, so what these pin is the refusal itself — the answer a
    //! caller gets when an invariant is broken.

    use market_participants_model::{
        display::{
            asset::AssetDisplayCode, details::InstrumentDisplayDetails, grammar::format_instrument,
            values::InstrumentDisplayString,
        },
        instrument::{Instrument, InstrumentTerms, SpotTerms},
        outcome::OutcomeGroupId,
        settlement::InstrumentId,
    };
    use smol_str::format_smolstr;

    use super::{
        AHashMap, AssetDisplayTerms, DisplayError, OutcomeRegistry, outcome_group,
        resolve_listings, verify_instrument_equality,
    };
    use crate::catalog::test_support;

    /// One canonical identity of a fixture instrument.
    fn instrument_id(seed: char) -> InstrumentId {
        InstrumentId::from_identity_value(&format_smolstr!("v1/{}", seed.to_string().repeat(64)))
            .expect("the fixture identity is canonical")
    }

    /// One fixture spot instrument of two venue tokens.
    fn spot_instrument(base: &str, quote: &str) -> Instrument {
        Instrument::new(InstrumentTerms::Spot(
            SpotTerms::new(
                test_support::token(test_support::VENUE, base),
                test_support::token(test_support::VENUE, quote),
            )
            .expect("the fixture spot terms are valid"),
        ))
        .expect("the fixture spot instrument is valid")
    }

    /// The display details of one fixture spot instrument.
    fn spot_details(base: &str, quote: &str) -> InstrumentDisplayDetails {
        InstrumentDisplayDetails::Spot {
            base: token_code(base),
            quote: token_code(quote),
        }
    }

    #[test]
    fn a_name_that_is_not_its_details_rendering_is_refused() {
        let id = instrument_id('a');
        let mut names: AHashMap<InstrumentId, InstrumentDisplayString> = AHashMap::new();
        names.insert(
            id,
            format_instrument(&spot_details("BTC", "USD")).expect("the other rendering is a name"),
        );
        let mut published: AHashMap<InstrumentId, InstrumentDisplayDetails> = AHashMap::new();
        published.insert(id, spot_details("BTC", "USDT"));

        let error = verify_instrument_equality(&names, &published)
            .expect_err("a name that is not its details' rendering is refused");

        assert!(
            matches!(
                error,
                DisplayError::InstrumentNameMismatch { instrument } if instrument.as_ref() == &id
            ),
            "the invariant names the instrument it refused"
        );
    }

    #[test]
    fn a_listing_whose_instrument_the_pass_did_not_name_is_refused() {
        let route = test_support::route(test_support::VENUE, "spot");
        let entry = test_support::entry(&route, spot_instrument("BTC", "USDT"), "BTCUSDT");
        let candidates = [&entry];
        let names: AHashMap<InstrumentId, InstrumentDisplayString> = AHashMap::new();

        let error = resolve_listings(&candidates, &names)
            .expect_err("a listing whose instrument has no name is refused");

        assert!(
            matches!(
                error,
                DisplayError::InstrumentNameMismatch { instrument }
                    if instrument.as_ref() == entry.instrument.id()
            ),
            "the refusal names the instrument the listing belongs to"
        );
    }

    #[test]
    fn an_outcome_group_no_registry_carries_is_refused() {
        let group = OutcomeGroupId::from_identity_value(&format_smolstr!("v1/{}", "b".repeat(64)))
            .expect("the fixture group identity is canonical");

        let error = outcome_group(&OutcomeRegistry::new(), &group)
            .expect_err("a group no registry carries is refused");

        assert!(
            matches!(
                error,
                DisplayError::MissingOutcomeTerms { group: found } if found == group
            ),
            "the refusal names the group it could not find"
        );
    }

    /// The code one fixture native token publishes.
    fn token_code(code: &str) -> AssetDisplayCode {
        let terms = test_support::token_terms(test_support::VENUE, code);
        AssetDisplayCode::from_terms(AssetDisplayTerms::CryptoToken(&terms))
            .expect("a token renders")
            .expect("a token states a code")
    }
}

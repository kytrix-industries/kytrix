use std::convert::Infallible;

use i18n::{t_smol, translate::Translate};
use kytrix_service_provider::service::{
    engine::{ErrorCode, KytrixServiceError},
    ethereum_wallet::{EthereumWalletApiError, EthereumWalletMutationUnknown},
    solana_wallet::{SolanaWalletApiError, SolanaWalletMutationUnknown},
    x::{XApiError, XMutationUnknown},
};
use market_participants_connector::{
    account::error::{AccountCatalogError, AccountError, AccountNetworkError},
    catalog::{
        display::types::DisplayError,
        error::{CatalogError, CatalogValidationError, SourceError, SourceProtocolRejection},
    },
    error::{
        AuthError, DisconnectReason as MarketParticipantDisconnectReason, MarketParticipantError,
        OrderErrorKind,
    },
    market::{
        domain::DisconnectReason,
        error::{MarketError, MarketNetworkError, MarketRequestRejection},
    },
};
use market_participants_model::{
    display::{lexical::scope_display_code, values::InstrumentDisplayString},
    error::{
        ConversionDirection, IndexField, ListingIdentifierError, ListingPublicationError,
        ModelError, QuantityUnitExpectation, SettlementAssetRole,
    },
    identifier::VenueScopeId,
    listing::route::{VenueListingKey, VenueRoute},
};
use network::{
    error::NetworkError,
    transport::error::{TransportError, UpgradeFailure},
};
use smol_str::{SmolStr, format_smolstr};

use crate::{
    rpc::RpcCallError,
    venue::{VenueDisplay as _, route_display_string},
};

/// Renders a typed failure through the injected [`Translate`] context.
///
/// Every implementation maps the failure to a message key rather than formatting the
/// `Display` text, so a widget that renders the value states the catalogue's sentence in
/// the language the context carries instead of a diagnostic written for developers.
///
/// A failure that carries no sentence of its own delegates to the failure it wraps: the
/// outer type decides how the inner one is stated, and the arm that names it is the one
/// that supplies the context.
pub trait TranslatedError {
    /// Returns the failure as text in the language `translate` renders.
    fn translated_message(&self, translate: &Translate) -> SmolStr;
}

impl TranslatedError for Infallible {
    fn translated_message(&self, _translate: &Translate) -> SmolStr {
        match *self {}
    }
}

/// Whether a failure is the directory answering "this route's catalog is not in
/// memory yet" rather than reporting something wrong.
///
/// The answer is a wait, not a failure: the connection action loads a route's
/// catalog, and a read that arrives first is told to come back rather than being
/// refused. A widget reports it as its neutral `waiting` notice, because the user's
/// next move is the 「市场」 button — there is nothing to recover from.
///
/// A widget's own gate already asks the directory before it starts anything, so an
/// answer that reaches here is one the gate could not have seen: the route was
/// revoked between that question and the request, which is what a venue
/// re-discovering its scopes does in practice. Treating it as terminal would leave a
/// pane failed over a route the very next publication is about to bring back.
///
/// The variant is asked of the typed field each wrapper carries rather than found by
/// walking `Error::source`, because the wrappers are boxed: the source of
/// `MarketError::Catalog` is a boxing adapter, not the `CatalogError` a
/// reader would hope to downcast to.
pub fn is_catalog_not_loaded(error: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(error) = error.downcast_ref::<CatalogError>() {
        return matches!(error, CatalogError::CatalogNotLoaded { .. });
    }
    if let Some(error) = error.downcast_ref::<MarketError>() {
        return market_is_catalog_not_loaded(error);
    }
    if let Some(error) = error.downcast_ref::<AccountError>() {
        return account_is_catalog_not_loaded(error);
    }
    false
}

/// The same question asked of a market read's own error.
///
/// There is one place the directory's answer can be, and this is it:
/// `market_error_from_catalog` unwraps a source step's failure into
/// [`MarketError::CatalogSource`] and puts every other answer in
/// [`MarketError::Catalog`], so the typed error is never behind the source variant.
pub fn market_is_catalog_not_loaded(error: &MarketError) -> bool {
    match error {
        MarketError::Catalog(error) => {
            matches!(error.as_ref(), CatalogError::CatalogNotLoaded { .. })
        }
        _ => false,
    }
}

/// The same question asked of an account read's own error.
pub fn account_is_catalog_not_loaded(error: &AccountError) -> bool {
    match error {
        AccountError::Catalog(AccountCatalogError::Directory(error)) => {
            matches!(error.as_ref(), CatalogError::CatalogNotLoaded { .. })
        }
        _ => false,
    }
}

/// The answer a listing-name read gave, when it produced no name.
///
/// The set is closed and carries no text of its own: a listing is either named by
/// the generation that published it or it falls back to one of these answers, and
/// every locale spells them itself. No answer is a route, a listing id, a native
/// symbol or a connector's own message, so a fallback can never smuggle an identity
/// into a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalizedReason {
    /// The read produced the published name the presentation carries.
    ///
    /// This answer makes the presentation total, so a presentation that names a
    /// listing states that the read resolved rather than claiming a failure it did
    /// not have.
    Resolved,
    /// The listed instance has no published name.
    UnknownListing,
    /// The published generation holds no value the read could use.
    DisplayUnavailable,
}

impl TranslatedError for LocalizedReason {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::UnknownListing => t_smol!(translate, "error.display.unknown_listing"),
            Self::Resolved | Self::DisplayUnavailable => {
                t_smol!(translate, "error.display.display_unavailable")
            }
        }
    }
}

/// The name one listing is presented by, and the answer of the read that produced
/// it.
///
/// The boundary is fixed: a connector-aware caller resolves the name first and
/// renders this value, so no widget formats a name of its own and none falls back
/// to a native symbol, an identity digest or a connector reason. `display` is the
/// name the published generation gave the listing, and `reason` is what a read that
/// produced no name is shown as.
///
/// The name is the connector's own [`InstrumentDisplayString`] rather than a string
/// of this module's making: the connector is the only place that may render a
/// published name, so a second string type here would either duplicate that
/// authority or hold text no publication ever verified. The two types need no
/// connector instance or runtime — they are plain values the caller fills in at the
/// boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingErrorPresentation {
    /// The published name of the listing, when the read produced one.
    pub display: Option<InstrumentDisplayString>,
    /// The answer of the read.
    pub reason: LocalizedReason,
}

impl ListingErrorPresentation {
    /// Presents the outcome of one published listing-name read.
    ///
    /// The read is resolved first, and its outcome decides whether the presentation
    /// carries the published name or the localized fallback of the failure that
    /// refused it.
    pub fn of(read: Result<InstrumentDisplayString, DisplayError>) -> Self {
        match read {
            Ok(name) => Self {
                display: Some(name),
                reason: LocalizedReason::Resolved,
            },
            Err(error) => Self {
                display: None,
                reason: Self::reason_of(&error),
            },
        }
    }

    /// Classifies one name read failure.
    ///
    /// A read that named no published listing has its own message; every other
    /// failure of a read — a missing asset or reference term, a generation
    /// mismatch, a formatter refusal — is the unavailable name, whose cause the
    /// typed error already carries for a caller that wants it.
    fn reason_of(error: &DisplayError) -> LocalizedReason {
        match error {
            DisplayError::ListingNotFound { .. } => LocalizedReason::UnknownListing,
            _ => LocalizedReason::DisplayUnavailable,
        }
    }
}

impl TranslatedError for ListingErrorPresentation {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        let Some(display) = &self.display else {
            return self.reason.translated_message(translate);
        };
        SmolStr::new(display.as_str())
    }
}

/// Renders the typed identity of one listing as `ROUTE:listing_id`.
///
/// This is an identity/diagnostic presentation and not a name: the value it
/// returns is never an [`InstrumentDisplayString`], the listing it names was never
/// published under it, and no widget may draw it as a main name or a business
/// label. It exists for the diagnostics of a catalog validation failure, whose
/// batch was refused before any name existed.
///
/// The route half is the user-visible form: the message this is
/// interpolated into is read by a user, so it states the venue the way the
/// catalogue names it and the scope in the protocol's own upper case —
/// `Binance/SPOT` in English, `币安/SPOT` in Chinese — while the canonical
/// lower-case `VenueRoute::Display` stays with machine logs and serialization.
fn listing_identity_diagnostic(listing: &VenueListingKey, translate: &Translate) -> SmolStr {
    format_smolstr!(
        "{}:{}",
        route_display_string(&listing.route, translate),
        listing.listing_id
    )
}

/// The user-visible name of the scope one stored credential set belongs to.
///
/// A credential set is stored under the scope name the settings and the request path both use,
/// and what a reader sees on the settings page is that name in the protocol's own upper case. A
/// name that is not a canonical scope cannot have been stored, so the fallback is the name as it
/// was written rather than a refusal: this is a label, and a label that will not render is worse
/// than one that is spelled oddly.
fn credential_scope_display_code(scope: &str) -> SmolStr {
    VenueScopeId::new(scope)
        .map_or_else(|_| SmolStr::new(scope), |scope| scope_display_code(&scope))
}

impl TranslatedError for SourceError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Network { reason } => t_smol!(
                translate,
                "error.instrument_catalog.network",
                reason = reason
            ),
            Self::Authentication { reason } => {
                t_smol!(
                    translate,
                    "error.instrument_catalog.authentication",
                    reason = reason
                )
            }
            Self::RateLimited { .. } => t_smol!(translate, "error.instrument_catalog.rate_limited"),
            Self::Rejected {
                code,
                message,
                retryable,
            } => {
                t_smol!(
                    translate,
                    "error.instrument_catalog.rejected",
                    code = code,
                    message = message,
                    retryable = retryable
                )
            }
            Self::Decode { operation } => {
                t_smol!(
                    translate,
                    "error.instrument_catalog.decode",
                    operation = operation
                )
            }
            // The reason is classified by the source, so each failure of one step
            // is stated by its own message: a raw source sentence never reaches
            // the user through this arm, and the one diagnostic that names a
            // listing keeps the identity in its typed field instead of the text.
            Self::Protocol { operation, reason } => match reason {
                SourceProtocolRejection::UnexpectedResponse => t_smol!(
                    translate,
                    "error.instrument_catalog.protocol_unexpected_response",
                    operation = operation
                ),
                SourceProtocolRejection::Model { error } => t_smol!(
                    translate,
                    "error.instrument_catalog.protocol_model",
                    operation = operation,
                    cause = error.translated_message(translate)
                ),
                SourceProtocolRejection::ListingTermsIncomplete { .. } => t_smol!(
                    translate,
                    "error.instrument_catalog.protocol_listing_terms",
                    operation = operation
                ),
                SourceProtocolRejection::Venue { reason } => t_smol!(
                    translate,
                    "error.instrument_catalog.protocol_venue",
                    operation = operation,
                    reason = reason
                ),
            },
            Self::Configuration { reason } => {
                t_smol!(
                    translate,
                    "error.instrument_catalog.configuration",
                    reason = reason
                )
            }
            // The one catalog failure that is about a market other than the route's:
            // the directory belongs to the market participant that publishes it, so the venue
            // is named in the reader's language and the address or cause it failed
            // with stays the diagnostic detail.
            Self::SymbolDirectory { venue, reason } => t_smol!(
                translate,
                "error.instrument_catalog.symbol_directory",
                market = venue.display_string(translate),
                detail = reason
            ),
            Self::Lost => t_smol!(translate, "error.instrument_catalog.lost"),
            Self::Other { reason } => {
                t_smol!(translate, "error.instrument_catalog.other", reason = reason)
            }
        }
    }
}

/// Why one staged catalog batch was refused.
///
/// Every variant names the route, the listing key, the typed identifier or the
/// field that failed, so the message is built from those values instead of from
/// a free-form reason string.
impl TranslatedError for CatalogValidationError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::ListingRouteMismatch { route, listing } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.catalog_validation.listing_route_mismatch",
                    route = route_display_string(route, translate),
                    listing = listing
                )
            }
            Self::SnapshotKeyMismatch { listing, snapshot } => {
                let listing = listing_identity_diagnostic(listing, translate);
                let snapshot = listing_identity_diagnostic(snapshot, translate);
                t_smol!(
                    translate,
                    "error.catalog_validation.snapshot_key_mismatch",
                    listing = listing,
                    snapshot = snapshot
                )
            }
            Self::InstrumentTermsMismatch {
                listing,
                declared,
                computed,
            } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.catalog_validation.instrument_terms_mismatch",
                    listing = listing,
                    declared = declared,
                    computed = computed
                )
            }
            Self::InstrumentKindMismatch {
                listing,
                declared,
                kind,
            } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.catalog_validation.instrument_kind_mismatch",
                    listing = listing,
                    declared = declared,
                    kind = kind
                )
            }
            Self::DuplicateNativeIdentifier { route, identifier } => t_smol!(
                translate,
                "error.catalog_validation.duplicate_native_identifier",
                route = route_display_string(route, translate),
                identifier = identifier
            ),
            Self::InvalidListingIdentifiers { listing, kind, .. } => {
                let listing = listing_identity_diagnostic(listing, translate);
                let kind = kind.label();
                t_smol!(
                    translate,
                    "error.catalog_validation.invalid_listing_identifiers",
                    listing = listing,
                    kind = kind
                )
            }
            Self::InvalidTradingRules { listing, .. } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.catalog_validation.invalid_trading_rules",
                    listing = listing
                )
            }
            Self::OutcomePriceQuoteMismatch {
                listing,
                expected,
                found,
            } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.catalog_validation.outcome_price_quote_mismatch",
                    listing = listing,
                    expected = expected,
                    found = found
                )
            }
            Self::OutcomePriceBasisUnsupported { listing, .. } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.catalog_validation.outcome_price_basis_unsupported",
                    listing = listing
                )
            }
            Self::ListingIdentifierRouteMismatch { route, .. } => t_smol!(
                translate,
                "error.catalog_validation.listing_identifier_route_mismatch",
                route = route_display_string(route, translate)
            ),
            Self::IdentifierNormalizationMismatch { listing, kind, .. } => {
                let listing = listing_identity_diagnostic(listing, translate);
                let kind = kind.label();
                t_smol!(
                    translate,
                    "error.catalog_validation.identifier_normalization_mismatch",
                    listing = listing,
                    kind = kind
                )
            }
            Self::IncompleteOutcomeSet {
                route,
                listing,
                kind,
                count,
            } => {
                let listing = listing_identity_diagnostic(listing, translate);
                // The sentence has to agree with its count: one missing object is not
                // "1 missing outcome group objects".
                if *count == 1 {
                    t_smol!(
                        translate,
                        "error.catalog_validation.incomplete_outcome_set_one",
                        route = route_display_string(route, translate),
                        listing = listing,
                        kind = kind,
                        count = count
                    )
                } else {
                    t_smol!(
                        translate,
                        "error.catalog_validation.incomplete_outcome_set",
                        route = route_display_string(route, translate),
                        listing = listing,
                        kind = kind,
                        count = count
                    )
                }
            }
            Self::OutcomeGroupIncomplete { group, .. } => t_smol!(
                translate,
                "error.catalog_validation.outcome_group_incomplete",
                group = group
            ),
            Self::OutcomeListingLegUnknown {
                listing,
                group,
                member,
                side,
            } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.catalog_validation.outcome_listing_leg_unknown",
                    listing = listing,
                    group = group,
                    member = member,
                    side = side
                )
            }
            Self::ListingEventUnknown { listing, event } => {
                let listing = listing_identity_diagnostic(listing, translate);
                let event = event.as_str();
                t_smol!(
                    translate,
                    "error.catalog_validation.listing_event_unknown",
                    listing = listing,
                    event = event
                )
            }
            Self::OutcomeGroupLineageInvalid { group } => t_smol!(
                translate,
                "error.catalog_validation.outcome_group_lineage_invalid",
                group = group
            ),
            Self::OutcomeRiskPoolIncomplete { pool, .. } => t_smol!(
                translate,
                "error.catalog_validation.outcome_risk_pool_incomplete",
                pool = pool
            ),
            Self::AmbiguousRiskPoolMembership { group } => t_smol!(
                translate,
                "error.catalog_validation.ambiguous_risk_pool_membership",
                group = group
            ),
            Self::OutcomeEventIncomplete { event, .. } => t_smol!(
                translate,
                "error.catalog_validation.outcome_event_incomplete",
                event = event
            ),
            Self::OutcomeRiskPoolLineageInvalid { pool } => t_smol!(
                translate,
                "error.catalog_validation.outcome_risk_pool_lineage_invalid",
                pool = pool
            ),
            Self::InstrumentLineageInvalid { .. } => {
                t_smol!(
                    translate,
                    "error.catalog_validation.instrument_lineage_invalid"
                )
            }
            Self::DuplicateListingId { route, listing_id } => t_smol!(
                translate,
                "error.catalog_validation.duplicate_listing_id",
                route = route_display_string(route, translate),
                listing_id = listing_id
            ),
            Self::InvalidListingContinuityTerms { listing, .. } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.catalog_validation.invalid_listing_continuity_terms",
                    listing = listing
                )
            }
            Self::AmbiguousListingContinuity { listing, .. } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.catalog_validation.ambiguous_listing_continuity",
                    listing = listing
                )
            }
            Self::ListingWithoutIdentity { listing } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.catalog_validation.listing_without_identity",
                    listing = listing
                )
            }
            Self::DisplayRejected { error, .. } => error.translated_message(translate),
        }
    }
}

/// The display publication and read failures.
///
/// The naming gate refuses a whole batch before it is published, so its message
/// names the identity the failure is about — a listing, an asset, a reference or an
/// outcome group — and never renders a name the catalog never published.
impl TranslatedError for DisplayError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Format { error } => format_smolstr!("{error}"),
            Self::ListingNotFound { listing } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.display.listing_not_found",
                    listing = listing
                )
            }
            Self::MissingAssetTerms { asset } => {
                t_smol!(
                    translate,
                    "error.display.missing_asset_terms",
                    asset = asset
                )
            }
            Self::MissingIndexTerms { index } => {
                t_smol!(
                    translate,
                    "error.display.missing_index_terms",
                    index = index
                )
            }
            Self::MissingExternalReferenceTerms { reference } => t_smol!(
                translate,
                "error.display.missing_external_reference_terms",
                reference = reference
            ),
            Self::MissingReferenceTerms { reference } => t_smol!(
                translate,
                "error.display.missing_reference_terms",
                reference = reference
            ),
            Self::MissingOutcomeTerms { group } => {
                t_smol!(
                    translate,
                    "error.display.missing_outcome_terms",
                    group = group
                )
            }
            Self::AssetDisplayCodeMissing { asset } => {
                t_smol!(translate, "error.display.missing_asset_code", asset = asset)
            }
            Self::StagedCodeMismatch { asset } => {
                t_smol!(
                    translate,
                    "error.display.staged_code_mismatch",
                    asset = asset
                )
            }
            Self::InstrumentNameMismatch { instrument } => t_smol!(
                translate,
                "error.display.instrument_name_mismatch",
                instrument = instrument
            ),
            Self::ListingIdentifierEcho { listing, kind, .. } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.display.listing_identifier_echo",
                    listing = listing,
                    kind = kind.label()
                )
            }
            Self::ListingIdentifierMismatch {
                listing,
                kind,
                value,
            } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.display.listing_identifier_mismatch",
                    listing = listing,
                    kind = kind.label(),
                    value = value
                )
            }
            Self::ReferenceDirectionMismatch { reference, field } => t_smol!(
                translate,
                "error.display.reference_direction_mismatch",
                reference = reference,
                field = field
            ),
            Self::GenerationMismatch { expected, actual } => t_smol!(
                translate,
                "error.display.generation_mismatch",
                expected = expected,
                actual = actual
            ),
        }
    }
}

impl TranslatedError for CatalogError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Source(source) | Self::SourceFailed { error: source, .. } => {
                source.translated_message(translate)
            }
            Self::InvalidCatalog { route, error } => t_smol!(
                translate,
                "error.catalog.invalid_catalog",
                route = route_display_string(route, translate),
                cause = error.translated_message(translate)
            ),
            Self::InvalidRegistries { error } => t_smol!(
                translate,
                "error.catalog.invalid_registries",
                cause = error.translated_message(translate)
            ),
            Self::RouteNotRegistered { route } => t_smol!(
                translate,
                "error.catalog.route_not_registered",
                route = route_display_string(route, translate)
            ),
            Self::CatalogNotLoaded { route } => t_smol!(
                translate,
                "error.catalog.catalog_not_loaded",
                route = route_display_string(route, translate)
            ),
            Self::ListingNotFound { listing } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.catalog.listing_not_found",
                    listing = listing
                )
            }
            Self::ListingRouteMismatch { expected, found } => {
                let expected = route_display_string(expected, translate);
                let found = listing_identity_diagnostic(found, translate);
                t_smol!(
                    translate,
                    "error.catalog.listing_route_mismatch",
                    expected = expected,
                    found = found
                )
            }
            Self::NativeIdentifierNotFound { route, .. } => t_smol!(
                translate,
                "error.catalog.native_identifier_not_found",
                route = route_display_string(route, translate)
            ),
            Self::NativeGroupIdentifierNotFound { route, .. } => t_smol!(
                translate,
                "error.catalog.native_group_identifier_not_found",
                route = route_display_string(route, translate)
            ),
            Self::NativeEventIdentifierNotFound { route, .. } => t_smol!(
                translate,
                "error.catalog.native_event_identifier_not_found",
                route = route_display_string(route, translate)
            ),
            Self::NativeSymbolInvalid { route, symbol } => t_smol!(
                translate,
                "error.catalog.native_symbol_invalid",
                route = route_display_string(route, translate),
                symbol = symbol
            ),
            Self::NativeIdentifierAmbiguous { route, .. } => t_smol!(
                translate,
                "error.catalog.native_identifier_ambiguous",
                route = route_display_string(route, translate)
            ),
            Self::NativeIdentifierRouteMismatch { route, .. } => t_smol!(
                translate,
                "error.catalog.native_identifier_route_mismatch",
                route = route_display_string(route, translate)
            ),
            Self::NativeSymbolNotFound { route, symbol } => t_smol!(
                translate,
                "error.catalog.native_symbol_not_found",
                route = route_display_string(route, translate),
                symbol = symbol
            ),
            Self::NativeSymbolAbsent { listing } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.catalog.native_symbol_absent",
                    listing = listing
                )
            }
            Self::OutcomeGroupNotFound { group } => {
                t_smol!(
                    translate,
                    "error.catalog.outcome_group_not_found",
                    group = group
                )
            }
            Self::OutcomeEventNotFound { event } => {
                t_smol!(
                    translate,
                    "error.catalog.outcome_event_not_found",
                    event = event
                )
            }
            Self::OutcomeRiskPoolNotFound { pool } => {
                t_smol!(
                    translate,
                    "error.catalog.outcome_risk_pool_not_found",
                    pool = pool
                )
            }
            Self::RouteRevisionChanged { route, .. } => t_smol!(
                translate,
                "error.catalog.route_revision_changed",
                route = route_display_string(route, translate)
            ),
            Self::DuplicateCatalogSourceId { id } => {
                t_smol!(
                    translate,
                    "error.catalog.duplicate_catalog_source_id",
                    id = id
                )
            }
            Self::InvalidCatalogSourceId { id } => {
                t_smol!(
                    translate,
                    "error.catalog.invalid_catalog_source_id",
                    id = id
                )
            }
            Self::MissingCatalogSource { route, capability } => t_smol!(
                translate,
                "error.catalog.missing_catalog_source",
                route = route_display_string(route, translate),
                capability = capability
            ),
            Self::CatalogSourceOnNonCatalogRoute { route, id, .. } => t_smol!(
                translate,
                "error.catalog.catalog_source_on_non_catalog_route",
                route = route_display_string(route, translate),
                id = id
            ),
            Self::UnregisteredCatalogSource { route, id } => t_smol!(
                translate,
                "error.catalog.unregistered_catalog_source",
                route = route_display_string(route, translate),
                id = id
            ),
            Self::InconsistentRouteRegistration { route } => t_smol!(
                translate,
                "error.catalog.inconsistent_route_registration",
                route = route_display_string(route, translate)
            ),
            Self::RegisteredRouteMismatch { route } => t_smol!(
                translate,
                "error.catalog.registered_route_mismatch",
                route = route_display_string(route, translate)
            ),
            Self::IdentifierNormalizationMismatch { listing, kind, .. } => {
                let listing = listing_identity_diagnostic(listing, translate);
                let kind = kind.label();
                t_smol!(
                    translate,
                    "error.catalog.identifier_normalization_mismatch",
                    listing = listing,
                    kind = kind
                )
            }
        }
    }
}

impl<E: TranslatedError> TranslatedError for RpcCallError<E> {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::DeadlineExceeded { request_id } => {
                t_smol!(translate, "error.rpc.deadline", request_id = request_id)
            }
            Self::ServiceClosed { request_id } => {
                t_smol!(
                    translate,
                    "error.rpc.service_closed",
                    request_id = request_id
                )
            }
            Self::ResponseDropped { request_id } => {
                t_smol!(
                    translate,
                    "error.rpc.response_dropped",
                    request_id = request_id
                )
            }
            Self::Service { source, .. } => source.translated_message(translate),
        }
    }
}

impl TranslatedError for MarketError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Network(source) => {
                t_smol!(
                    translate,
                    "error.market.network",
                    cause = source.translated_message(translate)
                )
            }
            Self::CatalogSource(source) => source.translated_message(translate),
            Self::Catalog(error) => error.translated_message(translate),
            Self::ListingRouteMismatch { listing, route } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.market.listing_route_mismatch",
                    listing = listing,
                    route = route_display_string(route, translate)
                )
            }
            Self::UnsupportedCapability { route, capability } => {
                let capability = capability.label();
                t_smol!(
                    translate,
                    "error.market.unsupported_capability",
                    route = route_display_string(route, translate),
                    capability = capability
                )
            }
            Self::Model(error) => error.translated_message(translate),
            Self::Protocol { operation } => {
                t_smol!(translate, "error.market.protocol", operation = operation)
            }
            // The reason is the refusal itself, not a sentence about it: the arm
            // names the state the listing was refused in, so a listing id the
            // request carried cannot be read as part of the message.
            Self::InvalidRequest { operation, reason } => {
                let MarketRequestRejection::ListingNotReadable { status } = reason;
                let status = status.label();
                t_smol!(
                    translate,
                    "error.market.listing_not_readable",
                    operation = operation,
                    status = status
                )
            }
            Self::Disconnected(reason) => t_smol!(
                translate,
                "error.market.disconnected",
                cause = reason.translated_message(translate)
            ),
            Self::ServiceClosed => t_smol!(translate, "error.market.service_closed"),
            Self::Configuration(detail) => {
                t_smol!(translate, "error.market.configuration", detail = detail)
            }
            Self::Timeout {
                operation,
                timeout_ms,
            } => {
                t_smol!(
                    translate,
                    "error.market.timeout",
                    operation = operation,
                    timeout_ms = timeout_ms
                )
            }
        }
    }
}

impl TranslatedError for MarketNetworkError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        self.error().translated_message(translate)
    }
}

impl TranslatedError for AccountError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Network(source) => source.translated_message(translate),
            Self::Catalog(source) => source.translated_message(translate),
            Self::Model(error) => error.translated_message(translate),
            Self::UnknownAsset { route, asset } => t_smol!(
                translate,
                "error.market_participants_account.unknown_asset",
                route = route_display_string(route, translate),
                asset = asset
            ),
            Self::Protocol { operation } => {
                t_smol!(
                    translate,
                    "error.market_participants_account.protocol",
                    operation = operation
                )
            }
            // The listing and the route stay typed fields of the error and are
            // drawn as the identity diagnostic a protocol failure is reported by:
            // an account query that names a listing of another route never had a
            // published name to show.
            Self::ListingRouteMismatch { listing, route } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.market_participants_account.listing_route_mismatch",
                    listing = listing,
                    route = route_display_string(route, translate)
                )
            }
        }
    }
}

impl TranslatedError for AccountCatalogError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Source(source) => source.translated_message(translate),
            Self::Network(source) => source.translated_message(translate),
            Self::Directory(error) => error.translated_message(translate),
            Self::ListingNotFound { route, listing } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.market_participants_account.listing_not_found",
                    route = route_display_string(route, translate),
                    listing = listing
                )
            }
        }
    }
}

impl TranslatedError for AccountNetworkError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        self.error().translated_message(translate)
    }
}

impl TranslatedError for MarketParticipantError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::ConnectionFailed {
                route,
                endpoint,
                source,
            } => {
                t_smol!(
                    translate,
                    "error.market_participants.connection_failed",
                    venue = route.venue.display_string(translate),
                    endpoint = endpoint,
                    cause = source.translated_message(translate)
                )
            }
            // A venue-level request names the venue rather than a route: it is the
            // discovery conversation a load opens before any scope of the venue is
            // registered, so there is no route to report it against.
            Self::VenueRequestFailed {
                venue,
                endpoint,
                source,
            } => {
                t_smol!(
                    translate,
                    "error.market_participants.venue_request_failed",
                    venue = venue.display_string(translate),
                    endpoint = endpoint,
                    cause = source.translated_message(translate)
                )
            }
            Self::AuthenticationFailed { route, source, .. } => t_smol!(
                translate,
                "error.market_participants.authentication_failed",
                venue = route.venue.display_string(translate),
                cause = source.translated_message(translate)
            ),
            Self::RateLimited { .. } => {
                t_smol!(translate, "error.market_participants.rate_limited")
            }
            Self::Rejected { code, message, .. } => {
                t_smol!(
                    translate,
                    "error.market_participants.rejected",
                    code = code,
                    message = message
                )
            }
            Self::OrderError { kind, .. } => t_smol!(
                translate,
                "error.market_participants.order_error",
                cause = kind.translated_message(translate)
            ),
            Self::InsufficientBalance {
                asset,
                required,
                available,
                ..
            } => {
                t_smol!(
                    translate,
                    "error.market_participants.insufficient_balance",
                    asset = asset,
                    required = required,
                    available = available
                )
            }
            Self::VenueUnsupported { venue } => t_smol!(
                translate,
                "error.market_participants.venue_unsupported",
                market = venue.display_string(translate)
            ),
            Self::VenuePublishesNoScopeTable { venue } => t_smol!(
                translate,
                "error.market_participants.venue_publishes_no_scope_table",
                market = venue.display_string(translate)
            ),
            Self::UnusableCredentials { venue } => t_smol!(
                translate,
                "error.market_participants.unusable_credentials",
                market = venue.display_string(translate)
            ),
            Self::IncompleteCredentials { venue, scope } => match scope {
                // A venue that signs its markets apart can be half configured, and then the
                // market is what the reader has to finish: a sentence naming only the venue
                // would send them to a page with two credential sets on it. The market is
                // spelled the way the settings page spells it, so the name in the sentence is
                // the name of the section the reader has to open.
                Some(scope) => t_smol!(
                    translate,
                    "error.market_participants.incomplete_market_credentials",
                    market = venue.display_string(translate),
                    scope = credential_scope_display_code(scope)
                ),
                None => t_smol!(
                    translate,
                    "error.market_participants.incomplete_credentials",
                    market = venue.display_string(translate)
                ),
            },
            Self::Configuration(detail) => t_smol!(
                translate,
                "error.market_participants.configuration",
                detail = detail
            ),
            Self::RuntimeUnavailable => {
                t_smol!(translate, "error.market_participants.runtime_unavailable")
            }
            Self::CapabilityMismatch { expected, actual } => {
                let expected = expected.label();
                let actual = actual.label();
                t_smol!(
                    translate,
                    "error.market_participants.capability_mismatch",
                    expected = expected,
                    actual = actual
                )
            }
            Self::RouteMismatch { request, context } => t_smol!(
                translate,
                "error.market_participants.route_mismatch",
                request = route_display_string(request, translate),
                context = route_display_string(context, translate)
            ),
            Self::ListingRouteMismatch { listing, route } => {
                let listing = listing_identity_diagnostic(listing, translate);
                t_smol!(
                    translate,
                    "error.market_participants.listing_route_mismatch",
                    listing = listing,
                    route = route_display_string(route, translate)
                )
            }
            Self::ListingActionNotAllowed {
                listing, status, ..
            } => {
                let listing = listing_identity_diagnostic(listing, translate);
                let status = status.label();
                t_smol!(
                    translate,
                    "error.market_participants.listing_action_not_allowed",
                    listing = listing,
                    status = status
                )
            }
            Self::ProtocolError {
                operation, reason, ..
            } => {
                t_smol!(
                    translate,
                    "error.market_participants.protocol",
                    operation = operation,
                    reason = reason
                )
            }
            Self::DecodeFailed { operation, .. } => {
                t_smol!(
                    translate,
                    "error.market_participants.decode_failed",
                    operation = operation
                )
            }
            Self::Timeout {
                operation,
                timeout_ms,
            } => {
                t_smol!(
                    translate,
                    "error.market_participants.timeout",
                    operation = operation,
                    timeout_ms = timeout_ms
                )
            }
            Self::UnsupportedCapability { route, capability } => {
                let capability = capability.label();
                t_smol!(
                    translate,
                    "error.market_participants.unsupported_capability",
                    route = route_display_string(route, translate),
                    capability = capability
                )
            }
            Self::InvalidLimit {
                operation,
                requested,
                maximum,
            } => {
                t_smol!(
                    translate,
                    "error.market_participants.invalid_limit",
                    operation = operation,
                    requested = requested,
                    maximum = maximum
                )
            }
            Self::UnsupportedTimeFrame { route, timeframe } => t_smol!(
                translate,
                "error.market_participants.unsupported_time_frame",
                route = route_display_string(route, translate),
                timeframe = timeframe
            ),
            Self::UnsupportedAdjustment { route, adjustment } => t_smol!(
                translate,
                "error.market_participants.unsupported_adjustment",
                route = route_display_string(route, translate),
                adjustment = adjustment
            ),
            // A symbol directory belongs to the market participant that publishes it rather
            // than to the route that reads it, so the venue is stated as a typed
            // market in the reader's language and the address or cause it failed
            // with is the diagnostic detail.
            Self::SymbolDirectoryFetchFailed { venue, reason } => t_smol!(
                translate,
                "error.market_participants.symbol_directory_failed",
                market = venue.display_string(translate),
                detail = reason
            ),
            Self::VenueNotLoaded { venue, scope } => t_smol!(
                translate,
                "error.market_participants.venue_not_loaded",
                venue = venue.display_string(translate),
                scope = scope_display_code(scope)
            ),
            Self::Catalog(error) => error.translated_message(translate),
            Self::MarketModel(error) => error.translated_message(translate),
            Self::Disconnected(reason) => t_smol!(
                translate,
                "error.market_participants.disconnected",
                cause = reason.translated_message(translate)
            ),
            Self::ServiceClosed => t_smol!(translate, "error.market_participants.service_closed"),
            Self::Internal(detail) => t_smol!(
                translate,
                "error.market_participants.internal",
                detail = detail
            ),
        }
    }
}

impl TranslatedError for AuthError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::AccountIdentityMissing => {
                t_smol!(translate, "error.auth.account_identity_missing")
            }
            Self::InvalidCredentials => t_smol!(translate, "error.auth.invalid_credentials"),
            Self::ApiKeyNotFound => t_smol!(translate, "error.auth.api_key_not_found"),
            Self::NoMarketDataPermission => {
                t_smol!(translate, "error.auth.no_market_data_permission")
            }
            Self::SignatureInvalid => t_smol!(translate, "error.auth.signature_invalid"),
            Self::TimestampOutOfRange => t_smol!(translate, "error.auth.timestamp_out_of_range"),
            Self::IpNotWhitelisted => t_smol!(translate, "error.auth.ip_not_whitelisted"),
            Self::SessionExpired => t_smol!(translate, "error.auth.session_expired"),
            Self::HandshakeTimeout => t_smol!(translate, "error.auth.handshake_timeout"),
            Self::ResponseMalformed => t_smol!(translate, "error.auth.response_malformed"),
            Self::UnusableCredentials { venue } => t_smol!(
                translate,
                "error.auth.unusable_credentials",
                market = venue.display_string(translate)
            ),
            Self::IncompleteCredentials { venue, scope } => match scope {
                // A venue that signs its markets apart can be half configured, and then the
                // market is what the reader has to finish: a sentence naming only the venue
                // would send them to a page with two credential sets on it. The market is
                // spelled the way the settings page spells it, so the name in the sentence is
                // the name of the section the reader has to open.
                Some(scope) => t_smol!(
                    translate,
                    "error.auth.incomplete_market_credentials",
                    market = venue.display_string(translate),
                    scope = credential_scope_display_code(scope)
                ),
                None => t_smol!(
                    translate,
                    "error.auth.incomplete_credentials",
                    market = venue.display_string(translate)
                ),
            },
            Self::ServiceClosed => t_smol!(translate, "error.auth.service_closed"),
            Self::InvalidAccountIdentity { value } => {
                t_smol!(
                    translate,
                    "error.auth.invalid_account_identity",
                    value = value
                )
            }
        }
    }
}

impl TranslatedError for OrderErrorKind {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::InvalidPrice { reason } => {
                t_smol!(translate, "error.order.invalid_price", reason = reason)
            }
            Self::InvalidQuantity { reason } => {
                t_smol!(translate, "error.order.invalid_quantity", reason = reason)
            }
            Self::OrderNotFound => t_smol!(translate, "error.order.not_found"),
            Self::OrderAlreadyFilled => t_smol!(translate, "error.order.already_filled"),
            Self::OrderAlreadyCanceled => t_smol!(translate, "error.order.already_canceled"),
            Self::PositionLimitExceeded => {
                t_smol!(translate, "error.order.position_limit_exceeded")
            }
            Self::MarketSuspended => t_smol!(translate, "error.order.market_suspended"),
            Self::SelfTradeProtection => t_smol!(translate, "error.order.self_trade_protection"),
            Self::Other { code, message } => {
                t_smol!(
                    translate,
                    "error.order.other",
                    code = code,
                    message = message
                )
            }
        }
    }
}

impl TranslatedError for NetworkError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::DnsResolutionFailed { host } => {
                t_smol!(
                    translate,
                    "error.network.dns_resolution_failed",
                    host = host
                )
            }
            Self::TcpConnectionFailed { endpoint } => {
                t_smol!(
                    translate,
                    "error.network.tcp_connection_failed",
                    endpoint = endpoint
                )
            }
            Self::TlsHandshakeFailed { endpoint } => {
                t_smol!(
                    translate,
                    "error.network.tls_handshake_failed",
                    endpoint = endpoint
                )
            }
            Self::WebSocketUpgradeFailed { endpoint, failure } => t_smol!(
                translate,
                "error.network.websocket_upgrade_failed",
                endpoint = endpoint,
                cause = failure.translated_message(translate)
            ),
            Self::ProxyConnectionFailed {
                proxy_type,
                message,
            } => {
                t_smol!(
                    translate,
                    "error.network.proxy_connection_failed",
                    proxy_type = proxy_type,
                    message = message
                )
            }
            Self::Io { message } => t_smol!(translate, "error.network.io", message = message),
            Self::InvalidData { operation, reason } => t_smol!(
                translate,
                "error.network.invalid_data",
                operation = operation,
                reason = reason
            ),
            Self::LimitExceeded { operation, limit } => t_smol!(
                translate,
                "error.network.limit_exceeded",
                operation = operation,
                limit = limit
            ),
        }
    }
}

impl TranslatedError for DisconnectReason {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Normal => t_smol!(translate, "error.disconnect.normal"),
            Self::Network(source) => {
                t_smol!(
                    translate,
                    "error.disconnect.network",
                    cause = source.translated_message(translate)
                )
            }
            Self::ServerClosed { code, reason } => t_smol!(
                translate,
                "error.disconnect.server_closed",
                code = code,
                reason = reason
            ),
            Self::ProxyChanged => t_smol!(translate, "error.disconnect.proxy_changed"),
            Self::AuthenticationExpired => {
                t_smol!(translate, "error.disconnect.authentication_expired")
            }
            Self::HeartbeatTimeout => t_smol!(translate, "error.disconnect.heartbeat_timeout"),
        }
    }
}

impl TranslatedError for MarketParticipantDisconnectReason {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Normal => t_smol!(translate, "error.disconnect.normal"),
            Self::Network(source) => {
                t_smol!(
                    translate,
                    "error.disconnect.network",
                    cause = source.translated_message(translate)
                )
            }
            Self::ServerClosed { code, reason } => t_smol!(
                translate,
                "error.disconnect.server_closed",
                code = code,
                reason = reason
            ),
            Self::ProxyChanged => t_smol!(translate, "error.disconnect.proxy_changed"),
            Self::AuthenticationExpired => {
                t_smol!(translate, "error.disconnect.authentication_expired")
            }
            Self::HeartbeatTimeout => t_smol!(translate, "error.disconnect.heartbeat_timeout"),
        }
    }
}

impl TranslatedError for ListingIdentifierError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::InvalidValue { kind, value } => {
                let kind = kind.label();
                t_smol!(
                    translate,
                    "error.listing_identifier.invalid_value",
                    kind = kind,
                    value = value
                )
            }
            Self::IdentifierNotFound { route, .. } => t_smol!(
                translate,
                "error.listing_identifier.identifier_not_found",
                route = route_display_string(route, translate)
            ),
            Self::NativeIdentifierRouteMismatch { route, owner, .. } => t_smol!(
                translate,
                "error.listing_identifier.route_mismatch",
                route = route_display_string(route, translate),
                owner = route_display_string(owner, translate)
            ),
            Self::IdentifierProductMismatch { route, .. } => t_smol!(
                translate,
                "error.listing_identifier.product_mismatch",
                route = route_display_string(route, translate)
            ),
            Self::NativeIdentifierAmbiguous { route, matches, .. } => t_smol!(
                translate,
                "error.listing_identifier.ambiguous",
                route = route_display_string(route, translate),
                matches = matches
            ),
        }
    }
}

impl TranslatedError for ListingPublicationError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::SnapshotKeyMismatch { listing, snapshot } => {
                let listing = listing_identity_diagnostic(listing, translate);
                let snapshot = listing_identity_diagnostic(snapshot, translate);
                t_smol!(
                    translate,
                    "error.listing_publication.snapshot_key_mismatch",
                    listing = listing,
                    snapshot = snapshot
                )
            }
            Self::IdentifierEchoMismatch { kind, value } => {
                let kind = kind.label();
                t_smol!(
                    translate,
                    "error.listing_publication.identifier_echo_mismatch",
                    kind = kind,
                    value = value
                )
            }
        }
    }
}

impl TranslatedError for ModelError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::EmptyDecimal => t_smol!(translate, "error.model.empty_decimal"),
            Self::InvalidDecimal => t_smol!(translate, "error.model.invalid_decimal"),
            Self::NegativeDecimal => t_smol!(translate, "error.model.negative_decimal"),
            Self::DecimalPrecisionExceeded { maximum } => {
                t_smol!(
                    translate,
                    "error.model.decimal_precision",
                    maximum = maximum
                )
            }
            Self::DecimalOverflow => t_smol!(translate, "error.model.decimal_overflow"),
            Self::ZeroTick => t_smol!(translate, "error.model.zero_tick"),
            Self::EmptyIdentifier { field } => {
                t_smol!(translate, "error.model.empty_identifier", field = field)
            }
            Self::ZeroValue { field } => {
                t_smol!(translate, "error.model.zero_value", field = field)
            }
            Self::InvalidTimestamp { field } => {
                t_smol!(translate, "error.model.invalid_timestamp", field = field)
            }
            Self::InvalidTimeRange => t_smol!(translate, "error.model.invalid_time_range"),
            Self::InvalidTimeOrder { field, limit } => t_smol!(
                translate,
                "error.model.invalid_time_order",
                field = field,
                limit = limit
            ),
            Self::TimeWindowTooLarge { maximum } => {
                t_smol!(
                    translate,
                    "error.model.time_window_too_large",
                    maximum = maximum
                )
            }
            Self::InvalidIso4217Code { kind } => {
                t_smol!(translate, "error.model.invalid_iso_4217_code", kind = kind)
            }
            Self::UnsupportedSecurityForm { form } => {
                t_smol!(
                    translate,
                    "error.model.unsupported_security_form",
                    form = form
                )
            }
            Self::UnsupportedInstrumentKind { kind } => {
                t_smol!(
                    translate,
                    "error.model.unsupported_instrument_kind",
                    kind = kind
                )
            }
            Self::FractionOutOfRange => t_smol!(translate, "error.model.fraction_out_of_range"),
            Self::NonPositiveRatio => t_smol!(translate, "error.model.non_positive_ratio"),
            Self::EmptyCollection { field } => {
                t_smol!(translate, "error.model.empty_collection", field = field)
            }
            Self::UnsortedCollection { field } => {
                t_smol!(translate, "error.model.unsorted_collection", field = field)
            }
            Self::DuplicateCollectionEntry { field } => {
                t_smol!(
                    translate,
                    "error.model.duplicate_collection_entry",
                    field = field
                )
            }
            Self::NonPositiveQuantity { field } => {
                t_smol!(
                    translate,
                    "error.model.non_positive_quantity",
                    field = field
                )
            }
            Self::ContradictoryDeliveryLegs => {
                t_smol!(translate, "error.model.contradictory_delivery_legs")
            }
            Self::ContractSizeWithoutAsset => {
                t_smol!(translate, "error.model.contract_size_without_asset")
            }
            Self::ConversionDirectionMismatch { direction } => {
                // The direction is a fixed role name, so it is rendered from the
                // locale instead of from the model label: a Chinese sentence that
                // embedded `underlying`/`quote`/`basis` would switch languages in
                // the middle of the message.
                let direction = match direction {
                    ConversionDirection::Underlying => {
                        t_smol!(translate, "error.model.conversion_direction_underlying")
                    }
                    ConversionDirection::Quote => {
                        t_smol!(translate, "error.model.conversion_direction_quote")
                    }
                    ConversionDirection::Basis => {
                        t_smol!(translate, "error.model.conversion_direction_basis")
                    }
                };
                t_smol!(
                    translate,
                    "error.model.conversion_direction_mismatch",
                    direction = direction
                )
            }
            Self::ConversionReferenceMismatch => {
                t_smol!(translate, "error.model.conversion_reference_mismatch")
            }
            Self::DeliveryLegMismatch => t_smol!(translate, "error.model.delivery_leg_mismatch"),
            Self::InstrumentTermsMismatch { field } => {
                t_smol!(
                    translate,
                    "error.model.instrument_terms_mismatch",
                    field = field
                )
            }
            Self::StrikeQuoteMismatch => t_smol!(translate, "error.model.strike_quote_mismatch"),
            Self::IdenticalSpotAssets => t_smol!(translate, "error.model.identical_spot_assets"),
            Self::IndexMismatch { field } => t_smol!(
                translate,
                match field {
                    IndexField::ReferenceBasis => "error.model.index_mismatch_reference_basis",
                    IndexField::ContractUnit => "error.model.index_mismatch_contract_unit",
                    IndexField::ContractUnitIndex => {
                        "error.model.index_mismatch_contract_unit_index"
                    }
                    IndexField::ContractUnitQuoteAsset => {
                        "error.model.index_mismatch_contract_unit_quote_asset"
                    }
                }
            ),
            Self::IndexPointPayoffUnsupported => {
                t_smol!(translate, "error.model.index_point_payoff_unsupported")
            }
            // The two assets of a payoff unit mismatch are the ones the contract
            // tracks and the one its unit is denominated in. Rendering them would
            // print canonical identities, which no user-facing message may do, and
            // no display code exists for them yet, so the message states the same
            // failure without them.
            Self::PayoffUnitMismatch { .. } => {
                t_smol!(translate, "error.model.payoff_unit_mismatch")
            }
            Self::IncompatibleDeliveryMode => {
                t_smol!(translate, "error.model.incompatible_delivery_mode")
            }
            Self::IncompatiblePnlModel => t_smol!(translate, "error.model.incompatible_pnl_model"),
            Self::IncompatiblePriceBasis => {
                t_smol!(translate, "error.model.incompatible_price_basis")
            }
            Self::InverseContractSizeMismatch => {
                t_smol!(translate, "error.model.inverse_contract_size_mismatch")
            }
            Self::InverseRequiresCashDelivery => {
                t_smol!(translate, "error.model.inverse_requires_cash_delivery")
            }
            Self::InverseSettlementMismatch => {
                t_smol!(translate, "error.model.inverse_settlement_mismatch")
            }
            Self::RequiredConversionReference => {
                t_smol!(translate, "error.model.required_conversion_reference")
            }
            Self::SettlementAssetMismatch => {
                t_smol!(translate, "error.model.settlement_asset_mismatch")
            }
            Self::UnexpectedConversionReference => {
                t_smol!(translate, "error.model.unexpected_conversion_reference")
            }
            Self::UnderlyingWithoutAsset => {
                t_smol!(translate, "error.model.underlying_without_asset")
            }
            Self::IndexUnderlyingRequiresCashDelivery => {
                t_smol!(
                    translate,
                    "error.model.index_underlying_requires_cash_delivery"
                )
            }
            // The field a quantity-unit mismatch carries is an internal path — the
            // contract unit, a market request field — and no user-facing message may
            // print it, so the rendering names what the quantity has to be instead.
            Self::QuantityUnitMismatch { expected, .. } => t_smol!(
                translate,
                match expected {
                    QuantityUnitExpectation::TradedQuantity => {
                        "error.model.quantity_unit_mismatch_traded_quantity"
                    }
                    QuantityUnitExpectation::Notional => {
                        "error.model.quantity_unit_mismatch_notional"
                    }
                    QuantityUnitExpectation::ListedUnit => {
                        "error.model.quantity_unit_mismatch_listed_unit"
                    }
                    QuantityUnitExpectation::NotionalAsset => {
                        "error.model.quantity_unit_mismatch_notional_asset"
                    }
                    QuantityUnitExpectation::OutcomeInstrument => {
                        "error.model.quantity_unit_mismatch_outcome_instrument"
                    }
                }
            ),
            Self::InvalidIdentityValue { version, value } => t_smol!(
                translate,
                "error.model.invalid_identity_value",
                version = version,
                value = value
            ),
            Self::InvalidIdentifierNamespace { value } => t_smol!(
                translate,
                "error.model.invalid_identifier_namespace",
                value = value
            ),
            Self::InvalidIdentifierValue { value } => {
                t_smol!(
                    translate,
                    "error.model.invalid_identifier_value",
                    value = value
                )
            }
            Self::InvalidNativeIdentifier { symbol } => {
                t_smol!(
                    translate,
                    "error.model.invalid_native_identifier",
                    symbol = symbol
                )
            }
            Self::UnknownVenue { value } => {
                t_smol!(translate, "error.model.unknown_venue", value = value)
            }
            Self::InvalidVenueScopeId { value } => {
                t_smol!(
                    translate,
                    "error.model.invalid_venue_scope_id",
                    value = value
                )
            }
            Self::InvalidChainId { value } => {
                t_smol!(translate, "error.model.invalid_chain_id", value = value)
            }
            Self::InvalidWalletAddress { value } => {
                t_smol!(
                    translate,
                    "error.model.invalid_wallet_address",
                    value = value
                )
            }
            Self::InvalidContractAddress { value } => {
                t_smol!(
                    translate,
                    "error.model.invalid_contract_address",
                    value = value
                )
            }
            Self::InvalidSecurityIssuerId { value } => {
                t_smol!(
                    translate,
                    "error.model.invalid_security_issuer_id",
                    value = value
                )
            }
            Self::InvalidShareClassDesignation { value } => t_smol!(
                translate,
                "error.model.invalid_share_class_designation",
                value = value
            ),
            Self::MissingAdrRatio { value } => {
                t_smol!(translate, "error.model.missing_adr_ratio", value = value)
            }
            Self::InvalidCommodityCode { value } => {
                t_smol!(
                    translate,
                    "error.model.invalid_commodity_code",
                    value = value
                )
            }
            Self::InvalidMic { value } => {
                t_smol!(translate, "error.model.invalid_mic", value = value)
            }
            Self::InvalidCtfConditionId { value } => {
                t_smol!(
                    translate,
                    "error.model.invalid_ctf_condition_id",
                    value = value
                )
            }
            Self::InvalidCtfTokenId { value } => {
                t_smol!(translate, "error.model.invalid_ctf_token_id", value = value)
            }
            Self::InvalidReferenceTermsVersion { value } => t_smol!(
                translate,
                "error.model.invalid_reference_terms_version",
                value = value
            ),
            Self::InvalidTimeZoneId { value } => {
                t_smol!(translate, "error.model.invalid_time_zone_id", value = value)
            }
            Self::InvalidTradingCalendarId { value } => {
                t_smol!(
                    translate,
                    "error.model.invalid_trading_calendar_id",
                    value = value
                )
            }
            Self::UnverifiedTokenOrigin { chain, token } => t_smol!(
                translate,
                "error.model.unverified_token_origin",
                chain = chain,
                token = token
            ),
            Self::InvalidListingContinuityTerms { field } => t_smol!(
                translate,
                "error.model.invalid_listing_continuity_terms",
                field = field
            ),
            // The four arms below state a route a model error carries as its two typed
            // parts, and the display layer keeps one user-visible spelling of a route: the
            // parts are put back into the `VenueRoute` they came from and rendered by
            // the one formatter, rather than upper-cased here as a second rule.
            Self::AmbiguousListingContinuity { venue, scope } => t_smol!(
                translate,
                "error.model.ambiguous_listing_continuity",
                route = route_display_string(&VenueRoute::new(*venue, scope.clone()), translate)
            ),
            Self::ListingNotFound { venue, scope, .. } => t_smol!(
                translate,
                "error.model.listing_not_found",
                route = route_display_string(&VenueRoute::new(*venue, scope.clone()), translate)
            ),
            Self::RouteNotRegistered { venue, scope } => t_smol!(
                translate,
                "error.model.route_not_registered",
                route = route_display_string(&VenueRoute::new(*venue, scope.clone()), translate)
            ),
            Self::ListingRouteMismatch { venue, scope, .. } => t_smol!(
                translate,
                "error.model.listing_route_mismatch",
                route = route_display_string(&VenueRoute::new(*venue, scope.clone()), translate)
            ),
            Self::IdentifierNormalizationMismatch { kind } => {
                let kind = kind.label();
                t_smol!(
                    translate,
                    "error.model.identifier_normalization_mismatch",
                    kind = kind
                )
            }
            Self::ListingIdentifier(error) => t_smol!(
                translate,
                "error.model.listing_identifier",
                cause = error.translated_message(translate)
            ),
            Self::ListingPublication(error) => t_smol!(
                translate,
                "error.model.listing_publication",
                cause = error.translated_message(translate)
            ),
            Self::OutcomeQuestionTooLong { maximum } => t_smol!(
                translate,
                "error.model.outcome_question_too_long",
                maximum = maximum
            ),
            Self::OutcomeGroupIncomplete { group, field } => t_smol!(
                translate,
                "error.model.outcome_group_incomplete",
                group = group,
                field = field
            ),
            Self::OutcomeGroupUnresolved { field } => {
                t_smol!(
                    translate,
                    "error.model.outcome_group_unresolved",
                    field = field
                )
            }
            Self::OutcomeRiskPoolIncomplete { pool, field } => t_smol!(
                translate,
                "error.model.outcome_risk_pool_incomplete",
                pool = pool,
                field = field
            ),
            Self::OutcomeRiskPoolUnresolved { field } => t_smol!(
                translate,
                "error.model.outcome_risk_pool_unresolved",
                field = field
            ),
            Self::OutcomeRiskPoolLineageInvalid { field } => t_smol!(
                translate,
                "error.model.outcome_risk_pool_lineage_invalid",
                field = field
            ),
            Self::OutcomeGroupLineageInvalid { field } => t_smol!(
                translate,
                "error.model.outcome_group_lineage_invalid",
                field = field
            ),
            Self::InstrumentLineageInvalid { field } => {
                t_smol!(
                    translate,
                    "error.model.instrument_lineage_invalid",
                    field = field
                )
            }
            Self::OutcomeSettlementIncomplete { group } => t_smol!(
                translate,
                "error.model.outcome_settlement_incomplete",
                group = group
            ),
            Self::OutcomeSettlementFractionInvalid {
                group,
                fraction_kind,
            } => {
                t_smol!(
                    translate,
                    "error.model.outcome_settlement_fraction_invalid",
                    group = group,
                    fraction_kind = fraction_kind
                )
            }
            Self::OutcomeConversionUnresolved { pool, field } => t_smol!(
                translate,
                "error.model.outcome_conversion_unresolved",
                pool = pool,
                field = field
            ),
            Self::OutcomeConversionCycle => {
                t_smol!(translate, "error.model.outcome_conversion_cycle")
            }
            // The settlement asset rule is one rule with three call sites, so the
            // message names the settlement that was actually refused instead of
            // describing one of the other two.
            Self::UnsupportedSettlementAsset { asset, role } => t_smol!(
                translate,
                match role {
                    SettlementAssetRole::CashDelivery => {
                        "error.model.unsupported_settlement_asset_cash_delivery"
                    }
                    SettlementAssetRole::OutcomeGroup => {
                        "error.model.unsupported_settlement_asset_outcome_group"
                    }
                    SettlementAssetRole::OutcomeRiskPool => {
                        "error.model.unsupported_settlement_asset_outcome_risk_pool"
                    }
                },
                asset = asset
            ),
            Self::ScheduledListingWithoutAvailability => {
                t_smol!(
                    translate,
                    "error.model.scheduled_listing_without_availability"
                )
            }
            Self::ListingNotTradable { status } => {
                t_smol!(
                    translate,
                    "error.model.listing_not_tradable",
                    status = status
                )
            }
            Self::TradingRulesMismatch { field } => {
                t_smol!(
                    translate,
                    "error.model.trading_rules_mismatch",
                    field = field
                )
            }
            Self::UnsortedOrderBook { side } => {
                t_smol!(translate, "error.model.unsorted_order_book", side = side)
            }
            Self::EmptyOrderBookLevel => t_smol!(translate, "error.model.empty_order_book_level"),
            Self::CrossedOrderBook => t_smol!(translate, "error.model.crossed_order_book"),
            Self::InvalidField { field, value } => t_smol!(
                translate,
                "error.model.invalid_field",
                field = field,
                value = value
            ),
            Self::InvalidCandlestickPrices => {
                t_smol!(translate, "error.model.invalid_candlestick_prices")
            }
        }
    }
}

impl TranslatedError for TransportError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::ProxyConfiguration => t_smol!(translate, "error.transport.proxy"),
            Self::HttpClientConfiguration => t_smol!(translate, "error.transport.http_client"),
            Self::ClientPoolUnavailable => t_smol!(translate, "error.transport.client_pool"),
            Self::RequestTimedOut => t_smol!(translate, "error.transport.timeout"),
            Self::ResponseReadFailed => t_smol!(translate, "error.transport.response_read"),
            Self::ResponseBodyTooLarge { limit } => {
                t_smol!(
                    translate,
                    "error.transport.response_too_large",
                    limit = limit
                )
            }
            Self::Cancelled => t_smol!(translate, "error.transport.cancelled"),
            Self::Disconnected => t_smol!(translate, "error.transport.disconnected"),
            Self::SlowSubscriber => t_smol!(translate, "error.transport.slow_subscriber"),
            Self::RouteChanged => t_smol!(translate, "error.transport.route_changed"),
            Self::CredentialChanged => t_smol!(translate, "error.transport.credential_changed"),
            Self::ResyncFailed => t_smol!(translate, "error.transport.resync_failed"),
            Self::Protocol { operation } => {
                t_smol!(translate, "error.transport.protocol", operation = operation)
            }
            Self::SubscriptionRejected { code, message } => {
                t_smol!(
                    translate,
                    "error.transport.rejected",
                    code = code,
                    message = message
                )
            }
            Self::Closed => t_smol!(translate, "error.transport.closed"),
            Self::InvalidSubscriptionCapacity => {
                t_smol!(translate, "error.transport.invalid_capacity")
            }
            Self::InvalidWebSocketEndpoint => {
                t_smol!(translate, "error.transport.invalid_websocket_endpoint")
            }
            Self::WebSocketConnect { failure } => t_smol!(
                translate,
                "error.transport.websocket_connect",
                cause = failure.translated_message(translate)
            ),
            Self::WebSocketIo => t_smol!(translate, "error.transport.websocket_io"),
        }
    }
}

impl TranslatedError for UpgradeFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Timeout => t_smol!(translate, "error.upgrade.timeout"),
            Self::Transport { detail } => {
                t_smol!(translate, "error.upgrade.transport", detail = detail)
            }
            Self::Rejected { status } => {
                t_smol!(translate, "error.upgrade.rejected", status = status)
            }
            Self::Handshake { detail } => {
                t_smol!(translate, "error.upgrade.handshake", detail = detail)
            }
        }
    }
}

impl TranslatedError for KytrixServiceError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::InvalidUrl(detail) => {
                t_smol!(translate, "error.kytrix.invalid_url", detail = detail)
            }
            Self::Connect(detail) => t_smol!(translate, "error.kytrix.connect", detail = detail),
            Self::Authentication(detail) => {
                t_smol!(translate, "error.kytrix.authentication", detail = detail)
            }
            Self::Read(detail) => t_smol!(translate, "error.kytrix.read", detail = detail),
            Self::Write(detail) => t_smol!(translate, "error.kytrix.write", detail = detail),
            Self::Timeout => t_smol!(translate, "error.kytrix.timeout"),
            Self::Closed => t_smol!(translate, "error.kytrix.closed"),
            Self::Protocol(detail) => {
                t_smol!(translate, "error.kytrix.protocol", detail = detail)
            }
            Self::Server(code) => translated_server_error(translate, *code),
            Self::Disconnected => t_smol!(translate, "error.kytrix.disconnected"),
            Self::SlowSubscriber => t_smol!(translate, "error.kytrix.slow_subscriber"),
            Self::AccountChanged => t_smol!(translate, "error.kytrix.account_changed"),
            Self::RouteChanged => t_smol!(translate, "error.kytrix.route_changed"),
        }
    }
}

fn translated_server_error(translate: &Translate, code: ErrorCode) -> SmolStr {
    match code {
        ErrorCode::Unspecified => t_smol!(translate, "error.kytrix.server_unspecified"),
        ErrorCode::RateLimit => t_smol!(translate, "error.kytrix.server_rate_limit"),
        ErrorCode::EntitlementRequired => {
            t_smol!(translate, "error.kytrix.server_entitlement_required")
        }
        ErrorCode::FinancialIndexTopicLimitExceeded => {
            t_smol!(translate, "error.kytrix.server_index_limit")
        }
        ErrorCode::InvalidTopic => t_smol!(translate, "error.kytrix.server_invalid_topic"),
        ErrorCode::AuthenticationUnavailable => {
            t_smol!(translate, "error.kytrix.server_authentication_unavailable")
        }
        ErrorCode::InvalidRequest => t_smol!(translate, "error.kytrix.server_invalid_request"),
        ErrorCode::NotReady => t_smol!(translate, "error.kytrix.server_not_ready"),
        ErrorCode::InvalidArgument => t_smol!(translate, "error.kytrix.server_invalid_argument"),
        ErrorCode::NotFound => t_smol!(translate, "error.kytrix.server_not_found"),
        ErrorCode::QuotaExceeded => t_smol!(translate, "error.kytrix.server_quota_exceeded"),
        ErrorCode::ProtectedAccount => t_smol!(translate, "error.kytrix.server_protected_account"),
        ErrorCode::UpstreamUnavailable => {
            t_smol!(translate, "error.kytrix.server_upstream_unavailable")
        }
        ErrorCode::Timeout => t_smol!(translate, "error.kytrix.server_timeout"),
        ErrorCode::MutationUnknown => t_smol!(translate, "error.kytrix.server_mutation_unknown"),
        ErrorCode::Protocol => t_smol!(translate, "error.kytrix.server_protocol"),
    }
}

impl TranslatedError for XApiError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::InvalidSearchQuery => t_smol!(translate, "widget.x_monitor.invalid_search_query"),
            Self::InvalidUsername => t_smol!(translate, "widget.x_monitor.invalid_username"),
            Self::AccountNotFound => t_smol!(translate, "widget.x_monitor.account_not_found"),
            Self::QuotaExceeded => t_smol!(translate, "widget.x_monitor.quota_exceeded"),
            Self::ProtectedAccount => t_smol!(translate, "widget.x_monitor.protected_account"),
            Self::Authentication => t_smol!(translate, "widget.x_monitor.authentication_failed"),
            Self::AccountChanged => t_smol!(translate, "error.x.account_changed"),
            Self::NotReady => t_smol!(translate, "error.x.not_ready"),
            Self::Service(error) => error.translated_message(translate),
            Self::Protocol(detail) => t_smol!(translate, "error.x.protocol", detail = detail),
            Self::Server(status) => t_smol!(translate, "error.x.server", status = status.as_u16()),
            Self::Timeout => t_smol!(translate, "error.x.timeout"),
            Self::Closed => t_smol!(translate, "error.x.closed"),
        }
    }
}

impl TranslatedError for XMutationUnknown {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Timeout => t_smol!(translate, "error.mutation.timeout"),
            Self::Transport => t_smol!(translate, "error.mutation.transport"),
            Self::ResponseLost => t_smol!(translate, "error.mutation.response_lost"),
            Self::InvalidResponse => t_smol!(translate, "error.mutation.invalid_response"),
            Self::Server(status) => {
                t_smol!(translate, "error.mutation.server", status = status.as_u16())
            }
            Self::AccountChanged => t_smol!(translate, "error.mutation.account_changed"),
            Self::ServiceClosed => t_smol!(translate, "error.mutation.service_closed"),
        }
    }
}

impl TranslatedError for EthereumWalletApiError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::InvalidAddress => t_smol!(translate, "widget.ethereum_wallet.invalid_address"),
            Self::Conflict => t_smol!(translate, "widget.ethereum_wallet.conflict"),
            Self::NotFound => t_smol!(translate, "widget.ethereum_wallet.not_found"),
            Self::Unavailable => t_smol!(translate, "widget.ethereum_wallet.unavailable"),
            Self::Authentication => {
                t_smol!(translate, "widget.ethereum_wallet.authentication_failed")
            }
            Self::Network(detail) => {
                t_smol!(translate, "error.ethereum_wallet.network", detail = detail)
            }
            Self::Service(error) => error.translated_message(translate),
            Self::Transport(error) => error.translated_message(translate),
            Self::Protocol(detail) => {
                t_smol!(translate, "error.ethereum_wallet.protocol", detail = detail)
            }
            Self::ResponseTooLarge => {
                t_smol!(translate, "error.ethereum_wallet.response_too_large")
            }
            Self::InvalidResponse => t_smol!(translate, "error.ethereum_wallet.invalid_response"),
            Self::Server(status) => {
                t_smol!(
                    translate,
                    "error.ethereum_wallet.server",
                    status = status.as_u16()
                )
            }
            Self::Timeout => t_smol!(translate, "error.ethereum_wallet.timeout"),
            Self::Closed => t_smol!(translate, "error.ethereum_wallet.closed"),
            Self::AccountChanged => t_smol!(translate, "error.ethereum_wallet.account_changed"),
            Self::NotReady => t_smol!(translate, "error.ethereum_wallet.not_ready"),
        }
    }
}

impl TranslatedError for EthereumWalletMutationUnknown {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Timeout => t_smol!(translate, "error.mutation.timeout"),
            Self::Transport => t_smol!(translate, "error.mutation.transport"),
            Self::ResponseLost => t_smol!(translate, "error.mutation.response_lost"),
            Self::InvalidResponse => t_smol!(translate, "error.mutation.invalid_response"),
            Self::Server(status) => {
                t_smol!(translate, "error.mutation.server", status = status.as_u16())
            }
            Self::AccountChanged => t_smol!(translate, "error.mutation.account_changed"),
            Self::ServiceClosed => t_smol!(translate, "error.mutation.service_closed"),
        }
    }
}

impl TranslatedError for SolanaWalletApiError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::InvalidAddress => t_smol!(translate, "widget.solana_wallet.invalid_address"),
            Self::Conflict => t_smol!(translate, "widget.solana_wallet.conflict"),
            Self::NotFound => t_smol!(translate, "widget.solana_wallet.not_found"),
            Self::Unavailable => t_smol!(translate, "widget.solana_wallet.unavailable"),
            Self::Authentication => {
                t_smol!(translate, "widget.solana_wallet.authentication_failed")
            }
            Self::Network(detail) => {
                t_smol!(translate, "error.solana_wallet.network", detail = detail)
            }
            Self::Service(error) => error.translated_message(translate),
            Self::Transport(error) => error.translated_message(translate),
            Self::Protocol(detail) => {
                t_smol!(translate, "error.solana_wallet.protocol", detail = detail)
            }
            Self::ResponseTooLarge => t_smol!(translate, "error.solana_wallet.response_too_large"),
            Self::InvalidResponse => t_smol!(translate, "error.solana_wallet.invalid_response"),
            Self::Server(status) => {
                t_smol!(
                    translate,
                    "error.solana_wallet.server",
                    status = status.as_u16()
                )
            }
            Self::Timeout => t_smol!(translate, "error.solana_wallet.timeout"),
            Self::Closed => t_smol!(translate, "error.solana_wallet.closed"),
            Self::AccountChanged => t_smol!(translate, "error.solana_wallet.account_changed"),
            Self::NotReady => t_smol!(translate, "error.solana_wallet.not_ready"),
        }
    }
}

impl TranslatedError for SolanaWalletMutationUnknown {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Timeout => t_smol!(translate, "error.mutation.timeout"),
            Self::Transport => t_smol!(translate, "error.mutation.transport"),
            Self::ResponseLost => t_smol!(translate, "error.mutation.response_lost"),
            Self::InvalidResponse => t_smol!(translate, "error.mutation.invalid_response"),
            Self::Server(status) => {
                t_smol!(translate, "error.mutation.server", status = status.as_u16())
            }
            Self::AccountChanged => t_smol!(translate, "error.mutation.account_changed"),
            Self::ServiceClosed => t_smol!(translate, "error.mutation.service_closed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fmt::Display, rc::Rc};

    use i18n::{language::codes::LanguageCode, translate::Translate};
    use kytrix_service_provider::service::{
        engine::ErrorCode,
        ethereum_wallet::{EthereumWalletApiError, EthereumWalletMutationUnknown},
        solana_wallet::{SolanaWalletApiError, SolanaWalletMutationUnknown},
        x::{XApiError, XMutationUnknown},
    };
    use market_participants_connector::{
        account::error::{AccountCatalogError, AccountError},
        catalog::error::SourceProtocolRejection,
        error::{AuthError, MarketParticipantError, OrderErrorKind},
        market::{
            domain::DisconnectReason,
            error::{MarketError, MarketNetworkError, MarketRequestRejection},
        },
    };
    use market_participants_model::{
        error::{ConversionDirection, ListingPublicationError, ModelError},
        identifier::{NativeSymbol, VenueId, VenueScopeId},
        listing::{
            listing_impl::ListingStatus,
            native::ListingIdentifier,
            route::{VenueListingKey, VenueRoute},
            venue::VenueListingId,
        },
        routing::ListingIdentifierKind,
    };
    use smol_str::{SmolStr, format_smolstr};

    use super::{
        CatalogError, CatalogValidationError, DisplayError, KytrixServiceError,
        ListingErrorPresentation, ListingIdentifierError, LocalizedReason, NetworkError,
        SourceError, TranslatedError, TransportError, UpgradeFailure,
        account_is_catalog_not_loaded, is_catalog_not_loaded, listing_identity_diagnostic,
        market_is_catalog_not_loaded, translated_server_error,
    };
    use crate::{
        protocol::RequestId,
        rpc::RpcCallError,
        venue::{VenueDisplay, route_display_string},
    };

    fn route(scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::Binance,
            VenueScopeId::new(scope).expect("the fixture scope is canonical"),
        )
    }

    /// A typed listing key whose identity is a canonical listing id of one repeated
    /// hex digit, which is all a fixture that compares keys needs.
    fn listing_key(digit: char) -> VenueListingKey {
        let id = format_smolstr!("listing-id/v1/{}", digit.to_string().repeat(64));
        VenueListingKey::new(
            route("spot"),
            VenueListingId::from_identity_value(&id).expect("the fixture listing id is canonical"),
        )
    }

    /// A translation context built from the embedded catalogue, rendering `language`.
    ///
    /// The composition root builds the same handle from the catalogue it loads; a test can
    /// build it with no application at all, which is the point of the injected context.
    fn translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    /// The exact text one failure states, in the shipped English and Chinese catalogues.
    ///
    /// The expected English sentence is the one the mapping's key defines, so an arm that
    /// names the wrong key — or binds a placeholder to the wrong value — turns red here
    /// instead of reaching a user as a sentence about something else. The Chinese render
    /// only has to be a different, non-empty sentence: the message keys are shared, and a
    /// locale that lost one would render the failure as nothing at all.
    fn assert_states<E: TranslatedError>(
        error: &E,
        english: &Translate,
        chinese: &Translate,
        expected: &str,
    ) {
        assert_eq!(
            error.translated_message(english).as_str(),
            expected,
            "the mapping states the catalogue's sentence"
        );
        let translated = error.translated_message(chinese);
        assert!(
            !translated.is_empty(),
            "{expected} states nothing in Chinese"
        );
        assert_ne!(
            translated.as_str(),
            expected,
            "{expected} is not the Chinese sentence"
        );
    }

    /// The failures the interface renders most often state the catalogue's sentences in
    /// both languages.
    ///
    /// These are the sentences a user reads instead of the failure's `Display` text: the
    /// message keys, their placeholders and the values bound to them are pinned here, so a
    /// ported mapping that names another key or loses a placeholder is caught by the text
    /// it would actually render.
    #[test]
    fn the_websocket_and_service_failures_state_the_catalogue_sentences() {
        let en = translate(LanguageCode::En);
        let zh = translate(LanguageCode::ZhCn);

        let refused = TransportError::WebSocketConnect {
            failure: UpgradeFailure::Rejected { status: 403 },
        };
        assert_eq!(
            refused.translated_message(&en).as_str(),
            "The WebSocket connection could not be established: The peer answered 403 instead of \
             switching protocols"
        );
        assert_eq!(
            refused.translated_message(&zh).as_str(),
            "无法建立 WebSocket 连接：对端返回 403，未切换协议"
        );

        let timed_out = NetworkError::WebSocketUpgradeFailed {
            endpoint: SmolStr::new_static("wss://stream.example.test/ws"),
            failure: UpgradeFailure::Timeout,
        };
        assert_eq!(
            timed_out.translated_message(&en).as_str(),
            "The WebSocket upgrade failed for wss://stream.example.test/ws: The connect attempt \
             timed out"
        );
        assert_eq!(
            timed_out.translated_message(&zh).as_str(),
            "WebSocket 升级失败：wss://stream.example.test/ws（连接尝试超时）"
        );

        let rejection = TransportError::SubscriptionRejected {
            code: SmolStr::new_static("4401"),
            message: SmolStr::new_static("token expired"),
        };
        assert_eq!(
            rejection.translated_message(&en).as_str(),
            "The remote endpoint rejected the subscription (4401): token expired"
        );

        let unready = KytrixServiceError::Server(ErrorCode::NotReady);
        assert_eq!(
            unready.translated_message(&en).as_str(),
            "The service is not ready"
        );
        assert_eq!(unready.translated_message(&zh).as_str(), "服务尚未就绪");
    }

    /// Stating a failure through the injected context needs no application at all: this
    /// test never creates one, so the mappings cannot be reaching a global.
    #[test]
    fn an_injected_context_renders_a_failure_without_any_application() {
        let translate = translate(LanguageCode::En);
        let failure = NetworkError::WebSocketUpgradeFailed {
            endpoint: SmolStr::new_static("wss://stream.example.test/ws"),
            failure: UpgradeFailure::Timeout,
        };

        let english = failure.translated_message(&translate);
        assert!(
            english.contains("wss://stream.example.test/ws"),
            "{english}"
        );
        assert!(english.contains("timed out"), "{english}");

        translate.set_language(LanguageCode::ZhCn);
        let chinese = failure.translated_message(&translate);
        assert!(!chinese.is_empty(), "the failure states itself in Chinese");
        assert_ne!(
            chinese, english,
            "the language the injected context carries decides the text"
        );
    }

    /// Why a socket never came up is the whole content of these messages, so the
    /// cause has to survive localization: a key that renders is not enough, since a
    /// placeholder the code never supplies renders as nothing at all.
    #[test]
    fn a_failed_upgrade_should_render_the_cause_it_was_given() {
        let translate = translate(LanguageCode::En);

        let refused = TransportError::WebSocketConnect {
            failure: UpgradeFailure::Rejected { status: 403 },
        }
        .translated_message(&translate);
        assert_eq!(
            refused.as_str(),
            "The WebSocket connection could not be established: The peer answered 403 instead of \
             switching protocols"
        );

        let timed_out = NetworkError::WebSocketUpgradeFailed {
            endpoint: SmolStr::new_static("wss://stream.example.test/ws"),
            failure: UpgradeFailure::Timeout,
        }
        .translated_message(&translate);
        assert_eq!(
            timed_out.as_str(),
            "The WebSocket upgrade failed for wss://stream.example.test/ws: The connect attempt \
             timed out"
        );
    }

    /// The two failures the Longbridge integration added to the market-participant catalogue read as
    /// sentences in every language the interface ships.
    ///
    /// A route that publishes no such adjustment and credentials the venue's own market-data
    /// profile says carry no permission are both refusals a user meets without a diagnostic
    /// sentence beside them: the exact text proves the mapping binds the route a reader copies
    /// and the adjustment the venue publishes.
    #[test]
    fn the_adjustment_and_market_data_failures_should_read_in_every_language() {
        let spot = route("spot");

        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let translate = translate(language);
            let shown_route = route_display_string(&spot, &translate);

            let unsupported = MarketParticipantError::UnsupportedAdjustment {
                route: spot.clone(),
                adjustment: SmolStr::new_static("forward"),
            }
            .translated_message(&translate);
            let expected = if language == LanguageCode::En {
                format!("forward is unsupported for {shown_route}")
            } else {
                format!("{shown_route} 不支持 forward")
            };
            assert_eq!(
                unsupported.as_str(),
                expected,
                "the refusal names the route a reader can copy in {language}"
            );
            assert!(
                !unsupported.contains("binance/spot"),
                "the canonical spelling is machine identity, not what a reader copies in \
                 {language}: {unsupported}"
            );

            let refused = AuthError::NoMarketDataPermission.translated_message(&translate);
            let expected = if language == LanguageCode::En {
                "These credentials have no market-data permission for this market"
            } else {
                "当前凭据没有该市场的行情权限"
            };
            assert_eq!(refused.as_str(), expected);
        }
    }

    /// Every instrument-directory failure the widget framework surfaces renders
    /// through its own message key: a missing key renders as an empty string, so
    /// this is what keeps a refused catalog from showing the user nothing at all.
    #[test]
    fn the_instrument_directory_failures_should_render_their_message_keys() {
        let translate = translate(LanguageCode::En);

        let listing = listing_key('a');
        let identity = listing_identity_diagnostic(&listing, &translate);
        let refused = CatalogError::InvalidCatalog {
            route: route("spot"),
            error: Box::new(CatalogValidationError::ListingRouteMismatch {
                route: route("spot"),
                listing: Box::new(listing.clone()),
            }),
        }
        .translated_message(&translate);
        assert_eq!(
            refused.as_str(),
            format!(
                "The instrument catalog of Binance/SPOT was refused: Listing {identity} does not \
                 belong to the fetched route Binance/SPOT"
            )
        );
        assert!(
            !refused.contains("binance/spot"),
            "a user-visible route names the venue and upper-cases the scope: {refused}"
        );

        let absent = CatalogError::ListingNotFound {
            listing: Box::new(listing),
        }
        .translated_message(&translate);
        assert_eq!(
            absent.as_str(),
            format!("Listing {identity} is absent from the published generation")
        );

        let source = CatalogError::Source(Box::new(SourceError::Decode {
            operation: SmolStr::new_static("binance spot catalog"),
        }))
        .translated_message(&translate);
        assert_eq!(
            source.as_str(),
            "Failed to decode the instrument catalog during binance spot catalog"
        );
    }

    /// A symbol directory failure is about the market participant that publishes the
    /// directory rather than the route that reads it: the sentence names that market
    /// in the reader's language, and the address or cause it failed with stays the
    /// diagnostic detail beside it.
    ///
    /// The failure is rendered in every language the interface ships, because "the
    /// venue is named as the reader's language names it" is a claim about each of
    /// them: the venue's display name is the localised half of the sentence.
    #[test]
    fn a_symbol_directory_failure_should_name_the_market_it_is_about() {
        let fetched = SourceError::SymbolDirectory {
            venue: VenueId::Longbridge,
            reason: SmolStr::new_static("nasdaqlisted.txt answered 503 Service Unavailable"),
        };
        let read = SourceError::SymbolDirectory {
            venue: VenueId::Longbridge,
            reason: SmolStr::new_static("nasdaqlisted.txt starts with an unknown header"),
        };

        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let translate = translate(language);
            let market = VenueId::Longbridge.display_string(&translate);

            let refusal = MarketParticipantError::SymbolDirectoryFetchFailed {
                venue: VenueId::Longbridge,
                reason: SmolStr::new_static("nasdaqlisted.txt answered 503 Service Unavailable"),
            }
            .translated_message(&translate);
            let expected = if language == LanguageCode::En {
                format!(
                    "The {market} symbol directory could not be fetched: nasdaqlisted.txt \
                     answered 503 Service Unavailable"
                )
            } else {
                format!(
                    "无法获取 {market} 的符号目录：nasdaqlisted.txt answered 503 Service Unavailable"
                )
            };
            assert_eq!(refusal.as_str(), expected);

            let directory = fetched.translated_message(&translate);
            let expected = if language == LanguageCode::En {
                format!(
                    "The {market} symbol directory could not be read: nasdaqlisted.txt answered \
                     503 Service Unavailable"
                )
            } else {
                format!(
                    "无法读取 {market} 的符号目录：nasdaqlisted.txt answered 503 Service Unavailable"
                )
            };
            assert_eq!(directory.as_str(), expected);

            let unread = read.translated_message(&translate);
            assert!(
                unread.contains(market.as_str()) && unread.contains("unknown header"),
                "the unread file names the venue and keeps its diagnostic in {language}: {unread}"
            );
        }

        // The two languages really name the venue differently, which is what makes the
        // assertions above about the language rather than about one catalogue's word.
        assert_eq!(
            VenueId::Longbridge
                .display_string(&translate(LanguageCode::En))
                .as_str(),
            "Longbridge"
        );
        assert_eq!(
            VenueId::Longbridge
                .display_string(&translate(LanguageCode::ZhCn))
                .as_str(),
            "长桥"
        );
    }

    /// The two listing-level model failures keep the key they are about, so a
    /// widget never has to fall back to `Display` text to say what failed.
    #[test]
    fn the_listing_model_failures_should_name_the_listing_they_are_about() {
        let en = translate(LanguageCode::En);
        let zh = translate(LanguageCode::ZhCn);

        let listing = listing_key('b');
        let mismatch = ListingPublicationError::SnapshotKeyMismatch {
            listing: listing.clone(),
            snapshot: listing_key('c'),
        };
        let rendered = ModelError::ListingPublication(Box::new(mismatch));
        let identity = listing_identity_diagnostic(&listing, &en);
        assert_eq!(
            rendered.translated_message(&en).as_str(),
            format!(
                "The listing publication is invalid: The snapshot of listing {identity} does not \
                 describe it"
            )
        );

        let identity = listing_identity_diagnostic(&listing, &zh);
        assert_eq!(
            rendered.translated_message(&zh).as_str(),
            format!("挂牌发布无效：挂牌 {identity} 的快照描述的不是该挂牌")
        );
    }

    /// A route a user reads is the user-visible form, in every template
    /// that states one: the venue in the language the catalogue is showing, the scope
    /// in the protocol's own upper case. The canonical route keeps its lower-case
    /// spelling for machine identity, logs and serialization.
    ///
    /// The two spellings are the same identity, so a template that renders the
    /// canonical one looks correct and is wrong: `binance/spot` reads as a different
    /// route from `Binance/SPOT`, and a user who copies one into a support request
    /// names a route that does not exist. Every family of template this module
    /// renders is checked, because the mistake is per-arm and one arm proves nothing
    /// about the others.
    #[test]
    fn a_user_visible_route_should_name_the_venue_and_upper_case_the_scope() {
        let translate = translate(LanguageCode::En);

        let spot = route("spot");
        let hip3 = VenueRoute::new(
            VenueId::Hyperliquid,
            VenueScopeId::new("perp-dex/xyz").expect("the fixture scope is canonical"),
        );

        assert_eq!(
            route_display_string(&spot, &translate).as_str(),
            "Binance/SPOT",
            "the user-visible form is what every template states, venue named first"
        );
        assert_eq!(
            spot.to_string(),
            "binance/spot",
            "the canonical spelling stays lower-case for machine identity"
        );
        assert_eq!(
            route_display_string(&hip3, &translate).as_str(),
            "Hyperliquid/PERP-DEX/XYZ",
            "the scope keeps its own upper case while the venue is the catalogue's"
        );

        let symbol = || {
            ListingIdentifier::Symbol(
                NativeSymbol::new("BTCUSDT").expect("the fixture symbol is canonical"),
            )
        };
        let rendered = [
            DisplayError::ListingNotFound {
                listing: Box::new(listing_key('e')),
            }
            .translated_message(&translate),
            CatalogValidationError::ListingRouteMismatch {
                route: hip3.clone(),
                listing: Box::new(listing_key('e')),
            }
            .translated_message(&translate),
            CatalogError::RouteNotRegistered {
                route: spot.clone(),
            }
            .translated_message(&translate),
            MarketError::UnsupportedCapability {
                route: spot.clone(),
                capability: market_participants_model::routing::CapabilityId::MarketTrade,
            }
            .translated_message(&translate),
            AccountCatalogError::ListingNotFound {
                route: spot.clone(),
                listing: Box::new(listing_key('f')),
            }
            .translated_message(&translate),
            MarketParticipantError::ListingRouteMismatch {
                listing: listing_key('d'),
                route: spot.clone(),
            }
            .translated_message(&translate),
            ListingIdentifierError::NativeIdentifierRouteMismatch {
                route: spot.clone(),
                identifier: symbol(),
                owner: hip3.clone(),
            }
            .translated_message(&translate),
            MarketParticipantError::RouteMismatch {
                request: spot,
                context: hip3.clone(),
            }
            .translated_message(&translate),
            ListingIdentifierError::IdentifierNotFound {
                route: hip3,
                identifier: symbol(),
            }
            .translated_message(&translate),
        ];

        let expected = ["Binance/SPOT", "Hyperliquid/PERP-DEX/XYZ"];
        for message in &rendered {
            assert!(
                expected.iter().any(|route| message.contains(route)),
                "a template states its route the way a user reads it: {message}"
            );
            assert!(
                !message.contains("binance/spot") && !message.contains("hyperliquid/perp-dex"),
                "a user-visible template must not state the canonical spelling: {message}"
            );
            assert!(!message.is_empty(), "every template renders");
        }
    }

    /// A venue named inside a failure is named by the catalogue, once, in the
    /// language the interface is showing.
    ///
    /// The incomplete-credentials failure used to carry the configuration error's own
    /// sentence, which spells the canonical identity in English. Interpolated as the
    /// cause of a template whose own `%{venue}` is the catalogue's name, that named
    /// the same venue twice — `币安 市场参与者账户认证失败：… the credentials of venue
    /// binance are incomplete`. The failure carries the typed venue instead, so this
    /// pins both halves of the promise: the catalogue name is there, and neither the
    /// canonical spelling nor the diagnostic sentence leaks through beside it.
    ///
    /// The scope is the other half of the same promise: a venue that signs its markets
    /// apart can be half configured, and then the sentence has to name the market, because
    /// a reader sent to the venue's page finds two credential sets on it.
    #[test]
    fn an_incomplete_credential_failure_should_name_the_venue_once_in_the_shown_language() {
        let translate = translate(LanguageCode::ZhCn);

        let route = route("spot");
        let rendered = [
            MarketParticipantError::AuthenticationFailed {
                route,
                account: None,
                source: AuthError::IncompleteCredentials {
                    venue: VenueId::Binance,
                    scope: Some(SmolStr::new_static("usd-m-perp")),
                },
            }
            .translated_message(&translate),
            MarketParticipantError::IncompleteCredentials {
                venue: VenueId::Binance,
                scope: Some(SmolStr::new_static("usd-m-perp")),
            }
            .translated_message(&translate),
            MarketParticipantError::UnusableCredentials {
                venue: VenueId::Binance,
            }
            .translated_message(&translate),
        ];

        for message in &rendered {
            assert!(
                message.contains("币安"),
                "the venue is the catalogue's name in the language showing: {message}"
            );
            assert!(
                !message.contains("binance"),
                "the canonical identity does not reach a user: {message}"
            );
            assert!(
                !message.contains("incomplete") && !message.contains("cannot be used"),
                "no diagnostic sentence is interpolated beside the name: {message}"
            );
        }

        // The market is what tells the reader which of the venue's two credential sets to
        // finish, and it is spelled the way the settings page spells its section.
        for message in &rendered[..2] {
            assert!(
                message.contains("USD-M-PERP"),
                "a half-configured market is the market the sentence names: {message}"
            );
        }
        assert!(
            !rendered[2].contains("USD-M-PERP"),
            "a failure that names no market does not invent one: {}",
            rendered[2]
        );
    }

    /// One message that states two routes states both the way a user reads them.
    ///
    /// The listing-identifier mismatch names the route that was looked up and the
    /// route the identifier belongs to, so it is the one template where a single
    /// missed argument leaves a sentence that contradicts itself: the same failure
    /// would say `binance/spot` and `hyperliquid/perp-dex/xyz` at once, and a user who
    /// copies either into a support request names a route that does not exist.
    #[test]
    fn both_routes_of_one_message_should_name_their_venues_and_upper_case_their_scopes() {
        let translate = translate(LanguageCode::En);

        let message = ListingIdentifierError::NativeIdentifierRouteMismatch {
            route: route("spot"),
            identifier: ListingIdentifier::Symbol(
                NativeSymbol::new("BTCUSDT").expect("the fixture symbol is canonical"),
            ),
            owner: VenueRoute::new(
                VenueId::Hyperliquid,
                VenueScopeId::new("perp-dex/xyz").expect("the fixture scope is canonical"),
            ),
        }
        .translated_message(&translate);

        assert_eq!(
            message.as_str(),
            "The native identifier belongs to Hyperliquid/PERP-DEX/XYZ rather than Binance/SPOT"
        );
    }

    /// The identity diagnostic of an unpublished listing is not a display name: it
    /// states the typed key, which is the only thing a refused validation has, and
    /// it carries the route's user-visible form because the template that carries it
    /// is read by a user.
    #[test]
    fn a_listing_identity_diagnostic_should_state_the_typed_key_and_not_a_name() {
        let translate = translate(LanguageCode::En);
        let listing = listing_key('d');
        let expected = format!(
            "{}:{}",
            route_display_string(&listing.route, &translate),
            listing.listing_id.as_str().as_str()
        );
        let diagnostic = listing_identity_diagnostic(&listing, &translate);

        assert_eq!(diagnostic.as_str(), expected);
        assert!(
            diagnostic.as_str().starts_with("Binance/SPOT:"),
            "the diagnostic carries the route as a reader reads it: {diagnostic}"
        );
        assert_ne!(
            diagnostic.as_str(),
            listing_key('d').listing_id.as_str().as_str(),
            "the identity is paired with the route that refused it"
        );
    }

    /// A listing-name read that produced a name is presented by that name and
    /// nothing else, and the answer of the read says so.
    #[test]
    fn a_published_listing_read_should_be_presented_by_its_published_name() {
        let translate = translate(LanguageCode::En);
        let settlement = market_participants_model::asset::CurrencyTerms {
            code: market_participants_model::asset::Iso4217Code::new("USD")
                .expect("the fixture currency is canonical"),
        };
        let code = || {
            market_participants_model::display::asset::AssetDisplayCode::from_terms(
                market_participants_model::display::asset::AssetDisplayTerms::Currency(&settlement),
            )
            .expect("a currency renders")
            .expect("a currency states a code")
        };
        let name = market_participants_model::display::grammar::format_instrument(
            &market_participants_model::display::details::InstrumentDisplayDetails::Spot {
                base: code(),
                quote: code(),
            },
        )
        .expect("the fixture pair renders");
        let presentation = ListingErrorPresentation::of(Ok(name.clone()));

        assert_eq!(
            presentation.display.as_ref().map(|name| name.as_str()),
            Some(name.as_str()),
            "the presentation carries the name the read produced"
        );
        assert_eq!(presentation.reason, LocalizedReason::Resolved);
        assert_eq!(
            presentation.translated_message(&translate),
            SmolStr::new(name.as_str()),
            "a caller renders the name itself, never a fallback beside it"
        );
    }

    /// A listing-name read that produced no name is the localized unknown, and the
    /// failure behind it decides which unknown: a listing the generation never named
    /// is not a generation that holds no value the read could use.
    #[test]
    fn a_refused_listing_read_should_be_presented_by_its_own_localized_unknown() {
        let en = translate(LanguageCode::En);
        let zh = translate(LanguageCode::ZhCn);

        let absent_listing = DisplayError::ListingNotFound {
            listing: Box::new(listing_key('e')),
        };
        let no_terms = DisplayError::MissingAssetTerms {
            asset: Box::new(market_participants_model::asset::AssetId::Currency(
                market_participants_model::asset::CurrencyId::new(
                    market_participants_model::asset::Iso4217Code::new("USD")
                        .expect("the fixture currency is canonical"),
                ),
            )),
        };
        let no_code = DisplayError::AssetDisplayCodeMissing {
            asset: Box::new(market_participants_model::asset::AssetId::Currency(
                market_participants_model::asset::CurrencyId::new(
                    market_participants_model::asset::Iso4217Code::new("EUR")
                        .expect("the fixture currency is canonical"),
                ),
            )),
        };

        let cases = [
            (
                absent_listing,
                LocalizedReason::UnknownListing,
                "Unknown listing",
                "未知挂牌",
            ),
            (
                no_terms,
                LocalizedReason::DisplayUnavailable,
                "Display unavailable",
                "显示不可用",
            ),
            (
                no_code,
                LocalizedReason::DisplayUnavailable,
                "Display unavailable",
                "显示不可用",
            ),
        ];

        for (error, reason, english, chinese) in cases {
            let presentation = ListingErrorPresentation::of(Err(error));

            assert_eq!(presentation.display, None, "no name was resolved");
            assert_eq!(presentation.reason, reason);
            assert_eq!(
                presentation.translated_message(&en).as_str(),
                english,
                "the presentation states the answer of the read"
            );
            assert_eq!(
                presentation.translated_message(&zh).as_str(),
                chinese,
                "the answer is defined by every locale"
            );
        }

        assert_ne!(
            LocalizedReason::UnknownListing
                .translated_message(&en)
                .as_str(),
            LocalizedReason::DisplayUnavailable
                .translated_message(&en)
                .as_str(),
            "the listing miss and the unavailable name are told apart"
        );
    }

    /// The listing presentation and its fallback reason state the catalogue's sentences,
    /// and they are the one mapping a widget renders without touching the venue catalogue.
    #[test]
    fn a_listing_presentation_states_the_catalogue_answers_in_both_languages() {
        let en = translate(LanguageCode::En);
        let zh = translate(LanguageCode::ZhCn);

        assert_eq!(
            LocalizedReason::UnknownListing
                .translated_message(&en)
                .as_str(),
            "Unknown listing"
        );
        assert_eq!(
            LocalizedReason::UnknownListing
                .translated_message(&zh)
                .as_str(),
            "未知挂牌"
        );
        assert_eq!(
            LocalizedReason::Resolved.translated_message(&en).as_str(),
            "Display unavailable",
            "a resolved read is rendered by its name, so its fallback shares the unavailable text"
        );
        assert_eq!(
            LocalizedReason::DisplayUnavailable
                .translated_message(&zh)
                .as_str(),
            "显示不可用"
        );

        let absent = ListingErrorPresentation::of(Err(DisplayError::ListingNotFound {
            listing: Box::new(listing_key('f')),
        }));
        assert_eq!(absent.reason, LocalizedReason::UnknownListing);
        assert_eq!(absent.translated_message(&en).as_str(), "Unknown listing");
        assert_eq!(absent.translated_message(&zh).as_str(), "未知挂牌");
    }

    /// A market request refused because a listing no longer reads states the
    /// published state, which is the refusal itself: the request's own listing id
    /// stays a typed field of the error and never reaches the message as a label.
    #[test]
    fn a_market_request_refusal_should_state_the_state_and_not_the_listing_identity() {
        let translate = translate(LanguageCode::En);

        let rendered = MarketError::InvalidRequest {
            operation: SmolStr::new_static("read listing market data"),
            reason: MarketRequestRejection::ListingNotReadable {
                status: ListingStatus::Delisted,
            },
        }
        .translated_message(&translate);

        assert_eq!(
            rendered.as_str(),
            "The read listing market data request named a listing published as delisted"
        );
        assert!(
            !rendered.contains("listing-id/v1/"),
            "an identity is not a label: {rendered}"
        );
    }

    /// A refused catalog step is stated by the step's own typed answer: the one
    /// answer that is about a listing keeps the listing as a typed field and prints
    /// no id, and the one that carries the model's refusal states that refusal in
    /// the selected language.
    #[test]
    fn a_refused_catalog_step_should_be_stated_by_its_typed_answer() {
        let translate = translate(LanguageCode::En);

        let listing = listing_key('d');
        let incomplete = SourceError::Protocol {
            operation: SmolStr::new_static("load position tiers"),
            reason: SourceProtocolRejection::ListingTermsIncomplete {
                listing: Box::new(listing.clone()),
            },
        }
        .translated_message(&translate);
        assert_eq!(
            incomplete.as_str(),
            "The instrument catalog cannot read the terms of a listing it staged during load \
             position tiers"
        );
        assert!(
            !incomplete.contains(listing.listing_id.as_str().as_str()),
            "the listing stays a typed diagnostic, not a name: {incomplete}"
        );

        let refused_terms = SourceError::Protocol {
            operation: SmolStr::new_static("build margin terms"),
            reason: SourceProtocolRejection::Model {
                error: Box::new(ModelError::ZeroTick),
            },
        }
        .translated_message(&translate);
        assert_eq!(
            refused_terms.as_str(),
            "The instrument catalog refused the build margin terms answer: The price tick must be \
             positive"
        );

        let unexpected = SourceError::Protocol {
            operation: SmolStr::new_static("fetch instrument catalog"),
            reason: SourceProtocolRejection::UnexpectedResponse,
        }
        .translated_message(&translate);
        assert_eq!(
            unexpected.as_str(),
            "The instrument catalog answered fetch instrument catalog with a response it cannot \
             read"
        );

        let venue = SourceError::Protocol {
            operation: SmolStr::new_static("fetch instrument catalog"),
            reason: SourceProtocolRejection::Venue {
                reason: SmolStr::new_static("the venue said no"),
            },
        }
        .translated_message(&translate);
        assert_eq!(
            venue.as_str(),
            "The venue refused the instrument catalog fetch instrument catalog request: the venue \
             said no"
        );
    }

    /// An account query that names a listing of another route is a typed refusal
    /// with a typed identity diagnostic, and the operation an account protocol
    /// failure is about is a fixed name rather than a formatted sentence.
    #[test]
    fn account_refusals_should_state_their_operation_and_their_typed_identity() {
        let translate = translate(LanguageCode::En);

        let listing = listing_key('d');
        let identity = listing_identity_diagnostic(&listing, &translate);
        let mismatch = AccountError::ListingRouteMismatch {
            listing: Box::new(listing),
            route: route("perpetual"),
        }
        .translated_message(&translate);
        assert_eq!(
            mismatch.as_str(),
            format!("Listing {identity} does not belong to route Binance/PERPETUAL")
        );

        let protocol = AccountError::Protocol {
            operation: "fetch balances",
        }
        .translated_message(&translate);
        assert_eq!(
            protocol.as_str(),
            "The account protocol returned an invalid response while performing fetch balances"
        );
    }

    /// The directory's "this route's catalog is not in memory" is recognised through
    /// every wrapper a market or account read reports it in, and nothing else is read
    /// as it: a wait that was mistaken for a failure leaves a pane failed over a route
    /// the next publication brings back, and a failure mistaken for a wait is a pane
    /// that never says what went wrong.
    #[test]
    fn a_directory_that_has_not_loaded_a_catalog_should_be_read_as_a_wait() {
        let not_loaded = CatalogError::CatalogNotLoaded {
            route: route("perp-dex/xyz"),
        };
        let market = MarketError::Catalog(Box::new(not_loaded.clone()));
        let account = AccountError::Catalog(AccountCatalogError::Directory(Box::new(not_loaded)));
        let through_a_service =
            AccountError::Catalog(AccountCatalogError::Source(SourceError::Other {
                reason: SmolStr::new_static("the source step failed"),
            }));

        assert!(
            is_catalog_not_loaded(&market) && market_is_catalog_not_loaded(&market),
            "a market read reports it as the directory's own answer"
        );
        assert!(
            is_catalog_not_loaded(&account) && account_is_catalog_not_loaded(&account),
            "an account read wraps it in its own catalog error"
        );
        assert!(
            !is_catalog_not_loaded(&through_a_service),
            "a source step's own failure is not the directory waiting for a catalog"
        );

        let refusals: [&(dyn std::error::Error + 'static); 4] = [
            &CatalogError::RouteNotRegistered {
                route: route("perp-dex/xyz"),
            },
            &MarketError::Catalog(Box::new(CatalogError::RouteNotRegistered {
                route: route("perp-dex/xyz"),
            })),
            &MarketError::Configuration(SmolStr::new_static("no route publishes a catalog")),
            &SourceError::Network {
                reason: SmolStr::new_static("the socket closed"),
            },
        ];
        for refusal in refusals {
            assert!(
                !is_catalog_not_loaded(refusal),
                "only a missing catalog is a wait: {refusal}"
            );
        }
    }

    /// A conversion reference disagrees with its contract on one named role, and
    /// that role is a lexical label rather than a value: rendering it from the model
    /// label would put an English word inside the Chinese sentence, so each locale
    /// names the role itself.
    ///
    /// All three roles are checked in both languages: the role is the only content
    /// the message has, so a role the mapping routes to the wrong key — or to a key
    /// no locale names — is the whole failure the user reads.
    #[test]
    fn a_conversion_direction_failure_should_name_its_role_in_the_selected_language() {
        let en = translate(LanguageCode::En);
        let zh = translate(LanguageCode::ZhCn);

        let roles = [
            (ConversionDirection::Underlying, "基础资产", "underlying"),
            (ConversionDirection::Quote, "计价资产", "quote"),
            (ConversionDirection::Basis, "价格基准", "basis"),
        ];

        for (direction, chinese_role, english_role) in roles {
            let failure = ModelError::ConversionDirectionMismatch { direction };

            assert_eq!(
                failure.translated_message(&zh).as_str(),
                format!("转换参考与其所属合约的{chinese_role}不一致")
            );
            assert_eq!(
                failure.translated_message(&en).as_str(),
                format!(
                    "The conversion reference disagrees with the contract on its {english_role}"
                )
            );
        }
    }

    /// Every server code the Kytrix service can answer with states its own sentence in
    /// both languages: the code is a closed set, and an arm the mapping forgot would
    /// otherwise render the failure as nothing at all.
    #[test]
    fn every_server_error_code_should_state_its_sentence() {
        let en = translate(LanguageCode::En);
        let zh = translate(LanguageCode::ZhCn);

        let cases = [
            (
                ErrorCode::Unspecified,
                "The Kytrix service returned an unspecified error",
                "Kytrix 服务返回未知错误",
            ),
            (
                ErrorCode::RateLimit,
                "The request rate limit was reached",
                "请求过于频繁，请稍后重试",
            ),
            (
                ErrorCode::EntitlementRequired,
                "The current account is not entitled to this feature",
                "当前账户没有此功能的使用权限",
            ),
            (
                ErrorCode::FinancialIndexTopicLimitExceeded,
                "The selected indices exceed the current limit",
                "所选指数超过当前额度",
            ),
            (
                ErrorCode::InvalidTopic,
                "The requested data topic is invalid",
                "请求的数据主题无效",
            ),
            (
                ErrorCode::AuthenticationUnavailable,
                "Authentication is temporarily unavailable",
                "认证服务暂不可用",
            ),
            (
                ErrorCode::InvalidRequest,
                "The service request is invalid",
                "服务请求无效",
            ),
            (
                ErrorCode::NotReady,
                "The service is not ready",
                "服务尚未就绪",
            ),
            (
                ErrorCode::InvalidArgument,
                "A request argument is invalid",
                "请求参数无效",
            ),
            (
                ErrorCode::NotFound,
                "The requested resource was not found",
                "请求的资源不存在",
            ),
            (
                ErrorCode::QuotaExceeded,
                "The current account quota is full",
                "当前账户额度已满",
            ),
            (
                ErrorCode::ProtectedAccount,
                "Protected accounts are not supported",
                "不支持受保护账户",
            ),
            (
                ErrorCode::UpstreamUnavailable,
                "The upstream service is unavailable",
                "上游服务暂不可用",
            ),
            (
                ErrorCode::Timeout,
                "The server operation timed out",
                "服务端操作超时",
            ),
            (
                ErrorCode::MutationUnknown,
                "The mutation outcome is unknown",
                "变更操作结果未知",
            ),
            (
                ErrorCode::Protocol,
                "The server reported a protocol error",
                "服务端协议错误",
            ),
        ];

        for (code, english, chinese) in cases {
            assert_eq!(translated_server_error(&en, code).as_str(), english);
            assert_eq!(translated_server_error(&zh, code).as_str(), chinese);
            assert_eq!(
                KytrixServiceError::Server(code)
                    .translated_message(&en)
                    .as_str(),
                english,
                "the service failure states the sentence of the code it carries"
            );
        }
    }

    /// Every error family of this module states a sentence of the catalogue, and the
    /// sentence says what the typed failure is about.
    ///
    /// The families are listed here rather than inferred, because this is the mapping
    /// contract P3-1 froze: one representative arm per family proves the family has an
    /// implementation that names a key of the shipped catalogue. The wrappers that
    /// only delegate (`MarketNetworkError`, `AccountNetworkError`) cannot be built
    /// without the runtime that classifies them, and their delegation compiles against
    /// the same trait the covered families implement.
    #[test]
    fn every_error_family_should_state_a_sentence_of_the_catalogue() {
        let en = translate(LanguageCode::En);
        let zh = translate(LanguageCode::ZhCn);
        let spot = route("spot");
        let listing = listing_key('a');
        let identity = listing_identity_diagnostic(&listing, &en);

        assert_states(
            &RpcCallError::<NetworkError>::DeadlineExceeded {
                request_id: RequestId::new(7),
            },
            &en,
            &zh,
            "RPC request 7 exceeded its deadline",
        );
        assert_states(
            &RpcCallError::<NetworkError>::Service {
                request_id: RequestId::new(8),
                source: NetworkError::Io {
                    message: SmolStr::new_static("connection reset"),
                },
            },
            &en,
            &zh,
            "Network I/O error: connection reset",
        );
        assert_states(
            &MarketError::ServiceClosed,
            &en,
            &zh,
            "The market service is closed",
        );
        assert_states(
            &MarketError::Network(MarketNetworkError::from(
                MarketParticipantError::ServiceClosed,
            )),
            &en,
            &zh,
            "The market network request failed: The market participant service is closed",
        );
        assert_states(
            &MarketError::CatalogSource(SourceError::Lost),
            &en,
            &zh,
            "The instrument catalog request was lost before it answered",
        );
        assert_states(
            &MarketError::Protocol {
                operation: SmolStr::new_static("read listing market data"),
            },
            &en,
            &zh,
            "The market protocol returned an invalid response while performing read listing \
             market data",
        );
        assert_states(
            &MarketError::Timeout {
                operation: SmolStr::new_static("read listing market data"),
                timeout_ms: 2500,
            },
            &en,
            &zh,
            "The read listing market data market operation timed out after 2500ms",
        );
        assert_states(
            &AccountError::Catalog(AccountCatalogError::Source(SourceError::Lost)),
            &en,
            &zh,
            "The instrument catalog request was lost before it answered",
        );
        assert_states(
            &AccountError::Model(ModelError::EmptyDecimal),
            &en,
            &zh,
            "The decimal value is empty",
        );
        assert_states(
            &AccountCatalogError::Directory(Box::new(CatalogError::RouteNotRegistered {
                route: spot.clone(),
            })),
            &en,
            &zh,
            "Route Binance/SPOT is not a registered catalog route",
        );
        assert_states(
            &AuthError::SessionExpired,
            &en,
            &zh,
            "The market participant session has expired",
        );
        assert_states(
            &OrderErrorKind::OrderNotFound,
            &en,
            &zh,
            "The order was not found",
        );
        assert_states(
            &MarketParticipantError::Timeout {
                operation: SmolStr::new_static("read balances"),
                timeout_ms: 3000,
            },
            &en,
            &zh,
            "read balances timed out after 3000ms",
        );
        assert_states(
            &MarketParticipantError::RateLimited {
                retry_after_ms: None,
                remaining_quota: None,
            },
            &en,
            &zh,
            "The market participant rate limit was reached",
        );
        assert_states(
            &MarketError::Disconnected(DisconnectReason::Normal),
            &en,
            &zh,
            "The market stream disconnected: The remote endpoint closed the connection normally",
        );
        assert_states(
            &MarketParticipantError::Disconnected(
                market_participants_connector::error::DisconnectReason::Normal,
            ),
            &en,
            &zh,
            "The market participant disconnected: The remote endpoint closed the connection \
             normally",
        );
        assert_states(
            &DisconnectReason::Normal,
            &en,
            &zh,
            "The remote endpoint closed the connection normally",
        );
        assert_states(
            &SourceError::Lost,
            &en,
            &zh,
            "The instrument catalog request was lost before it answered",
        );
        assert_states(
            &CatalogError::RouteNotRegistered {
                route: spot.clone(),
            },
            &en,
            &zh,
            "Route Binance/SPOT is not a registered catalog route",
        );
        assert_states(
            &ListingIdentifierError::IdentifierNotFound {
                route: spot.clone(),
                identifier: ListingIdentifier::Symbol(
                    NativeSymbol::new("BTCUSDT").expect("the fixture symbol is canonical"),
                ),
            },
            &en,
            &zh,
            "The native identifier is not registered on route Binance/SPOT",
        );
        assert_states(
            &ListingPublicationError::IdentifierEchoMismatch {
                kind: ListingIdentifierKind::Symbol,
                value: SmolStr::new_static("BTCUSDT"),
            },
            &en,
            &zh,
            "The published symbol identifier BTCUSDT does not echo the listing's own",
        );
        assert_states(
            &KytrixServiceError::InvalidUrl(SmolStr::new_static("not a URL")),
            &en,
            &zh,
            "The Kytrix service URL is invalid: not a URL",
        );
        assert_states(
            &KytrixServiceError::Closed,
            &en,
            &zh,
            "The Kytrix service connector is closed",
        );
        assert_states(
            &NetworkError::Io {
                message: SmolStr::new_static("connection reset"),
            },
            &en,
            &zh,
            "Network I/O error: connection reset",
        );
        assert_states(
            &TransportError::RequestTimedOut,
            &en,
            &zh,
            "The network request timed out",
        );
        assert_states(
            &UpgradeFailure::Timeout,
            &en,
            &zh,
            "The connect attempt timed out",
        );
        assert_states(
            &XApiError::QuotaExceeded,
            &en,
            &zh,
            "Your current tier's X account quota is full",
        );
        assert_states(
            &XApiError::Service(KytrixServiceError::Closed),
            &en,
            &zh,
            "The Kytrix service connector is closed",
        );
        assert_states(
            &XMutationUnknown::Timeout,
            &en,
            &zh,
            "The mutation timed out and its outcome is unknown",
        );
        assert_states(
            &EthereumWalletApiError::InvalidAddress,
            &en,
            &zh,
            "The Ethereum wallet address is invalid",
        );
        assert_states(
            &EthereumWalletApiError::Transport(TransportError::Cancelled),
            &en,
            &zh,
            "The network operation was cancelled",
        );
        assert_states(
            &EthereumWalletMutationUnknown::Timeout,
            &en,
            &zh,
            "The mutation timed out and its outcome is unknown",
        );
        assert_states(
            &SolanaWalletApiError::InvalidAddress,
            &en,
            &zh,
            "The Solana wallet address is invalid",
        );
        assert_states(
            &SolanaWalletMutationUnknown::Timeout,
            &en,
            &zh,
            "The mutation timed out and its outcome is unknown",
        );
        assert_states(
            &MarketParticipantError::ListingRouteMismatch {
                listing,
                route: spot,
            },
            &en,
            &zh,
            &format!("Listing {identity} does not belong to route Binance/SPOT"),
        );
    }

    /// Every key the mappings of this module look a message up under is one every locale
    /// defines and renders.
    ///
    /// A key no locale defines translates to an empty string rather than failing, so a key
    /// that leaves the locales — as `error.model.empty_identifier` did — reaches the user as
    /// a failure the widget draws with nothing written in it. The keys are read out of this
    /// module's own mappings rather than listed beside them, so an arm that names a new key
    /// is covered by the arm that names it.
    #[test]
    fn every_message_key_the_error_mappings_name_should_be_defined_in_every_locale() {
        let i18n = i18n::global::embedded::load().expect("the embedded locales load");
        let keys = message_keys(include_str!("./error.rs"));

        assert!(
            keys.len() > 100,
            "the mappings should name their messages, and {} was read out of them",
            keys.len()
        );
        assert!(
            keys.contains(&"error.model.empty_identifier".to_owned()),
            "the empty-identifier failure is one of the mapped messages"
        );

        for language in i18n.languages() {
            let defined = i18n
                .keys(language)
                .expect("an embedded language exposes its keys")
                .collect::<Vec<_>>();

            for key in &keys {
                assert!(
                    defined.contains(&key.as_str()),
                    "{key} is named by an error mapping but {language} does not define it"
                );
                assert!(
                    i18n.try_translate(language, key)
                        .is_some_and(|text| !text.is_empty()),
                    "{key} is named by an error mapping but renders empty in {language}"
                );
            }
        }
    }

    /// Every placeholder a message declares is one the mapping that names it supplies.
    ///
    /// A placeholder nothing was bound to renders as the empty string rather than as the
    /// placeholder itself, so a mapping and the message it names can disagree — the code passes
    /// `route` while the message spells `%{venue}` — and what reaches the user is a sentence
    /// with a hole in it instead of a failure they can act on. Each message is therefore
    /// rendered twice: once with exactly the arguments its own mapping binds, and once with
    /// every placeholder of the catalogue bound to a marker of its own. The two renders have to
    /// agree, which is only true when the mapping supplies every placeholder the message
    /// declares, and neither may leave a `%{…}` behind.
    ///
    /// The translation call sites of the widget crates that name this catalogue's messages.
    ///
    /// Almost every mapping of the catalogue is in this module, so the guard above
    /// reads its arguments out of this module's own source. A few families belong to
    /// the widget that states them — the force-order table's caption, the wallet
    /// monitor's failure notices and the sentence one row of the watch list carries —
    /// and nothing about "a message and its arguments cannot drift apart" stops at a
    /// crate boundary: rewording `%{markets}` to `%{market}` on either side would ship a
    /// sentence with a hole in it with both guards green. The sources are listed rather
    /// than discovered because `include_str!` takes a literal, and a file that moves is
    /// a compile error rather than a silently uncovered one.
    ///
    /// The guard reads a call site's key from the invocation itself: the reader takes the
    /// second argument as the key, so an invocation whose key is a constant is read as the
    /// *name* of that constant, which no locale defines and nothing checks. A listed source
    /// therefore names every key where it uses it.
    const WIDGET_MESSAGE_SOURCES: &[&str] = &[
        include_str!("../../widgets/force_order/src/view.rs"),
        include_str!("../../widgets/hyperliquid_wallet/src/model.rs"),
        include_str!("../../widgets/hyperliquid_wallet/src/service.rs"),
        include_str!("../../widgets/watch_list/src/view.rs"),
    ];

    #[test]
    fn every_message_placeholder_should_be_supplied_by_the_mapping_that_names_it() {
        let i18n = i18n::global::embedded::load().expect("the embedded locales load");
        let mut mappings = message_arguments(include_str!("./error.rs"));
        for source in WIDGET_MESSAGE_SOURCES {
            mappings.extend(message_arguments(source));
        }

        assert!(
            mappings.len() > 100,
            "the mappings should bind their arguments, and {} was read out of them",
            mappings.len()
        );

        let mut placeholders = locale_placeholders(include_str!("../../i18n/locales/en.yaml"));
        placeholders.extend(locale_placeholders(include_str!(
            "../../i18n/locales/zh-CN.yaml"
        )));
        placeholders.extend(
            mappings
                .iter()
                .flat_map(|(_, arguments)| arguments.iter().cloned()),
        );
        let markers = placeholders
            .into_iter()
            .map(|name| {
                let marker = format!("<{name}>");
                (name, marker)
            })
            .collect::<Vec<_>>();
        let mut everything = Vec::with_capacity(markers.len());
        for (name, marker) in &markers {
            let value: &dyn Display = marker;
            everything.push((name.as_str(), value));
        }

        for language in i18n.languages() {
            for (key, arguments) in &mappings {
                let mut supplied = Vec::with_capacity(arguments.len());
                for argument in arguments {
                    if let Some((name, marker)) = markers.iter().find(|(name, _)| name == argument)
                    {
                        let value: &dyn Display = marker;
                        supplied.push((name.as_str(), value));
                    }
                }
                let rendered = i18n.translate_with(language, key, &supplied);

                assert!(
                    !rendered.contains("%{"),
                    "{key} renders an unreplaced placeholder in {language}: {rendered}"
                );
                assert_eq!(
                    rendered,
                    i18n.translate_with(language, key, &everything),
                    "{key} declares a placeholder in {language} that the mapping naming it \
                     does not supply"
                );
            }
        }
    }

    /// The attribute that opens the tests of a source file, which is where the production half
    /// of it ends.
    ///
    /// [`production_source`] tells the attribute that opens a test module from the one marking
    /// an accessor a test drives, and the readers of these files slice them with it, so spelling
    /// it out here would be a second one to find.
    const TEST_MODULE_ATTRIBUTE: &str = concat!("#[cfg", "(test)]");

    /// The macros that render one message: the global `t!` the widget crates still use and
    /// the injected `t_smol!` this module uses.
    ///
    /// Both are read by the guards below, because the migration ports the widget crates
    /// after this crate's contract: a reader that only looked for `t!(` would silently stop
    /// covering every mapping this module already converted, and the key or placeholder it
    /// names would be checked by nothing.
    const TRANSLATION_CALLS: &[&str] = &["t_smol!(", "t!("];

    /// The first translation invocation of one source: the offset of its macro name and the
    /// length of that name.
    ///
    /// `t_smol!(` does not contain `t!(` (the `t` is followed by `_`), so each name is searched
    /// for on its own and the earliest one wins. A match only counts where the name starts a
    /// token: `t!(` is a substring of `assert!(` and `debug_assert!(` (and `t_smol!(` of any
    /// name ending in `_t_smol`), so a match preceded by an identifier character is not an
    /// invocation — reading a condition's message as a translation would fail the locale guard
    /// on a key no mapping names.
    fn next_translation_call(source: &str) -> Option<(usize, usize)> {
        TRANSLATION_CALLS
            .iter()
            .flat_map(|call| {
                source
                    .match_indices(call)
                    .filter(|(offset, _)| {
                        !source[..*offset]
                            .chars()
                            .next_back()
                            .is_some_and(|previous| previous.is_alphanumeric() || previous == '_')
                    })
                    .map(|(offset, found)| (offset, found.len()))
            })
            .min_by_key(|(offset, _)| *offset)
    }

    /// The message keys the translation invocations of one source file name, in source order.
    ///
    /// Only the production half of the file is read, sliced the way [`production_source`] slices
    /// it: the keys a test writes for itself are not messages the interface renders. An
    /// invocation is written both on one line and one argument per line, so the literal is
    /// looked for after the context the macro is given rather than assumed to follow it on
    /// the same line. The context is whatever expression the call site hands the macro — a bare
    /// translator or a path such as `services.translate` — which is also what
    /// [`message_arguments`] accepts.
    fn message_keys(source: &str) -> Vec<String> {
        let production = production_source(source);
        let mut keys = Vec::new();
        let mut rest = production;

        while let Some((invocation, length)) = next_translation_call(rest) {
            rest = &rest[invocation + length..];
            let Some(opening) = rest.find('"') else {
                break;
            };
            let context = rest[..opening].trim();
            let Some(context) = context.strip_suffix(',') else {
                continue;
            };
            if context.is_empty() {
                continue;
            }
            let key = &rest[opening + 1..];
            let Some(closing) = key.find('"') else {
                break;
            };
            keys.push(key[..closing].to_owned());
            rest = &key[closing..];
        }

        keys
    }

    /// The key and the argument names of every translation invocation of one source file, in
    /// source order.
    ///
    /// The mapping half of the file is what is read — the slice [`production_source`] takes,
    /// where the first `#[cfg(test)]` would cut the mappings after an accessor a test drives
    /// short — and the invocations are read out of it rather than listed beside it, so an arm
    /// that binds a new argument is covered by the arm that binds it.
    ///
    /// The first argument is the context the macro renders through: any non-empty expression
    /// resolves it — a bare `translate`, `&translate`, or the `services.translate` field path
    /// a widget is handed. That is the same rule [`message_keys`] reads, so both readers
    /// accept and skip the same call sites.
    fn message_arguments(source: &str) -> Vec<(String, Vec<String>)> {
        let production = production_source(source);
        let mut mappings = Vec::new();
        let mut rest = production;

        while let Some((start, length)) = next_translation_call(rest) {
            let Some((body, after)) = invocation_body(&rest[start + length..]) else {
                break;
            };
            rest = after;
            let arguments = top_level_arguments(body);
            // The context the macro is given comes first, and it is an expression rather than
            // a literal: an invocation written without one is not a mapping of this module.
            if arguments.len() < 2 || arguments[0].trim().is_empty() {
                continue;
            }
            let key = arguments[1].trim().trim_matches('"');
            let bound = arguments[2..]
                .iter()
                .filter_map(|argument| bound_name(argument))
                .collect();

            mappings.push((key.to_owned(), bound));
        }

        mappings
    }

    /// Both readers of the widget sources skip the accessor a test drives rather than the
    /// mappings written after it, and both read the global macro and its injected successor
    /// alike.
    ///
    /// `hyperliquid_wallet` marks such an accessor in the middle of its production half,
    /// and both readers sliced each source at the first `#[cfg(test)]` until this rule was
    /// written down: every translation invocation below that accessor was read by nothing, so a
    /// mapping there could lose a placeholder — or name a key no locale defines — with both
    /// guards green. The mappings past the accessor and past the test module are what the two
    /// readers have to disagree about.
    #[test]
    fn a_mapping_after_an_inline_test_accessor_should_still_be_read() {
        let source = concat!(
            "impl Panel {\n",
            "    #[cfg(test)]\n",
            "    fn len(&self) -> usize {\n",
            "        self.order.len()\n",
            "    }\n",
            "}\n",
            "\n",
            "fn label(cx: &App) -> SharedString {\n",
            "    t!(cx, \"widget.panel.waiting\", market = joined_names(cx, &markets))\n",
            "}\n",
            "\n",
            "fn caption(translate: &Translate) -> SmolStr {\n",
            "    t_smol!(translate, \"widget.panel.queued\", market = markets)\n",
            "}\n",
            "\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    fn copy(cx: &App) -> SharedString {\n",
            "        t!(cx, \"widget.panel.unread\", market = markets)\n",
            "    }\n",
            "}\n",
        );

        assert_eq!(
            message_keys(source),
            [
                "widget.panel.waiting".to_owned(),
                "widget.panel.queued".to_owned()
            ]
        );
        assert_eq!(
            message_arguments(source),
            [
                ("widget.panel.waiting".to_owned(), vec!["market".to_owned()]),
                ("widget.panel.queued".to_owned(), vec!["market".to_owned()])
            ]
        );
    }

    /// Both readers accept the context of a translation call as any expression — the bare
    /// translator, a borrow of it, or the `services.translate` field path a widget is handed —
    /// and neither reads an assertion's message as a translation.
    ///
    /// The key reader always took any expression as the context, but the placeholder reader
    /// required a single identifier and skipped the field-path form: a `services.translate`
    /// call site was checked by one guard and silently by neither placeholder check, so a
    /// mapping could lose a placeholder with both guards green. The macro name was also
    /// searched as a plain substring, which made `assert!(` and `debug_assert!(` read as
    /// translation calls. Both cases are pinned here.
    #[test]
    fn both_readers_accept_any_translation_context_and_ignore_assertions() {
        let source = concat!(
            "fn label(cx: &App) -> SharedString {\n",
            "    t!(cx, \"widget.panel.waiting\", market = joined_names(cx, &markets))\n",
            "}\n",
            "\n",
            "fn caption(translate: &Translate) -> SmolStr {\n",
            "    t_smol!(translate, \"widget.panel.queued\", market = markets)\n",
            "}\n",
            "\n",
            "fn body(services: &WidgetServices) -> SmolStr {\n",
            "    t_smol!(services.translate, \"widget.panel.queued\", market = markets)\n",
            "}\n",
            "\n",
            "fn checked(ready: bool) -> bool {\n",
            "    assert!(ready, \"widget.panel.unread\");\n",
            "    debug_assert!(ready, \"widget.panel.unread\");\n",
            "    ready\n",
            "}\n",
        );

        assert_eq!(
            message_keys(source),
            [
                "widget.panel.waiting".to_owned(),
                "widget.panel.queued".to_owned(),
                "widget.panel.queued".to_owned()
            ],
            "the field-path context is read like the bare translator, and the assertions are not"
        );
        assert_eq!(
            message_arguments(source),
            [
                ("widget.panel.waiting".to_owned(), vec!["market".to_owned()]),
                ("widget.panel.queued".to_owned(), vec!["market".to_owned()]),
                ("widget.panel.queued".to_owned(), vec!["market".to_owned()])
            ],
            "the placeholder reader accepts the field-path context instead of skipping it"
        );
    }

    /// The body of one translation invocation and the text that follows its closing parenthesis.
    ///
    /// The scan is aware of string literals and of nested groups, so an argument that is itself
    /// a call — `cause = failure.translated_message(translate)` — does not end the invocation
    /// early.
    fn invocation_body(source: &str) -> Option<(&str, &str)> {
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;

        for (index, character) in source.char_indices() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    in_string = false;
                }
                continue;
            }
            match character {
                '"' => in_string = true,
                '(' => depth += 1,
                ')' if depth == 0 => return Some((&source[..index], &source[index + 1..])),
                ')' => depth -= 1,
                _ => {}
            }
        }

        None
    }

    /// The top-level comma-separated arguments of one invocation body, in source order.
    fn top_level_arguments(body: &str) -> Vec<&str> {
        let mut arguments = Vec::with_capacity(4);
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut start = 0usize;

        for (index, character) in body.char_indices() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    in_string = false;
                }
                continue;
            }
            match character {
                '"' => in_string = true,
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    arguments.push(&body[start..index]);
                    start = index + 1;
                }
                _ => {}
            }
        }
        arguments.push(&body[start..]);

        arguments
    }

    /// The name one argument of a translation invocation binds, when it binds one.
    ///
    /// The macro takes an argument as `name = value` or as a bare local variable of that name,
    /// so an argument that is neither names nothing to check.
    fn bound_name(argument: &str) -> Option<String> {
        let argument = argument.trim();
        let name = argument
            .split_once('=')
            .map_or(argument, |(name, _)| name.trim())
            .trim();
        let mut characters = name.chars();
        let starts = characters
            .next()
            .is_some_and(|first| first.is_alphabetic() || first == '_');
        let rest = characters.all(|character| character.is_alphanumeric() || character == '_');

        (starts && rest).then(|| name.to_owned())
    }

    /// Every placeholder name the locale documents spell, which is the whole set a message in
    /// them can interpolate.
    ///
    /// The translator renders a placeholder nothing supplies as the empty string rather than as
    /// the placeholder itself, so the names a message declares cannot be read back out of a
    /// render: the documents the messages are parsed from are the only place they are written
    /// down.
    fn locale_placeholders(source: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        let mut rest = source;

        while let Some(opening) = rest.find("%{") {
            rest = &rest[opening + "%{".len()..];
            let Some(closing) = rest.find('}') else {
                break;
            };
            names.insert(rest[..closing].to_owned());
            rest = &rest[closing + 1..];
        }

        names
    }

    /// The production half of one source file: everything above the test module it declares.
    ///
    /// The slice is taken at the test module rather than at the first `#[cfg(test)]`, because
    /// the widget sources mark accessors a test drives with that attribute in the middle of
    /// their production half — a slice taken at the first attribute would leave the translation
    /// calls after it unread. A source that declares no test module is read whole.
    fn production_source(source: &str) -> &str {
        let mut offset = 0usize;

        while let Some(attribute) = source[offset..].find(TEST_MODULE_ATTRIBUTE) {
            offset += attribute + TEST_MODULE_ATTRIBUTE.len();
            let declaration = source[offset..].trim_start();
            let opens_a_module = ["mod ", "pub mod ", "pub(crate) mod ", "pub(super) mod "]
                .iter()
                .any(|shape| declaration.starts_with(shape));
            if opens_a_module {
                return &source[..offset - TEST_MODULE_ATTRIBUTE.len()];
            }
        }

        source
    }
}

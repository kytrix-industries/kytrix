use std::convert::Infallible;

use gpui_kit::{App, SharedString};
use i18n::t;
use kytrix_service_provider::service::{
    cryptocurrency_wallet_monitoring::{
        CryptocurrencyWalletMonitoringApiError, CryptocurrencyWalletMonitoringMutationUnknown,
    },
    engine::{ErrorCode, KytrixServiceError},
    x::{XAccountMonitoringApiError, XAccountMonitoringMutationUnknown},
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

/// Renders a typed failure as text in the language selected by the translator.
///
/// Widgets surface these strings directly, so every implementation must map the
/// failure to a message key instead of formatting the `Display` text.
pub trait LocalizedError {
    fn localized_message(&self, cx: &App) -> SharedString;
}

impl LocalizedError for Infallible {
    fn localized_message(&self, _cx: &App) -> SharedString {
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

impl LocalizedReason {
    /// The fallback text one answer is written as.
    ///
    /// A reason is what the interface shows in place of a name, so it is a closed
    /// value every locale defines. A presentation that carries a name is rendered
    /// by that name instead, which is why the resolved answer and the unavailable
    /// display share one sentence: both state that there is no name to show.
    pub fn localized_message(self, cx: &App) -> SharedString {
        match self {
            Self::UnknownListing => t!(cx, "error.display.unknown_listing"),
            Self::Resolved | Self::DisplayUnavailable => {
                t!(cx, "error.display.display_unavailable")
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

impl LocalizedError for ListingErrorPresentation {
    fn localized_message(&self, cx: &App) -> SharedString {
        let Some(display) = &self.display else {
            return self.reason.localized_message(cx);
        };
        SharedString::new(display.as_str())
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
fn listing_identity_diagnostic(listing: &VenueListingKey, cx: &App) -> SmolStr {
    format_smolstr!(
        "{}:{}",
        route_display_string(&listing.route, cx),
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

impl LocalizedError for SourceError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Network { reason } => t!(cx, "error.instrument_catalog.network", reason = reason),
            Self::Authentication { reason } => {
                t!(
                    cx,
                    "error.instrument_catalog.authentication",
                    reason = reason
                )
            }
            Self::RateLimited { .. } => t!(cx, "error.instrument_catalog.rate_limited"),
            Self::Rejected {
                code,
                message,
                retryable,
            } => {
                t!(
                    cx,
                    "error.instrument_catalog.rejected",
                    code = code,
                    message = message,
                    retryable = retryable
                )
            }
            Self::Decode { operation } => {
                t!(cx, "error.instrument_catalog.decode", operation = operation)
            }
            // The reason is classified by the source, so each failure of one step
            // is stated by its own message: a raw source sentence never reaches
            // the user through this arm, and the one diagnostic that names a
            // listing keeps the identity in its typed field instead of the text.
            Self::Protocol { operation, reason } => match reason {
                SourceProtocolRejection::UnexpectedResponse => t!(
                    cx,
                    "error.instrument_catalog.protocol_unexpected_response",
                    operation = operation
                ),
                SourceProtocolRejection::Model { error } => t!(
                    cx,
                    "error.instrument_catalog.protocol_model",
                    operation = operation,
                    cause = error.localized_message(cx)
                ),
                SourceProtocolRejection::ListingTermsIncomplete { .. } => t!(
                    cx,
                    "error.instrument_catalog.protocol_listing_terms",
                    operation = operation
                ),
                SourceProtocolRejection::Venue { reason } => t!(
                    cx,
                    "error.instrument_catalog.protocol_venue",
                    operation = operation,
                    reason = reason
                ),
            },
            Self::Configuration { reason } => {
                t!(
                    cx,
                    "error.instrument_catalog.configuration",
                    reason = reason
                )
            }
            // The one catalog failure that is about a market other than the route's:
            // the directory belongs to the market participant that publishes it, so the venue
            // is named in the reader's language and the address or cause it failed
            // with stays the diagnostic detail.
            Self::SymbolDirectory { venue, reason } => t!(
                cx,
                "error.instrument_catalog.symbol_directory",
                market = venue.display_string(cx),
                detail = reason
            ),
            Self::Lost => t!(cx, "error.instrument_catalog.lost"),
            Self::Other { reason } => t!(cx, "error.instrument_catalog.other", reason = reason),
        }
    }
}

/// Why one staged catalog batch was refused.
///
/// Every variant names the route, the listing key, the typed identifier or the
/// field that failed, so the message is built from those values instead of from
/// a free-form reason string.
impl LocalizedError for CatalogValidationError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::ListingRouteMismatch { route, listing } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
                    "error.catalog_validation.listing_route_mismatch",
                    route = route_display_string(route, cx),
                    listing = listing
                )
            }
            Self::SnapshotKeyMismatch { listing, snapshot } => {
                let listing = listing_identity_diagnostic(listing, cx);
                let snapshot = listing_identity_diagnostic(snapshot, cx);
                t!(
                    cx,
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
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
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
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
                    "error.catalog_validation.instrument_kind_mismatch",
                    listing = listing,
                    declared = declared,
                    kind = kind
                )
            }
            Self::DuplicateNativeIdentifier { route, identifier } => t!(
                cx,
                "error.catalog_validation.duplicate_native_identifier",
                route = route_display_string(route, cx),
                identifier = identifier
            ),
            Self::InvalidListingIdentifiers { listing, kind, .. } => {
                let listing = listing_identity_diagnostic(listing, cx);
                let kind = kind.label();
                t!(
                    cx,
                    "error.catalog_validation.invalid_listing_identifiers",
                    listing = listing,
                    kind = kind
                )
            }
            Self::InvalidTradingRules { listing, .. } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
                    "error.catalog_validation.invalid_trading_rules",
                    listing = listing
                )
            }
            Self::OutcomePriceQuoteMismatch {
                listing,
                expected,
                found,
            } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
                    "error.catalog_validation.outcome_price_quote_mismatch",
                    listing = listing,
                    expected = expected,
                    found = found
                )
            }
            Self::OutcomePriceBasisUnsupported { listing, .. } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
                    "error.catalog_validation.outcome_price_basis_unsupported",
                    listing = listing
                )
            }
            Self::ListingIdentifierRouteMismatch { route, .. } => t!(
                cx,
                "error.catalog_validation.listing_identifier_route_mismatch",
                route = route_display_string(route, cx)
            ),
            Self::IdentifierNormalizationMismatch { listing, kind, .. } => {
                let listing = listing_identity_diagnostic(listing, cx);
                let kind = kind.label();
                t!(
                    cx,
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
                let listing = listing_identity_diagnostic(listing, cx);
                // The sentence has to agree with its count: one missing object is not
                // "1 missing outcome group objects".
                if *count == 1 {
                    t!(
                        cx,
                        "error.catalog_validation.incomplete_outcome_set_one",
                        route = route_display_string(route, cx),
                        listing = listing,
                        kind = kind,
                        count = count
                    )
                } else {
                    t!(
                        cx,
                        "error.catalog_validation.incomplete_outcome_set",
                        route = route_display_string(route, cx),
                        listing = listing,
                        kind = kind,
                        count = count
                    )
                }
            }
            Self::OutcomeGroupIncomplete { group, .. } => t!(
                cx,
                "error.catalog_validation.outcome_group_incomplete",
                group = group
            ),
            Self::OutcomeListingLegUnknown {
                listing,
                group,
                member,
                side,
            } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
                    "error.catalog_validation.outcome_listing_leg_unknown",
                    listing = listing,
                    group = group,
                    member = member,
                    side = side
                )
            }
            Self::ListingEventUnknown { listing, event } => {
                let listing = listing_identity_diagnostic(listing, cx);
                let event = event.as_str();
                t!(
                    cx,
                    "error.catalog_validation.listing_event_unknown",
                    listing = listing,
                    event = event
                )
            }
            Self::OutcomeGroupLineageInvalid { group } => t!(
                cx,
                "error.catalog_validation.outcome_group_lineage_invalid",
                group = group
            ),
            Self::OutcomeRiskPoolIncomplete { pool, .. } => t!(
                cx,
                "error.catalog_validation.outcome_risk_pool_incomplete",
                pool = pool
            ),
            Self::AmbiguousRiskPoolMembership { group } => t!(
                cx,
                "error.catalog_validation.ambiguous_risk_pool_membership",
                group = group
            ),
            Self::OutcomeEventIncomplete { event, .. } => t!(
                cx,
                "error.catalog_validation.outcome_event_incomplete",
                event = event
            ),
            Self::OutcomeRiskPoolLineageInvalid { pool } => t!(
                cx,
                "error.catalog_validation.outcome_risk_pool_lineage_invalid",
                pool = pool
            ),
            Self::InstrumentLineageInvalid { .. } => {
                t!(cx, "error.catalog_validation.instrument_lineage_invalid")
            }
            Self::DuplicateListingId { route, listing_id } => t!(
                cx,
                "error.catalog_validation.duplicate_listing_id",
                route = route_display_string(route, cx),
                listing_id = listing_id
            ),
            Self::InvalidListingContinuityTerms { listing, .. } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
                    "error.catalog_validation.invalid_listing_continuity_terms",
                    listing = listing
                )
            }
            Self::AmbiguousListingContinuity { listing, .. } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
                    "error.catalog_validation.ambiguous_listing_continuity",
                    listing = listing
                )
            }
            Self::ListingWithoutIdentity { listing } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
                    "error.catalog_validation.listing_without_identity",
                    listing = listing
                )
            }
            Self::DisplayRejected { error, .. } => error.localized_message(cx),
        }
    }
}

/// The display publication and read failures.
///
/// The naming gate refuses a whole batch before it is published, so its message
/// names the identity the failure is about — a listing, an asset, a reference or an
/// outcome group — and never renders a name the catalog never published.
impl LocalizedError for DisplayError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Format { error } => SharedString::from(error.to_string()),
            Self::ListingNotFound { listing } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(cx, "error.display.listing_not_found", listing = listing)
            }
            Self::MissingAssetTerms { asset } => {
                t!(cx, "error.display.missing_asset_terms", asset = asset)
            }
            Self::MissingIndexTerms { index } => {
                t!(cx, "error.display.missing_index_terms", index = index)
            }
            Self::MissingExternalReferenceTerms { reference } => t!(
                cx,
                "error.display.missing_external_reference_terms",
                reference = reference
            ),
            Self::MissingReferenceTerms { reference } => t!(
                cx,
                "error.display.missing_reference_terms",
                reference = reference
            ),
            Self::MissingOutcomeTerms { group } => {
                t!(cx, "error.display.missing_outcome_terms", group = group)
            }
            Self::AssetDisplayCodeMissing { asset } => {
                t!(cx, "error.display.missing_asset_code", asset = asset)
            }
            Self::StagedCodeMismatch { asset } => {
                t!(cx, "error.display.staged_code_mismatch", asset = asset)
            }
            Self::InstrumentNameMismatch { instrument } => t!(
                cx,
                "error.display.instrument_name_mismatch",
                instrument = instrument
            ),
            Self::ListingIdentifierEcho { listing, kind, .. } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
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
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
                    "error.display.listing_identifier_mismatch",
                    listing = listing,
                    kind = kind.label(),
                    value = value
                )
            }
            Self::ReferenceDirectionMismatch { reference, field } => t!(
                cx,
                "error.display.reference_direction_mismatch",
                reference = reference,
                field = field
            ),
            Self::GenerationMismatch { expected, actual } => t!(
                cx,
                "error.display.generation_mismatch",
                expected = expected,
                actual = actual
            ),
        }
    }
}

impl LocalizedError for CatalogError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Source(source) | Self::SourceFailed { error: source, .. } => {
                source.localized_message(cx)
            }
            Self::InvalidCatalog { route, error } => t!(
                cx,
                "error.catalog.invalid_catalog",
                route = route_display_string(route, cx),
                cause = error.localized_message(cx)
            ),
            Self::InvalidRegistries { error } => t!(
                cx,
                "error.catalog.invalid_registries",
                cause = error.localized_message(cx)
            ),
            Self::RouteNotRegistered { route } => t!(
                cx,
                "error.catalog.route_not_registered",
                route = route_display_string(route, cx)
            ),
            Self::CatalogNotLoaded { route } => t!(
                cx,
                "error.catalog.catalog_not_loaded",
                route = route_display_string(route, cx)
            ),
            Self::ListingNotFound { listing } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(cx, "error.catalog.listing_not_found", listing = listing)
            }
            Self::ListingRouteMismatch { expected, found } => {
                let expected = route_display_string(expected, cx);
                let found = listing_identity_diagnostic(found, cx);
                t!(
                    cx,
                    "error.catalog.listing_route_mismatch",
                    expected = expected,
                    found = found
                )
            }
            Self::NativeIdentifierNotFound { route, .. } => t!(
                cx,
                "error.catalog.native_identifier_not_found",
                route = route_display_string(route, cx)
            ),
            Self::NativeGroupIdentifierNotFound { route, .. } => t!(
                cx,
                "error.catalog.native_group_identifier_not_found",
                route = route_display_string(route, cx)
            ),
            Self::NativeEventIdentifierNotFound { route, .. } => t!(
                cx,
                "error.catalog.native_event_identifier_not_found",
                route = route_display_string(route, cx)
            ),
            Self::NativeSymbolInvalid { route, symbol } => t!(
                cx,
                "error.catalog.native_symbol_invalid",
                route = route_display_string(route, cx),
                symbol = symbol
            ),
            Self::NativeIdentifierAmbiguous { route, .. } => t!(
                cx,
                "error.catalog.native_identifier_ambiguous",
                route = route_display_string(route, cx)
            ),
            Self::NativeIdentifierRouteMismatch { route, .. } => t!(
                cx,
                "error.catalog.native_identifier_route_mismatch",
                route = route_display_string(route, cx)
            ),
            Self::NativeSymbolNotFound { route, symbol } => t!(
                cx,
                "error.catalog.native_symbol_not_found",
                route = route_display_string(route, cx),
                symbol = symbol
            ),
            Self::NativeSymbolAbsent { listing } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(cx, "error.catalog.native_symbol_absent", listing = listing)
            }
            Self::OutcomeGroupNotFound { group } => {
                t!(cx, "error.catalog.outcome_group_not_found", group = group)
            }
            Self::OutcomeEventNotFound { event } => {
                t!(cx, "error.catalog.outcome_event_not_found", event = event)
            }
            Self::OutcomeRiskPoolNotFound { pool } => {
                t!(cx, "error.catalog.outcome_risk_pool_not_found", pool = pool)
            }
            Self::RouteRevisionChanged { route, .. } => t!(
                cx,
                "error.catalog.route_revision_changed",
                route = route_display_string(route, cx)
            ),
            Self::DuplicateCatalogSourceId { id } => {
                t!(cx, "error.catalog.duplicate_catalog_source_id", id = id)
            }
            Self::InvalidCatalogSourceId { id } => {
                t!(cx, "error.catalog.invalid_catalog_source_id", id = id)
            }
            Self::MissingCatalogSource { route, capability } => t!(
                cx,
                "error.catalog.missing_catalog_source",
                route = route_display_string(route, cx),
                capability = capability
            ),
            Self::CatalogSourceOnNonCatalogRoute { route, id, .. } => t!(
                cx,
                "error.catalog.catalog_source_on_non_catalog_route",
                route = route_display_string(route, cx),
                id = id
            ),
            Self::UnregisteredCatalogSource { route, id } => t!(
                cx,
                "error.catalog.unregistered_catalog_source",
                route = route_display_string(route, cx),
                id = id
            ),
            Self::InconsistentRouteRegistration { route } => t!(
                cx,
                "error.catalog.inconsistent_route_registration",
                route = route_display_string(route, cx)
            ),
            Self::RegisteredRouteMismatch { route } => t!(
                cx,
                "error.catalog.registered_route_mismatch",
                route = route_display_string(route, cx)
            ),
            Self::IdentifierNormalizationMismatch { listing, kind, .. } => {
                let listing = listing_identity_diagnostic(listing, cx);
                let kind = kind.label();
                t!(
                    cx,
                    "error.catalog.identifier_normalization_mismatch",
                    listing = listing,
                    kind = kind
                )
            }
        }
    }
}

impl<E: LocalizedError> LocalizedError for RpcCallError<E> {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::DeadlineExceeded { request_id } => {
                t!(cx, "error.rpc.deadline", request_id = request_id)
            }
            Self::ServiceClosed { request_id } => {
                t!(cx, "error.rpc.service_closed", request_id = request_id)
            }
            Self::ResponseDropped { request_id } => {
                t!(cx, "error.rpc.response_dropped", request_id = request_id)
            }
            Self::Service { source, .. } => source.localized_message(cx),
        }
    }
}

impl LocalizedError for MarketError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Network(source) => {
                t!(
                    cx,
                    "error.market.network",
                    cause = source.localized_message(cx)
                )
            }
            Self::CatalogSource(source) => source.localized_message(cx),
            Self::Catalog(error) => error.localized_message(cx),
            Self::ListingRouteMismatch { listing, route } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
                    "error.market.listing_route_mismatch",
                    listing = listing,
                    route = route_display_string(route, cx)
                )
            }
            Self::UnsupportedCapability { route, capability } => {
                let capability = capability.label();
                t!(
                    cx,
                    "error.market.unsupported_capability",
                    route = route_display_string(route, cx),
                    capability = capability
                )
            }
            Self::Model(error) => error.localized_message(cx),
            Self::Protocol { operation } => {
                t!(cx, "error.market.protocol", operation = operation)
            }
            // The reason is the refusal itself, not a sentence about it: the arm
            // names the state the listing was refused in, so a listing id the
            // request carried cannot be read as part of the message.
            Self::InvalidRequest { operation, reason } => {
                let MarketRequestRejection::ListingNotReadable { status } = reason;
                let status = status.label();
                t!(
                    cx,
                    "error.market.listing_not_readable",
                    operation = operation,
                    status = status
                )
            }
            Self::Disconnected(reason) => t!(
                cx,
                "error.market.disconnected",
                cause = reason.localized_message(cx)
            ),
            Self::ServiceClosed => t!(cx, "error.market.service_closed"),
            Self::Configuration(detail) => t!(cx, "error.market.configuration", detail = detail),
            Self::Timeout {
                operation,
                timeout_ms,
            } => {
                t!(
                    cx,
                    "error.market.timeout",
                    operation = operation,
                    timeout_ms = timeout_ms
                )
            }
        }
    }
}

impl LocalizedError for MarketNetworkError {
    fn localized_message(&self, cx: &App) -> SharedString {
        self.error().localized_message(cx)
    }
}

impl LocalizedError for AccountError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Network(source) => source.localized_message(cx),
            Self::Catalog(source) => source.localized_message(cx),
            Self::Model(error) => error.localized_message(cx),
            Self::UnknownAsset { route, asset } => t!(
                cx,
                "error.market_participants_account.unknown_asset",
                route = route_display_string(route, cx),
                asset = asset
            ),
            Self::Protocol { operation } => {
                t!(
                    cx,
                    "error.market_participants_account.protocol",
                    operation = operation
                )
            }
            // The listing and the route stay typed fields of the error and are
            // drawn as the identity diagnostic a protocol failure is reported by:
            // an account query that names a listing of another route never had a
            // published name to show.
            Self::ListingRouteMismatch { listing, route } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
                    "error.market_participants_account.listing_route_mismatch",
                    listing = listing,
                    route = route_display_string(route, cx)
                )
            }
        }
    }
}

impl LocalizedError for AccountCatalogError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Source(source) => source.localized_message(cx),
            Self::Network(source) => source.localized_message(cx),
            Self::Directory(error) => error.localized_message(cx),
            Self::ListingNotFound { route, listing } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
                    "error.market_participants_account.listing_not_found",
                    route = route_display_string(route, cx),
                    listing = listing
                )
            }
        }
    }
}

impl LocalizedError for AccountNetworkError {
    fn localized_message(&self, cx: &App) -> SharedString {
        self.error().localized_message(cx)
    }
}

impl LocalizedError for MarketParticipantError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::ConnectionFailed {
                route,
                endpoint,
                source,
            } => {
                t!(
                    cx,
                    "error.market_participants.connection_failed",
                    venue = route.venue.display_string(cx),
                    endpoint = endpoint,
                    cause = source.localized_message(cx)
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
                t!(
                    cx,
                    "error.market_participants.venue_request_failed",
                    venue = venue.display_string(cx),
                    endpoint = endpoint,
                    cause = source.localized_message(cx)
                )
            }
            Self::AuthenticationFailed { route, source, .. } => t!(
                cx,
                "error.market_participants.authentication_failed",
                venue = route.venue.display_string(cx),
                cause = source.localized_message(cx)
            ),
            Self::RateLimited { .. } => t!(cx, "error.market_participants.rate_limited"),
            Self::Rejected { code, message, .. } => {
                t!(
                    cx,
                    "error.market_participants.rejected",
                    code = code,
                    message = message
                )
            }
            Self::OrderError { kind, .. } => t!(
                cx,
                "error.market_participants.order_error",
                cause = kind.localized_message(cx)
            ),
            Self::InsufficientBalance {
                asset,
                required,
                available,
                ..
            } => {
                t!(
                    cx,
                    "error.market_participants.insufficient_balance",
                    asset = asset,
                    required = required,
                    available = available
                )
            }
            Self::VenueUnsupported { venue } => t!(
                cx,
                "error.market_participants.venue_unsupported",
                market = venue.display_string(cx)
            ),
            Self::VenuePublishesNoScopeTable { venue } => t!(
                cx,
                "error.market_participants.venue_publishes_no_scope_table",
                market = venue.display_string(cx)
            ),
            Self::UnusableCredentials { venue } => t!(
                cx,
                "error.market_participants.unusable_credentials",
                market = venue.display_string(cx)
            ),
            Self::IncompleteCredentials { venue, scope } => match scope {
                // A venue that signs its markets apart can be half configured, and then the
                // market is what the reader has to finish: a sentence naming only the venue
                // would send them to a page with two credential sets on it. The market is
                // spelled the way the settings page spells it, so the name in the sentence is
                // the name of the section the reader has to open.
                Some(scope) => t!(
                    cx,
                    "error.market_participants.incomplete_market_credentials",
                    market = venue.display_string(cx),
                    scope = credential_scope_display_code(scope)
                ),
                None => t!(
                    cx,
                    "error.market_participants.incomplete_credentials",
                    market = venue.display_string(cx)
                ),
            },
            Self::Configuration(detail) => t!(
                cx,
                "error.market_participants.configuration",
                detail = detail
            ),
            Self::RuntimeUnavailable => t!(cx, "error.market_participants.runtime_unavailable"),
            Self::CapabilityMismatch { expected, actual } => {
                let expected = expected.label();
                let actual = actual.label();
                t!(
                    cx,
                    "error.market_participants.capability_mismatch",
                    expected = expected,
                    actual = actual
                )
            }
            Self::RouteMismatch { request, context } => t!(
                cx,
                "error.market_participants.route_mismatch",
                request = route_display_string(request, cx),
                context = route_display_string(context, cx)
            ),
            Self::ListingRouteMismatch { listing, route } => {
                let listing = listing_identity_diagnostic(listing, cx);
                t!(
                    cx,
                    "error.market_participants.listing_route_mismatch",
                    listing = listing,
                    route = route_display_string(route, cx)
                )
            }
            Self::ListingActionNotAllowed {
                listing, status, ..
            } => {
                let listing = listing_identity_diagnostic(listing, cx);
                let status = status.label();
                t!(
                    cx,
                    "error.market_participants.listing_action_not_allowed",
                    listing = listing,
                    status = status
                )
            }
            Self::ProtocolError {
                operation, reason, ..
            } => {
                t!(
                    cx,
                    "error.market_participants.protocol",
                    operation = operation,
                    reason = reason
                )
            }
            Self::DecodeFailed { operation, .. } => {
                t!(
                    cx,
                    "error.market_participants.decode_failed",
                    operation = operation
                )
            }
            Self::Timeout {
                operation,
                timeout_ms,
            } => {
                t!(
                    cx,
                    "error.market_participants.timeout",
                    operation = operation,
                    timeout_ms = timeout_ms
                )
            }
            Self::UnsupportedCapability { route, capability } => {
                let capability = capability.label();
                t!(
                    cx,
                    "error.market_participants.unsupported_capability",
                    route = route_display_string(route, cx),
                    capability = capability
                )
            }
            Self::InvalidLimit {
                operation,
                requested,
                maximum,
            } => {
                t!(
                    cx,
                    "error.market_participants.invalid_limit",
                    operation = operation,
                    requested = requested,
                    maximum = maximum
                )
            }
            Self::UnsupportedTimeFrame { route, timeframe } => t!(
                cx,
                "error.market_participants.unsupported_time_frame",
                route = route_display_string(route, cx),
                timeframe = timeframe
            ),
            Self::UnsupportedAdjustment { route, adjustment } => t!(
                cx,
                "error.market_participants.unsupported_adjustment",
                route = route_display_string(route, cx),
                adjustment = adjustment
            ),
            // A symbol directory belongs to the market participant that publishes it rather
            // than to the route that reads it, so the venue is stated as a typed
            // market in the reader's language and the address or cause it failed
            // with is the diagnostic detail.
            Self::SymbolDirectoryFetchFailed { venue, reason } => t!(
                cx,
                "error.market_participants.symbol_directory_failed",
                market = venue.display_string(cx),
                detail = reason
            ),
            Self::VenueNotLoaded { venue, scope } => t!(
                cx,
                "error.market_participants.venue_not_loaded",
                venue = venue.display_string(cx),
                scope = scope_display_code(scope)
            ),
            Self::Catalog(error) => error.localized_message(cx),
            Self::MarketModel(error) => error.localized_message(cx),
            Self::Disconnected(reason) => t!(
                cx,
                "error.market_participants.disconnected",
                cause = reason.localized_message(cx)
            ),
            Self::ServiceClosed => t!(cx, "error.market_participants.service_closed"),
            Self::Internal(detail) => t!(cx, "error.market_participants.internal", detail = detail),
        }
    }
}

impl LocalizedError for AuthError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::AccountIdentityMissing => t!(cx, "error.auth.account_identity_missing"),
            Self::InvalidCredentials => t!(cx, "error.auth.invalid_credentials"),
            Self::ApiKeyNotFound => t!(cx, "error.auth.api_key_not_found"),
            Self::NoMarketDataPermission => t!(cx, "error.auth.no_market_data_permission"),
            Self::SignatureInvalid => t!(cx, "error.auth.signature_invalid"),
            Self::TimestampOutOfRange => t!(cx, "error.auth.timestamp_out_of_range"),
            Self::IpNotWhitelisted => t!(cx, "error.auth.ip_not_whitelisted"),
            Self::SessionExpired => t!(cx, "error.auth.session_expired"),
            Self::HandshakeTimeout => t!(cx, "error.auth.handshake_timeout"),
            Self::ResponseMalformed => t!(cx, "error.auth.response_malformed"),
            Self::UnusableCredentials { venue } => t!(
                cx,
                "error.auth.unusable_credentials",
                market = venue.display_string(cx)
            ),
            Self::IncompleteCredentials { venue, scope } => match scope {
                // A venue that signs its markets apart can be half configured, and then the
                // market is what the reader has to finish: a sentence naming only the venue
                // would send them to a page with two credential sets on it. The market is
                // spelled the way the settings page spells it, so the name in the sentence is
                // the name of the section the reader has to open.
                Some(scope) => t!(
                    cx,
                    "error.auth.incomplete_market_credentials",
                    market = venue.display_string(cx),
                    scope = credential_scope_display_code(scope)
                ),
                None => t!(
                    cx,
                    "error.auth.incomplete_credentials",
                    market = venue.display_string(cx)
                ),
            },
            Self::ServiceClosed => t!(cx, "error.auth.service_closed"),
            Self::InvalidAccountIdentity { value } => {
                t!(cx, "error.auth.invalid_account_identity", value = value)
            }
        }
    }
}

impl LocalizedError for OrderErrorKind {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::InvalidPrice { reason } => t!(cx, "error.order.invalid_price", reason = reason),
            Self::InvalidQuantity { reason } => {
                t!(cx, "error.order.invalid_quantity", reason = reason)
            }
            Self::OrderNotFound => t!(cx, "error.order.not_found"),
            Self::OrderAlreadyFilled => t!(cx, "error.order.already_filled"),
            Self::OrderAlreadyCanceled => t!(cx, "error.order.already_canceled"),
            Self::PositionLimitExceeded => t!(cx, "error.order.position_limit_exceeded"),
            Self::MarketSuspended => t!(cx, "error.order.market_suspended"),
            Self::SelfTradeProtection => t!(cx, "error.order.self_trade_protection"),
            Self::Other { code, message } => {
                t!(cx, "error.order.other", code = code, message = message)
            }
        }
    }
}

impl LocalizedError for NetworkError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::DnsResolutionFailed { host } => {
                t!(cx, "error.network.dns_resolution_failed", host = host)
            }
            Self::TcpConnectionFailed { endpoint } => {
                t!(
                    cx,
                    "error.network.tcp_connection_failed",
                    endpoint = endpoint
                )
            }
            Self::TlsHandshakeFailed { endpoint } => {
                t!(
                    cx,
                    "error.network.tls_handshake_failed",
                    endpoint = endpoint
                )
            }
            Self::WebSocketUpgradeFailed { endpoint, failure } => t!(
                cx,
                "error.network.websocket_upgrade_failed",
                endpoint = endpoint,
                cause = failure.localized_message(cx)
            ),
            Self::ProxyConnectionFailed {
                proxy_type,
                message,
            } => {
                t!(
                    cx,
                    "error.network.proxy_connection_failed",
                    proxy_type = proxy_type,
                    message = message
                )
            }
            Self::Io { message } => t!(cx, "error.network.io", message = message),
            Self::InvalidData { operation, reason } => t!(
                cx,
                "error.network.invalid_data",
                operation = operation,
                reason = reason
            ),
            Self::LimitExceeded { operation, limit } => t!(
                cx,
                "error.network.limit_exceeded",
                operation = operation,
                limit = limit
            ),
        }
    }
}

impl LocalizedError for DisconnectReason {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Normal => t!(cx, "error.disconnect.normal"),
            Self::Network(source) => {
                t!(
                    cx,
                    "error.disconnect.network",
                    cause = source.localized_message(cx)
                )
            }
            Self::ServerClosed { code, reason } => t!(
                cx,
                "error.disconnect.server_closed",
                code = code,
                reason = reason
            ),
            Self::ProxyChanged => t!(cx, "error.disconnect.proxy_changed"),
            Self::AuthenticationExpired => t!(cx, "error.disconnect.authentication_expired"),
            Self::HeartbeatTimeout => t!(cx, "error.disconnect.heartbeat_timeout"),
        }
    }
}

impl LocalizedError for MarketParticipantDisconnectReason {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Normal => t!(cx, "error.disconnect.normal"),
            Self::Network(source) => {
                t!(
                    cx,
                    "error.disconnect.network",
                    cause = source.localized_message(cx)
                )
            }
            Self::ServerClosed { code, reason } => t!(
                cx,
                "error.disconnect.server_closed",
                code = code,
                reason = reason
            ),
            Self::ProxyChanged => t!(cx, "error.disconnect.proxy_changed"),
            Self::AuthenticationExpired => t!(cx, "error.disconnect.authentication_expired"),
            Self::HeartbeatTimeout => t!(cx, "error.disconnect.heartbeat_timeout"),
        }
    }
}

impl LocalizedError for ListingIdentifierError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::InvalidValue { kind, value } => {
                let kind = kind.label();
                t!(
                    cx,
                    "error.listing_identifier.invalid_value",
                    kind = kind,
                    value = value
                )
            }
            Self::IdentifierNotFound { route, .. } => t!(
                cx,
                "error.listing_identifier.identifier_not_found",
                route = route_display_string(route, cx)
            ),
            Self::NativeIdentifierRouteMismatch { route, owner, .. } => t!(
                cx,
                "error.listing_identifier.route_mismatch",
                route = route_display_string(route, cx),
                owner = route_display_string(owner, cx)
            ),
            Self::IdentifierProductMismatch { route, .. } => t!(
                cx,
                "error.listing_identifier.product_mismatch",
                route = route_display_string(route, cx)
            ),
            Self::NativeIdentifierAmbiguous { route, matches, .. } => t!(
                cx,
                "error.listing_identifier.ambiguous",
                route = route_display_string(route, cx),
                matches = matches
            ),
        }
    }
}

impl LocalizedError for ListingPublicationError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::SnapshotKeyMismatch { listing, snapshot } => {
                let listing = listing_identity_diagnostic(listing, cx);
                let snapshot = listing_identity_diagnostic(snapshot, cx);
                t!(
                    cx,
                    "error.listing_publication.snapshot_key_mismatch",
                    listing = listing,
                    snapshot = snapshot
                )
            }
            Self::IdentifierEchoMismatch { kind, value } => {
                let kind = kind.label();
                t!(
                    cx,
                    "error.listing_publication.identifier_echo_mismatch",
                    kind = kind,
                    value = value
                )
            }
        }
    }
}

impl LocalizedError for ModelError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::EmptyDecimal => t!(cx, "error.model.empty_decimal"),
            Self::InvalidDecimal => t!(cx, "error.model.invalid_decimal"),
            Self::NegativeDecimal => t!(cx, "error.model.negative_decimal"),
            Self::DecimalPrecisionExceeded { maximum } => {
                t!(cx, "error.model.decimal_precision", maximum = maximum)
            }
            Self::DecimalOverflow => t!(cx, "error.model.decimal_overflow"),
            Self::ZeroTick => t!(cx, "error.model.zero_tick"),
            Self::EmptyIdentifier { field } => {
                t!(cx, "error.model.empty_identifier", field = field)
            }
            Self::ZeroValue { field } => t!(cx, "error.model.zero_value", field = field),
            Self::InvalidTimestamp { field } => {
                t!(cx, "error.model.invalid_timestamp", field = field)
            }
            Self::InvalidTimeRange => t!(cx, "error.model.invalid_time_range"),
            Self::InvalidTimeOrder { field, limit } => t!(
                cx,
                "error.model.invalid_time_order",
                field = field,
                limit = limit
            ),
            Self::TimeWindowTooLarge { maximum } => {
                t!(cx, "error.model.time_window_too_large", maximum = maximum)
            }
            Self::InvalidIso4217Code { kind } => {
                t!(cx, "error.model.invalid_iso_4217_code", kind = kind)
            }
            Self::UnsupportedSecurityForm { form } => {
                t!(cx, "error.model.unsupported_security_form", form = form)
            }
            Self::UnsupportedInstrumentKind { kind } => {
                t!(cx, "error.model.unsupported_instrument_kind", kind = kind)
            }
            Self::FractionOutOfRange => t!(cx, "error.model.fraction_out_of_range"),
            Self::NonPositiveRatio => t!(cx, "error.model.non_positive_ratio"),
            Self::EmptyCollection { field } => {
                t!(cx, "error.model.empty_collection", field = field)
            }
            Self::UnsortedCollection { field } => {
                t!(cx, "error.model.unsorted_collection", field = field)
            }
            Self::DuplicateCollectionEntry { field } => {
                t!(cx, "error.model.duplicate_collection_entry", field = field)
            }
            Self::NonPositiveQuantity { field } => {
                t!(cx, "error.model.non_positive_quantity", field = field)
            }
            Self::ContradictoryDeliveryLegs => t!(cx, "error.model.contradictory_delivery_legs"),
            Self::ContractSizeWithoutAsset => {
                t!(cx, "error.model.contract_size_without_asset")
            }
            Self::ConversionDirectionMismatch { direction } => {
                // The direction is a fixed role name, so it is rendered from the
                // locale instead of from the model label: a Chinese sentence that
                // embedded `underlying`/`quote`/`basis` would switch languages in
                // the middle of the message.
                let direction = match direction {
                    ConversionDirection::Underlying => {
                        t!(cx, "error.model.conversion_direction_underlying")
                    }
                    ConversionDirection::Quote => t!(cx, "error.model.conversion_direction_quote"),
                    ConversionDirection::Basis => t!(cx, "error.model.conversion_direction_basis"),
                };
                t!(
                    cx,
                    "error.model.conversion_direction_mismatch",
                    direction = direction
                )
            }
            Self::ConversionReferenceMismatch => {
                t!(cx, "error.model.conversion_reference_mismatch")
            }
            Self::DeliveryLegMismatch => t!(cx, "error.model.delivery_leg_mismatch"),
            Self::InstrumentTermsMismatch { field } => {
                t!(cx, "error.model.instrument_terms_mismatch", field = field)
            }
            Self::StrikeQuoteMismatch => t!(cx, "error.model.strike_quote_mismatch"),
            Self::IdenticalSpotAssets => t!(cx, "error.model.identical_spot_assets"),
            Self::IndexMismatch { field } => t!(
                cx,
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
                t!(cx, "error.model.index_point_payoff_unsupported")
            }
            // The two assets of a payoff unit mismatch are the ones the contract
            // tracks and the one its unit is denominated in. Rendering them would
            // print canonical identities, which no user-facing message may do, and
            // no display code exists for them yet, so the message states the same
            // failure without them.
            Self::PayoffUnitMismatch { .. } => t!(cx, "error.model.payoff_unit_mismatch"),
            Self::IncompatibleDeliveryMode => t!(cx, "error.model.incompatible_delivery_mode"),
            Self::IncompatiblePnlModel => t!(cx, "error.model.incompatible_pnl_model"),
            Self::IncompatiblePriceBasis => t!(cx, "error.model.incompatible_price_basis"),
            Self::InverseContractSizeMismatch => {
                t!(cx, "error.model.inverse_contract_size_mismatch")
            }
            Self::InverseRequiresCashDelivery => {
                t!(cx, "error.model.inverse_requires_cash_delivery")
            }
            Self::InverseSettlementMismatch => t!(cx, "error.model.inverse_settlement_mismatch"),
            Self::RequiredConversionReference => {
                t!(cx, "error.model.required_conversion_reference")
            }
            Self::SettlementAssetMismatch => t!(cx, "error.model.settlement_asset_mismatch"),
            Self::UnexpectedConversionReference => {
                t!(cx, "error.model.unexpected_conversion_reference")
            }
            Self::UnderlyingWithoutAsset => t!(cx, "error.model.underlying_without_asset"),
            Self::IndexUnderlyingRequiresCashDelivery => {
                t!(cx, "error.model.index_underlying_requires_cash_delivery")
            }
            // The field a quantity-unit mismatch carries is an internal path — the
            // contract unit, a market request field — and no user-facing message may
            // print it, so the rendering names what the quantity has to be instead.
            Self::QuantityUnitMismatch { expected, .. } => t!(
                cx,
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
            Self::InvalidIdentityValue { version, value } => t!(
                cx,
                "error.model.invalid_identity_value",
                version = version,
                value = value
            ),
            Self::InvalidIdentifierNamespace { value } => t!(
                cx,
                "error.model.invalid_identifier_namespace",
                value = value
            ),
            Self::InvalidIdentifierValue { value } => {
                t!(cx, "error.model.invalid_identifier_value", value = value)
            }
            Self::InvalidNativeIdentifier { symbol } => {
                t!(cx, "error.model.invalid_native_identifier", symbol = symbol)
            }
            Self::UnknownVenue { value } => {
                t!(cx, "error.model.unknown_venue", value = value)
            }
            Self::InvalidVenueScopeId { value } => {
                t!(cx, "error.model.invalid_venue_scope_id", value = value)
            }
            Self::InvalidChainId { value } => {
                t!(cx, "error.model.invalid_chain_id", value = value)
            }
            Self::InvalidWalletAddress { value } => {
                t!(cx, "error.model.invalid_wallet_address", value = value)
            }
            Self::InvalidContractAddress { value } => {
                t!(cx, "error.model.invalid_contract_address", value = value)
            }
            Self::InvalidSecurityIssuerId { value } => {
                t!(cx, "error.model.invalid_security_issuer_id", value = value)
            }
            Self::InvalidShareClassDesignation { value } => t!(
                cx,
                "error.model.invalid_share_class_designation",
                value = value
            ),
            Self::MissingAdrRatio { value } => {
                t!(cx, "error.model.missing_adr_ratio", value = value)
            }
            Self::InvalidCommodityCode { value } => {
                t!(cx, "error.model.invalid_commodity_code", value = value)
            }
            Self::InvalidMic { value } => t!(cx, "error.model.invalid_mic", value = value),
            Self::InvalidCtfConditionId { value } => {
                t!(cx, "error.model.invalid_ctf_condition_id", value = value)
            }
            Self::InvalidCtfTokenId { value } => {
                t!(cx, "error.model.invalid_ctf_token_id", value = value)
            }
            Self::InvalidReferenceTermsVersion { value } => t!(
                cx,
                "error.model.invalid_reference_terms_version",
                value = value
            ),
            Self::InvalidTimeZoneId { value } => {
                t!(cx, "error.model.invalid_time_zone_id", value = value)
            }
            Self::InvalidTradingCalendarId { value } => {
                t!(cx, "error.model.invalid_trading_calendar_id", value = value)
            }
            Self::UnverifiedTokenOrigin { chain, token } => t!(
                cx,
                "error.model.unverified_token_origin",
                chain = chain,
                token = token
            ),
            Self::InvalidListingContinuityTerms { field } => t!(
                cx,
                "error.model.invalid_listing_continuity_terms",
                field = field
            ),
            // The four arms below state a route a model error carries as its two typed
            // parts, and the display layer keeps one user-visible spelling of a route: the
            // parts are put back into the `VenueRoute` they came from and rendered by
            // the one formatter, rather than upper-cased here as a second rule.
            Self::AmbiguousListingContinuity { venue, scope } => t!(
                cx,
                "error.model.ambiguous_listing_continuity",
                route = route_display_string(&VenueRoute::new(*venue, scope.clone()), cx)
            ),
            Self::ListingNotFound { venue, scope, .. } => t!(
                cx,
                "error.model.listing_not_found",
                route = route_display_string(&VenueRoute::new(*venue, scope.clone()), cx)
            ),
            Self::RouteNotRegistered { venue, scope } => t!(
                cx,
                "error.model.route_not_registered",
                route = route_display_string(&VenueRoute::new(*venue, scope.clone()), cx)
            ),
            Self::ListingRouteMismatch { venue, scope, .. } => t!(
                cx,
                "error.model.listing_route_mismatch",
                route = route_display_string(&VenueRoute::new(*venue, scope.clone()), cx)
            ),
            Self::IdentifierNormalizationMismatch { kind } => {
                let kind = kind.label();
                t!(
                    cx,
                    "error.model.identifier_normalization_mismatch",
                    kind = kind
                )
            }
            Self::ListingIdentifier(error) => t!(
                cx,
                "error.model.listing_identifier",
                cause = error.localized_message(cx)
            ),
            Self::ListingPublication(error) => t!(
                cx,
                "error.model.listing_publication",
                cause = error.localized_message(cx)
            ),
            Self::OutcomeQuestionTooLong { maximum } => t!(
                cx,
                "error.model.outcome_question_too_long",
                maximum = maximum
            ),
            Self::OutcomeGroupIncomplete { group, field } => t!(
                cx,
                "error.model.outcome_group_incomplete",
                group = group,
                field = field
            ),
            Self::OutcomeGroupUnresolved { field } => {
                t!(cx, "error.model.outcome_group_unresolved", field = field)
            }
            Self::OutcomeRiskPoolIncomplete { pool, field } => t!(
                cx,
                "error.model.outcome_risk_pool_incomplete",
                pool = pool,
                field = field
            ),
            Self::OutcomeRiskPoolUnresolved { field } => t!(
                cx,
                "error.model.outcome_risk_pool_unresolved",
                field = field
            ),
            Self::OutcomeRiskPoolLineageInvalid { field } => t!(
                cx,
                "error.model.outcome_risk_pool_lineage_invalid",
                field = field
            ),
            Self::OutcomeGroupLineageInvalid { field } => t!(
                cx,
                "error.model.outcome_group_lineage_invalid",
                field = field
            ),
            Self::InstrumentLineageInvalid { field } => {
                t!(cx, "error.model.instrument_lineage_invalid", field = field)
            }
            Self::OutcomeSettlementIncomplete { group } => t!(
                cx,
                "error.model.outcome_settlement_incomplete",
                group = group
            ),
            Self::OutcomeSettlementFractionInvalid {
                group,
                fraction_kind,
            } => {
                t!(
                    cx,
                    "error.model.outcome_settlement_fraction_invalid",
                    group = group,
                    fraction_kind = fraction_kind
                )
            }
            Self::OutcomeConversionUnresolved { pool, field } => t!(
                cx,
                "error.model.outcome_conversion_unresolved",
                pool = pool,
                field = field
            ),
            Self::OutcomeConversionCycle => t!(cx, "error.model.outcome_conversion_cycle"),
            // The settlement asset rule is one rule with three call sites, so the
            // message names the settlement that was actually refused instead of
            // describing one of the other two.
            Self::UnsupportedSettlementAsset { asset, role } => t!(
                cx,
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
                t!(cx, "error.model.scheduled_listing_without_availability")
            }
            Self::ListingNotTradable { status } => {
                t!(cx, "error.model.listing_not_tradable", status = status)
            }
            Self::TradingRulesMismatch { field } => {
                t!(cx, "error.model.trading_rules_mismatch", field = field)
            }
            Self::UnsortedOrderBook { side } => {
                t!(cx, "error.model.unsorted_order_book", side = side)
            }
            Self::EmptyOrderBookLevel => t!(cx, "error.model.empty_order_book_level"),
            Self::CrossedOrderBook => t!(cx, "error.model.crossed_order_book"),
            Self::InvalidField { field, value } => t!(
                cx,
                "error.model.invalid_field",
                field = field,
                value = value
            ),
            Self::InvalidCandlestickPrices => t!(cx, "error.model.invalid_candlestick_prices"),
        }
    }
}

impl LocalizedError for TransportError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::ProxyConfiguration => t!(cx, "error.transport.proxy"),
            Self::HttpClientConfiguration => t!(cx, "error.transport.http_client"),
            Self::ClientPoolUnavailable => t!(cx, "error.transport.client_pool"),
            Self::RequestTimedOut => t!(cx, "error.transport.timeout"),
            Self::ResponseReadFailed => t!(cx, "error.transport.response_read"),
            Self::ResponseBodyTooLarge { limit } => {
                t!(cx, "error.transport.response_too_large", limit = limit)
            }
            Self::Cancelled => t!(cx, "error.transport.cancelled"),
            Self::Disconnected => t!(cx, "error.transport.disconnected"),
            Self::SlowSubscriber => t!(cx, "error.transport.slow_subscriber"),
            Self::RouteChanged => t!(cx, "error.transport.route_changed"),
            Self::CredentialChanged => t!(cx, "error.transport.credential_changed"),
            Self::ResyncFailed => t!(cx, "error.transport.resync_failed"),
            Self::Protocol { operation } => {
                t!(cx, "error.transport.protocol", operation = operation)
            }
            Self::SubscriptionRejected { code, message } => {
                t!(
                    cx,
                    "error.transport.rejected",
                    code = code,
                    message = message
                )
            }
            Self::Closed => t!(cx, "error.transport.closed"),
            Self::InvalidSubscriptionCapacity => t!(cx, "error.transport.invalid_capacity"),
            Self::InvalidWebSocketEndpoint => t!(cx, "error.transport.invalid_websocket_endpoint"),
            Self::WebSocketConnect { failure } => t!(
                cx,
                "error.transport.websocket_connect",
                cause = failure.localized_message(cx)
            ),
            Self::WebSocketIo => t!(cx, "error.transport.websocket_io"),
        }
    }
}

impl LocalizedError for UpgradeFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Timeout => t!(cx, "error.upgrade.timeout"),
            Self::Transport { detail } => t!(cx, "error.upgrade.transport", detail = detail),
            Self::Rejected { status } => t!(cx, "error.upgrade.rejected", status = status),
            Self::Handshake { detail } => t!(cx, "error.upgrade.handshake", detail = detail),
        }
    }
}

impl LocalizedError for KytrixServiceError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::InvalidUrl(detail) => t!(cx, "error.kytrix.invalid_url", detail = detail),
            Self::Connect(detail) => t!(cx, "error.kytrix.connect", detail = detail),
            Self::Authentication(detail) => t!(cx, "error.kytrix.authentication", detail = detail),
            Self::Read(detail) => t!(cx, "error.kytrix.read", detail = detail),
            Self::Write(detail) => t!(cx, "error.kytrix.write", detail = detail),
            Self::Timeout => t!(cx, "error.kytrix.timeout"),
            Self::Closed => t!(cx, "error.kytrix.closed"),
            Self::Protocol(detail) => t!(cx, "error.kytrix.protocol", detail = detail),
            Self::Server(code) => localized_server_error(cx, *code),
            Self::Disconnected => t!(cx, "error.kytrix.disconnected"),
            Self::SlowSubscriber => t!(cx, "error.kytrix.slow_subscriber"),
            Self::AccountChanged => t!(cx, "error.kytrix.account_changed"),
            Self::RouteChanged => t!(cx, "error.kytrix.route_changed"),
        }
    }
}

fn localized_server_error(cx: &App, code: ErrorCode) -> SharedString {
    match code {
        ErrorCode::Unspecified => t!(cx, "error.kytrix.server_unspecified"),
        ErrorCode::RateLimit => t!(cx, "error.kytrix.server_rate_limit"),
        ErrorCode::EntitlementRequired => t!(cx, "error.kytrix.server_entitlement_required"),
        ErrorCode::FinancialIndexTopicLimitExceeded => t!(cx, "error.kytrix.server_index_limit"),
        ErrorCode::InvalidTopic => t!(cx, "error.kytrix.server_invalid_topic"),
        ErrorCode::AuthenticationUnavailable => {
            t!(cx, "error.kytrix.server_authentication_unavailable")
        }
        ErrorCode::InvalidRequest => t!(cx, "error.kytrix.server_invalid_request"),
        ErrorCode::NotReady => t!(cx, "error.kytrix.server_not_ready"),
        ErrorCode::InvalidArgument => t!(cx, "error.kytrix.server_invalid_argument"),
        ErrorCode::NotFound => t!(cx, "error.kytrix.server_not_found"),
        ErrorCode::QuotaExceeded => t!(cx, "error.kytrix.server_quota_exceeded"),
        ErrorCode::ProtectedAccount => t!(cx, "error.kytrix.server_protected_account"),
        ErrorCode::UpstreamUnavailable => t!(cx, "error.kytrix.server_upstream_unavailable"),
        ErrorCode::Timeout => t!(cx, "error.kytrix.server_timeout"),
        ErrorCode::MutationUnknown => t!(cx, "error.kytrix.server_mutation_unknown"),
        ErrorCode::Protocol => t!(cx, "error.kytrix.server_protocol"),
    }
}

impl LocalizedError for XAccountMonitoringApiError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::InvalidSearchQuery => t!(cx, "widget.x_monitor.invalid_search_query"),
            Self::InvalidUsername => t!(cx, "widget.x_monitor.invalid_username"),
            Self::AccountNotFound => t!(cx, "widget.x_monitor.account_not_found"),
            Self::QuotaExceeded => t!(cx, "widget.x_monitor.quota_exceeded"),
            Self::ProtectedAccount => t!(cx, "widget.x_monitor.protected_account"),
            Self::Authentication => t!(cx, "widget.x_monitor.authentication_failed"),
            Self::AccountChanged => t!(cx, "error.x.account_changed"),
            Self::NotReady => t!(cx, "error.x.not_ready"),
            Self::Service(error) => error.localized_message(cx),
            Self::Protocol(detail) => t!(cx, "error.x.protocol", detail = detail),
            Self::Server(status) => t!(cx, "error.x.server", status = status.as_u16()),
            Self::Timeout => t!(cx, "error.x.timeout"),
            Self::Closed => t!(cx, "error.x.closed"),
        }
    }
}

impl LocalizedError for XAccountMonitoringMutationUnknown {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Timeout => t!(cx, "error.mutation.timeout"),
            Self::Transport => t!(cx, "error.mutation.transport"),
            Self::ResponseLost => t!(cx, "error.mutation.response_lost"),
            Self::InvalidResponse => t!(cx, "error.mutation.invalid_response"),
            Self::Server(status) => t!(cx, "error.mutation.server", status = status.as_u16()),
            Self::AccountChanged => t!(cx, "error.mutation.account_changed"),
            Self::ServiceClosed => t!(cx, "error.mutation.service_closed"),
        }
    }
}

impl LocalizedError for CryptocurrencyWalletMonitoringApiError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::InvalidAddress => t!(
                cx,
                "widget.cryptocurrency_wallet_monitoring.invalid_address"
            ),
            Self::Conflict => t!(cx, "widget.cryptocurrency_wallet_monitoring.conflict"),
            Self::NotFound => t!(cx, "widget.cryptocurrency_wallet_monitoring.not_found"),
            Self::Unavailable => t!(cx, "widget.cryptocurrency_wallet_monitoring.unavailable"),
            Self::Authentication => t!(
                cx,
                "widget.cryptocurrency_wallet_monitoring.authentication_failed"
            ),
            Self::Network(detail) => t!(
                cx,
                "error.cryptocurrency_wallet_monitoring.network",
                detail = detail
            ),
            Self::Service(error) => error.localized_message(cx),
            Self::Transport(error) => error.localized_message(cx),
            Self::Protocol(detail) => t!(
                cx,
                "error.cryptocurrency_wallet_monitoring.protocol",
                detail = detail
            ),
            Self::ResponseTooLarge => t!(
                cx,
                "error.cryptocurrency_wallet_monitoring.response_too_large"
            ),
            Self::InvalidResponse => t!(
                cx,
                "error.cryptocurrency_wallet_monitoring.invalid_response"
            ),
            Self::Server(status) => t!(
                cx,
                "error.cryptocurrency_wallet_monitoring.server",
                status = status.as_u16()
            ),
            Self::Timeout => t!(cx, "error.cryptocurrency_wallet_monitoring.timeout"),
            Self::Closed => t!(cx, "error.cryptocurrency_wallet_monitoring.closed"),
            Self::AccountChanged => {
                t!(cx, "error.cryptocurrency_wallet_monitoring.account_changed")
            }
            Self::NotReady => t!(cx, "error.cryptocurrency_wallet_monitoring.not_ready"),
        }
    }
}

impl LocalizedError for CryptocurrencyWalletMonitoringMutationUnknown {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Timeout => t!(cx, "error.mutation.timeout"),
            Self::Transport => t!(cx, "error.mutation.transport"),
            Self::ResponseLost => t!(cx, "error.mutation.response_lost"),
            Self::InvalidResponse => t!(cx, "error.mutation.invalid_response"),
            Self::Server(status) => t!(cx, "error.mutation.server", status = status.as_u16()),
            Self::AccountChanged => t!(cx, "error.mutation.account_changed"),
            Self::ServiceClosed => t!(cx, "error.mutation.service_closed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fmt::Display};

    use gpui_kit::TestAppContext;
    use i18n::language::codes::LanguageCode;
    use market_participants_connector::{
        account::error::{AccountCatalogError, AccountError},
        catalog::error::SourceProtocolRejection,
        error::MarketParticipantError,
        market::error::{MarketError, MarketRequestRejection},
    };
    use market_participants_model::{
        error::{ConversionDirection, ModelError},
        identifier::{VenueId, VenueScopeId},
        listing::{
            listing_impl::ListingStatus,
            route::{VenueListingKey, VenueRoute},
            venue::VenueListingId,
        },
    };
    use smol_str::{SmolStr, format_smolstr};

    use super::{
        CatalogError, CatalogValidationError, DisplayError, ListingErrorPresentation,
        ListingIdentifierError, LocalizedError, LocalizedReason, NetworkError, SourceError,
        TransportError, UpgradeFailure, account_is_catalog_not_loaded, is_catalog_not_loaded,
        listing_identity_diagnostic, market_is_catalog_not_loaded,
    };
    use crate::venue::{VenueDisplay, route_display_string};

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

    /// Why a socket never came up is the whole content of these messages, so the
    /// cause has to survive localization: a key that renders is not enough, since a
    /// placeholder the code never supplies renders as nothing at all.
    #[test]
    fn a_failed_upgrade_should_render_the_cause_it_was_given() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

            let refused = TransportError::WebSocketConnect {
                failure: UpgradeFailure::Rejected { status: 403 },
            }
            .localized_message(cx)
            .to_string();
            assert!(refused.contains("403"), "{refused}");

            let timed_out = NetworkError::WebSocketUpgradeFailed {
                endpoint: SmolStr::new_static("wss://stream.example.test/ws"),
                failure: UpgradeFailure::Timeout,
            }
            .localized_message(cx)
            .to_string();
            assert!(
                timed_out.contains("wss://stream.example.test/ws")
                    && timed_out.contains("timed out"),
                "{timed_out}"
            );
        });
    }

    /// The two failures the Longbridge integration added to the market-participant catalogue read as
    /// sentences in every language the interface ships.
    ///
    /// A route that publishes no such adjustment and credentials the venue's own market-data
    /// profile says carry no permission are both refusals a user meets without a diagnostic
    /// sentence beside them: the key guard proves the message is defined, and this proves the arm
    /// that renders it binds what the message declares — the route a reader copies and the
    /// adjustment the venue publishes.
    #[test]
    fn the_adjustment_and_market_data_failures_should_read_in_every_language() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

            let spot = route("spot");

            for language in [LanguageCode::En, LanguageCode::ZhCn] {
                cx.set_global(language);

                let unsupported = MarketParticipantError::UnsupportedAdjustment {
                    route: spot.clone(),
                    adjustment: SmolStr::new_static("forward"),
                }
                .localized_message(cx)
                .to_string();
                let route = route_display_string(&spot, cx);

                assert!(
                    !unsupported.is_empty(),
                    "the refusal says nothing in {language}"
                );
                assert!(
                    unsupported.contains(route.as_ref()),
                    "the refusal names the route a reader can copy in {language}: {unsupported}"
                );
                assert!(
                    !unsupported.contains("binance/spot"),
                    "the canonical spelling is machine identity, not what a reader copies in \
                     {language}: {unsupported}"
                );
                assert!(
                    unsupported.contains("forward"),
                    "the refusal names the adjustment in {language}: {unsupported}"
                );

                let refused =
                    market_participants_connector::error::AuthError::NoMarketDataPermission
                        .localized_message(cx)
                        .to_string();
                assert!(
                    !refused.is_empty(),
                    "the permission refusal says nothing in {language}"
                );
            }
        });
    }

    /// Every instrument-directory failure the widget framework surfaces renders
    /// through its own message key: a missing key renders as an empty string, so
    /// this is what keeps a refused catalog from showing the user nothing at all.
    #[test]
    fn the_instrument_directory_failures_should_render_their_message_keys() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

            let listing = listing_key('a');
            let refused = CatalogError::InvalidCatalog {
                route: route("spot"),
                error: Box::new(CatalogValidationError::ListingRouteMismatch {
                    route: route("spot"),
                    listing: Box::new(listing.clone()),
                }),
            }
            .localized_message(cx)
            .to_string();
            assert!(refused.contains("Binance/SPOT"), "{refused}");
            assert!(
                !refused.contains("binance/spot"),
                "a user-visible route names the venue and upper-cases the scope: {refused}"
            );
            assert!(
                refused.contains(listing.listing_id.as_str().as_str()),
                "the refused listing has to be named: {refused}"
            );

            let absent = CatalogError::ListingNotFound {
                listing: Box::new(listing),
            }
            .localized_message(cx)
            .to_string();
            assert!(absent.contains("Binance/SPOT"), "{absent}");

            let source = CatalogError::Source(Box::new(SourceError::Decode {
                operation: SmolStr::new_static("binance spot catalog"),
            }))
            .localized_message(cx)
            .to_string();
            assert!(
                source.contains("binance spot catalog") && !source.is_empty(),
                "{source}"
            );
        });
    }

    /// A symbol directory failure is about the market participant that publishes the
    /// directory rather than the route that reads it: the sentence names that market
    /// in the reader's language, and the address or cause it failed with stays the
    /// diagnostic detail beside it.
    ///
    /// The failure is rendered in every language the interface ships, because "the
    /// venue is named as the reader's language names it" is a claim about each of
    /// them: the venue's display name is the localised half of the sentence, and a
    /// locale that lost the message would render the failure as nothing at all.
    #[test]
    fn a_symbol_directory_failure_should_name_the_market_it_is_about() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

            let fetched = SourceError::SymbolDirectory {
                venue: VenueId::Longbridge,
                reason: SmolStr::new_static("nasdaqlisted.txt answered 503 Service Unavailable"),
            };
            let read = SourceError::SymbolDirectory {
                venue: VenueId::Longbridge,
                reason: SmolStr::new_static("nasdaqlisted.txt starts with an unknown header"),
            };

            for language in [LanguageCode::En, LanguageCode::ZhCn] {
                cx.set_global(language);
                let market = VenueId::Longbridge.display_string(cx);

                let refusal = MarketParticipantError::SymbolDirectoryFetchFailed {
                    venue: VenueId::Longbridge,
                    reason: SmolStr::new_static(
                        "nasdaqlisted.txt answered 503 Service Unavailable",
                    ),
                }
                .localized_message(cx)
                .to_string();
                assert!(
                    !refusal.is_empty(),
                    "the refusal says nothing in {language}"
                );
                assert!(
                    refusal.contains(market.as_ref()),
                    "the refusal names the venue in {language} as {market}: {refusal}"
                );
                assert!(
                    refusal.contains("503 Service Unavailable"),
                    "the refusal keeps the diagnostic it failed with in {language}: {refusal}"
                );

                let directory = fetched.localized_message(cx).to_string();
                assert!(
                    !directory.is_empty(),
                    "the directory says nothing in {language}"
                );
                assert!(
                    directory.contains(market.as_ref()),
                    "the directory names the venue in {language} as {market}: {directory}"
                );
                assert!(
                    directory.contains("503 Service Unavailable"),
                    "the directory keeps the diagnostic it failed with in {language}: {directory}"
                );

                let unread = read.localized_message(cx).to_string();
                assert!(
                    !unread.is_empty(),
                    "the unread file says nothing in {language}"
                );
                assert!(
                    unread.contains(market.as_ref()),
                    "the unread file names the venue in {language} as {market}: {unread}"
                );
                assert!(
                    unread.contains("unknown header"),
                    "the unread file keeps the diagnostic in {language}: {unread}"
                );
            }

            // The two languages really name the venue differently, which is what makes the
            // assertions above about the language rather than about one catalogue's word.
            cx.set_global(LanguageCode::En);
            assert_eq!(
                VenueId::Longbridge.display_string(cx).as_ref(),
                "Longbridge"
            );
            cx.set_global(LanguageCode::ZhCn);
            assert_eq!(VenueId::Longbridge.display_string(cx).as_ref(), "长桥");
        });
    }

    /// The two listing-level model failures keep the key they are about, so a
    /// widget never has to fall back to `Display` text to say what failed.
    #[test]
    fn the_listing_model_failures_should_name_the_listing_they_are_about() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

            let listing = listing_key('b');
            let mismatch =
                market_participants_model::error::ListingPublicationError::SnapshotKeyMismatch {
                    listing: listing.clone(),
                    snapshot: listing_key('c'),
                };
            let rendered = market_participants_model::error::ModelError::ListingPublication(
                Box::new(mismatch),
            )
            .localized_message(cx)
            .to_string();

            assert!(
                rendered.contains(listing.listing_id.as_str().as_str()),
                "{rendered}"
            );
            assert!(!rendered.is_empty(), "{rendered}");
        });
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
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

            let spot = route("spot");
            let hip3 = VenueRoute::new(
                VenueId::Hyperliquid,
                VenueScopeId::new("perp-dex/xyz").expect("the fixture scope is canonical"),
            );

            assert_eq!(
                route_display_string(&spot, cx).as_str(),
                "Binance/SPOT",
                "the user-visible form is what every template states, venue named first"
            );
            assert_eq!(
                spot.to_string(),
                "binance/spot",
                "the canonical spelling stays lower-case for machine identity"
            );
            assert_eq!(
                route_display_string(&hip3, cx).as_str(),
                "Hyperliquid/PERP-DEX/XYZ",
                "the scope keeps its own upper case while the venue is the catalogue's"
            );

            let rendered = [
                DisplayError::ListingNotFound {
                    listing: Box::new(listing_key('e')),
                }
                .localized_message(cx)
                .to_string(),
                CatalogValidationError::ListingRouteMismatch {
                    route: hip3.clone(),
                    listing: Box::new(listing_key('e')),
                }
                .localized_message(cx)
                .to_string(),
                CatalogError::RouteNotRegistered {
                    route: spot.clone(),
                }
                .localized_message(cx)
                .to_string(),
                MarketError::UnsupportedCapability {
                    route: spot.clone(),
                    capability: market_participants_model::routing::CapabilityId::MarketTrades,
                }
                .localized_message(cx)
                .to_string(),
                AccountCatalogError::ListingNotFound {
                    route: spot.clone(),
                    listing: Box::new(listing_key('f')),
                }
                .localized_message(cx)
                .to_string(),
                MarketParticipantError::ListingRouteMismatch {
                    listing: listing_key('d'),
                    route: spot.clone(),
                }
                .localized_message(cx)
                .to_string(),
                market_participants_model::error::ListingIdentifierError::NativeIdentifierRouteMismatch {
                    route: spot.clone(),
                    identifier: market_participants_model::listing::native::ListingIdentifier::Symbol(
                        market_participants_model::identifier::NativeSymbol::new("BTCUSDT")
                            .expect("the fixture symbol is canonical"),
                    ),
                    owner: hip3.clone(),
                }
                .localized_message(cx)
                .to_string(),
                MarketParticipantError::RouteMismatch {
                    request: spot,
                    context: hip3.clone(),
                }
                .localized_message(cx)
                .to_string(),
                market_participants_model::error::ListingIdentifierError::IdentifierNotFound {
                    route: hip3,
                    identifier: market_participants_model::listing::native::ListingIdentifier::Symbol(
                        market_participants_model::identifier::NativeSymbol::new("BTCUSDT")
                            .expect("the fixture symbol is canonical"),
                    ),
                }
                .localized_message(cx)
                .to_string(),
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
        });
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
    #[gpui_kit::test]
    fn an_incomplete_credential_failure_should_name_the_venue_once_in_the_shown_language(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");
            cx.set_global(i18n::language::codes::LanguageCode::ZhCn);

            let route = route("spot");
            let rendered = [
                MarketParticipantError::AuthenticationFailed {
                    route,
                    account: None,
                    source:
                        market_participants_connector::error::AuthError::IncompleteCredentials {
                            venue: VenueId::Binance,
                            scope: Some(SmolStr::new_static("usd-m-perp")),
                        },
                }
                .localized_message(cx)
                .to_string(),
                MarketParticipantError::IncompleteCredentials {
                    venue: VenueId::Binance,
                    scope: Some(SmolStr::new_static("usd-m-perp")),
                }
                .localized_message(cx)
                .to_string(),
                MarketParticipantError::UnusableCredentials {
                    venue: VenueId::Binance,
                }
                .localized_message(cx)
                .to_string(),
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
        });
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
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

            let message = ListingIdentifierError::NativeIdentifierRouteMismatch {
                route: route("spot"),
                identifier: market_participants_model::listing::native::ListingIdentifier::Symbol(
                    market_participants_model::identifier::NativeSymbol::new("BTCUSDT")
                        .expect("the fixture symbol is canonical"),
                ),
                owner: VenueRoute::new(
                    VenueId::Hyperliquid,
                    VenueScopeId::new("perp-dex/xyz").expect("the fixture scope is canonical"),
                ),
            }
            .localized_message(cx)
            .to_string();

            assert!(message.contains("Binance/SPOT"), "{message}");
            assert!(
                message.contains("Hyperliquid/PERP-DEX/XYZ"),
                "the owning route is stated as a user reads it too: {message}"
            );
            assert!(
                !message.contains("binance/spot") && !message.contains("hyperliquid/perp-dex"),
                "neither route of one message may be the canonical spelling: {message}"
            );
        });
    }

    /// The identity diagnostic of an unpublished listing is not a display name: it
    /// states the typed key, which is the only thing a refused validation has, and
    /// it carries the route's user-visible form because the template that carries it
    /// is read by a user.
    #[test]
    fn a_listing_identity_diagnostic_should_state_the_typed_key_and_not_a_name() {
        let cx = TestAppContext::single();
        let (diagnostic, expected) = cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");
            let listing = listing_key('d');
            let expected = format!(
                "{}:{}",
                route_display_string(&listing.route, cx),
                listing.listing_id.as_str()
            );
            (listing_identity_diagnostic(&listing, cx), expected)
        });

        assert_eq!(diagnostic.as_str(), expected);
        assert!(
            diagnostic.as_str().starts_with("Binance/SPOT:"),
            "the diagnostic carries the route as a reader reads it: {diagnostic}"
        );
        assert_ne!(
            diagnostic.as_str(),
            listing_key('d').listing_id.as_str(),
            "the identity is paired with the route that refused it"
        );
    }

    /// A listing-name read that produced a name is presented by that name and
    /// nothing else, and the answer of the read says so.
    #[test]
    fn a_published_listing_read_should_be_presented_by_its_published_name() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

            let settlement = market_participants_model::asset::CurrencyTerms {
                code: market_participants_model::asset::Iso4217Code::new("USD")
                    .expect("the fixture currency is canonical"),
            };
            let code = || {
                market_participants_model::display::asset::AssetDisplayCode::from_terms(
                    market_participants_model::display::asset::AssetDisplayTerms::Currency(
                        &settlement,
                    ),
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
                presentation.localized_message(cx),
                gpui_kit::SharedString::new(name.as_str()),
                "a caller renders the name itself, never a fallback beside it"
            );
        });
    }

    /// A listing-name read that produced no name is the localized unknown, and the
    /// failure behind it decides which unknown: a listing the generation never named
    /// is not a generation that holds no value the read could use.
    #[test]
    fn a_refused_listing_read_should_be_presented_by_its_own_localized_unknown() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

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
                (absent_listing, LocalizedReason::UnknownListing),
                (no_terms, LocalizedReason::DisplayUnavailable),
                (no_code, LocalizedReason::DisplayUnavailable),
            ];

            for (error, reason) in cases {
                let presentation = ListingErrorPresentation::of(Err(error));

                assert_eq!(presentation.display, None, "no name was resolved");
                assert_eq!(presentation.reason, reason);
                assert_eq!(
                    presentation.localized_message(cx),
                    reason.localized_message(cx),
                    "the presentation states the answer of the read"
                );
                assert!(
                    !presentation.localized_message(cx).is_empty(),
                    "every answer is defined by every locale"
                );
            }

            assert_ne!(
                LocalizedReason::UnknownListing.localized_message(cx),
                LocalizedReason::DisplayUnavailable.localized_message(cx),
                "the listing miss and the unavailable name are told apart"
            );
        });
    }

    /// A market request refused because a listing no longer reads states the
    /// published state, which is the refusal itself: the request's own listing id
    /// stays a typed field of the error and never reaches the message as a label.
    #[test]
    fn a_market_request_refusal_should_state_the_state_and_not_the_listing_identity() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

            let rendered = MarketError::InvalidRequest {
                operation: SmolStr::new_static("read listing market data"),
                reason: MarketRequestRejection::ListingNotReadable {
                    status: ListingStatus::Delisted,
                },
            }
            .localized_message(cx)
            .to_string();

            assert!(rendered.contains("delisted"), "{rendered}");
            assert!(rendered.contains("read listing market data"), "{rendered}");
            assert!(
                !rendered.contains("listing-id/v1/"),
                "an identity is not a label: {rendered}"
            );
        });
    }

    /// A refused catalog step is stated by the step's own typed answer: the one
    /// answer that is about a listing keeps the listing as a typed field and prints
    /// no id, and the one that carries the model's refusal states that refusal in
    /// the selected language.
    #[test]
    fn a_refused_catalog_step_should_be_stated_by_its_typed_answer() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

            let listing = listing_key('d');
            let incomplete = SourceError::Protocol {
                operation: SmolStr::new_static("load position tiers"),
                reason: SourceProtocolRejection::ListingTermsIncomplete {
                    listing: Box::new(listing.clone()),
                },
            }
            .localized_message(cx)
            .to_string();

            assert!(incomplete.contains("load position tiers"), "{incomplete}");
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
            .localized_message(cx)
            .to_string();

            assert!(
                refused_terms.contains("build margin terms"),
                "{refused_terms}"
            );
            assert!(
                refused_terms.contains(&ModelError::ZeroTick.localized_message(cx).to_string()),
                "the model's own refusal is stated: {refused_terms}"
            );

            let unexpected = SourceError::Protocol {
                operation: SmolStr::new_static("fetch instrument catalog"),
                reason: SourceProtocolRejection::UnexpectedResponse,
            }
            .localized_message(cx)
            .to_string();
            assert!(
                unexpected.contains("fetch instrument catalog"),
                "{unexpected}"
            );

            let venue = SourceError::Protocol {
                operation: SmolStr::new_static("fetch instrument catalog"),
                reason: SourceProtocolRejection::Venue {
                    reason: SmolStr::new_static("the venue said no"),
                },
            }
            .localized_message(cx)
            .to_string();
            assert!(venue.contains("the venue said no"), "{venue}");
        });
    }

    /// An account query that names a listing of another route is a typed refusal
    /// with a typed identity diagnostic, and the operation an account protocol
    /// failure is about is a fixed name rather than a formatted sentence.
    #[test]
    fn account_refusals_should_state_their_operation_and_their_typed_identity() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

            let listing = listing_key('d');
            let mismatch = AccountError::ListingRouteMismatch {
                listing: Box::new(listing.clone()),
                route: route("perpetual"),
            }
            .localized_message(cx)
            .to_string();

            assert!(mismatch.contains("Binance/PERPETUAL"), "{mismatch}");
            assert!(
                mismatch.contains(listing.listing_id.as_str().as_str()),
                "{mismatch}"
            );

            let protocol = AccountError::Protocol {
                operation: "fetch balances",
            }
            .localized_message(cx)
            .to_string();
            assert!(protocol.contains("fetch balances"), "{protocol}");
        });
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
    /// The `t!` call sites of the widget crates that name this catalogue's messages.
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
    /// second argument as the key, so a `t!` whose key is a constant is read as the *name*
    /// of that constant, which no locale defines and nothing checks. A listed source
    /// therefore names every key where it uses it.
    const WIDGET_MESSAGE_SOURCES: &[&str] = &[
        include_str!("../../widgets/force_orders/src/view.rs"),
        include_str!("../../widgets/hyperliquid_wallet_monitor/src/model.rs"),
        include_str!("../../widgets/hyperliquid_wallet_monitor/src/service.rs"),
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

    /// The production source of every widget that hands the framework a notice.
    ///
    /// Listed rather than discovered: `include_str!` takes a literal, so a file that moves is a
    /// compile error rather than one that silently leaves the guard's reach, and the guard below
    /// reads exactly these files. Each entry is spelled the way the repository spells the file,
    /// so a failure names the source a reader has to open; the paths are relative to this file
    /// because that is what `include_str!` resolves against.
    ///
    /// `empty` is not here: it never enters the waiting state, so it never hands over a notice.
    /// `perpetual_statistics` is the one entry whose file is not a `view.rs` — it publishes more
    /// than one widget and keeps their shared pane a directory below `src`.
    const WIDGET_NOTICE_SOURCES: &[(&str, &str)] = &[
        (
            "crates/widgets/candlestick/src/view.rs",
            include_str!("../../widgets/candlestick/src/view.rs"),
        ),
        (
            "crates/widgets/crypto_fgi/src/view.rs",
            include_str!("../../widgets/crypto_fgi/src/view.rs"),
        ),
        (
            "crates/widgets/etf/src/view.rs",
            include_str!("../../widgets/etf/src/view.rs"),
        ),
        (
            "crates/widgets/financial_calendar/src/view.rs",
            include_str!("../../widgets/financial_calendar/src/view.rs"),
        ),
        (
            "crates/widgets/force_orders/src/view.rs",
            include_str!("../../widgets/force_orders/src/view.rs"),
        ),
        (
            "crates/widgets/hormuz_vessel_map/src/view.rs",
            include_str!("../../widgets/hormuz_vessel_map/src/view.rs"),
        ),
        (
            "crates/widgets/hyperliquid_wallet_monitor/src/view.rs",
            include_str!("../../widgets/hyperliquid_wallet_monitor/src/view.rs"),
        ),
        (
            "crates/widgets/financial_indices/src/view.rs",
            include_str!("../../widgets/financial_indices/src/view.rs"),
        ),
        (
            "crates/widgets/market_trades/src/view.rs",
            include_str!("../../widgets/market_trades/src/view.rs"),
        ),
        (
            "crates/widgets/news/src/view.rs",
            include_str!("../../widgets/news/src/view.rs"),
        ),
        (
            "crates/widgets/order_book/src/view.rs",
            include_str!("../../widgets/order_book/src/view.rs"),
        ),
        (
            "crates/widgets/pentagon_pizza_index/src/view.rs",
            include_str!("../../widgets/pentagon_pizza_index/src/view.rs"),
        ),
        (
            "crates/widgets/perpetual_statistics/src/widget/display/pane.rs",
            include_str!("../../widgets/perpetual_statistics/src/widget/display/pane.rs"),
        ),
        (
            "crates/widgets/x_account_monitoring/src/view.rs",
            include_str!("../../widgets/x_account_monitoring/src/view.rs"),
        ),
        (
            "crates/widgets/volume_heatmap/src/view.rs",
            include_str!("../../widgets/volume_heatmap/src/view.rs"),
        ),
        (
            "crates/widgets/cryptocurrency_wallet_monitoring/src/view.rs",
            include_str!("../../widgets/cryptocurrency_wallet_monitoring/src/view.rs"),
        ),
        (
            "crates/widgets/watch_list/src/view.rs",
            include_str!("../../widgets/watch_list/src/view.rs"),
        ),
    ];

    /// The two spellings a widget hands a message key over in: the constructor, and the variant
    /// it constructs. A widget that writes the variant out by hand names the same key.
    const NOTICE_MESSAGE_CALLS: &[&str] = &["Notice::message(", "Notice::Message("];

    /// The key of the one notice a widget does not name itself.
    ///
    /// A pane that needs a market loaded hands over the venues it waits for and the venues it
    /// can read at all, and the sentence that names the second set is the catalogue's own:
    /// `venue::market_not_loaded_sentence` renders `common.market_not_loaded` from them.
    const MARKET_NOT_LOADED_KEY: &str = "common.market_not_loaded";

    /// Every key a widget hands a notice over under is one every locale defines and renders.
    ///
    /// A notice is a reference to the catalogue rather than a sentence, so nothing else reads its
    /// key: a key no locale defines translates to the empty string, and the placeholder of a
    /// waiting pane would be blank instead of the guidance it exists to give. The keys are read
    /// out of the call sites that name them, so a notice that is added — or one whose key is
    /// mistyped — is covered by the call site rather than by a list written beside it.
    #[test]
    fn every_notice_a_widget_reports_should_be_named_in_every_locale() {
        let i18n = i18n::global::embedded::load().expect("the embedded locales load");
        let mut named = Vec::new();
        for (listed, source) in WIDGET_NOTICE_SOURCES {
            named.extend(notice_keys(source).into_iter().map(|key| (*listed, key)));
        }

        let distinct = named
            .iter()
            .map(|(_, key)| key.as_str())
            .collect::<BTreeSet<_>>();
        assert!(
            distinct.len() >= 12,
            "the widgets hand their notices over by key, and {} distinct key(s) were read out of \
             them: {distinct:?}",
            distinct.len()
        );

        for language in i18n.languages() {
            let defined = i18n
                .keys(language)
                .expect("an embedded language exposes its keys")
                .collect::<Vec<_>>();

            for (source, key) in &named {
                assert!(
                    defined.contains(&key.as_str()),
                    "{source} hands a notice over under {key}, which {language} does not define"
                );
                assert!(
                    i18n.try_translate(language, key)
                        .is_some_and(|text| !text.is_empty()),
                    "{source} hands a notice over under {key}, which renders empty in {language}"
                );
            }
        }
    }

    /// Every widget source that hands a notice over in production is one the guard above reads.
    ///
    /// [`WIDGET_NOTICE_SOURCES`] is written by hand, so a widget that starts reporting a notice is
    /// covered only if someone names it there, and the key it hands over — the whole content of a
    /// waiting pane — would be checked by nothing. The walk reads every widget source, recursively
    /// because a crate that publishes more than one widget keeps their shared pane a directory
    /// below `src`, slices each file at its test module, and compares the repository spelling of
    /// every file that hands a notice over with the list — in both directions, so an entry whose
    /// file stops handing one over is removed rather than kept as a file nothing needs read. Each
    /// entry is then read back through the path it names, because `include_str!` cannot take that
    /// path from the list: without this, the path a reader is sent to would be a label nothing
    /// checks against the source actually read.
    #[test]
    fn every_production_notice_call_site_should_be_listed_for_the_guard_to_read() {
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the widget crate lives one directory below the crates directory");
        let widgets = crates.join("widgets");
        let workspace = crates
            .parent()
            .expect("the crates directory lives below the workspace root");
        let mut discovered = Vec::new();
        let mut pending = vec![widgets.clone()];

        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).expect("a widget crate is readable") {
                let path = entry.expect("a widget source is readable").path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                // A module that is a directory keeps its tests in sibling `*_tests.rs` files and
                // the fakes several of them share in `test_support.rs`, both declared
                // `#[cfg(test)]` by their parent module (`crates/widgets/AGENTS.md` §2). Neither
                // is compiled into a widget, so neither hands a notice over: a notice a test
                // writes for itself is not one a pane draws.
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default();
                if !path.extension().is_some_and(|extension| extension == "rs")
                    || name.ends_with("_tests.rs")
                    || name == "test_support.rs"
                {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("a widget source is readable");
                if names_a_notice(production_source(&source)) {
                    discovered.push(format!(
                        "crates/widgets/{}",
                        path.strip_prefix(&widgets)
                            .expect("a source read out of the widget crates is below them")
                            .to_string_lossy()
                            .replace('\\', "/")
                    ));
                }
            }
        }

        assert!(
            !discovered.is_empty(),
            "the walk found no production notice call site, so it is reading nothing"
        );

        let unlisted = discovered
            .iter()
            .filter(|source| {
                !WIDGET_NOTICE_SOURCES
                    .iter()
                    .any(|(listed, _)| *listed == source.as_str())
            })
            .collect::<Vec<_>>();
        assert!(
            unlisted.is_empty(),
            "these widget sources hand a notice over in production and no guard reads them, so \
             the key they hand over is checked by nothing: {unlisted:?} — add each to \
             WIDGET_NOTICE_SOURCES"
        );

        let unread = WIDGET_NOTICE_SOURCES
            .iter()
            .filter(|(listed, _)| !discovered.iter().any(|source| source == listed))
            .collect::<Vec<_>>();
        assert!(
            unread.is_empty(),
            "these sources are listed as handing a notice over and no longer do, so the list \
             names files the guard does not need: {unread:?} — remove each from \
             WIDGET_NOTICE_SOURCES"
        );

        for (listed, source) in WIDGET_NOTICE_SOURCES {
            let on_disk = std::fs::read_to_string(workspace.join(listed))
                .unwrap_or_else(|error| panic!("{listed} is readable: {error}"));
            // Compared as a predicate rather than with `assert_eq!`, because the two sides are
            // whole files and a failure has to name the entry to fix rather than print them.
            assert!(
                on_disk == *source,
                "{listed} is listed, but the source read for it is another file"
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
        use i18n::language::codes::LanguageCode;

        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales load");

            let roles = [
                (ConversionDirection::Underlying, "基础资产", "underlying"),
                (ConversionDirection::Quote, "计价资产", "quote"),
                (ConversionDirection::Basis, "价格基准", "basis"),
            ];

            for (direction, chinese_role, english_role) in roles {
                let failure = ModelError::ConversionDirectionMismatch { direction };

                cx.set_global(LanguageCode::ZhCn);
                let chinese = failure.localized_message(cx).to_string();
                assert!(chinese.contains(chinese_role), "{chinese}");
                assert!(
                    !chinese.contains(english_role),
                    "a Chinese message must not carry the English role label: {chinese}"
                );

                cx.set_global(LanguageCode::En);
                let english = failure.localized_message(cx).to_string();
                assert!(english.contains(english_role), "{english}");
            }
        });
    }

    /// The attribute that opens the tests of a source file, which is where the production half
    /// of it ends.
    ///
    /// [`production_source`] tells the attribute that opens a test module from the one marking
    /// an accessor a test drives, and the readers of these files slice them with it, so spelling
    /// it out here would be a second one to find.
    const TEST_MODULE_ATTRIBUTE: &str = concat!("#[cfg", "(test)]");

    /// The message keys the `t!` invocations of one source file name, in source order.
    ///
    /// Only the production half of the file is read, sliced the way [`production_source`] slices
    /// it: the keys a test writes for itself are not messages the interface renders. An
    /// invocation is written both on one line and one argument per line, so the literal is
    /// looked for after the translator the macro is given rather than assumed to follow it on
    /// the same line.
    fn message_keys(source: &str) -> Vec<String> {
        let production = production_source(source);
        let mut keys = Vec::new();
        let mut rest = production;

        while let Some(invocation) = rest.find("t!(") {
            rest = &rest[invocation + "t!(".len()..];
            let Some(opening) = rest.find('"') else {
                break;
            };
            let translator = rest[..opening]
                .trim_start()
                .strip_prefix("cx")
                .map(str::trim_start);
            if !translator.is_some_and(|arguments| arguments.starts_with(',')) {
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

    /// Whether one production half hands the framework a notice.
    ///
    /// The identifier has to be the framework's own constructor or variant rather than any
    /// mention of the word: a widget that draws a notice of its own names a private function, and
    /// a widget that carries one in a field constructs it here.
    fn names_a_notice(production: &str) -> bool {
        NOTICE_MESSAGE_CALLS
            .iter()
            .any(|call| production.contains(call))
            || production.contains("Notice::MarketNotLoaded")
    }

    /// The message keys the production half of one widget source hands a notice over under.
    ///
    /// A notice names its key as a literal at every call site, so the literal after the call is
    /// the whole of what it names. The literal is looked for after the call rather than on the
    /// same line, because a call site is written both on one line and one argument per line. The
    /// market-not-loaded notice is the one whose key is not at the call site: the widget hands
    /// over the venues it needs, and the catalogue renders them through a key of its own.
    fn notice_keys(source: &str) -> Vec<String> {
        let production = production_source(source);
        let mut keys = Vec::new();

        for &call in NOTICE_MESSAGE_CALLS {
            let mut rest = production;
            while let Some(handed_over) = rest.find(call) {
                rest = &rest[handed_over + call.len()..];
                let Some(opening) = rest.find('"') else {
                    break;
                };
                let key = &rest[opening + 1..];
                let Some(closing) = key.find('"') else {
                    break;
                };
                keys.push(key[..closing].to_owned());
                rest = &key[closing..];
            }
        }

        if production.contains("Notice::MarketNotLoaded") {
            keys.push(MARKET_NOT_LOADED_KEY.to_owned());
        }

        keys
    }

    /// The key and the argument names of every `t!` invocation of one source file, in source
    /// order.
    ///
    /// The mapping half of the file is what is read — the slice [`production_source`] takes,
    /// where the first `#[cfg(test)]` would cut the mappings after an accessor a test drives
    /// short — and the invocations are read out of it rather than listed beside it, so an arm
    /// that binds a new argument is covered by the arm that binds it.
    fn message_arguments(source: &str) -> Vec<(String, Vec<String>)> {
        let production = production_source(source);
        let mut mappings = Vec::new();
        let mut rest = production;

        while let Some(start) = rest.find("t!(") {
            let Some((body, after)) = invocation_body(&rest[start + "t!(".len()..]) else {
                break;
            };
            rest = after;
            let arguments = top_level_arguments(body);
            // The translator the macro is given comes first, and it is a context rather than a
            // literal: an invocation written without one is not a mapping of this module.
            if arguments
                .first()
                .and_then(|translator| bound_name(translator))
                .is_none()
            {
                continue;
            }
            let Some(key) = arguments.get(1).map(|key| key.trim().trim_matches('"')) else {
                continue;
            };
            let bound = arguments[2..]
                .iter()
                .filter_map(|argument| bound_name(argument))
                .collect();

            mappings.push((key.to_owned(), bound));
        }

        mappings
    }

    /// Both readers of the widget sources skip the accessor a test drives rather than the
    /// mappings written after it.
    ///
    /// `hyperliquid_wallet_monitor` marks such an accessor in the middle of its production half,
    /// and both readers sliced each source at the first `#[cfg(test)]` until this rule was
    /// written down: every `t!` invocation below that accessor was read by nothing, so a mapping
    /// there could lose a placeholder — or name a key no locale defines — with both guards
    /// green. The mappings past the accessor and past the test module are what the two readers
    /// have to disagree about.
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
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    fn copy(cx: &App) -> SharedString {\n",
            "        t!(cx, \"widget.panel.unread\", market = markets)\n",
            "    }\n",
            "}\n",
        );

        assert_eq!(message_keys(source), ["widget.panel.waiting".to_owned()]);
        assert_eq!(
            message_arguments(source),
            [("widget.panel.waiting".to_owned(), vec!["market".to_owned()])]
        );
    }

    /// The body of one `t!` invocation and the text that follows its closing parenthesis.
    ///
    /// The scan is aware of string literals and of nested groups, so an argument that is itself
    /// a call — `cause = source.localized_message(cx)` — does not end the invocation early.
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

    /// The name one argument of a `t!` invocation binds, when it binds one.
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
    /// their production half — a slice taken at the first attribute would leave the `t!`
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

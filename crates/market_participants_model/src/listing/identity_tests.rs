use crate::{
    asset::{
        AssetId, CryptoTokenId, CryptoTokenOrigin, SecurityForm, SecurityIssuerId, ShareClassId,
        ShareClassTerms, TokenReference,
    },
    error::{ListingPublicationError, ModelError},
    identifier::{
        CanonicalTerms, CtfConditionId, CtfTokenId, EquityLocalCode, IdentifierNamespace,
        IdentifierValue, LocalSymbol, MarketParticipantSeriesId, Mic, NativeSymbol, OccSymbol,
        TokenIssuerId, VenueId, VenueScopeId,
    },
    instrument::{Instrument, InstrumentKind, InstrumentTerms, SpotTerms},
    listing::{
        continuity::{ListingContinuityKey, ListingContinuityTerms},
        continuity_series::{FutureSeriesIdentifiers, OptionSeriesIdentifiers},
        identifiers::{
            BinanceListingIdentifiers, EchoValidationStatus, EquityListingIdentifiers,
            HyperliquidListingIdentifiers, PolymarketListingIdentifiers, VenueListingIdentifiers,
        },
        listing_impl::VenueListing,
        native::{
            ListingIdentifier, NativeIdentifier, NativeIdentifierResolution,
            OutcomeEventIdentifier, OutcomeGroupIdentifier,
        },
        navigation::{NativeKeyIndex, expected_price_terms, sorted_listing_keys},
        route::VenueRoute,
        venue::{
            BinanceProduct, HyperliquidAssetId, HyperliquidAssetProduct, HyperliquidOutcomeId,
            OkxInstrumentType,
        },
    },
    market::{
        price::MarketPrice,
        units::{MarketQuantity, QuantityUnit},
    },
    routing::{
        IdentifierNormalizationProduct, IdentifierNormalizationRule, ListingIdentifierKind,
        SymbolNormalization, normalization_mode,
    },
    settlement::ContractUnit,
};

fn hyperliquid(scope: &str) -> VenueRoute {
    VenueRoute::new(
        VenueId::Hyperliquid,
        VenueScopeId::new(scope).expect("scope should be valid"),
    )
}

/// The route a fixture of a market this build does not trade is addressed on.
///
/// A route is an addressing key of *this* build: its venue is one of the venues
/// this program carries an adapter, a configuration and a catalogue for. What a
/// listing of an equity, an option series or an outcome market actually *is* is
/// carried by its continuity terms and its typed identifiers — the MIC, the
/// issuer, the OCC symbol, the condition id — and never by the venue, so a
/// fixture about one of those families names the product scope it belongs to on
/// a venue this build speaks rather than inventing a venue that has no adapter.
fn fixture_route(scope: &str) -> VenueRoute {
    VenueRoute::new(
        VenueId::Binance,
        VenueScopeId::new(scope).expect("scope should be valid"),
    )
}

fn binance_route() -> VenueRoute {
    VenueRoute::new(
        VenueId::Binance,
        VenueScopeId::new("spot").expect("scope should be valid"),
    )
}

fn binance_key(instrument: &str) -> ListingContinuityKey {
    ListingContinuityKey {
        route: binance_route(),
        terms: ListingContinuityTerms::Binance {
            product: BinanceProduct::Spot,
            instrument_id: NativeSymbol::new(instrument).expect("symbol should be valid"),
        },
    }
}

fn binance_spot_instrument() -> Instrument {
    Instrument::new(InstrumentTerms::Spot(
        SpotTerms::new(binance_token("BTC"), binance_token("USDT")).expect("terms should be valid"),
    ))
    .expect("instrument should be valid")
}

fn binance_token(code: &str) -> AssetId {
    AssetId::CryptoToken(
        CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
            issuer: TokenIssuerId::new("binance").expect("issuer should be valid"),
            namespace: IdentifierNamespace::new("binance").expect("namespace should be valid"),
            token: TokenReference::Native {
                code: IdentifierValue::new(code).expect("code should be valid"),
            },
        })
        .expect("token should be valid"),
    )
}

fn binance_spot_identifiers() -> VenueListingIdentifiers {
    VenueListingIdentifiers::Binance(BinanceListingIdentifiers {
        product: BinanceProduct::Spot,
        future_series: None,
        option_series: None,
    })
}

fn binance_listing(
    key: ListingContinuityKey,
    symbol: &str,
    aliases: &[&str],
) -> (VenueListing, Instrument) {
    let instrument = binance_spot_instrument();
    let listing = VenueListing::new(
        key.listing_key().expect("listing key should be valid"),
        instrument.id().clone(),
        Some(NativeSymbol::new(symbol).expect("symbol should be valid")),
        aliases
            .iter()
            .map(|alias| NativeSymbol::new(*alias).expect("alias should be valid"))
            .collect(),
        binance_spot_identifiers(),
    )
    .expect("listing should be valid");

    (listing, instrument)
}

fn echo_mismatch(kind: ListingIdentifierKind, value: &str) -> ModelError {
    ModelError::from(ListingPublicationError::IdentifierEchoMismatch {
        kind,
        value: value.into(),
    })
}

/// A listing whose typed identifiers are compared against declared symbols.
#[test]
fn publication_should_refuse_only_a_contradicted_echo() {
    let key = ListingContinuityKey {
        route: hyperliquid("core-perp"),
        terms: ListingContinuityTerms::HyperliquidAsset {
            product: HyperliquidAssetProduct::Perpetual,
            scope: VenueScopeId::new("core-perp").expect("scope should be valid"),
            asset_id: HyperliquidAssetId(7),
        },
    }
    .listing_key()
    .expect("listing key should be valid");
    let listing = VenueListing::new(
        key,
        binance_spot_instrument().id().clone(),
        Some(NativeSymbol::new("BTC").expect("symbol should be valid")),
        Vec::new(),
        VenueListingIdentifiers::Hyperliquid(HyperliquidListingIdentifiers::Perpetual {
            dex: VenueScopeId::new("core-perp").expect("scope should be valid"),
            asset_id: HyperliquidAssetId(7),
            coin: NativeSymbol::new("ETH").expect("coin should be valid"),
        }),
    )
    .expect("listing should be valid");
    let status = listing
        .validate_echoed_identifiers(|_| SymbolNormalization::AsciiCaseInsensitive)
        .expect("normalization should succeed");

    assert_eq!(
        status,
        EchoValidationStatus::Mismatch {
            kind: ListingIdentifierKind::HyperliquidAsset,
            value: "ETH".into(),
        }
    );
    assert_eq!(
        ModelError::from(ListingPublicationError::IdentifierEchoMismatch {
            kind: ListingIdentifierKind::HyperliquidAsset,
            value: "ETH".into(),
        }),
        echo_mismatch(ListingIdentifierKind::HyperliquidAsset, "ETH")
    );
}

#[test]
fn continuity_terms_should_derive_a_stable_listing_id() {
    let first = binance_key("BTCUSDT")
        .listing_id()
        .expect("listing should be valid");
    let second = binance_key("BTCUSDT")
        .listing_id()
        .expect("listing should be valid");

    assert_eq!(first, second);
    assert!(first.as_str().starts_with("listing-id/v1/"));
    assert_eq!(first.as_str().len(), "listing-id/v1".len() + 1 + 64);
    assert_eq!(
        first.as_str(),
        "listing-id/v1/499fabc4b66359d159c32c53b8df3b995a52eee9a697df524fe756b822e05c5f",
        "the full Binance continuity terms derive a pinned listing identity"
    );
}

#[test]
fn a_renamed_alias_should_not_change_the_listing_id() {
    let key = binance_key("BTCUSDT");
    let (original, _instrument) = binance_listing(key.clone(), "BTCUSDT", &["XBTUSDT"]);
    let (renamed, _instrument) = binance_listing(key, "BTC-USDT", &["btc_usdt", "BTCUSDT.P"]);

    assert_ne!(
        original.native_symbol, renamed.native_symbol,
        "the display symbol really changed"
    );
    assert_ne!(
        original.aliases, renamed.aliases,
        "the aliases really changed"
    );
    assert_eq!(
        original.key.listing_id, renamed.key.listing_id,
        "a display alias is not part of the listing identity"
    );
}

#[test]
fn changed_continuity_terms_should_change_the_listing_id() {
    let original = binance_key("BTCUSDT")
        .listing_id()
        .expect("listing should be valid");
    let other_product = ListingContinuityKey {
        route: binance_route(),
        terms: ListingContinuityTerms::Binance {
            product: BinanceProduct::Perpetual,
            instrument_id: NativeSymbol::new("BTCUSDT").expect("symbol should be valid"),
        },
    }
    .listing_id()
    .expect("listing should be valid");
    let other_instrument = binance_key("ETHUSDT")
        .listing_id()
        .expect("listing should be valid");

    assert_ne!(
        original, other_product,
        "the product takes part in the continuity identity"
    );
    assert_ne!(
        original, other_instrument,
        "the native instrument takes part in the continuity identity"
    );
}

#[test]
fn a_reused_native_identifier_without_proof_should_need_a_new_continuity_term() {
    let route = VenueRoute::new(
        VenueId::Hyperliquid,
        VenueScopeId::new("core-perp").expect("scope should be valid"),
    );
    let scope = VenueScopeId::new("core-perp").expect("scope should be valid");
    let delisted = ListingContinuityKey {
        route: route.clone(),
        terms: ListingContinuityTerms::HyperliquidAsset {
            product: HyperliquidAssetProduct::Perpetual,
            scope: scope.clone(),
            asset_id: HyperliquidAssetId(1),
        },
    }
    .listing_id()
    .expect("listing should be valid");
    let relisted = ListingContinuityKey {
        route,
        terms: ListingContinuityTerms::HyperliquidAsset {
            product: HyperliquidAssetProduct::Perpetual,
            scope,
            asset_id: HyperliquidAssetId(2),
        },
    }
    .listing_id()
    .expect("listing should be valid");

    assert_ne!(
        delisted, relisted,
        "reusing a native slot without a continuity proof must not reuse the id"
    );
}

#[test]
fn continuity_terms_should_reject_a_series_without_identity() {
    let key = ListingContinuityKey {
        route: fixture_route("futures"),
        terms: ListingContinuityTerms::FutureSeries(FutureSeriesIdentifiers::default()),
    };

    assert_eq!(
        key.listing_id(),
        Err(ModelError::InvalidListingContinuityTerms {
            field: "future series without a verified identifier"
        })
    );
    assert!(key.listing_key().is_err());
}

#[test]
fn continuity_terms_should_reject_an_option_series_without_identity() {
    let key = ListingContinuityKey {
        route: fixture_route("equity"),
        terms: ListingContinuityTerms::OptionSeries(OptionSeriesIdentifiers::default()),
    };

    assert!(matches!(
        key.listing_id(),
        Err(ModelError::InvalidListingContinuityTerms { .. })
    ));
}

#[test]
fn continuity_terms_should_separate_scopes_of_the_same_asset_id() {
    let core = ListingContinuityKey {
        route: hyperliquid("core-perp"),
        terms: ListingContinuityTerms::HyperliquidAsset {
            product: HyperliquidAssetProduct::Perpetual,
            scope: VenueScopeId::new("core-perp").expect("scope should be valid"),
            asset_id: HyperliquidAssetId(0),
        },
    }
    .listing_id()
    .expect("listing should be valid");
    let dex = ListingContinuityKey {
        route: hyperliquid("perp-dex/xyz"),
        terms: ListingContinuityTerms::HyperliquidAsset {
            product: HyperliquidAssetProduct::Perpetual,
            scope: VenueScopeId::new("perp-dex/xyz").expect("scope should be valid"),
            asset_id: HyperliquidAssetId(0),
        },
    }
    .listing_id()
    .expect("listing should be valid");

    assert_ne!(core, dex);
}

#[test]
fn continuity_key_should_round_trip_through_serde() {
    let key = binance_key("BTCUSDT");
    let encoded = serde_json::to_string(&key).expect("key should serialize");
    let decoded =
        serde_json::from_str::<ListingContinuityKey>(&encoded).expect("key should decode");

    assert_eq!(decoded, key);
}

#[test]
fn equity_continuity_should_use_issuer_share_class_and_mic() {
    let issuer = SecurityIssuerId::new("cik/320193").expect("issuer should be valid");
    let share_class = |designation: &str| {
        ShareClassId::new(&ShareClassTerms {
            issuer: issuer.clone(),
            designation: designation.into(),
            form: SecurityForm::CommonStock,
            par_value: None,
            adr_ratio: None,
        })
        .expect("share class should be valid")
    };
    let route = fixture_route("equity");
    let listing_id = |share_class: &ShareClassId, mic: &str| {
        ListingContinuityKey {
            route: route.clone(),
            terms: ListingContinuityTerms::Equity {
                issuer: issuer.clone(),
                share_class: share_class.clone(),
                mic: Mic::new(mic).expect("mic should be valid"),
                local_code: EquityLocalCode::new("AAPL").expect("code should be valid"),
            },
        }
        .listing_id()
        .expect("listing should be valid")
    };
    let common = share_class("common");
    let xnas = listing_id(&common, "XNAS");

    assert_ne!(
        xnas,
        listing_id(&common, "XSHG"),
        "the MIC is part of the equity continuity terms"
    );
    assert_ne!(
        xnas,
        listing_id(&share_class("preferred"), "XNAS"),
        "the share class is part of the equity continuity terms"
    );
    assert_eq!(
        xnas,
        listing_id(&common, "XNAS"),
        "the same equity terms derive the same listing identity"
    );
}

#[test]
fn polymarket_continuity_should_require_condition_and_token() {
    let route = fixture_route("ctf-clob/polygon");
    let condition = || {
        CtfConditionId::new("0x".to_owned() + &"a".repeat(64)).expect("condition should be valid")
    };
    let listing_id = |token: &str| {
        ListingContinuityKey {
            route: route.clone(),
            terms: ListingContinuityTerms::Polymarket {
                condition_id: condition(),
                clob_token_id: CtfTokenId::new(token).expect("token should be valid"),
            },
        }
        .listing_id()
        .expect("listing should be valid")
    };
    let full = listing_id("123");

    assert_eq!(
        full.as_str(),
        "listing-id/v1/c82aeeb75e960d62cb1cb4095418ad52adb185bd49775f82f1977352b9411620",
        "the full Polymarket continuity terms derive a pinned listing identity"
    );
    assert_ne!(
        full,
        listing_id("456"),
        "the CLOB token is part of the continuity identity"
    );
    assert_ne!(
        full,
        ListingContinuityKey {
            route,
            terms: ListingContinuityTerms::Polymarket {
                condition_id: CtfConditionId::new("0x".to_owned() + &"b".repeat(64))
                    .expect("condition should be valid"),
                clob_token_id: CtfTokenId::new("123").expect("token should be valid"),
            },
        }
        .listing_id()
        .expect("listing should be valid"),
        "the condition is part of the continuity identity"
    );
    assert_eq!(
        CtfTokenId::new(""),
        Err(ModelError::InvalidCtfTokenId { value: "".into() }),
        "a token-less Polymarket identifier cannot even be constructed"
    );
    assert_eq!(
        CtfConditionId::new(""),
        Err(ModelError::InvalidCtfConditionId { value: "".into() }),
        "a condition-less Polymarket identifier cannot even be constructed"
    );
    let missing_token = format!(
        r#"{{"route":{{"venue":"binance","scope":"ctf-clob/polygon"}},"terms":{{"kind":"polymarket","terms":{{"condition_id":"0x{}"}}}}}}"#,
        "a".repeat(64)
    );

    assert!(
        serde_json::from_str::<ListingContinuityKey>(&missing_token).is_err(),
        "a wire key without a CLOB token must be rejected"
    );
}

#[test]
fn native_keys_should_detect_a_normalization_collision() {
    let mut index = NativeKeyIndex::with_capacity(2);
    let first = binance_key("BTCUSDT")
        .listing_key()
        .expect("listing should be valid");
    let second = binance_key("btcusdt")
        .listing_key()
        .expect("listing should be valid");

    index
        .insert(
            SymbolNormalization::AsciiCaseInsensitive
                .apply("BTCUSDT")
                .expect("value should normalize"),
            first,
        )
        .expect("first key should insert");
    assert_eq!(
        index.insert(
            SymbolNormalization::AsciiCaseInsensitive
                .apply("btcusdt")
                .expect("value should normalize"),
            second,
        ),
        Err(ModelError::AmbiguousListingContinuity {
            venue: VenueId::Binance,
            scope: VenueScopeId::new("spot").expect("scope should be valid"),
        })
    );
    assert_eq!(index.len(), 1);
    assert!(index.get("BTCUSDT").is_some());
}

#[test]
fn hyperliquid_listing_should_require_the_route_scope() {
    let identifiers = HyperliquidListingIdentifiers::Perpetual {
        dex: VenueScopeId::new("perp-dex/abc").expect("scope should be valid"),
        asset_id: HyperliquidAssetId(1),
        coin: NativeSymbol::new("abc:ABC").expect("coin should be valid"),
    };

    assert!(matches!(
        identifiers.validate(&hyperliquid("core-perp")),
        Err(ModelError::InvalidListingContinuityTerms { .. })
    ));
    assert!(identifiers.validate(&hyperliquid("perp-dex/abc")).is_ok());
}

#[test]
fn hyperliquid_outcome_should_allow_a_missing_coin() {
    let side =
        crate::outcome::OutcomeSideId::from_identity_value(&format!("v1/{}", "7".repeat(64)))
            .expect("side identity");
    let identifiers = HyperliquidListingIdentifiers::Outcome {
        venue: VenueScopeId::new("outcome-venue/ab").expect("scope should be valid"),
        outcome_id: HyperliquidOutcomeId(7),
        side,
        asset_id: HyperliquidAssetId(9),
        coin: None,
        token_name: IdentifierValue::new("YES").expect("token name should be valid"),
    };

    assert_eq!(
        identifiers.scope().map(VenueScopeId::as_str),
        Some("outcome-venue/ab")
    );
    assert_eq!(
        identifiers.instrument_kind(),
        crate::instrument::InstrumentKind::Outcome
    );
}

#[test]
fn listing_identifiers_should_match_the_instrument_kind() {
    let route = binance_route();
    let identifiers =
        VenueListingIdentifiers::Binance(crate::listing::identifiers::BinanceListingIdentifiers {
            product: crate::listing::venue::BinanceProduct::Spot,
            future_series: None,
            option_series: None,
        });

    assert!(
        identifiers
            .validate_for(&route, crate::instrument::InstrumentKind::Spot)
            .is_ok()
    );
    assert!(matches!(
        identifiers.validate_for(&route, crate::instrument::InstrumentKind::Perpetual),
        Err(ModelError::InvalidListingContinuityTerms { .. })
    ));
    assert_eq!(
        identifiers.primary_kind(),
        ListingIdentifierKind::BinanceInstrument
    );
}

#[test]
fn listing_identifier_kind_should_map_one_to_one() {
    let scope = VenueScopeId::new("core-perp").expect("scope should be valid");
    let identifiers: [(ListingIdentifier, ListingIdentifierKind, Option<&str>); 11] = [
        (
            ListingIdentifier::Symbol(
                NativeSymbol::new("BTCUSDT").expect("symbol should be valid"),
            ),
            ListingIdentifierKind::Symbol,
            Some("BTCUSDT"),
        ),
        (
            ListingIdentifier::BinanceInstrument {
                symbol: NativeSymbol::new("BTCUSDT").expect("symbol should be valid"),
                product: BinanceProduct::Spot,
            },
            ListingIdentifierKind::BinanceInstrument,
            Some("BTCUSDT"),
        ),
        (
            ListingIdentifier::OkxInstrument {
                instrument_id: NativeSymbol::new("BTC-USDT").expect("symbol should be valid"),
                instrument_type: OkxInstrumentType::Spot,
            },
            ListingIdentifierKind::OkxInstrument,
            Some("BTC-USDT"),
        ),
        (
            ListingIdentifier::HyperliquidAsset {
                scope: scope.clone(),
                product: HyperliquidAssetProduct::Perpetual,
                asset_id: HyperliquidAssetId(7),
            },
            ListingIdentifierKind::HyperliquidAsset,
            None,
        ),
        (
            ListingIdentifier::HyperliquidOutcome {
                scope,
                outcome_id: HyperliquidOutcomeId(3),
                asset_id: HyperliquidAssetId(9),
            },
            ListingIdentifierKind::HyperliquidOutcome,
            None,
        ),
        (
            ListingIdentifier::PolymarketToken(
                CtfTokenId::new("1").expect("token should be valid"),
            ),
            ListingIdentifierKind::PolymarketToken,
            None,
        ),
        (
            ListingIdentifier::EquityCode {
                mic: Mic::new("XNAS").expect("mic should be valid"),
                local_code: EquityLocalCode::new("AAPL").expect("code should be valid"),
            },
            ListingIdentifierKind::EquityLocalCode,
            Some("AAPL"),
        ),
        (
            ListingIdentifier::FutureSeries(FutureSeriesIdentifiers {
                market_participant_series_id: Some(
                    MarketParticipantSeriesId::new("ES-202612").expect("series should be valid"),
                ),
                local_symbol: None,
                contract_code: None,
            }),
            ListingIdentifierKind::FutureSeries,
            None,
        ),
        (
            ListingIdentifier::OccSymbol(
                OccSymbol::new("AAPL261218C00150000").expect("occ symbol should be valid"),
            ),
            ListingIdentifierKind::OccSymbol,
            Some("AAPL261218C00150000"),
        ),
        (
            ListingIdentifier::MarketParticipantSeries(
                MarketParticipantSeriesId::new("AAPL-20261218-C150")
                    .expect("series should be valid"),
            ),
            ListingIdentifierKind::MarketParticipantSeries,
            Some("AAPL-20261218-C150"),
        ),
        (
            ListingIdentifier::OptionLocalSymbol(
                LocalSymbol::new("AAPL261218C150").expect("local symbol should be valid"),
            ),
            ListingIdentifierKind::OptionLocalSymbol,
            Some("AAPL261218C150"),
        ),
    ];
    let mut kinds = std::collections::BTreeSet::new();
    for (identifier, kind, symbol) in &identifiers {
        assert_eq!(
            identifier.kind(),
            *kind,
            "{identifier:?} must report its own kind"
        );
        assert_eq!(
            identifier.symbol_value(),
            *symbol,
            "{identifier:?} must report only the symbol it really carries"
        );
        assert!(
            kinds.insert(*kind),
            "two typed identifier variants must never share one kind"
        );
    }
    assert_eq!(kinds.len(), identifiers.len());
    assert_eq!(
        identifiers[3].0.scope().map(VenueScopeId::as_str),
        Some("core-perp"),
        "a scoped identifier must report the route scope it carries"
    );
    assert_eq!(identifiers[0].0.scope(), None);
    assert!(ListingIdentifierKind::OccSymbol.is_series_internal());
    assert!(ListingIdentifierKind::MarketParticipantSeries.is_series_internal());
    assert!(ListingIdentifierKind::OptionLocalSymbol.is_series_internal());
    assert!(!ListingIdentifierKind::FutureSeries.is_series_internal());
    assert!(ListingIdentifierKind::FutureSeries.is_symbol_based());
    assert!(!ListingIdentifierKind::PolymarketToken.is_symbol_based());
}

#[test]
fn listing_identifier_products_should_reject_every_kind_mismatch() {
    let mismatched: [(ListingIdentifierKind, IdentifierNormalizationProduct); 11] = [
        (
            ListingIdentifierKind::Symbol,
            IdentifierNormalizationProduct::Binance(BinanceProduct::Spot),
        ),
        (
            ListingIdentifierKind::BinanceInstrument,
            IdentifierNormalizationProduct::Okx(OkxInstrumentType::Spot),
        ),
        (
            ListingIdentifierKind::OkxInstrument,
            IdentifierNormalizationProduct::Hyperliquid(HyperliquidAssetProduct::Spot),
        ),
        (
            ListingIdentifierKind::HyperliquidAsset,
            IdentifierNormalizationProduct::HyperliquidOutcome,
        ),
        (
            ListingIdentifierKind::HyperliquidOutcome,
            IdentifierNormalizationProduct::Hyperliquid(HyperliquidAssetProduct::Spot),
        ),
        (
            ListingIdentifierKind::PolymarketToken,
            IdentifierNormalizationProduct::Binance(BinanceProduct::Spot),
        ),
        (
            ListingIdentifierKind::EquityLocalCode,
            IdentifierNormalizationProduct::Binance(BinanceProduct::Spot),
        ),
        (
            ListingIdentifierKind::FutureSeries,
            IdentifierNormalizationProduct::Binance(BinanceProduct::Future),
        ),
        (
            ListingIdentifierKind::OccSymbol,
            IdentifierNormalizationProduct::Instrument(InstrumentKind::Option),
        ),
        (
            ListingIdentifierKind::MarketParticipantSeries,
            IdentifierNormalizationProduct::Instrument(InstrumentKind::Option),
        ),
        (
            ListingIdentifierKind::OptionLocalSymbol,
            IdentifierNormalizationProduct::Binance(BinanceProduct::Option),
        ),
    ];

    assert_eq!(mismatched.len(), 11);
    for (kind, product) in mismatched {
        let rules = [IdentifierNormalizationRule::new(
            kind,
            Some(product),
            SymbolNormalization::AsciiCaseInsensitive,
        )];

        assert_eq!(
            normalization_mode(&rules, kind, Some(product)),
            Err(ModelError::IdentifierNormalizationMismatch { kind }),
            "{kind} must reject the declared product {product:?}"
        );
    }
    assert_eq!(
        normalization_mode(&[], ListingIdentifierKind::Symbol, None)
            .expect("an undeclared kind falls back to exact"),
        SymbolNormalization::Exact
    );
}

#[test]
fn echoed_typed_identifiers_should_match_the_display_symbols() {
    let key = ListingContinuityKey {
        route: hyperliquid("core-perp"),
        terms: ListingContinuityTerms::HyperliquidAsset {
            product: HyperliquidAssetProduct::Perpetual,
            scope: VenueScopeId::new("core-perp").expect("scope should be valid"),
            asset_id: HyperliquidAssetId(7),
        },
    }
    .listing_key()
    .expect("listing key should be valid");
    let listing = |symbol: Option<&str>, aliases: &[&str], coin: &str| {
        VenueListing::new(
            key.clone(),
            binance_spot_instrument().id().clone(),
            symbol.map(|symbol| NativeSymbol::new(symbol).expect("symbol should be valid")),
            aliases
                .iter()
                .map(|alias| NativeSymbol::new(*alias).expect("alias should be valid"))
                .collect(),
            VenueListingIdentifiers::Hyperliquid(HyperliquidListingIdentifiers::Perpetual {
                dex: VenueScopeId::new("core-perp").expect("scope should be valid"),
                asset_id: HyperliquidAssetId(7),
                coin: NativeSymbol::new(coin).expect("coin should be valid"),
            }),
        )
        .expect("listing should be valid")
    };
    let case_insensitive = |_| SymbolNormalization::AsciiCaseInsensitive;

    assert_eq!(
        listing(Some("BTC"), &[], "BTC").validate_echoed_identifiers(case_insensitive),
        Ok(EchoValidationStatus::Verified),
        "an identical echo is accepted under the route normalization"
    );
    assert_eq!(
        listing(Some("btc"), &[], "BTC").validate_echoed_identifiers(case_insensitive),
        Ok(EchoValidationStatus::Verified),
        "normalization folds the two spellings"
    );
    assert_eq!(
        listing(None, &["btc"], "BTC").validate_echoed_identifiers(case_insensitive),
        Ok(EchoValidationStatus::Verified),
        "an alias is a display symbol of the same listing"
    );
    assert_eq!(
        listing(Some("BTC"), &[], "ETH").validate_echoed_identifiers(case_insensitive),
        Ok(EchoValidationStatus::Mismatch {
            kind: ListingIdentifierKind::HyperliquidAsset,
            value: "ETH".into(),
        }),
        "an echoed coin that names another listing is a mismatch"
    );
    assert_eq!(
        listing(Some("BTC"), &[], "btc")
            .validate_echoed_identifiers(|_| SymbolNormalization::Exact),
        Ok(EchoValidationStatus::Mismatch {
            kind: ListingIdentifierKind::HyperliquidAsset,
            value: "btc".into(),
        }),
        "an exact route does not fold case"
    );
    assert_eq!(
        listing(None, &[], "BTC").validate_echoed_identifiers(case_insensitive),
        Ok(EchoValidationStatus::NoComparableIdentifier),
        "a listing without a display symbol has no echo to verify"
    );
}

#[test]
fn equity_listing_should_report_all_three_echo_states() {
    let route = fixture_route("equity");
    let identifiers = |local_code: &str, ticker: &str| EquityListingIdentifiers {
        mic: Mic::new("XNAS").expect("mic should be valid"),
        board: None,
        ticker: ticker.into(),
        local_code: EquityLocalCode::new(local_code).expect("code should be valid"),
    };
    let key = ListingContinuityKey {
        route,
        terms: ListingContinuityTerms::Equity {
            issuer: SecurityIssuerId::new("cik/320193").expect("issuer should be valid"),
            share_class: ShareClassId::new(&ShareClassTerms {
                issuer: SecurityIssuerId::new("cik/320193").expect("issuer should be valid"),
                designation: "common".into(),
                form: SecurityForm::CommonStock,
                par_value: None,
                adr_ratio: None,
            })
            .expect("share class should be valid"),
            mic: Mic::new("XNAS").expect("mic should be valid"),
            local_code: EquityLocalCode::new("AAPL").expect("code should be valid"),
        },
    }
    .listing_key()
    .expect("listing key should be valid");
    let listing = |symbol: Option<&str>, local_code: &str, ticker: &str| {
        VenueListing::new(
            key.clone(),
            binance_spot_instrument().id().clone(),
            symbol.map(|symbol| NativeSymbol::new(symbol).expect("symbol should be valid")),
            Vec::new(),
            VenueListingIdentifiers::Equity(identifiers(local_code, ticker)),
        )
        .expect("listing should be valid")
    };
    let exact = |_| SymbolNormalization::Exact;

    // The equity family compares two typed fields, so one agreeing field cannot
    // hide a contradicting one.
    assert_eq!(
        listing(Some("AAPL"), "AAPL", "AAPL").validate_echoed_identifiers(exact),
        Ok(EchoValidationStatus::Verified)
    );
    assert_eq!(
        listing(Some("AAPL"), "AAPL", "MSFT").validate_echoed_identifiers(exact),
        Ok(EchoValidationStatus::Mismatch {
            kind: ListingIdentifierKind::Symbol,
            value: "MSFT".into(),
        }),
        "a ticker that contradicts the declared symbol is a mismatch"
    );
    assert_eq!(
        listing(Some("AAPL"), "AAPL.O", "AAPL").validate_echoed_identifiers(exact),
        Ok(EchoValidationStatus::Mismatch {
            kind: ListingIdentifierKind::EquityLocalCode,
            value: "AAPL.O".into(),
        }),
        "a local code that contradicts the declared symbol is a mismatch"
    );
    assert_eq!(
        listing(None, "AAPL", "AAPL").validate_echoed_identifiers(exact),
        Ok(EchoValidationStatus::NoComparableIdentifier),
        "an equity ticker is typed metadata, so a listing without route aliases has no echo"
    );
}

/// One listing whose whole typed identity is one series identifier set.
///
/// The echo check compares every typed identifier of a listing with the declared display
/// symbols, so a listing that pins one identifier of a series declares a symbol
/// that spells that one value and nothing else.
fn series_listing_of(
    terms: ListingContinuityTerms,
    identifiers: VenueListingIdentifiers,
    symbol: &str,
) -> VenueListing {
    let route = fixture_route("index-option");
    let key = ListingContinuityKey { route, terms }
        .listing_key()
        .expect("listing key should be valid");

    VenueListing::new(
        key,
        binance_spot_instrument().id().clone(),
        Some(NativeSymbol::new(symbol).expect("symbol should be valid")),
        Vec::new(),
        identifiers,
    )
    .expect("listing should be valid")
}

/// every verified identifier of an option series is an echo
/// obligation of its own.
///
/// Each case publishes a series that carries exactly one of the three identifiers,
/// so the status of the listing is decided by that identifier alone: a declared
/// symbol that spells it is `Verified`, and a declared symbol that spells anything
/// else is a `Mismatch` naming the identifier and the value it failed to echo. A
/// model that stopped echoing one of the three fields would answer
/// `NoComparableIdentifier` for both listings instead, because no comparable typed
/// identifier would be left.
#[test]
fn every_option_series_identifier_should_be_echoed_by_a_declared_symbol() {
    let exact = |_| SymbolNormalization::Exact;
    let cases: [(ListingIdentifierKind, &str, OptionSeriesIdentifiers); 3] = [
        (
            ListingIdentifierKind::OccSymbol,
            "AAPL261218C00150000",
            OptionSeriesIdentifiers {
                occ_symbol: Some(
                    OccSymbol::new("AAPL261218C00150000").expect("occ symbol should be valid"),
                ),
                market_participant_series_id: None,
                local_symbol: None,
            },
        ),
        (
            ListingIdentifierKind::MarketParticipantSeries,
            "AAPL-20261218-C150",
            OptionSeriesIdentifiers {
                occ_symbol: None,
                market_participant_series_id: Some(
                    MarketParticipantSeriesId::new("AAPL-20261218-C150")
                        .expect("series should be valid"),
                ),
                local_symbol: None,
            },
        ),
        (
            ListingIdentifierKind::OptionLocalSymbol,
            "AAPL261218C150",
            OptionSeriesIdentifiers {
                occ_symbol: None,
                market_participant_series_id: None,
                local_symbol: Some(
                    LocalSymbol::new("AAPL261218C150").expect("local symbol should be valid"),
                ),
            },
        ),
    ];

    for (kind, value, series) in cases {
        let option = |symbol: &str| {
            series_listing_of(
                ListingContinuityTerms::OptionSeries(series.clone()),
                VenueListingIdentifiers::OptionSeries(series.clone()),
                symbol,
            )
        };

        assert_eq!(
            option(value).validate_echoed_identifiers(exact),
            Ok(EchoValidationStatus::Verified),
            "{kind} is echoed when the declared symbol spells the only identifier of the series"
        );
        assert_eq!(
            option("OTHER-SERIES").validate_echoed_identifiers(exact),
            Ok(EchoValidationStatus::Mismatch {
                kind,
                value: value.into(),
            }),
            "{kind} is a mismatch when no declared symbol carries it"
        );
    }
}

/// the market-participant series id and the local symbol of a future
/// series are echo obligations of their own.
///
/// The two fields are the first two positions of the echo order, and the contract
/// code after them is pinned end to end by
/// `a_declared_native_family_is_verified_native` in the connector's display gate.
///
/// A future series states its local symbol under the plain `Symbol` family, which
/// is the family whose route normalization the display symbols themselves are
/// compared with, while an option series states its own local symbol under the
/// series-internal `OptionLocalSymbol` family. The two cases below assert the kind
/// the model echoes each field under, because that kind is what the status reports
/// to the echo gate.
#[test]
fn every_future_series_identifier_should_be_echoed_by_a_declared_symbol() {
    let exact = |_| SymbolNormalization::Exact;
    let cases: [(ListingIdentifierKind, &str, FutureSeriesIdentifiers); 2] = [
        (
            ListingIdentifierKind::MarketParticipantSeries,
            "ES-202612",
            FutureSeriesIdentifiers {
                market_participant_series_id: Some(
                    MarketParticipantSeriesId::new("ES-202612").expect("series should be valid"),
                ),
                local_symbol: None,
                contract_code: None,
            },
        ),
        (
            ListingIdentifierKind::Symbol,
            "ESZ6",
            FutureSeriesIdentifiers {
                market_participant_series_id: None,
                local_symbol: Some(LocalSymbol::new("ESZ6").expect("local symbol should be valid")),
                contract_code: None,
            },
        ),
    ];

    for (kind, value, series) in cases {
        let future = |symbol: &str| {
            series_listing_of(
                ListingContinuityTerms::FutureSeries(series.clone()),
                VenueListingIdentifiers::FutureSeries(series.clone()),
                symbol,
            )
        };

        assert_eq!(
            future(value).validate_echoed_identifiers(exact),
            Ok(EchoValidationStatus::Verified),
            "{kind} is echoed when the declared symbol spells the only identifier of the series"
        );
        assert_eq!(
            future("OTHER-SERIES").validate_echoed_identifiers(exact),
            Ok(EchoValidationStatus::Mismatch {
                kind,
                value: value.into(),
            }),
            "{kind} is a mismatch when no declared symbol carries it"
        );
    }
}

#[test]
fn hyperliquid_outcome_listing_should_echo_its_coin_only_when_declared() {
    let route = VenueRoute::new(
        VenueId::Hyperliquid,
        VenueScopeId::new("outcome-venue/ab").expect("scope should be valid"),
    );
    let side =
        crate::outcome::OutcomeSideId::from_identity_value(&format!("v1/{}", "7".repeat(64)))
            .expect("side identity");
    let key = ListingContinuityKey {
        route,
        terms: ListingContinuityTerms::HyperliquidOutcome {
            scope: VenueScopeId::new("outcome-venue/ab").expect("scope should be valid"),
            outcome_id: HyperliquidOutcomeId(7),
            asset_id: HyperliquidAssetId(9),
        },
    }
    .listing_key()
    .expect("listing key should be valid");
    let listing = |symbol: Option<&str>, coin: Option<NativeSymbol>| {
        VenueListing::new(
            key.clone(),
            binance_spot_instrument().id().clone(),
            symbol.map(|symbol| NativeSymbol::new(symbol).expect("symbol should be valid")),
            Vec::new(),
            VenueListingIdentifiers::Hyperliquid(HyperliquidListingIdentifiers::Outcome {
                venue: VenueScopeId::new("outcome-venue/ab").expect("scope should be valid"),
                outcome_id: HyperliquidOutcomeId(7),
                side: side.clone(),
                asset_id: HyperliquidAssetId(9),
                coin,
                token_name: IdentifierValue::new("YES").expect("token name should be valid"),
            }),
        )
        .expect("listing should be valid")
    };
    let exact = |_| SymbolNormalization::Exact;
    let coin = |code: &str| Some(NativeSymbol::new(code).expect("coin should be valid"));

    assert_eq!(
        listing(Some("YES"), coin("YES")).validate_echoed_identifiers(exact),
        Ok(EchoValidationStatus::Verified)
    );
    assert_eq!(
        listing(Some("YES"), coin("NO")).validate_echoed_identifiers(exact),
        Ok(EchoValidationStatus::Mismatch {
            kind: ListingIdentifierKind::HyperliquidOutcome,
            value: "NO".into(),
        })
    );
    assert_eq!(
        listing(Some("YES"), None).validate_echoed_identifiers(exact),
        Ok(EchoValidationStatus::NoComparableIdentifier),
        "an outcome without a protocol coin has no typed value to compare"
    );
}

#[test]
fn families_without_a_comparable_identifier_should_never_verify() {
    let listing = VenueListing::new(
        binance_key("BTCUSDT")
            .listing_key()
            .expect("listing key should be valid"),
        binance_spot_instrument().id().clone(),
        Some(NativeSymbol::new("BTCUSDT").expect("symbol should be valid")),
        vec![NativeSymbol::new("BTC-USDT").expect("alias should be valid")],
        binance_spot_identifiers(),
    )
    .expect("listing should be valid");

    // Binance declares no comparable typed echo, so a declared symbol cannot be
    // reported as verified even though the symbols agree with themselves. The
    // status keeps the listing publishable without claiming a verification that
    // never happened.
    assert_eq!(
        listing.validate_echoed_identifiers(|_| SymbolNormalization::AsciiCaseInsensitive),
        Ok(EchoValidationStatus::NoComparableIdentifier),
        "an uncovered family is not a publication error"
    );
}

#[test]
fn native_identifier_resolution_should_separate_its_three_levels() {
    let listing = binance_key("BTCUSDT")
        .listing_key()
        .expect("listing should be valid");
    let group =
        crate::outcome::OutcomeGroupId::from_identity_value(&format!("v1/{}", "1".repeat(64)))
            .expect("group identity");
    let event =
        crate::outcome::OutcomeEventId::from_identity_value(&format!("v1/{}", "2".repeat(64)))
            .expect("event identity");
    let resolutions = [
        NativeIdentifierResolution::Listing(listing),
        NativeIdentifierResolution::OutcomeGroup(group.clone()),
        NativeIdentifierResolution::OutcomeEvent(event.clone()),
    ];

    assert_ne!(resolutions[0], resolutions[1]);
    assert_eq!(
        resolutions[1],
        NativeIdentifierResolution::OutcomeGroup(group)
    );
    assert_eq!(
        resolutions[2],
        NativeIdentifierResolution::OutcomeEvent(event)
    );
    assert!(matches!(
        NativeIdentifier::Group(OutcomeGroupIdentifier::PolymarketCondition(
            CtfConditionId::new("0x".to_owned() + &"b".repeat(64)).expect("condition")
        )),
        NativeIdentifier::Group(_)
    ));
    assert!(matches!(
        NativeIdentifier::Event(OutcomeEventIdentifier::PolymarketEvent(
            crate::identifier::PolymarketEventId::new("event").expect("event id")
        )),
        NativeIdentifier::Event(_)
    ));
}

#[test]
fn polymarket_listing_should_keep_typed_identifiers() {
    let identifiers = PolymarketListingIdentifiers {
        event_id: Some(
            crate::identifier::PolymarketEventId::new("event-1").expect("event should be valid"),
        ),
        market_id: crate::identifier::PolymarketMarketId::new("market-1")
            .expect("market should be valid"),
        condition_id: CtfConditionId::new("0x".to_owned() + &"c".repeat(64))
            .expect("condition should be valid"),
        clob_token_id: CtfTokenId::new("42").expect("token should be valid"),
        outcome_label: "Yes".into(),
    };

    assert_eq!(identifiers.outcome_label, "Yes");
    assert!(identifiers.event_id.is_some());
}

#[test]
fn expected_price_terms_should_follow_the_instrument() {
    let instrument = Instrument::new(InstrumentTerms::Spot(
        SpotTerms::new(
            AssetId::CryptoToken(
                CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
                    issuer: TokenIssuerId::new("binance").expect("issuer should be valid"),
                    namespace: IdentifierNamespace::new("binance")
                        .expect("namespace should be valid"),
                    token: TokenReference::Native {
                        code: IdentifierValue::new("BTC").expect("code should be valid"),
                    },
                })
                .expect("token should be valid"),
            ),
            AssetId::CryptoToken(
                CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
                    issuer: TokenIssuerId::new("binance").expect("issuer should be valid"),
                    namespace: IdentifierNamespace::new("binance")
                        .expect("namespace should be valid"),
                    token: TokenReference::Native {
                        code: IdentifierValue::new("USDT").expect("code should be valid"),
                    },
                })
                .expect("token should be valid"),
            ),
        )
        .expect("terms should be valid"),
    ))
    .expect("instrument should be valid");
    let (asset, basis) = expected_price_terms(&instrument).expect("basis should resolve");

    assert_eq!(
        asset,
        AssetId::CryptoToken(
            CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
                issuer: TokenIssuerId::new("binance").expect("issuer should be valid"),
                namespace: IdentifierNamespace::new("binance").expect("namespace should be valid"),
                token: TokenReference::Native {
                    code: IdentifierValue::new("USDT").expect("code should be valid"),
                },
            })
            .expect("token should be valid")
        )
    );
    assert_eq!(basis, crate::listing::venue::PriceBasis::PerAssetUnit);
}

#[test]
fn listing_order_should_use_the_full_key() {
    let first = binance_key("AAA")
        .listing_key()
        .expect("listing should be valid");
    let second = binance_key("BBB")
        .listing_key()
        .expect("listing should be valid");

    let sorted = sorted_listing_keys(vec![second.clone(), first.clone(), first.clone()]);

    assert_eq!(sorted.len(), 2);
    assert_eq!(sorted[0].listing_id, first.listing_id);
    assert_eq!(sorted[1].listing_id, second.listing_id);
}

#[test]
fn listing_publication_should_keep_typed_identifiers_authoritative() {
    let instrument = Instrument::new(InstrumentTerms::Spot(
        SpotTerms::new(
            AssetId::CryptoToken(
                CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
                    issuer: TokenIssuerId::new("binance").expect("issuer should be valid"),
                    namespace: IdentifierNamespace::new("binance")
                        .expect("namespace should be valid"),
                    token: TokenReference::Native {
                        code: IdentifierValue::new("BTC").expect("code should be valid"),
                    },
                })
                .expect("token should be valid"),
            ),
            AssetId::CryptoToken(
                CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
                    issuer: TokenIssuerId::new("binance").expect("issuer should be valid"),
                    namespace: IdentifierNamespace::new("binance")
                        .expect("namespace should be valid"),
                    token: TokenReference::Native {
                        code: IdentifierValue::new("USDT").expect("code should be valid"),
                    },
                })
                .expect("token should be valid"),
            ),
        )
        .expect("terms should be valid"),
    ))
    .expect("instrument should be valid");
    let listing = VenueListing::new(
        binance_key("BTCUSDT")
            .listing_key()
            .expect("listing key should be valid"),
        instrument.id().clone(),
        Some(NativeSymbol::new("BTCUSDT").expect("symbol should be valid")),
        Vec::new(),
        VenueListingIdentifiers::Binance(crate::listing::identifiers::BinanceListingIdentifiers {
            product: crate::listing::venue::BinanceProduct::Spot,
            future_series: None,
            option_series: None,
        }),
    )
    .expect("listing should be valid");
    let keys = listing
        .normalized_keys(|_| SymbolNormalization::AsciiCaseInsensitive)
        .expect("keys should normalize");

    assert!(keys.contains(&smol_str::SmolStr::new("BTCUSDT")));
    assert_eq!(
        listing.validate_for(&instrument),
        Ok(()),
        "typed identifiers must agree with the instrument kind"
    );
}

#[test]
fn market_units_should_not_be_confused_with_contract_units() {
    let quantity = MarketQuantity {
        value: crate::decimal::Quantity::from(1_u8),
        unit: QuantityUnit::Contracts,
    };

    assert!(quantity.validate_traded("position").is_ok());
    assert!(quantity.validate_amount("notional").is_err());
    assert_eq!(
        ContractUnit::asset_quantity(
            crate::decimal::Quantity::from(1_u8),
            AssetId::CryptoToken(
                CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
                    issuer: TokenIssuerId::new("binance").expect("issuer should be valid"),
                    namespace: IdentifierNamespace::new("binance")
                        .expect("namespace should be valid"),
                    token: TokenReference::Native {
                        code: IdentifierValue::new("BTC").expect("code should be valid"),
                    },
                })
                .expect("token should be valid"),
            ),
        )
        .expect("unit should be valid")
        .quantity(),
        crate::decimal::Quantity::from(1_u8)
    );
    assert_eq!(
        MarketPrice {
            value: crate::decimal::Price::from(1_u32),
            quote_asset: AssetId::CryptoToken(
                CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
                    issuer: TokenIssuerId::new("binance").expect("issuer should be valid"),
                    namespace: IdentifierNamespace::new("binance")
                        .expect("namespace should be valid"),
                    token: TokenReference::Native {
                        code: IdentifierValue::new("USDT").expect("code should be valid"),
                    },
                })
                .expect("token should be valid"),
            ),
            basis: crate::listing::venue::PriceBasis::PerAssetUnit,
        }
        .validate_against(
            &AssetId::CryptoToken(
                CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
                    issuer: TokenIssuerId::new("binance").expect("issuer should be valid"),
                    namespace: IdentifierNamespace::new("binance")
                        .expect("namespace should be valid"),
                    token: TokenReference::Native {
                        code: IdentifierValue::new("USDT").expect("code should be valid"),
                    },
                })
                .expect("token should be valid"),
            ),
            &crate::listing::venue::PriceBasis::PerAssetUnit
        ),
        Ok(())
    );
}

#[test]
fn canonical_identity_terms_should_be_reachable_for_continuity() {
    let mut encoder = crate::identifier::CanonicalEncoder::new();
    binance_key("BTCUSDT").terms.encode(&mut encoder);

    assert!(!encoder.as_bytes().is_empty());
}

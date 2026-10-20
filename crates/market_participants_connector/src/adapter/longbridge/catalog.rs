//! Construction of the venue's catalog entries from the Nasdaq symbol directory.
//!
//! A directory row is two facts — a code and the market it trades on — and
//! everything a listing needs is derived from them: the issuer and share class the
//! security is identified by, the spot instrument of that security against USD, the
//! rules a US share trades under, and the listing key the row is published as. The
//! derivation happens once, here, so the security a candle request names is the
//! security the directory published rather than a second reading of the same code.

use std::sync::Arc;

use market_participants_model::{
    asset::{
        CurrencyTerms, Iso4217Code, SecurityForm, SecurityIssuerId, SecurityTerms, ShareClassId,
        ShareClassTerms,
    },
    decimal::{Quantity, Tick},
    display::asset::AssetDisplayCode,
    identifier::{EquityLocalCode, NativeSymbol, TradingCalendarId},
    instrument::{Instrument, InstrumentTerms, SpotTerms},
    listing::{
        continuity::{ListingContinuityKey, ListingContinuityTerms},
        identifiers::{EquityListingIdentifiers, VenueListingIdentifiers},
        listing_impl::ListingStatus,
        route::VenueRoute,
        rules::{
            CommonTradingRules, LotPolicy, PriceLimitRule, PriceRule, ProductTradingRules,
            QuantityRule, SettlementCycle, ShortSalePolicy, SpotTradingRules, TradingRules,
            VenueTradingPolicy,
        },
        venue::PriceBasis,
    },
    market::units::QuantityUnit,
};
use smol_str::{SmolStr, format_smolstr};

use super::{SymbolRow, fetch_symbol_rows, symbols::MARKET_SUFFIX};
use crate::{
    adapter::{ExecutionContext, NativeCatalogEntry, ProtocolResponse},
    catalog::{display::publication::StagedAssetCode, registry::asset::AssetTerms},
    error::MarketParticipantError,
    market_participants::model::MarketCatalogQuery,
};

/// The issuer namespace the venue's securities are identified inside.
const ISSUER_NAMESPACE: &str = "longbridge";

/// The tick of one US cent in the fixed-point scale of the model.
///
/// A price or a tick is a decimal with eighteen fractional digits, so one hundredth
/// of a US dollar is ten to the sixteenth.
const CENT: u128 = 10_000_000_000_000_000;

/// The trading calendar the venue's US listings are published under.
///
/// It identifies the calendar a listing's sessions are read against; it carries no
/// session table of its own.
const US_EQUITY_CALENDAR: &str = "us-equity";

/// Turns one symbol-directory row into a complete catalog entry.
///
/// The row's code is the share class designation exactly as the directory published
/// it — a class separator and all — while the issuer the security is identified
/// under is the scrubbed root of the same code, and both `SecurityTerms` and
/// `ListingContinuityTerms` carry the same share class: a security and a listing
/// derived from two different values would be two identities for one row.
pub(crate) fn catalog_entry(
    route: &VenueRoute,
    row: SymbolRow,
) -> Result<NativeCatalogEntry, MarketParticipantError> {
    let issuer_code = issuer_code(&row.code)?;
    let issuer = SecurityIssuerId::new(format_smolstr!("{ISSUER_NAMESPACE}/{issuer_code}"))
        .map_err(|error| model_error("decode the Longbridge security issuer", error))?;
    let share_class_terms = ShareClassTerms {
        issuer: issuer.clone(),
        designation: row.code.clone(),
        form: SecurityForm::CommonStock,
        par_value: None,
        adr_ratio: None,
    };
    share_class_terms
        .validate()
        .map_err(|error| model_error("build the Longbridge share class", error))?;
    let share_class = ShareClassId::new(&share_class_terms)
        .map_err(|error| model_error("build the Longbridge share class", error))?;
    let security_terms = SecurityTerms {
        issuer: issuer.clone(),
        share_class,
        form: SecurityForm::CommonStock,
    };
    let security = AssetTerms::Security {
        terms: security_terms,
        share_class: share_class_terms,
    };
    let usd = AssetTerms::Currency(CurrencyTerms {
        code: Iso4217Code::new("USD").expect("USD is an ISO 4217 code"),
    });

    let spot = SpotTerms::new(security.asset_id(), usd.asset_id())
        .map_err(|error| model_error("build the Longbridge spot instrument", error))?;
    let instrument = Instrument::new(InstrumentTerms::Spot(spot))
        .map_err(|error| model_error("build the Longbridge spot instrument", error))?;

    let rules = TradingRules {
        common: CommonTradingRules {
            price: PriceRule {
                quote_asset: usd.asset_id(),
                basis: PriceBasis::PerAssetUnit,
                tick: Tick::from_inner(CENT)
                    .map_err(|error| model_error("decode the Longbridge price tick", error))?,
                minimum: None,
                maximum: None,
            },
            quantity: QuantityRule {
                unit: QuantityUnit::AssetUnits {
                    asset: security.asset_id(),
                },
                step: Quantity::from(1_u8),
                minimum: Some(Quantity::from(1_u8)),
                maximum: None,
            },
            notional: None,
            venue_policy: Arc::new(VenueTradingPolicy {
                sessions: TradingCalendarId::new(US_EQUITY_CALENDAR).map_err(|error| {
                    model_error("decode the Longbridge trading calendar", error)
                })?,
                settlement_cycle: SettlementCycle::TPlus { business_days: 1 },
                price_limits: PriceLimitRule::None,
            }),
        },
        product: Arc::new(ProductTradingRules::Spot(
            SpotTradingRules::new(
                LotPolicy::WholeLots {
                    lot_size: Quantity::from(1_u8),
                },
                ShortSalePolicy::Restricted,
            )
            .map_err(|error| model_error("build the Longbridge spot rules", error))?,
        )),
    };
    rules
        .validate_for(&instrument)
        .map_err(|error| model_error("validate the Longbridge trading rules", error))?;

    let native_symbol = NativeSymbol::new(format_smolstr!("{}.{MARKET_SUFFIX}", row.code))
        .map_err(|error| model_error("decode the Longbridge native symbol", error))?;
    let aliases = vec![
        NativeSymbol::new(row.code.as_str())
            .map_err(|error| model_error("decode the Longbridge native symbol", error))?,
    ];
    let local_code = EquityLocalCode::new(row.code.as_str())
        .map_err(|error| model_error("decode the Longbridge local code", error))?;
    let identifiers = VenueListingIdentifiers::Equity(EquityListingIdentifiers {
        mic: row.mic.clone(),
        board: None,
        ticker: row.code.clone(),
        local_code: local_code.clone(),
    });
    // A security states no display code of its own — its issuer, designation and form
    // are not a name — so the code the venue published for it is what a name renders,
    // and the publication refuses a batch whose named asset has none. The directory's
    // own symbol is that code: it is the ticker the market addresses the security by.
    let security_code = AssetDisplayCode::from_staged_code(row.code.as_str())
        .map_err(|error| model_error("decode the Longbridge display code", error))?;
    let asset_codes = vec![StagedAssetCode {
        asset: security.asset_id(),
        code: security_code,
    }];

    let terms = ListingContinuityTerms::Equity {
        issuer,
        share_class,
        mic: row.mic,
        local_code,
    };
    let listing = ListingContinuityKey {
        route: route.clone(),
        terms,
    }
    .listing_key()
    .map_err(|error| model_error("derive the Longbridge listing identity", error))?;

    Ok(NativeCatalogEntry {
        listing,
        native_symbol,
        aliases,
        identifiers,
        rules,
        // The directory states what is listed, not what is trading: `Financial
        // Status` is a compliance state (a deficient issuer still trades), so this
        // is the only honest value a row carries.
        status: ListingStatus::Trading,
        available_from: None,
        available_until: None,
        instrument,
        asset_terms: vec![security, usd],
        // A spot instrument tracks no price reference of its own.
        reference_terms: Vec::new(),
        asset_codes,
    })
}

/// Fetches and publishes the venue's symbol directory as its route's catalog.
///
/// The directory is one download and one strict parse, and every row that survives
/// it becomes a listing. The answer is the route's catalog rather than a directory
/// of its own: the model has one shape of "what a route publishes", and a second
/// one would be a second thing for every reader of a catalog to know about.
pub(crate) async fn fetch_markets(
    context: &ExecutionContext,
    query: &MarketCatalogQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    let route = query.route();
    let rows = fetch_symbol_rows(context).await?;
    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        entries.push(catalog_entry(route, row)?);
    }
    entries.sort_unstable_by(|left, right| left.native_symbol.cmp(&right.native_symbol));
    Ok(ProtocolResponse::Markets(entries))
}

/// Returns the issuer code one directory symbol is identified by.
///
/// A `SecurityIssuerId` admits alphanumerics only, and a directory symbol may carry
/// a class separator, a preferred-share mark or an index caret. The code is
/// therefore the part before the first separator with everything outside the
/// admitted alphabet dropped — the directory's own symbol is the share class
/// designation and stays exactly as published, so nothing here is ever a display
/// value.
fn issuer_code(code: &str) -> Result<SmolStr, MarketParticipantError> {
    // The leading separator has to go first: a preferred share (`$XYZ`) or an index
    // (`^IXIC`) starts with one, and splitting at index zero would answer an empty
    // root.
    let trimmed = code.trim_start_matches(['.', '-', '^', '$']);
    let root = trimmed
        .split(['.', '-', '^', '$'])
        .next()
        .unwrap_or_default();
    let cleaned: SmolStr = format_smolstr!(
        "{}",
        root.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_uppercase()
    );
    if cleaned.is_empty() {
        return Err(protocol_error(
            "decode the Longbridge issuer",
            format_smolstr!("{code} names no issuer root"),
        ));
    }
    Ok(cleaned)
}

/// Builds one protocol failure that names the operation it was raised in.
fn protocol_error(operation: &'static str, reason: impl Into<SmolStr>) -> MarketParticipantError {
    MarketParticipantError::ProtocolError {
        operation: operation.into(),
        reason: reason.into(),
        raw_data: None,
    }
}

/// Wraps one model rejection as the protocol failure of an operation.
fn model_error(operation: &'static str, error: impl std::fmt::Display) -> MarketParticipantError {
    protocol_error(operation, format_smolstr!("{error}"))
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        asset::{AssetId, CurrencyId, CurrencyTerms, Iso4217Code, SecurityForm},
        decimal::Tick,
        identifier::{Mic, VenueId, VenueScopeId},
        instrument::{InstrumentKind, InstrumentTerms},
        listing::{
            continuity::{ListingContinuityKey, ListingContinuityTerms},
            identifiers::{EchoValidationStatus, VenueListingIdentifiers},
            listing_impl::{ListingStatus, VenueListing},
            route::VenueRoute,
        },
        market::units::QuantityUnit,
        routing::{CapabilityId, ListingIdentifierKind, SymbolNormalization},
    };
    use smol_str::SmolStr;

    use super::{
        super::symbols::{NASDAQ_LISTED_FILE, OTHER_LISTED_FILE},
        CENT, SymbolRow, catalog_entry, fetch_markets, issuer_code,
    };
    use crate::{
        adapter::ProtocolResponse,
        catalog::registry::asset::AssetTerms,
        error::MarketParticipantError,
        market_participants::model::MarketCatalogQuery,
        routing::LONGBRIDGE_US_SCOPE,
        test_support::{
            self, DirectoryAnswer, NASDAQ_LISTED_SAMPLE, OTHER_LISTED_SAMPLE,
            SymbolDirectoryFixture,
        },
    };

    /// One `nasdaqlisted.txt` body whose third row carries a control character after a
    /// designator, which is the one shape the parse must drop before this module sees it.
    const NASDAQ_LISTED_WITH_ONE_UNREADABLE_ROW: &str = concat!(
        "Symbol|Security Name|Market Category|Test Issue|Financial Status|Round Lot Size|ETF|NextShares\r\n",
        "AAPL|Apple Inc. Common Stock|Q|N|N|100|N|N\r\n",
        "A.B\u{0}C|A Control Character After A Designator|Q|N|N|100|N|N\r\n",
        "$XYZ|A Preferred Share|Q|N|N|100|N|N\r\n",
        "^IXIC|An Index|Q|N|N|100|N|N\r\n",
        "File Creation Time: 1009202621:31|||||||\r\n",
    );

    /// The other half of the directory, with no row of its own: a load reads both files,
    /// and the row this test is about is in the first one.
    const OTHER_LISTED_WITH_NO_ROWS: &str = concat!(
        "ACT Symbol|Security Name|Exchange|CQS Symbol|ETF|Round Lot Size|Test Issue|NASDAQ Symbol\r\n",
        "File Creation Time: 1009202621:31||||||\r\n",
    );

    /// The route the venue publishes its US listings on.
    fn route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Longbridge,
            VenueScopeId::new(LONGBRIDGE_US_SCOPE).expect("the US scope is canonical"),
        )
    }

    /// One directory row, as the parse would have published it.
    fn row(code: &str, mic: &str) -> SymbolRow {
        SymbolRow {
            code: SmolStr::new(code),
            name: SmolStr::new_static("Apple Inc. Common Stock"),
            mic: Mic::new(mic).expect("the fixture MIC is canonical"),
        }
    }

    /// The normalization the route declares, which is what an echo is compared
    /// under: a value is verified against the declared symbols in the mode of its
    /// own identifier kind.
    fn declared_mode(kind: ListingIdentifierKind) -> SymbolNormalization {
        crate::venue::specs::scope_entries(VenueId::Longbridge)
            .into_iter()
            .find(|entry| entry.scope.as_str() == LONGBRIDGE_US_SCOPE)
            .expect("the venue declares its US scope")
            .normalization
            .into_iter()
            .find(|rule| rule.identifier == kind)
            .map_or(SymbolNormalization::Exact, |rule| rule.mode)
    }

    /// One row of the directory becomes a spot listing of the security it names,
    /// with the aliases and typed identifiers a request is resolved by.
    #[test]
    fn one_directory_row_becomes_a_spot_listing() {
        let entry = catalog_entry(&route(), row("AAPL", "XNAS")).expect("the row is publishable");

        assert_eq!(entry.instrument.kind(), InstrumentKind::Spot);
        assert_eq!(entry.native_symbol.as_str(), "AAPL.US");
        assert_eq!(
            entry
                .aliases
                .iter()
                .map(|alias| alias.as_str())
                .collect::<Vec<_>>(),
            ["AAPL"]
        );
        assert_eq!(entry.status, ListingStatus::Trading);
        assert!(
            entry.reference_terms.is_empty(),
            "a spot instrument tracks no price reference"
        );

        let VenueListingIdentifiers::Equity(identifiers) = &entry.identifiers else {
            panic!("a US share is an equity listing");
        };
        assert_eq!(identifiers.mic.as_str(), "XNAS");
        assert!(identifiers.board.is_none());
        assert_eq!(identifiers.ticker, "AAPL");
        assert_eq!(identifiers.local_code.as_str(), "AAPL");
        assert_eq!(identifiers.instrument_kind(), InstrumentKind::Spot);

        // The rules the entry states are the rules the instrument validates under,
        // which is what the catalogue's publication gate re-runs.
        entry
            .rules
            .validate_for(&entry.instrument)
            .expect("the entry's rules belong to its instrument");

        // The price is quoted in the currency the instrument is quoted in, and the
        // quantity is measured in the security itself.
        let InstrumentTerms::Spot(spot) = entry.instrument.terms() else {
            panic!("the instrument is a spot instrument");
        };
        assert_eq!(
            entry.rules.common.price.quote_asset,
            AssetId::Currency(CurrencyId::of_terms(&CurrencyTerms {
                code: Iso4217Code::new("USD").expect("USD is an ISO 4217 code"),
            }))
        );
        assert_eq!(&entry.rules.common.price.quote_asset, spot.quote());
        assert_eq!(
            entry.rules.common.quantity.unit,
            QuantityUnit::AssetUnits {
                asset: spot.base().clone()
            }
        );
        assert_eq!(
            entry.rules.common.price.tick,
            Tick::from_inner(CENT).expect("one cent is a positive tick")
        );
    }

    /// Both assets a listing's rules name travel with it, and the security among
    /// them is the instrument's own base.
    #[test]
    fn the_entry_carries_both_of_its_assets() {
        let entry = catalog_entry(&route(), row("AAPL", "XNAS")).expect("the row is publishable");

        assert_eq!(entry.asset_terms.len(), 2);
        let AssetTerms::Security { terms, share_class } = &entry.asset_terms[0] else {
            panic!("the first asset of a share listing is the security");
        };
        assert_eq!(share_class.designation, "AAPL");
        assert_eq!(terms.form, SecurityForm::CommonStock);

        let AssetTerms::Currency(currency) = &entry.asset_terms[1] else {
            panic!("the second asset of a share listing is its currency");
        };
        assert_eq!(
            currency.code,
            Iso4217Code::new("USD").expect("USD is an ISO 4217 code")
        );

        let InstrumentTerms::Spot(spot) = entry.instrument.terms() else {
            panic!("the instrument is a spot instrument");
        };
        assert_eq!(spot.base(), &entry.asset_terms[0].asset_id());
        assert_eq!(spot.quote(), &entry.asset_terms[1].asset_id());
    }

    /// The listing identity is derived from the same security the entry publishes:
    /// a listing built from the entry's own typed identifiers and its own security
    /// terms is the listing key the entry states.
    #[test]
    fn the_listing_identity_is_the_security_it_publishes() {
        let entry = catalog_entry(&route(), row("BRK.B", "XNYS")).expect("the row is publishable");
        let AssetTerms::Security { terms, .. } = &entry.asset_terms[0] else {
            panic!("the first asset of a share listing is the security");
        };
        let VenueListingIdentifiers::Equity(identifiers) = &entry.identifiers else {
            panic!("a US share is an equity listing");
        };

        let expected = ListingContinuityKey {
            route: route(),
            terms: ListingContinuityTerms::Equity {
                issuer: terms.issuer.clone(),
                share_class: terms.share_class,
                mic: identifiers.mic.clone(),
                local_code: identifiers.local_code.clone(),
            },
        }
        .listing_key()
        .expect("the fixture terms determine one listing");

        assert_eq!(entry.listing, expected);
        assert_eq!(entry.native_symbol.as_str(), "BRK.B.US");
    }

    /// The typed identifiers and the declared symbols are one identity: the code a
    /// listing is addressed by is the code it echoes, under the mode its route
    /// declares.
    #[test]
    fn the_listing_echoes_the_code_it_is_addressed_by() {
        let entry = catalog_entry(&route(), row("AAPL", "XNAS")).expect("the row is publishable");

        let listing = VenueListing::new(
            entry.listing.clone(),
            *entry.instrument.id(),
            Some(entry.native_symbol.clone()),
            entry.aliases.clone(),
            entry.identifiers.clone(),
        )
        .expect("the identifiers belong to their route");
        listing
            .validate_for(&entry.instrument)
            .expect("the listing agrees with the instrument it implements");

        assert_eq!(
            listing
                .validate_echoed_identifiers(declared_mode)
                .expect("the echo check runs"),
            EchoValidationStatus::Verified
        );
    }

    /// The market a row states is the market its identifiers carry, including the
    /// one market-participant code the field definitions omit.
    #[test]
    fn the_row_keeps_the_market_it_states() {
        let entry = catalog_entry(&route(), row("DDS", "TXSE")).expect("the row is publishable");
        let VenueListingIdentifiers::Equity(identifiers) = &entry.identifiers else {
            panic!("a US share is an equity listing");
        };

        assert_eq!(identifiers.mic.as_str(), "TXSE");
        assert_eq!(entry.native_symbol.as_str(), "DDS.US");
    }

    /// The issuer root is the part of a directory code a `SecurityIssuerId` admits:
    /// a class separator, a preferred-share mark and an index caret all leave the
    /// root, and a code that is nothing but separators names no issuer at all.
    #[test]
    fn the_issuer_code_scrubs_every_separator_the_directory_uses() {
        for (code, expected) in [
            ("AAPL", "AAPL"),
            ("BRK.B", "BRK"),
            ("BRK-B", "BRK"),
            ("ET$I", "ET"),
            ("^IXIC", "IXIC"),
            ("$XYZ", "XYZ"),
            ("ZXYZ.A", "ZXYZ"),
            ("aapl", "AAPL"),
        ] {
            assert_eq!(
                issuer_code(code).expect("the code names an issuer root"),
                expected,
                "{code}"
            );
        }

        for code in ["...", "$", "^", "-", "."] {
            assert!(
                matches!(
                    issuer_code(code),
                    Err(MarketParticipantError::ProtocolError { .. })
                ),
                "{code} names no issuer root"
            );
        }
    }

    /// A directory load becomes the route's catalog, sorted by the symbol each
    /// listing is addressed by, and the rows a product decision drops are not in it.
    #[tokio::test]
    async fn a_loaded_directory_becomes_the_routes_catalog() {
        let _fixture = SymbolDirectoryFixture::install(vec![
            (
                NASDAQ_LISTED_FILE,
                DirectoryAnswer::File(NASDAQ_LISTED_SAMPLE),
            ),
            (
                OTHER_LISTED_FILE,
                DirectoryAnswer::File(OTHER_LISTED_SAMPLE),
            ),
        ])
        .await;
        let context = test_support::loopback_execution(route(), CapabilityId::MarketCatalog);
        let query = MarketCatalogQuery { route: route() };

        let response = fetch_markets(&context, &query)
            .await
            .expect("the fixture directory reads");
        let ProtocolResponse::Markets(entries) = response else {
            panic!("a catalog fetch answers with the route's catalog");
        };

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.native_symbol.as_str())
                .collect::<Vec<_>>(),
            ["A.US", "AA.US", "AACG.US"],
            "the ETF rows are not listings and the answer is sorted by native symbol"
        );
        for entry in &entries {
            assert_eq!(entry.listing.route, route());
        }
    }

    /// One row the parse cannot address is one row dropped, not a directory that fails
    /// to load: a published directory holding `A.B\0C` still becomes the route's catalog,
    /// and that row is not in it.
    ///
    /// This is the failure the alphabet rule exists to remove, measured at the seam it
    /// happens at: the parse is where a row is judged, so a row that reached
    /// [`catalog_entry`] would make this whole fetch an `Err` — one row of unsigned
    /// remote content taking the venue's catalog down with it. The codes below the bad
    /// one are the other half of the same rule: `$XYZ` and `^IXIC` start with a
    /// designator and are addressed rather than dropped, which is what their issuer roots
    /// being `XYZ` and `IXIC` states.
    #[tokio::test]
    async fn a_directory_holding_one_unreadable_row_still_becomes_the_catalog() {
        let _fixture = SymbolDirectoryFixture::install(vec![
            (
                NASDAQ_LISTED_FILE,
                DirectoryAnswer::File(NASDAQ_LISTED_WITH_ONE_UNREADABLE_ROW),
            ),
            (
                OTHER_LISTED_FILE,
                DirectoryAnswer::File(OTHER_LISTED_WITH_NO_ROWS),
            ),
        ])
        .await;
        let context = test_support::loopback_execution(route(), CapabilityId::MarketCatalog);
        let query = MarketCatalogQuery { route: route() };

        let response = fetch_markets(&context, &query)
            .await
            .expect("a directory with one unaddressable row still loads");
        let ProtocolResponse::Markets(entries) = response else {
            panic!("a catalog fetch answers with the route's catalog");
        };

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.native_symbol.as_str())
                .collect::<Vec<_>>(),
            ["$XYZ.US", "AAPL.US", "^IXIC.US"],
            "the unaddressable row is dropped, the order is the native symbol's, and the \
             leading designators are published"
        );
        assert!(
            entries
                .iter()
                .all(|entry| entry.native_symbol.as_str() != "A.B\u{0}C.US"),
            "the control character reached no identifier"
        );
        for (symbol, root) in [("$XYZ.US", "XYZ"), ("^IXIC.US", "IXIC")] {
            let entry = entries
                .iter()
                .find(|entry| entry.native_symbol.as_str() == symbol)
                .unwrap_or_else(|| panic!("{symbol} is published"));
            let AssetTerms::Security { terms, .. } = &entry.asset_terms[0] else {
                panic!("the first asset of a share listing is the security");
            };
            assert_eq!(
                terms.issuer.as_str(),
                format!("longbridge/{root}"),
                "{symbol} is identified by the root its leading designator delimits"
            );
        }
    }
}

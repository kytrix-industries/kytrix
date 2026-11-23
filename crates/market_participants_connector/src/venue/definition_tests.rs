use app_config::market_participants::settings::default_venues;
use futures::future::BoxFuture;
use market_participants_model::{
    identifier::VenueId,
    instrument::InstrumentKind,
    routing::{
        CapabilityId, IdentifierNormalizationProduct, IdentifierNormalizationRule,
        ListingIdentifierKind, SymbolNormalization,
    },
};

use super::{
    config::{LoadCancellation, VenueConfig, VenueConversation},
    definition::{
        CapabilitySupport, VenueLoadData, definition_of, supported_venues, venues_supporting,
    },
    metadata::{VenueMetadata, VenueReading},
    specs,
};
use crate::{
    catalog::source::CatalogSourceId,
    error::MarketParticipantError,
    routing::{
        BINANCE_VENUE, CORE_PERP_SCOPE, CORE_SPOT_SCOPE, HYPERLIQUID_VENUE, LONGBRIDGE_US_SCOPE,
        LONGBRIDGE_VENUE, OKX_VENUE, PERPETUAL_SCOPE, SPOT_SCOPE, SWAP_SCOPE,
    },
};

fn venue(name: &str) -> VenueId {
    VenueId::from_name(name).expect("the built-in venue identity is canonical")
}

struct NoDeployments;

impl VenueConversation for NoDeployments {
    fn read_venue(&self) -> BoxFuture<'static, Result<VenueReading, MarketParticipantError>> {
        Box::pin(async {
            Ok(VenueReading::Verified {
                deployments: Vec::new(),
                outcome_scopes: Vec::new(),
                metadata: std::sync::Arc::new(VenueMetadata::empty()),
            })
        })
    }
}

async fn load(venue: &VenueId) -> VenueLoadData {
    let config = VenueConfig::new(*venue, std::sync::Arc::new(NoDeployments));
    let cancellation = LoadCancellation::new();
    definition_of(*venue)
        .loader
        .load(&config, &cancellation)
        .await
        .expect("the fixture venue publishes its scopes")
}

async fn scope_names(venue: &VenueId) -> Vec<String> {
    load(venue)
        .await
        .scopes
        .iter()
        .map(|entry| entry.scope.as_str().to_owned())
        .collect()
}

#[tokio::test]
async fn the_definition_table_lists_every_venue() {
    let venues = supported_venues();

    assert_eq!(
        venues,
        vec![
            venue("binance"),
            venue("okx"),
            venue("hyperliquid"),
            venue("longbridge"),
            venue("polymarket")
        ],
        "the build speaks five venues, and polymarket is the last of them"
    );
    assert_eq!(
        supported_venues(),
        venues,
        "the venue list has one order for every caller"
    );

    for name in ["binance", "okx", "hyperliquid", "longbridge", "polymarket"] {
        let supported = venue(name);
        let definition = definition_of(supported);
        assert_eq!(
            definition.venue, supported,
            "the definition names its venue"
        );
        assert!(
            !definition.capabilities.is_empty(),
            "{supported} declares the capabilities its loader produces"
        );
    }

    assert_eq!(
        scope_names(&venue("binance")).await,
        vec!["spot", "usd-m-perp"]
    );
    assert_eq!(scope_names(&venue("okx")).await, vec!["spot", "swap"]);
    assert_eq!(
        scope_names(&venue("hyperliquid")).await,
        vec!["core-spot", "core-perp"]
    );
    assert_eq!(scope_names(&venue("longbridge")).await, vec!["us"]);
    assert_eq!(scope_names(&venue("polymarket")).await, vec!["predict"]);

    for supported in &venues {
        let definition = definition_of(*supported);
        let mut declared: Vec<CapabilityId> = definition.capabilities.to_vec();
        if *supported == venue("hyperliquid") {
            declared.retain(|capability| {
                !matches!(
                    capability,
                    CapabilityId::PredictionEvents | CapabilityId::PredictionSeries
                )
            });
        }
        declared.sort_unstable();
        let mut produced: Vec<CapabilityId> = load(supported)
            .await
            .scopes
            .iter()
            .flat_map(|entry| entry.capabilities.iter().map(|support| support.capability))
            .collect();
        produced.sort_unstable();
        produced.dedup();

        assert_eq!(
            produced, declared,
            "{supported} declares exactly the capabilities its scopes carry"
        );
    }
}

#[test]
fn the_definition_table_covers_every_venue_this_build_serves() {
    let listed = supported_venues();
    let mut listed_sorted = listed.clone();
    listed_sorted.sort_unstable();
    assert_eq!(
        listed,
        vec![
            venue("binance"),
            venue("okx"),
            venue("hyperliquid"),
            venue("longbridge"),
            venue("polymarket")
        ],
        "the served set is exactly the five product venues, in product order"
    );

    let mut all: Vec<VenueId> = VenueId::ALL.to_vec();
    all.sort_unstable();
    assert_eq!(
        listed_sorted, all,
        "every venue the identity carries is one this build serves"
    );

    let answered: Vec<VenueId> = VenueId::ALL
        .into_iter()
        .filter(|venue| definition_of(*venue).venue == *venue)
        .collect();
    assert_eq!(
        answered, all,
        "the lookup answers exactly the venues the table serves"
    );
}

#[test]
fn a_definition_carries_its_capability_names() {
    assert_eq!(
        venues_supporting(CapabilityId::ForceOrder),
        vec![venue("binance"), venue("okx")],
        "Hyperliquid publishes no liquidation stream"
    );
    assert_eq!(
        venues_supporting(CapabilityId::Candlesticks),
        vec![
            venue("binance"),
            venue("okx"),
            venue("hyperliquid"),
            venue("longbridge")
        ],
        "every venue answers candlesticks"
    );
    assert_eq!(
        venues_supporting(CapabilityId::Wallet),
        vec![venue("hyperliquid")],
        "only Hyperliquid monitors a wallet"
    );
    assert_eq!(
        venues_supporting(CapabilityId::AllTickers),
        vec![venue("binance"), venue("okx"), venue("hyperliquid")],
        "Longbridge answers tickers one listing at a time, so it serves no whole-scope read"
    );
    assert_eq!(
        venues_supporting(CapabilityId::PredictionOrderBook),
        vec![venue("polymarket")],
        "only the prediction venue publishes a book for an event leg"
    );
}

#[test]
fn a_definition_states_whether_a_load_needs_credentials() {
    for venue_name in ["binance", "okx", "hyperliquid"] {
        let definition = definition_of(venue(venue_name));
        assert!(
            !definition.requires_credentials_to_load,
            "{venue_name}'s reads are public, so a load of it is offered as it is"
        );
    }
    let longbridge = definition_of(venue("longbridge"));
    assert!(
        longbridge.requires_credentials_to_load,
        "every read of the venue's market signs, so a load without a credential is not worth offering"
    );
}

#[test]
fn the_longbridge_route_declares_one_us_scope() {
    let entries = specs::scope_entries(venue(LONGBRIDGE_VENUE));

    assert_eq!(entries.len(), 1, "the venue publishes one scope");
    let entry = &entries[0];
    assert_eq!(entry.scope.as_str(), LONGBRIDGE_US_SCOPE);
    assert_eq!(
        entry.catalog_source.as_ref().map(CatalogSourceId::as_str),
        Some("longbridge-us-catalog"),
        "the scope's directory is the venue's own US catalog"
    );
    assert_eq!(
        entry.capabilities,
        vec![
            CapabilitySupport {
                capability: CapabilityId::MarketCatalog,
                needs_auth: false,
            },
            CapabilitySupport {
                capability: CapabilityId::Candlesticks,
                needs_auth: true,
            },
            CapabilitySupport {
                capability: CapabilityId::Tickers,
                needs_auth: true,
            },
        ],
        "the directory is public, and candlesticks and tickers are authenticated"
    );
    assert_eq!(
        entry.normalization,
        vec![IdentifierNormalizationRule::new(
            ListingIdentifierKind::Symbol,
            Some(IdentifierNormalizationProduct::Instrument(
                InstrumentKind::Spot
            )),
            SymbolNormalization::AsciiCaseInsensitive,
        )],
        "the display symbol folds ASCII case, and no second rule is declared"
    );
}

#[test]
fn every_static_scope_is_named_by_the_venues_settings() {
    let configured = default_venues();
    let expected = [
        (venue(BINANCE_VENUE), vec![SPOT_SCOPE, PERPETUAL_SCOPE]),
        (venue(OKX_VENUE), vec![SPOT_SCOPE, SWAP_SCOPE]),
        (
            venue(HYPERLIQUID_VENUE),
            vec![CORE_SPOT_SCOPE, CORE_PERP_SCOPE],
        ),
        (venue(LONGBRIDGE_VENUE), vec![LONGBRIDGE_US_SCOPE]),
    ];

    for (venue, scopes) in expected {
        let settings = configured
            .get(&venue)
            .unwrap_or_else(|| panic!("{venue} is configured by the built-in table"));
        assert_eq!(
            settings.scope_ids(),
            scopes.as_slice(),
            "{venue}'s settings publish exactly the scopes this build routes, in the order it routes them"
        );
        for scope in &scopes {
            assert!(
                settings.publishes(scope),
                "{venue} is configured without its {scope} scope"
            );
            assert!(
                !settings.publishes("perp-dex/xyz"),
                "{venue} names no scope of its own for a scope it deploys"
            );
        }
        assert!(
            settings.publishes(settings.primary_scope()),
            "{venue}'s primary scope is one of its own, which is what a scope it deploys is reached through"
        );
    }
}

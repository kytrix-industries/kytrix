//! Unit tests of the static definition table.
//!
//! The table is read the way its two public accessors read it: the venue list is
//! exactly the venues this build serves and has one stable order, and a
//! capability name maps to the venues whose loader can produce it. The definitions
//! are pinned to the scopes their loaders publish, so a definition, its capability
//! names and its scope set cannot drift apart.
//!
//! A loader is asked the way the load transaction asks it: through the venue's own
//! connection configuration and with a cancellation nobody has cancelled. The
//! fixture conversation answers no deployment, so a venue that would discover
//! scopes of its own answers its statically known ones here.

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

/// One built-in venue identity.
fn venue(name: &str) -> VenueId {
    VenueId::from_name(name).expect("the built-in venue identity is canonical")
}

/// A venue conversation that deploys nothing and publishes no metadata.
struct NoDeployments;

impl VenueConversation for NoDeployments {
    fn read_venue(&self) -> BoxFuture<'static, Result<VenueReading, MarketParticipantError>> {
        Box::pin(async {
            Ok(VenueReading::Verified {
                deployments: Vec::new(),
                metadata: std::sync::Arc::new(VenueMetadata::empty()),
            })
        })
    }
}

/// Asks one venue's loader what it publishes.
async fn load(venue: &VenueId) -> VenueLoadData {
    let config = VenueConfig::new(*venue, std::sync::Arc::new(NoDeployments));
    let cancellation = LoadCancellation::new();
    definition_of(*venue)
        .loader
        .load(&config, &cancellation)
        .await
        .expect("the fixture venue publishes its scopes")
}

/// The scope names one venue's loader publishes, in order.
async fn scope_names(venue: &VenueId) -> Vec<String> {
    load(venue)
        .await
        .scopes
        .iter()
        .map(|entry| entry.scope.as_str().to_owned())
        .collect()
}

/// The table lists exactly the venues this build serves, in one stable order, and
/// each of them has a definition that produces the scopes it is defined by.
///
/// The capability names of a definition are checked against what its loader
/// actually produces, so the venue-level list the unloaded hints read cannot
/// claim a capability no scope of that venue carries.
#[tokio::test]
async fn the_definition_table_lists_four_venues() {
    let venues = supported_venues();

    assert_eq!(
        venues,
        vec![
            venue("binance"),
            venue("okx"),
            venue("hyperliquid"),
            venue("longbridge")
        ],
        "the build speaks four venues, and longbridge is the last of them"
    );
    assert_eq!(
        supported_venues(),
        venues,
        "the venue list has one order for every caller"
    );

    for name in ["binance", "okx", "hyperliquid", "longbridge"] {
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

    for supported in &venues {
        let definition = definition_of(*supported);
        let mut declared: Vec<CapabilityId> = definition.capabilities.to_vec();
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

/// The definition table lists every venue this build serves, and no other.
///
/// The table's entries and [`definition_of`]'s match are one fact stated twice, and
/// this is the guard that keeps them from drifting apart in either direction: the
/// served list is exactly the product venues in product order, and every identity the
/// list names is one the lookup answers. A venue added to [`VenueId`] is already a
/// compile error in that match; what this closes is the other direction — an entry no
/// arm answers would still leave `supported_venues()` long, and every caller that
/// walks it (the market menu, the settings rows, the notices that name markets) would
/// draw a venue nothing can load. Comparing the table against the identity's own set
/// is what holds that.
///
/// The lookup and the table answer the same set, which is every venue the identity
/// carries: the venue list has no room left for one that is carried and unserved, and
/// a venue that becomes unserved again is a compile error in the match above rather
/// than an absence here.
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
            venue("longbridge")
        ],
        "the served set is exactly the four product venues, in product order"
    );

    let mut all: Vec<VenueId> = VenueId::ALL.to_vec();
    all.sort_unstable();
    assert_eq!(
        listed_sorted, all,
        "every venue the identity carries is one this build serves"
    );

    // The lookup and the table are one fact stated twice: the match answers one
    // definition per identity and the table lists the same venues, so an entry nothing
    // answers and an answer no entry backs are both impossible here. This is also what
    // makes `VenueLoadError::UnsupportedVenue` unreachable in this build.
    let answered: Vec<VenueId> = VenueId::ALL
        .into_iter()
        .filter(|venue| definition_of(*venue).venue == *venue)
        .collect();
    assert_eq!(
        answered, all,
        "the lookup answers exactly the venues the table serves"
    );
}

/// A definition answers the capability names its loader produces, and nothing
/// else: the liquidation stream belongs to the two market participants that publish one, the
/// wallet monitor to the venue that has one, the whole-scope ticker read to the venues
/// that answer it, and a capability no loader produces to no venue at all.
#[test]
fn a_definition_carries_its_capability_names() {
    assert_eq!(
        venues_supporting(CapabilityId::ForceOrders),
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
}

/// The venue's two definitions state whether a load without credentials is worth
/// offering, and the two answers are the two kinds of read there are.
///
/// A venue whose every read signs cannot be read at all without a credential, so a
/// load of it would leave a market nothing can open; a venue whose reads are public
/// is offered unconditionally, and none of the three market participants has a read that needs
/// a key. Longbridge's directory is public and the market behind it is not, which is
/// the one case where the question has to be asked before a load is started.
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

/// The one Longbridge route declares what this build publishes for the venue: one US-equity
/// scope, the catalog source its directory is bound to, its three capabilities with whether
/// each needs the venue's credential, and one normalization rule.
///
/// This test is what pins the row, through the same [`specs::scope_entries`] the venue's
/// loader answers with, so a changed scope name, catalog source, capability or rule fails
/// here rather than reaching a load. The venue's own settings are the other half of the same
/// facts, and [`every_static_scope_is_named_by_the_venues_settings`] holds the scope name
/// across the two crates.
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

/// Every statically published scope of this build is one the venue's own settings
/// publish — and the settings publish nothing else.
///
/// `app_config` cannot depend on this crate, so the scopes a venue's settings carry
/// and the scope constants this build routes by are two literal tables that nothing
/// but this test holds together: a rename here that missed the settings would leave
/// the scope addressed by a route whose venue answers no origins and no credentials
/// for it, and a name the settings carry while this build routes nothing by it would
/// be a row nothing can reach. Neither of those would any other test notice. The
/// scopes are listed by constant, so the assertion is about the names this crate's
/// own routes carry rather than about strings this test re-typed.
///
/// A scope a venue deploys at runtime — HyperCore's `perp-dex/*` — is deliberately
/// absent from both lists: it is not a scope a venue publishes, and no settings row
/// names one. It is reached through the venue's own primary scope instead.
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

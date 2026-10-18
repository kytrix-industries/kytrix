//! The fixture seam, driven the way a test outside this crate drives it.
//!
//! The seam's own coverage lives here rather than beside it because a fixture is a
//! *configuration*, and this target builds one through the public API the other crates'
//! tests use: `fixture::venues` hands back the settings snapshot a root is built over,
//! with every address the venues it names publish resolving at the sockets the caller
//! owns. Nothing here is process-wide, so two tests in one binary never read each
//! other's sockets.
//!
//! `test-support` is a dev-dependency feature, and this target is what asks for it: the
//! crate's own `[dev-dependencies]` turns it on, so `cargo test -p app_config` builds the
//! seam exactly as a workspace build does.

use app_config::market_participants::{
    fixture::{self, FixtureAddress},
    settings::{
        BINANCE_SPOT, BINANCE_USD_M_PERP, HYPERLIQUID_CORE_PERP, OKX_SPOT, OKX_SWAP, default_venues,
    },
};
use market_participants_model::identifier::VenueId;
use url::Url;

/// A pair of sockets, one per role.
fn everywhere() -> FixtureAddress {
    FixtureAddress {
        http: Url::parse("http://127.0.0.1:8080").expect("the fixture address is a URL"),
        websocket: Url::parse("ws://127.0.0.1:8081").expect("the fixture stream address is a URL"),
    }
}

/// Everything the seam promises, driven in one installation.
#[test]
fn a_fixture_installation_reaches_a_venue_at_the_sockets_it_names() {
    let fixture = everywhere();
    let venues = fixture::venues(VenueId::ALL.map(|venue| (venue, fixture.clone()))).into_venues();

    assert_eq!(
        venues.len(),
        VenueId::ALL.len(),
        "a fixture installation is a complete snapshot"
    );
    for venue in VenueId::ALL {
        for scope in venues[&venue].scope_ids() {
            let origins = venues[&venue]
                .origins(scope)
                .expect("a published scope is reached on a pair of its own");
            assert_eq!(
                (origins.http, origins.websocket),
                (fixture.http.clone(), fixture.websocket.clone()),
                "{venue}/{scope} is reached at the fixture's own sockets"
            );
        }
        let deployed = venues[&venue].deployed_origins();
        assert_eq!(
            (deployed.http, deployed.websocket),
            (fixture.http.clone(), fixture.websocket.clone()),
            "{venue}'s deployed scopes are reached there too"
        );
    }

    assert_eq!(
        venues[&VenueId::Okx].origins(OKX_SPOT),
        venues[&VenueId::Okx].origins(OKX_SWAP),
        "every scope of one venue is reached at the same pair"
    );

    // What an address is *called* never moves, so a file written from a fixture is a
    // file this build decodes and reads back as the same settings.
    let binance =
        serde_json::to_value(&venues[&VenueId::Binance]).expect("a fixture row serializes");
    assert_eq!(
        binance["origins"][BINANCE_SPOT]["http"],
        serde_json::json!("https://api.binance.com"),
        "the stored request address is the one the venue publishes"
    );
    assert_eq!(
        binance["origins"][BINANCE_SPOT]["websocket"],
        serde_json::json!("wss://stream.binance.com:9443"),
        "and so is the other role's"
    );
    assert_eq!(
        serde_json::to_value(&venues[&VenueId::Okx]).expect("a fixture row serializes")["region"],
        serde_json::json!("global"),
        "the stored region is a region this build speaks"
    );
    assert_eq!(
        serde_json::to_value(&venues[&VenueId::Hyperliquid]).expect("a fixture row serializes")["http"],
        serde_json::json!("https://api.hyperliquid.xyz/"),
        "and so is the one address HyperCore publishes"
    );

    // The built-in table is what a venue publishes, and a fixture installation is separate:
    // building one leaves `default_venues` alone, and the venues the caller did not name keep
    // the addresses they publish.
    assert_eq!(
        default_venues()[&VenueId::Binance]
            .origins(BINANCE_SPOT)
            .map(|origins| origins.http.to_string()),
        published("https://api.binance.com")
    );
    assert_eq!(
        default_venues()[&VenueId::Binance]
            .origins(BINANCE_USD_M_PERP)
            .map(|origins| origins.websocket.to_string()),
        published("wss://fstream.binance.com")
    );
    assert_eq!(
        default_venues()[&VenueId::Hyperliquid]
            .origins(HYPERLIQUID_CORE_PERP)
            .map(|origins| origins.http.to_string()),
        published("https://api.hyperliquid.xyz/")
    );
    assert_eq!(
        default_venues()[&VenueId::Okx]
            .origins(OKX_SPOT)
            .expect("the venue publishes the scope its requests are addressed by")
            .http
            .as_str(),
        "https://openapi.okx.com/",
        "and the region speaks to the international API again"
    );
}

/// One venue's fixtures are that venue's: a snapshot that names only Binance leaves OKX and
/// HyperCore where their venues publish them.
///
/// This is the property that makes two tests in one binary independent — each builds its own
/// snapshot, and the venues it did not point stay reachable at their real addresses.
#[test]
fn a_fixture_points_only_the_venues_it_names() {
    let fixture = everywhere();
    let venues = fixture::venues([(VenueId::Binance, fixture.clone())]).into_venues();

    assert_eq!(
        venues[&VenueId::Binance]
            .origins(BINANCE_SPOT)
            .map(|origins| origins.http),
        Some(fixture.http.clone()),
        "the venue the caller named is reached at the caller's socket"
    );
    assert_eq!(
        venues[&VenueId::Okx]
            .origins(OKX_SPOT)
            .map(|origins| origins.http.to_string()),
        published("https://openapi.okx.com"),
        "a venue the caller did not name is reached where it publishes its address"
    );
    assert_eq!(
        venues[&VenueId::Hyperliquid]
            .origins(HYPERLIQUID_CORE_PERP)
            .map(|origins| origins.http.to_string()),
        published("https://api.hyperliquid.xyz/"),
        "and so is the other one"
    );

    // A snapshot is a value, so two of them coexist: the second does not move the first.
    let other = Url::parse("http://127.0.0.1:9090").expect("the other fixture address is a URL");
    let moved = fixture::venues([(VenueId::Binance, FixtureAddress::at(&other))]).into_venues();

    assert_eq!(
        moved[&VenueId::Binance]
            .origins(BINANCE_USD_M_PERP)
            .map(|origins| origins.http.as_str().to_owned()),
        Some("http://127.0.0.1:9090/".to_owned()),
        "the second snapshot dials its own sockets"
    );
    assert_eq!(
        venues[&VenueId::Binance]
            .origins(BINANCE_USD_M_PERP)
            .map(|origins| origins.http),
        Some(fixture.http),
        "and the first one still dials the sockets it was built with"
    );
}

/// The stream role moves with the request role when one address serves both, and apart when
/// the caller names them apart.
#[test]
fn both_roles_of_a_fixture_move_together() {
    let single = Url::parse("http://127.0.0.1:8080").expect("the fixture address is a URL");
    let paired = fixture::venues([(VenueId::Binance, FixtureAddress::at(&single))]).into_venues();
    let origins = paired[&VenueId::Binance]
        .origins(BINANCE_SPOT)
        .expect("the venue publishes the scope its requests are addressed by");

    assert_eq!(
        (origins.http.as_str(), origins.websocket.as_str()),
        ("http://127.0.0.1:8080/", "http://127.0.0.1:8080/"),
        "one socket declared for both roles answers both"
    );

    let split = fixture::venues([(VenueId::Binance, everywhere())]).into_venues();
    let origins = split[&VenueId::Binance]
        .origins(BINANCE_SPOT)
        .expect("the venue publishes the scope its requests are addressed by");
    assert_eq!(
        (origins.http.as_str(), origins.websocket.as_str()),
        ("http://127.0.0.1:8080/", "ws://127.0.0.1:8081/"),
        "and a pair is dialled role by role"
    );
}

/// Returns one address as the URL a request would dial it at.
fn published(address: &str) -> Option<String> {
    Some(
        Url::parse(address)
            .expect("a published address is a URL")
            .to_string(),
    )
}

//! Fixture settings for a test that drives a venue at sockets it owns.
//!
//! Every address a real installation may store is one the venue publishes, and the origin
//! tables enforce that by being closed sets of choices. A test is the one caller that has to
//! reach a *fixture* instead: the venue it drives answers on a loopback socket, and no
//! published address names one.
//!
//! A fixture replaces where a published address *resolves*, never what it is called. The choice
//! a settings row holds keeps the spelling the venue publishes — so a file written from a
//! fixture is a file this build decodes, and a test can still assert what that file says — and
//! only the address a request dials moves. What carries the resolution is the settings
//! themselves, not state a request path looks up while it dials: two tests in one binary each
//! hold their own configuration, and a request dials whatever its own configuration says.
//!
//! The seam sits behind the crate's `test-support` feature, which no crate in this workspace
//! enables but from a `[dev-dependencies]` edge: a plain `cargo build` or `cargo build
//! --release` of the application cannot reach it. It is an ordinary cargo feature rather than
//! something the compiler enforces, so `--all-features` would compile it — which is why the
//! feature is documented as opt-in for tests rather than as impossible to enable.

use std::collections::BTreeMap;

use market_participants_model::identifier::VenueId;
use url::Url;

use crate::market_participants::settings::{FixturePair, VenueSettings, default_venues};

/// The sockets one venue's fixture answers on, by role.
///
/// The two roles are apart because a fixture serves one protocol per socket: a test that drives
/// a stream serves it on a socket of its own, and the pair a venue resolves has to name both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureAddress {
    /// The socket the venue's requests are sent to.
    pub http: Url,
    /// The socket the venue's streams are opened on.
    pub websocket: Url,
}

impl FixtureAddress {
    /// Declares one address serving both roles.
    ///
    /// A fixture socket that answers whatever the test asks it — a raw HTTP server a test drives
    /// a stream over, most of all — is reached on the same address for requests and for streams.
    pub fn at(address: &Url) -> Self {
        Self {
            http: address.clone(),
            websocket: address.clone(),
        }
    }

    /// Returns the sockets a venue pointed here is dialed at.
    pub fn bindings(&self) -> FixturePair {
        FixturePair {
            http: self.http.clone(),
            websocket: self.websocket.clone(),
        }
    }
}

/// Returns the configuration of every venue this build speaks, with each venue the caller names
/// reached where the caller's own sockets listen.
///
/// The venues the caller does not name keep the addresses their venue publishes. Every venue
/// this build speaks is configured either way, so the answer is a complete settings snapshot: it
/// is what a test root is built from.
#[must_use = "the configuration carries the fixture addresses, so a root built from it dials them"]
pub fn venues(pointed: impl IntoIterator<Item = (VenueId, FixtureAddress)>) -> FixtureInstallation {
    let mut venues = default_venues();
    for (venue, address) in pointed {
        let Some(settings) = venues.get_mut(&venue) else {
            continue;
        };
        settings.repoint(&address);
    }
    FixtureInstallation { venues }
}

/// One fixture installation: the settings that were pointed at fixture sockets.
///
/// A caller keeps this beside the root it built, because the configuration is what carries the
/// fixture addresses — a root built from it dials them, and one built from anything else dials
/// the addresses its venues publish.
#[derive(Clone, Debug)]
pub struct FixtureInstallation {
    venues: BTreeMap<VenueId, VenueSettings>,
}

impl FixtureInstallation {
    /// Returns the configuration this installation put in force.
    pub fn venues(&self) -> &BTreeMap<VenueId, VenueSettings> {
        &self.venues
    }

    /// Takes the configuration this installation put in force.
    pub fn into_venues(self) -> BTreeMap<VenueId, VenueSettings> {
        self.venues
    }

    /// Moves the venues named to other fixture addresses, keeping this installation.
    ///
    /// A test that reconfigures a venue mid-test states the addresses it moves to here: a venue
    /// this call does not name keeps the address it has, and the venues it never named keep the
    /// addresses their venue publishes.
    pub fn repoint(&mut self, pointed: impl IntoIterator<Item = (VenueId, FixtureAddress)>) {
        for (venue, address) in pointed {
            let Some(settings) = self.venues.get_mut(&venue) else {
                continue;
            };
            settings.repoint(&address);
        }
    }
}

//! One page per venue this build serves.
//!
//! A venue's page is native UI for the settings that venue's API has. Which module draws it is a
//! fact of the venue rather than of a declaration, so adding a venue adds a page beside the venues
//! already served, and the compiler is what says one is missing.

use i18n::translate::Translate;
use market_participants_model::identifier::VenueId;
use widget::venue::VenueDisplay;

use crate::{
    host::SettingsHost,
    pages::{
        page::PageView,
        venues::{binance, hyperliquid, longbridge, okx, polymarket},
    },
    search::{PageSearch, words},
};

/// Opens the page of one venue.
pub(crate) fn page_view(venue: VenueId, host: &SettingsHost, key: &str) -> Box<dyn PageView> {
    match venue {
        VenueId::Binance => Box::new(binance::VenuePage::new(host.clone(), key)),
        VenueId::Okx => Box::new(okx::VenuePage::new(host.clone(), key)),
        VenueId::Hyperliquid => Box::new(hyperliquid::VenuePage::new(host.clone(), key)),
        VenueId::Longbridge => Box::new(longbridge::VenuePage::new(host.clone(), key)),
        VenueId::Polymarket => Box::new(polymarket::VenuePage::new(host.clone(), key)),
    }
}

/// The text one venue's page and its settings are searched by.
pub(crate) fn search(venue: VenueId, translate: &Translate) -> PageSearch {
    let title = i18n::t_smol!(translate, venue.name_key());
    let fields = match venue {
        VenueId::Binance => binance::search_fields(translate),
        VenueId::Okx => okx::search_fields(translate),
        VenueId::Hyperliquid => hyperliquid::search_fields(translate),
        VenueId::Longbridge => longbridge::search_fields(translate),
        VenueId::Polymarket => polymarket::search_fields(translate),
    };

    PageSearch {
        label: words(&[
            title.as_ref(),
            venue.as_str(),
            "market_participants",
            "venue",
            "credentials",
            "origin",
            "市场参与者",
            "场所",
            "凭据",
            "地址",
        ]),
        fields,
    }
}

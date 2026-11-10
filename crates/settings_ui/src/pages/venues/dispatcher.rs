//! One page per venue this build serves.
//!
//! A venue's page is native UI for the settings that venue's API has. Which module draws it is a
//! fact of the venue rather than of a declaration, so adding a venue adds a page beside the venues
//! already served, and the compiler is what says one is missing.

use gpui_kit::{AnyElement, App};
use i18n::t;
use market_participants_model::identifier::VenueId;
use widget::venue::VenueDisplay;

use crate::{
    layout::PageArgs,
    pages::venues::{binance, hyperliquid, longbridge, okx, polymarket},
    search::{PageSearch, words},
};

pub(crate) fn render(venue: VenueId, args: &mut PageArgs<'_, '_>) -> AnyElement {
    match venue {
        VenueId::Binance => binance::render(args),
        VenueId::Okx => okx::render(args),
        VenueId::Hyperliquid => hyperliquid::render(args),
        VenueId::Longbridge => longbridge::render(args),
        VenueId::Polymarket => polymarket::render(args),
    }
}

pub(crate) fn search(venue: VenueId, cx: &App) -> PageSearch {
    let title = t!(cx, venue.name_key());
    let fields = match venue {
        VenueId::Binance => binance::search_fields(cx),
        VenueId::Okx => okx::search_fields(cx),
        VenueId::Hyperliquid => hyperliquid::search_fields(cx),
        VenueId::Longbridge => longbridge::search_fields(cx),
        VenueId::Polymarket => polymarket::search_fields(cx),
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

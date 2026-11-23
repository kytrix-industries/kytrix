//! The icon one widget shows in the gallery.
//!
//! A widget declares its picture as an application asset path (`widgets/candlestick.png`).
//! The kit does not draw application assets in the catalogue: it has one icon set
//! (`base_ui::kit::icon`, 55 variants) that every part of the interface uses, so the page
//! maps the declared path onto the nearest icon of that set. The mapping is a table
//! rather than a rule because "nearest icon" is a design decision per widget, and a table
//! is what makes a missing entry visible: a path the table does not name falls back to the
//! gallery's own puzzle piece, and [`MISSING_ICONS`] is the register of concepts the icon
//! set does not carry yet (the concept a widget really wants, and the icon it is drawn
//! with until the asset batch adds one).
//!
//! The mapping is keyed by the declared asset path, not by the widget identity: the path
//! is what [`WidgetDescriptor::icon`] gives the page, and a widget that changes its
//! picture changes its icon here by editing one row.

use base_ui::kit::icon::IconKind;
use widget::descriptor::WidgetDescriptor;

/// The icon each declared widget picture maps to.
///
/// Every entry is a path a registered widget's [`WidgetView::ICON`] names; the tests pin
/// that the table covers the catalogue that ships and that every value is a kit icon.
///
/// [`WidgetView::ICON`]: widget::view::WidgetView::ICON
const WIDGET_ICONS: &[(&str, IconKind)] = &[
    ("widgets/candlestick.png", IconKind::Graph),
    (
        "widgets/coin_market_cap_fear_and_greed_index.png",
        IconKind::Faders,
    ),
    ("widgets/cryptocurrency_etf.png", IconKind::Graph),
    ("widgets/ethereum_wallet.png", IconKind::Bank),
    ("widgets/financial_calendar.png", IconKind::Table),
    ("widgets/indices.png", IconKind::Graph),
    ("widgets/force_order.png", IconKind::Table),
    ("widgets/global_shipping_chokepoint.png", IconKind::Plugs),
    ("widgets/hyperliquid_wallet.png", IconKind::Bank),
    ("widgets/market_trade.png", IconKind::Graph),
    ("widgets/news.png", IconKind::NotePencil),
    ("widgets/order_book.png", IconKind::Table),
    ("widgets/pentagon_pizza_index.png", IconKind::Graph),
    ("widgets/prediction_market.png", IconKind::Graph),
    ("widgets/open_interest.png", IconKind::Graph),
    ("widgets/top_trader_account_ratio.png", IconKind::Graph),
    ("widgets/top_trader_position_ratio.png", IconKind::Graph),
    ("widgets/long_short_account_ratio.png", IconKind::Graph),
    ("widgets/taker_volume.png", IconKind::Graph),
    ("widgets/solana_wallet.png", IconKind::Bank),
    ("widgets/volume_heatmap.png", IconKind::SquaresFour),
    ("widgets/watch_list.png", IconKind::Eye),
    ("widgets/x.png", IconKind::At),
];

/// The picture a widget that declares none is drawn with: the gallery's own mark.
const FALLBACK: IconKind = IconKind::PuzzlePiece;

/// Concepts the kit's 55-icon set does not carry, and what the page draws instead.
///
/// One row per concept (not per widget): the batch that adds an icon to the set replaces
/// the rows that name it and the mapping above starts using the new variant. The register
/// is what "缺图标登记" means concretely — an icon that is missing is recorded here, not
/// silently recoloured into a lookalike.
pub const MISSING_ICONS: &[(MissingIcon, IconKind)] = &[
    (MissingIcon::CandlestickChart, IconKind::Graph),
    (MissingIcon::Gauge, IconKind::Faders),
    (MissingIcon::Calendar, IconKind::Table),
    (MissingIcon::Globe, IconKind::Plugs),
    (MissingIcon::Wallet, IconKind::Bank),
    (MissingIcon::Heatmap, IconKind::SquaresFour),
    (MissingIcon::OrderBookLadder, IconKind::Table),
    (MissingIcon::ChipsOrCandles, IconKind::Graph),
];

/// One icon the kit does not carry yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingIcon {
    /// A candlestick chart (price series with wicks).
    CandlestickChart,
    /// A gauge / fear-and-greed dial.
    Gauge,
    /// A calendar page.
    Calendar,
    /// A globe or map (shipping choke points).
    Globe,
    /// A wallet.
    Wallet,
    /// A heat map (a grid of intensities).
    Heatmap,
    /// A depth ladder / order book.
    OrderBookLadder,
    /// A chip stack or candles for the index widgets.
    ChipsOrCandles,
}

impl MissingIcon {
    /// The concept this row registers, in words.
    pub const fn concept(self) -> &'static str {
        match self {
            Self::CandlestickChart => "candlestick chart",
            Self::Gauge => "gauge",
            Self::Calendar => "calendar",
            Self::Globe => "globe",
            Self::Wallet => "wallet",
            Self::Heatmap => "heatmap",
            Self::OrderBookLadder => "order book ladder",
            Self::ChipsOrCandles => "chips or candles",
        }
    }
}

/// The icon the page draws for one catalogue entry.
///
/// A widget with no declared picture, or with a picture this table does not name, gets
/// the gallery's puzzle piece: the page always draws a picture beside a name.
pub fn icon_for(descriptor: &WidgetDescriptor) -> IconKind {
    descriptor
        .icon()
        .and_then(icon_for_path)
        .unwrap_or(FALLBACK)
}

/// The icon one declared asset path maps to, if the table names it.
pub fn icon_for_path(path: &str) -> Option<IconKind> {
    WIDGET_ICONS
        .iter()
        .find(|(declared, _)| *declared == path)
        .map(|(_, icon)| *icon)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use base_ui::kit::icon::IconKind;

    use super::{FALLBACK, MISSING_ICONS, WIDGET_ICONS, icon_for, icon_for_path};

    /// The pictures the catalogue that ships declares. Kept as data rather than imported
    /// from the widget crates (this crate does not depend on them): the list is the page's
    /// contract with the catalogue, and the test below is what turns a drift into a
    /// failure instead of a puzzle piece on screen.
    const CATALOGUE_ICONS: [&str; 23] = [
        "widgets/candlestick.png",
        "widgets/coin_market_cap_fear_and_greed_index.png",
        "widgets/cryptocurrency_etf.png",
        "widgets/ethereum_wallet.png",
        "widgets/financial_calendar.png",
        "widgets/indices.png",
        "widgets/force_order.png",
        "widgets/global_shipping_chokepoint.png",
        "widgets/hyperliquid_wallet.png",
        "widgets/market_trade.png",
        "widgets/news.png",
        "widgets/order_book.png",
        "widgets/pentagon_pizza_index.png",
        "widgets/prediction_market.png",
        "widgets/open_interest.png",
        "widgets/top_trader_account_ratio.png",
        "widgets/top_trader_position_ratio.png",
        "widgets/long_short_account_ratio.png",
        "widgets/taker_volume.png",
        "widgets/solana_wallet.png",
        "widgets/volume_heatmap.png",
        "widgets/watch_list.png",
        "widgets/x.png",
    ];

    /// Every picture the catalogue declares maps to an icon of the kit's set, so a widget
    /// never appears without a picture because the table forgot it.
    #[test]
    fn every_listed_widget_picture_should_map_to_a_kit_icon() {
        for path in CATALOGUE_ICONS {
            let icon = icon_for_path(path).unwrap_or_else(|| {
                panic!("{path} is declared by a widget but the gallery has no icon for it")
            });
            assert!(
                IconKind::ALL.contains(&icon),
                "{path} maps to {} which is not one of the kit's 55 icons",
                icon.name()
            );
        }
    }

    /// The table names each picture once: two rows for one path would make which icon a
    /// widget gets depend on row order.
    #[test]
    fn no_picture_should_be_mapped_twice() {
        let paths = WIDGET_ICONS.iter().map(|(path, _)| *path);
        let unique: HashSet<&str> = paths.clone().collect();

        assert_eq!(paths.count(), unique.len(), "a path is mapped twice");
    }

    /// The table maps nothing the catalogue does not declare: an entry for a picture no
    /// widget has is a row that will never be read, and it hides a typo in a real one.
    #[test]
    fn the_table_should_map_exactly_the_catalogue_pictures() {
        let declared: HashSet<&str> = CATALOGUE_ICONS.into_iter().collect();
        let mapped: HashSet<&str> = WIDGET_ICONS.iter().map(|(path, _)| *path).collect();

        assert_eq!(
            mapped, declared,
            "the icon table and the catalogue's pictures disagree"
        );
    }

    /// A widget that declares no picture is drawn with the gallery's own, so a widget that
    /// has not been given one yet is still a picture beside its name.
    #[test]
    fn a_widget_without_a_picture_should_get_the_gallery_own_icon() {
        use app_config::widget::WidgetConfigSchemaVersion;
        use widget::{category::WidgetCategory, descriptor::WidgetDescriptor, identity::WidgetId};

        let descriptor = WidgetDescriptor::new(
            WidgetId::new_static("test.gallery.no_icon"),
            "widget.names.empty",
            WidgetCategory::Market,
            WidgetConfigSchemaVersion::V1,
        );

        assert_eq!(icon_for(&descriptor), FALLBACK);
    }

    /// An unknown picture is the gallery's own icon rather than nothing: a picture the
    /// page does not know is not a reason for the row to lose its picture.
    #[test]
    fn an_unknown_picture_should_get_the_gallery_own_icon() {
        assert_eq!(icon_for_path("widgets/does-not-exist.png"), None);
    }

    /// Every registered missing concept points at a real icon of the set, so the register
    /// is a record of the workaround that is actually drawn instead of a wish list.
    #[test]
    fn every_missing_icon_should_name_the_icon_drawn_instead() {
        let mut concepts = HashSet::new();
        for (missing, drawn) in MISSING_ICONS {
            assert!(
                concepts.insert(missing.concept()),
                "{} is registered twice",
                missing.concept()
            );
            assert!(
                IconKind::ALL.contains(drawn),
                "{} is drawn with {} which is not a kit icon",
                missing.concept(),
                drawn.name()
            );
            assert!(
                WIDGET_ICONS.iter().any(|(_, icon)| icon == drawn),
                "{} names an icon no widget actually uses; the register must describe \
                 the fallback the page draws, not an unused suggestion",
                missing.concept()
            );
        }
    }
}

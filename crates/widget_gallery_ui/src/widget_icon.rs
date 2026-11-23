use base_ui::kit::icon::IconKind;
use widget::descriptor::WidgetDescriptor;

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

const FALLBACK: IconKind = IconKind::PuzzlePiece;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingIcon {
    CandlestickChart,
    Gauge,
    Calendar,
    Globe,
    Wallet,
    Heatmap,
    OrderBookLadder,
    ChipsOrCandles,
}

impl MissingIcon {
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

pub fn icon_for(descriptor: &WidgetDescriptor) -> IconKind {
    descriptor
        .icon()
        .and_then(icon_for_path)
        .unwrap_or(FALLBACK)
}

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

    #[test]
    fn no_picture_should_be_mapped_twice() {
        let paths = WIDGET_ICONS.iter().map(|(path, _)| *path);
        let unique: HashSet<&str> = paths.clone().collect();

        assert_eq!(paths.count(), unique.len(), "a path is mapped twice");
    }

    #[test]
    fn the_table_should_map_exactly_the_catalogue_pictures() {
        let declared: HashSet<&str> = CATALOGUE_ICONS.into_iter().collect();
        let mapped: HashSet<&str> = WIDGET_ICONS.iter().map(|(path, _)| *path).collect();

        assert_eq!(
            mapped, declared,
            "the icon table and the catalogue's pictures disagree"
        );
    }

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

    #[test]
    fn an_unknown_picture_should_get_the_gallery_own_icon() {
        assert_eq!(icon_for_path("widgets/does-not-exist.png"), None);
    }

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

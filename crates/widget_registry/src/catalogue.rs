//! The static widget catalogue and its GPUI global installation.

use gpui_kit::App;

use super::registry::{WidgetRegistration, WidgetRegistry, WidgetRegistryError};

/// Installs the catalogue of every widget compiled into the application.
///
/// The registry is installed as a global rather than assembled by the
/// composition root so that adding a widget cannot be forgotten at startup.
///
/// # Errors
///
/// Fails when two registrations claim the same widget ID, which means two
/// catalogue entries disagree about a persisted identity.
pub fn install(cx: &mut App) -> Result<(), WidgetRegistryError> {
    let mut registry = WidgetRegistry::default();
    for registration in catalogue() {
        registry.register(registration)?;
    }

    cx.set_global(registry);
    Ok(())
}

/// Every statically linked widget, in catalogue order.
fn catalogue() -> impl ExactSizeIterator<Item = WidgetRegistration> {
    [
        WidgetRegistration::for_view::<candlestick::view::CandlestickWidget>(),
        WidgetRegistration::for_view::<coin_market_cap_fear_and_greed_index::view::CoinMarketCapFearAndGreedIndexWidget>(),
        WidgetRegistration::for_view::<cryptocurrency_etf::view::CryptocurrencyEtfWidget>(),
        WidgetRegistration::for_view::<empty::view::EmptyWidget>(),
        WidgetRegistration::for_view::<financial_calendar::view::FinancialCalendarWidget>(),
        WidgetRegistration::for_view::<force_order::view::ForceOrderWidget>(),
        WidgetRegistration::for_view::<
            global_shipping_chokepoint::view::GlobalShippingChokepointWidget,
        >(),
        WidgetRegistration::for_view::<
            hyperliquid_wallet::view::HyperliquidWalletWidget,
        >(),
        WidgetRegistration::for_view::<financial_index::view::FinancialIndexWidget>(),
        WidgetRegistration::for_view::<market_trade::view::MarketTradeWidget>(),
        WidgetRegistration::for_view::<news::view::NewsWidget>(),
        WidgetRegistration::for_view::<order_book::view::OrderBookWidget>(),
        WidgetRegistration::for_view::<pentagon_pizza_index::view::PentagonPizzaIndexWidget>(),
        WidgetRegistration::for_view::<
            perpetual_statistics::widget::open_interest::OpenInterestWidget,
        >(),
        WidgetRegistration::for_view::<
            perpetual_statistics::widget::top_trader_account_ratio::TopTraderAccountRatioWidget,
        >(),
        WidgetRegistration::for_view::<
            perpetual_statistics::widget::top_trader_position_ratio::TopTraderPositionRatioWidget,
        >(),
        WidgetRegistration::for_view::<
            perpetual_statistics::widget::long_short_account_ratio::LongShortAccountRatioWidget,
        >(),
        WidgetRegistration::for_view::<
            perpetual_statistics::widget::taker_volume::TakerVolumeWidget,
        >(),
        WidgetRegistration::for_view::<x::view::XWidget>(),
        WidgetRegistration::for_view::<volume_heatmap::view::VolumeHeatMapWidget>(),
        WidgetRegistration::for_view::<ethereum_wallet::view::EthereumWalletWidget>(),
        WidgetRegistration::for_view::<solana_wallet::view::SolanaWalletWidget>(),
        WidgetRegistration::for_view::<watch_list::view::WatchListWidget>(),
    ]
    .into_iter()
}

#[cfg(test)]
mod tests {
    use app_config::widget::WidgetConfigSchemaVersion;
    use gpui_kit::TestAppContext;
    use widget::{category::WidgetCategory, view::WidgetView};

    use super::install;
    use crate::registry::WidgetRegistry;

    const CANDLESTICK_ID: &str = "kytrix.candlestick";
    const COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ID: &str =
        "kytrix.coin_market_cap_fear_and_greed_index";
    const CRYPTOCURRENCY_ETF_ID: &str = "kytrix.cryptocurrency_etf";
    const EMPTY_ID: &str = "kytrix.empty";
    const FINANCIAL_CALENDAR_ID: &str = "kytrix.financial_calendar";
    const FORCE_ORDER_ID: &str = "kytrix.force_order";
    const GLOBAL_SHIPPING_CHOKEPOINT_ID: &str = "kytrix.global_shipping_chokepoint";
    const HYPERLIQUID_WALLET_ID: &str = "kytrix.hyperliquid_wallet";
    const FINANCIAL_INDEX_ID: &str = "kytrix.financial_index";
    const MARKET_TRADE_ID: &str = "kytrix.market_trade";
    const NEWS_ID: &str = "kytrix.news";
    const ORDER_BOOK_ID: &str = "kytrix.order_book";
    const PENTAGON_PIZZA_INDEX_ID: &str = "kytrix.pentagon_pizza_index";
    const X_ID: &str = "kytrix.x";
    const VOLUME_HEAT_MAP_ID: &str = "kytrix.volume_heatmap";
    const ETHEREUM_WALLET_ID: &str = "kytrix.ethereum_wallet";
    const SOLANA_WALLET_ID: &str = "kytrix.solana_wallet";
    const WATCH_LIST_ID: &str = "kytrix.watch_list";
    /// The five perpetual-statistics panes, which are one widget per series.
    const STATISTICS_IDS: [&str; 5] = [
        "kytrix.open_interest",
        "kytrix.top_trader_account_ratio",
        "kytrix.top_trader_position_ratio",
        "kytrix.long_short_account_ratio",
        "kytrix.taker_volume",
    ];

    /// The category every widget a selection surface lists declares, which is the
    /// group it is listed under. A widget the framework builds for itself is listed
    /// nowhere, so it is not in this table — the gallery's own tests cover the
    /// widget that is.
    const CATEGORIES: [(&str, WidgetCategory); 22] = [
        (CANDLESTICK_ID, WidgetCategory::Market),
        (
            COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ID,
            WidgetCategory::Market,
        ),
        (CRYPTOCURRENCY_ETF_ID, WidgetCategory::Market),
        (FINANCIAL_CALENDAR_ID, WidgetCategory::Monitoring),
        (FORCE_ORDER_ID, WidgetCategory::Monitoring),
        (GLOBAL_SHIPPING_CHOKEPOINT_ID, WidgetCategory::Monitoring),
        (HYPERLIQUID_WALLET_ID, WidgetCategory::Monitoring),
        (FINANCIAL_INDEX_ID, WidgetCategory::Market),
        (MARKET_TRADE_ID, WidgetCategory::Market),
        (NEWS_ID, WidgetCategory::Monitoring),
        (ORDER_BOOK_ID, WidgetCategory::Market),
        (PENTAGON_PIZZA_INDEX_ID, WidgetCategory::Monitoring),
        (STATISTICS_IDS[0], WidgetCategory::Statistics),
        (STATISTICS_IDS[1], WidgetCategory::Statistics),
        (STATISTICS_IDS[2], WidgetCategory::Statistics),
        (STATISTICS_IDS[3], WidgetCategory::Statistics),
        (STATISTICS_IDS[4], WidgetCategory::Statistics),
        (X_ID, WidgetCategory::Monitoring),
        (VOLUME_HEAT_MAP_ID, WidgetCategory::Statistics),
        (ETHEREUM_WALLET_ID, WidgetCategory::Monitoring),
        (SOLANA_WALLET_ID, WidgetCategory::Monitoring),
        (WATCH_LIST_ID, WidgetCategory::Market),
    ];

    #[gpui_kit::test]
    fn install_should_publish_the_whole_catalogue_as_a_global(cx: &mut TestAppContext) {
        cx.update(|cx| install(cx).expect("the catalogue should install"));

        cx.update(|cx| {
            let registry = cx.global::<WidgetRegistry>();

            assert!(registry.get(CANDLESTICK_ID).is_some());
            assert!(
                registry
                    .get(COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ID)
                    .is_some()
            );
            assert!(registry.get(EMPTY_ID).is_some());
            assert!(registry.get(CRYPTOCURRENCY_ETF_ID).is_some());
            assert!(registry.get(FINANCIAL_CALENDAR_ID).is_some());
            assert!(registry.get(FORCE_ORDER_ID).is_some());
            assert!(registry.get(GLOBAL_SHIPPING_CHOKEPOINT_ID).is_some());
            assert!(registry.get(HYPERLIQUID_WALLET_ID).is_some());
            assert!(registry.get(FINANCIAL_INDEX_ID).is_some());
            assert!(registry.get(MARKET_TRADE_ID).is_some());
            assert!(registry.get(NEWS_ID).is_some());
            assert!(registry.get(ORDER_BOOK_ID).is_some());
            assert!(registry.get(PENTAGON_PIZZA_INDEX_ID).is_some());
            assert!(registry.get(X_ID).is_some());
            assert!(registry.get(VOLUME_HEAT_MAP_ID).is_some());
            assert!(registry.get(ETHEREUM_WALLET_ID).is_some());
            assert!(registry.get(SOLANA_WALLET_ID).is_some());
            assert!(registry.get(WATCH_LIST_ID).is_some());
            for id in STATISTICS_IDS {
                assert!(registry.get(id).is_some(), "{id} should be registered");
            }
            assert_eq!(registry.iter().count(), 23);
        });
    }

    #[gpui_kit::test]
    fn every_widget_should_declare_the_category_it_is_listed_under(cx: &mut TestAppContext) {
        cx.update(|cx| install(cx).expect("the catalogue should install"));

        cx.update(|cx| {
            let registry = cx.global::<WidgetRegistry>();
            let listed = registry
                .iter()
                .filter(|registration| registration.descriptor().selectable())
                .count();

            assert_eq!(
                CATEGORIES.len(),
                listed,
                "a widget a selection surface can offer should be given a category here"
            );
            for (id, category) in CATEGORIES {
                let registration = registry
                    .get(id)
                    .unwrap_or_else(|| panic!("{id} should be registered"));

                assert_eq!(registration.descriptor().category(), category, "{id}");
            }
        });
    }

    /// The widget a pane nobody has chosen for yet shows is named by two crates:
    /// `app_config` writes the ID into the layout of such a pane, and the widget
    /// crate owns it. This is the test that keeps the two names the same widget.
    #[gpui_kit::test]
    fn the_widget_a_new_pane_falls_back_to_should_be_the_layout_default(cx: &mut TestAppContext) {
        cx.update(|cx| install(cx).expect("the catalogue should install"));

        cx.update(|cx| {
            let registration = cx
                .global::<WidgetRegistry>()
                .get(app_config::workspace::DEFAULT_WIDGET_ID)
                .expect("the catalogue should register the widget a new pane shows");
            let descriptor = registration.descriptor();

            assert_eq!(
                descriptor.id().as_str(),
                empty::view::EmptyWidget::ID.as_str(),
                "app_config and the widget crate should name the same widget"
            );
            assert!(
                !descriptor.selectable(),
                "the pane's fallback is built by the framework, never chosen by the user"
            );
        });
    }

    /// Every persisted config is schema V1.
    ///
    /// A widget that started at V2 has been reset rather than migrated: the version this
    /// build writes is the first one, and a stored envelope from any other version is
    /// reset on the way in.
    #[gpui_kit::test]
    fn every_widget_should_persist_its_config_at_schema_v1(cx: &mut TestAppContext) {
        cx.update(|cx| install(cx).expect("the catalogue should install"));

        cx.update(|cx| {
            for registration in cx.global::<WidgetRegistry>().iter() {
                let descriptor = registration.descriptor();

                assert_eq!(
                    descriptor.config_schema_version(),
                    WidgetConfigSchemaVersion::V1,
                    "{} should persist its config at schema V1",
                    descriptor.id()
                );
            }
        });
    }
}

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
        WidgetRegistration::for_view::<crypto_fgi::view::CryptoFgiWidget>(),
        WidgetRegistration::for_view::<empty::view::EmptyWidget>(),
        WidgetRegistration::for_view::<etf::view::EtfWidget>(),
        WidgetRegistration::for_view::<financial_calendar::view::FinancialCalendarWidget>(),
        WidgetRegistration::for_view::<force_orders::view::ForceOrdersWidget>(),
        WidgetRegistration::for_view::<hormuz_vessel_map::view::HormuzVesselMapWidget>(),
        WidgetRegistration::for_view::<
            hyperliquid_wallet_monitor::view::HyperliquidWalletMonitorWidget,
        >(),
        WidgetRegistration::for_view::<financial_indices::view::FinancialIndicesWidget>(),
        WidgetRegistration::for_view::<market_trades::view::MarketTradesWidget>(),
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
        WidgetRegistration::for_view::<x_account_monitoring::view::XAccountMonitoringWidget>(),
        WidgetRegistration::for_view::<volume_heatmap::view::VolumeHeatMapWidget>(),
        WidgetRegistration::for_view::<cryptocurrency_wallet_monitoring::view::CryptocurrencyWalletMonitoringWidget>(),
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

    const CANDLESTICK_ID: &str = "kytrix.market.candlestick";
    const CRYPTO_FGI_ID: &str = "kytrix.market.crypto_fgi";
    const EMPTY_ID: &str = "kytrix.empty";
    const ETF_ID: &str = "kytrix.market.etf_flow";
    const FINANCIAL_CALENDAR_ID: &str = "kytrix.monitoring.financial_calendar";
    const FORCE_ORDERS_ID: &str = "kytrix.market.force_orders";
    const HYPERLIQUID_WALLET_MONITOR_ID: &str = "kytrix.market.hyperliquid_wallet_monitor";
    const HORMUZ_VESSEL_MAP_ID: &str = "kytrix.maritime.hormuz_vessel_map";
    const FINANCIAL_INDICES_ID: &str = "kytrix.market.financial_indices";
    const MARKET_TRADES_ID: &str = "kytrix.market.market_trades";
    const NEWS_ID: &str = "kytrix.market.news";
    const ORDER_BOOK_ID: &str = "kytrix.market.order_book";
    const PENTAGON_PIZZA_INDEX_ID: &str = "kytrix.monitoring.pentagon_pizza_index";
    const X_ACCOUNT_MONITORING_ID: &str = "kytrix.market.x_account_monitoring";
    const VOLUME_HEAT_MAP_ID: &str = "kytrix.market.volume_heatmap";
    const CRYPTOCURRENCY_WALLET_MONITORING_ID: &str =
        "kytrix.market.cryptocurrency_wallet_monitoring";
    const WATCH_LIST_ID: &str = "kytrix.market.watch_list";
    /// The five perpetual-statistics panes, which are one widget per series.
    const STATISTICS_IDS: [&str; 5] = [
        "kytrix.market.open_interest",
        "kytrix.market.top_trader_account_ratio",
        "kytrix.market.top_trader_position_ratio",
        "kytrix.market.long_short_account_ratio",
        "kytrix.market.taker_volume",
    ];

    /// The category every widget a selection surface lists declares, which is the
    /// group it is listed under. A widget the framework builds for itself is listed
    /// nowhere, so it is not in this table — the gallery's own tests cover the
    /// widget that is.
    const CATEGORIES: [(&str, WidgetCategory); 21] = [
        (CANDLESTICK_ID, WidgetCategory::Market),
        (CRYPTO_FGI_ID, WidgetCategory::Market),
        (ETF_ID, WidgetCategory::Market),
        (FINANCIAL_CALENDAR_ID, WidgetCategory::Monitoring),
        (FORCE_ORDERS_ID, WidgetCategory::Monitoring),
        (HORMUZ_VESSEL_MAP_ID, WidgetCategory::Monitoring),
        (HYPERLIQUID_WALLET_MONITOR_ID, WidgetCategory::Monitoring),
        (FINANCIAL_INDICES_ID, WidgetCategory::Market),
        (MARKET_TRADES_ID, WidgetCategory::Market),
        (NEWS_ID, WidgetCategory::Monitoring),
        (ORDER_BOOK_ID, WidgetCategory::Market),
        (PENTAGON_PIZZA_INDEX_ID, WidgetCategory::Monitoring),
        (STATISTICS_IDS[0], WidgetCategory::Statistics),
        (STATISTICS_IDS[1], WidgetCategory::Statistics),
        (STATISTICS_IDS[2], WidgetCategory::Statistics),
        (STATISTICS_IDS[3], WidgetCategory::Statistics),
        (STATISTICS_IDS[4], WidgetCategory::Statistics),
        (X_ACCOUNT_MONITORING_ID, WidgetCategory::Monitoring),
        (VOLUME_HEAT_MAP_ID, WidgetCategory::Statistics),
        (
            CRYPTOCURRENCY_WALLET_MONITORING_ID,
            WidgetCategory::Monitoring,
        ),
        (WATCH_LIST_ID, WidgetCategory::Market),
    ];

    #[gpui_kit::test]
    fn install_should_publish_the_whole_catalogue_as_a_global(cx: &mut TestAppContext) {
        cx.update(|cx| install(cx).expect("the catalogue should install"));

        cx.update(|cx| {
            let registry = cx.global::<WidgetRegistry>();

            assert!(registry.get(CANDLESTICK_ID).is_some());
            assert!(registry.get(CRYPTO_FGI_ID).is_some());
            assert!(registry.get(EMPTY_ID).is_some());
            assert!(registry.get(ETF_ID).is_some());
            assert!(registry.get(FINANCIAL_CALENDAR_ID).is_some());
            assert!(registry.get(FORCE_ORDERS_ID).is_some());
            assert!(registry.get(HORMUZ_VESSEL_MAP_ID).is_some());
            assert!(registry.get(HYPERLIQUID_WALLET_MONITOR_ID).is_some());
            assert!(registry.get(FINANCIAL_INDICES_ID).is_some());
            assert!(registry.get(MARKET_TRADES_ID).is_some());
            assert!(registry.get(NEWS_ID).is_some());
            assert!(registry.get(ORDER_BOOK_ID).is_some());
            assert!(registry.get(PENTAGON_PIZZA_INDEX_ID).is_some());
            assert!(registry.get(X_ACCOUNT_MONITORING_ID).is_some());
            assert!(registry.get(VOLUME_HEAT_MAP_ID).is_some());
            assert!(registry.get(CRYPTOCURRENCY_WALLET_MONITORING_ID).is_some());
            assert!(registry.get(WATCH_LIST_ID).is_some());
            for id in STATISTICS_IDS {
                assert!(registry.get(id).is_some(), "{id} should be registered");
            }
            assert_eq!(registry.iter().count(), 22);
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

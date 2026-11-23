use super::registry::{WidgetRegistration, WidgetRegistry, WidgetRegistryError};

pub fn registry() -> Result<WidgetRegistry, WidgetRegistryError> {
    let mut registry = WidgetRegistry::default();
    for registration in catalogue() {
        registry.register(registration)?;
    }

    Ok(registry)
}

fn catalogue() -> impl ExactSizeIterator<Item = WidgetRegistration> {
    [
        WidgetRegistration::for_view::<candlestick::view::CandlestickWidget>(),
        WidgetRegistration::for_view::<
            coin_market_cap_fear_and_greed_index::view::CoinMarketCapFearAndGreedIndexWidget,
        >(),
        WidgetRegistration::for_view::<cryptocurrency_etf::view::CryptocurrencyEtfWidget>(),
        WidgetRegistration::for_view::<empty::view::EmptyWidget>(),
        WidgetRegistration::for_view::<financial_calendar::view::FinancialCalendarWidget>(),
        WidgetRegistration::for_view::<force_order::view::ForceOrderWidget>(),
        WidgetRegistration::for_view::<
            global_shipping_chokepoint::view::GlobalShippingChokepointWidget,
        >(),
        WidgetRegistration::for_view::<hyperliquid_wallet::view::HyperliquidWalletWidget>(),
        WidgetRegistration::for_view::<financial_index::view::FinancialIndexWidget>(),
        WidgetRegistration::for_view::<market_trade::view::MarketTradeWidget>(),
        WidgetRegistration::for_view::<news::view::NewsWidget>(),
        WidgetRegistration::for_view::<order_book::view::OrderBookWidget>(),
        WidgetRegistration::for_view::<pentagon_pizza_index::view::PentagonPizzaIndexWidget>(),
        WidgetRegistration::for_view::<prediction_market::view::PredictionMarketWidget>(),
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
        WidgetRegistration::for_view::<perpetual_statistics::widget::taker_volume::TakerVolumeWidget>(
        ),
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
    use std::{collections::BTreeSet, fs, path::Path};

    use app_config::widget::WidgetConfigSchemaVersion;
    use widget::{category::WidgetCategory, view::WidgetView};

    use super::registry;
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
    const PREDICTION_MARKET_ID: &str = "kytrix.prediction_market";
    const X_ID: &str = "kytrix.x";
    const VOLUME_HEAT_MAP_ID: &str = "kytrix.volume_heatmap";
    const ETHEREUM_WALLET_ID: &str = "kytrix.ethereum_wallet";
    const SOLANA_WALLET_ID: &str = "kytrix.solana_wallet";
    const WATCH_LIST_ID: &str = "kytrix.watch_list";
    const STATISTICS_IDS: [&str; 5] = [
        "kytrix.open_interest",
        "kytrix.top_trader_account_ratio",
        "kytrix.top_trader_position_ratio",
        "kytrix.long_short_account_ratio",
        "kytrix.taker_volume",
    ];

    const CATEGORIES: [(&str, WidgetCategory); 23] = [
        (CANDLESTICK_ID, WidgetCategory::Market),
        (
            COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ID,
            WidgetCategory::Market,
        ),
        (CRYPTOCURRENCY_ETF_ID, WidgetCategory::Statistics),
        (FINANCIAL_CALENDAR_ID, WidgetCategory::Monitoring),
        (FORCE_ORDER_ID, WidgetCategory::Monitoring),
        (GLOBAL_SHIPPING_CHOKEPOINT_ID, WidgetCategory::Monitoring),
        (HYPERLIQUID_WALLET_ID, WidgetCategory::Monitoring),
        (FINANCIAL_INDEX_ID, WidgetCategory::Market),
        (MARKET_TRADE_ID, WidgetCategory::Market),
        (NEWS_ID, WidgetCategory::Monitoring),
        (ORDER_BOOK_ID, WidgetCategory::Market),
        (PENTAGON_PIZZA_INDEX_ID, WidgetCategory::Monitoring),
        (PREDICTION_MARKET_ID, WidgetCategory::Market),
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

    const SHARED_CRATES: [(&str, &str); 5] = [
        (STATISTICS_IDS[0], "perpetual_statistics"),
        (STATISTICS_IDS[1], "perpetual_statistics"),
        (STATISTICS_IDS[2], "perpetual_statistics"),
        (STATISTICS_IDS[3], "perpetual_statistics"),
        (STATISTICS_IDS[4], "perpetual_statistics"),
    ];

    fn catalogue_registry() -> WidgetRegistry {
        registry().expect("the static catalogue has one registration per widget id")
    }

    #[test]
    fn every_widget_text_should_translate_in_every_loaded_language() {
        let i18n = i18n::embedded::load().expect("the embedded locales should load");
        let languages = i18n.languages();
        assert!(
            !languages.is_empty(),
            "the catalogue should have a language to translate into"
        );

        for registration in catalogue_registry().iter() {
            let descriptor = registration.descriptor();
            let keys = std::iter::once(descriptor.display_name()).chain(descriptor.description());
            for key in keys {
                for language in &languages {
                    assert!(
                        i18n.try_translate(*language, key)
                            .is_some_and(|message| !message.is_empty()),
                        "{} names `{key}`, which {language} does not translate",
                        descriptor.id()
                    );
                }
            }
        }
    }

    #[test]
    fn the_catalogue_should_register_every_widget_once() {
        let registry = catalogue_registry();

        for id in [
            CANDLESTICK_ID,
            COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ID,
            CRYPTOCURRENCY_ETF_ID,
            EMPTY_ID,
            FINANCIAL_CALENDAR_ID,
            FORCE_ORDER_ID,
            GLOBAL_SHIPPING_CHOKEPOINT_ID,
            HYPERLIQUID_WALLET_ID,
            FINANCIAL_INDEX_ID,
            MARKET_TRADE_ID,
            NEWS_ID,
            ORDER_BOOK_ID,
            PENTAGON_PIZZA_INDEX_ID,
            PREDICTION_MARKET_ID,
            X_ID,
            VOLUME_HEAT_MAP_ID,
            ETHEREUM_WALLET_ID,
            SOLANA_WALLET_ID,
            WATCH_LIST_ID,
        ] {
            assert!(registry.get(id).is_some(), "{id} should be registered");
        }
        for id in STATISTICS_IDS {
            assert!(registry.get(id).is_some(), "{id} should be registered");
        }

        assert_eq!(
            registry.iter().count(),
            24,
            "20 widget crates, with `perpetual_statistics` publishing five panes"
        );
    }

    #[test]
    fn every_widget_should_declare_the_category_it_is_listed_under() {
        let registry = catalogue_registry();
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
    }

    #[test]
    fn the_widget_a_new_pane_falls_back_to_should_be_the_layout_default() {
        let registration = catalogue_registry()
            .get(app_config::workspace::DEFAULT_WIDGET_ID)
            .expect("the catalogue should register the widget a new pane shows")
            .clone();
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
    }

    #[test]
    fn every_widget_should_persist_its_config_at_schema_v1() {
        for registration in catalogue_registry().iter() {
            let descriptor = registration.descriptor();

            assert_eq!(
                descriptor.config_schema_version(),
                WidgetConfigSchemaVersion::V1,
                "{} should persist its config at schema V1",
                descriptor.id()
            );
        }
    }

    #[test]
    fn every_registered_widget_should_mount_and_release() {
        use std::rc::Rc;

        use widget::{config::WidgetConfigChange, test_support::WidgetTestApp};

        let registry = catalogue_registry();
        let mut harness = WidgetTestApp::new();
        let services = Rc::clone(harness.services_rc());
        let window = harness.window();
        let parent = harness.root();
        let baseline_nodes = harness.node_count();
        let baseline = harness.signal_slot_stats();

        for registration in registry.iter() {
            let id = registration.descriptor().id();
            let widget = registry
                .mount_default(
                    id.as_str(),
                    harness.app_mut(),
                    window,
                    parent,
                    &services,
                    WidgetConfigChange::default(),
                )
                .unwrap_or_else(|error| panic!("{id} should mount: {error}"));

            assert_eq!(widget.widget_id(), *id);
            assert!(
                harness.tree().get(widget.surface()).is_ok(),
                "{id} built the surface a pane embeds"
            );

            assert!(
                widget.release(harness.app_mut()) > 0,
                "{id} owned the scope its mount minted"
            );
            assert!(widget.is_released());
            assert_eq!(
                harness.node_count(),
                baseline_nodes,
                "{id} left a node behind"
            );
        }

        assert!(
            harness.router().is_empty(),
            "every mounted widget unregisters its frame route"
        );
        assert_eq!(
            harness.signal_slot_stats().live,
            baseline.live,
            "every value slot the mounts minted is handed back"
        );
    }

    const TEST_MODULE_ATTRIBUTE: &str = concat!("#[cfg", "(test)]");
    const NOTICE_MESSAGE_CALLS: &[&str] = &["Notice::message(", "Notice::Message("];

    const MARKET_NOT_LOADED_KEY: &str = "common.market_not_loaded";

    const MINIMUM_DISTINCT_KEYS: usize = 12;

    fn notice_crates(registry: &WidgetRegistry) -> BTreeSet<String> {
        registry
            .iter()
            .map(|registration| {
                let id = registration.descriptor().id().as_str();
                SHARED_CRATES
                    .iter()
                    .find_map(|(widget, crate_name)| {
                        (*widget == id).then_some((*crate_name).to_owned())
                    })
                    .unwrap_or_else(|| {
                        id.strip_prefix("kytrix.")
                            .unwrap_or_else(|| panic!("{id} is not a kytrix widget id"))
                            .to_owned()
                    })
            })
            .collect()
    }

    fn crate_sources(widgets: &Path, crate_name: &str) -> Vec<(String, String)> {
        let root = widgets.join(crate_name);
        assert!(
            root.is_dir(),
            "the registry derives {crate_name} from a widget ID, and crates/widgets/{crate_name} \
             does not exist — the crate has to be named as the ID says, or named in \
             SHARED_CRATES"
        );

        let mut sources = Vec::new();
        let mut pending = vec![root];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).expect("a widget crate is readable") {
                let path = entry.expect("a widget source is readable").path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default();
                if !path.extension().is_some_and(|extension| extension == "rs")
                    || name.ends_with("_tests.rs")
                    || name == "test_support.rs"
                {
                    continue;
                }
                let source = fs::read_to_string(&path).expect("a widget source is readable");
                let listed = path
                    .strip_prefix(widgets)
                    .expect("a source read out of a widget crate is below it")
                    .to_string_lossy()
                    .replace('\\', "/");
                sources.push((format!("crates/widgets/{listed}"), source));
            }
        }

        sources
    }

    fn production_source(source: &str) -> &str {
        let mut offset = 0usize;

        while let Some(attribute) = source[offset..].find(TEST_MODULE_ATTRIBUTE) {
            offset += attribute + TEST_MODULE_ATTRIBUTE.len();
            let declaration = source[offset..].trim_start();
            let opens_a_module = ["mod ", "pub mod ", "pub(crate) mod ", "pub(super) mod "]
                .iter()
                .any(|shape| declaration.starts_with(shape));
            if opens_a_module {
                return &source[..offset - TEST_MODULE_ATTRIBUTE.len()];
            }
        }

        source
    }

    fn notice_keys(source: &str) -> Vec<String> {
        let production = production_source(source);
        let mut keys = Vec::new();

        for &call in NOTICE_MESSAGE_CALLS {
            let mut rest = production;
            while let Some(handed_over) = rest.find(call) {
                rest = &rest[handed_over + call.len()..];
                let Some(opening) = rest.find('"') else {
                    break;
                };
                let key = &rest[opening + 1..];
                let Some(closing) = key.find('"') else {
                    break;
                };
                keys.push(key[..closing].to_owned());
                rest = &key[closing..];
            }
        }

        if production.contains("Notice::MarketNotLoaded") {
            keys.push(MARKET_NOT_LOADED_KEY.to_owned());
        }

        keys
    }

    #[test]
    fn every_notice_a_widget_reports_should_be_named_in_every_locale() {
        let i18n = i18n::embedded::load().expect("the embedded locales should load");
        let registry = catalogue_registry();
        let widgets = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the registry crate lives one directory below the crates directory")
            .join("widgets");
        let crates = notice_crates(&registry);
        assert_eq!(
            crates.len(),
            20,
            "one crate per registered widget family: {crates:?}"
        );

        let mut named = Vec::new();
        for crate_name in &crates {
            for (source, text) in crate_sources(&widgets, crate_name) {
                named.extend(
                    notice_keys(&text)
                        .into_iter()
                        .map(|key| (source.clone(), key)),
                );
            }
        }

        let distinct = named
            .iter()
            .map(|(_, key)| key.as_str())
            .collect::<BTreeSet<_>>();
        assert!(
            distinct.len() >= MINIMUM_DISTINCT_KEYS,
            "the widgets hand their notices over by key, and {} distinct key(s) were read out \
             of them: {distinct:?}",
            distinct.len()
        );

        for language in i18n.languages() {
            for (source, key) in &named {
                assert!(
                    i18n.try_translate(language, key)
                        .is_some_and(|message| !message.is_empty()),
                    "{source} hands a notice over under `{key}`, which {language} does not define \
                     or renders empty"
                );
            }
        }
    }

    #[test]
    fn every_widget_crate_should_publish_a_registered_widget() {
        let widgets = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the registry crate lives one directory below the crates directory")
            .join("widgets");
        let registered = notice_crates(&catalogue_registry());

        let mut on_disk = BTreeSet::new();
        for entry in fs::read_dir(&widgets).expect("crates/widgets is readable") {
            let path = entry.expect("a widget crate is readable").path();
            if path.is_dir() {
                on_disk.insert(
                    path.file_name()
                        .expect("a widget crate has a name")
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }

        assert_eq!(
            on_disk, registered,
            "crates/widgets and the registered widget IDs have to name the same crates"
        );
    }
}

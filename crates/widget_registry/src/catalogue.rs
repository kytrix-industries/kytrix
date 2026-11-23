//! The static widget catalogue: the one list of every widget compiled into the
//! application.
//!
//! Nothing enumerates widgets at run time and nothing installs a catalogue in a process
//! global: [`registry`] assembles the table, and the composition root keeps the result
//! for the lifetime of the application, handing it to whatever mounts a pane. The list
//! is exhaustive by construction — a widget missing from it is a widget no pane can
//! restore — which is why the tests below walk the registry rather than a list written
//! beside it.
//!
//! # What the catalogue owns
//!
//! Identity only: which widget types exist, and the metadata each declares (its ID, its
//! name/description message keys, its category, its icon, and whether a selection
//! surface may offer it). The widget's own crate owns everything else — its config, its
//! body, its header controls — and the framework owns the presentation.
//!
//! # The notice guard
//!
//! A widget with nothing to show reports a [`Notice`](widget::status::Notice), which is a
//! **reference into the catalogue** rather than a sentence. Nothing else reads that key:
//! a key no locale defines renders as the empty string, so the placeholder of a waiting
//! pane would be blank instead of the guidance it exists to give. The guard used to be a
//! hand-maintained list of source files (`WIDGET_NOTICE_SOURCES`, deleted in P3-0 for the
//! migration); it is rebuilt here **from the registered widget IDs**, so registering a
//! widget is what brings its notices under the guard.

use super::registry::{WidgetRegistration, WidgetRegistry, WidgetRegistryError};

/// Assembles the catalogue of every widget compiled into the application.
///
/// The composition root calls this once and keeps the registry; there is no toolkit
/// global to install it into (see [`crate::registry`]).
///
/// # Errors
///
/// Fails when two registrations claim the same widget ID, which means two catalogue
/// entries disagree about a persisted identity.
pub fn registry() -> Result<WidgetRegistry, WidgetRegistryError> {
    let mut registry = WidgetRegistry::default();
    for registration in catalogue() {
        registry.register(registration)?;
    }

    Ok(registry)
}

/// Every statically linked widget, in catalogue order.
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

    /// The crates that publish more than one widget.
    ///
    /// A crate that publishes one widget keeps its name in the ID, so the crate
    /// directory is the segment after `kytrix.` (`kytrix.news` is `crates/widgets/news`).
    /// These five panes are one crate, so the directory is not derived from the ID.
    const SHARED_CRATES: [(&str, &str); 5] = [
        (STATISTICS_IDS[0], "perpetual_statistics"),
        (STATISTICS_IDS[1], "perpetual_statistics"),
        (STATISTICS_IDS[2], "perpetual_statistics"),
        (STATISTICS_IDS[3], "perpetual_statistics"),
        (STATISTICS_IDS[4], "perpetual_statistics"),
    ];

    /// The catalogue, with the duplicate-ID failure treated as the test failure it is.
    fn catalogue_registry() -> WidgetRegistry {
        registry().expect("the static catalogue has one registration per widget id")
    }

    /// A widget's name and description are message keys, and a key no locale defines
    /// translates to an empty string: a widget that names one the catalogue lacks would
    /// draw a blank line. Every registered widget is therefore checked in every loaded
    /// language.
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

    /// The catalogue is the whole set of widgets a pane can be restored into: the 20
    /// widget crates, with `perpetual_statistics` publishing five panes of its own.
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

    /// The widget a pane nobody has chosen for yet shows is named by two crates:
    /// `app_config` writes the ID into the layout of such a pane, and the widget
    /// crate owns it. This is the test that keeps the two names the same widget.
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

    /// Every persisted config is schema V1.
    ///
    /// A widget that started at V2 has been reset rather than migrated: the version this
    /// build writes is the first one, and a stored envelope from any other version is
    /// reset on the way in.
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

    /// Every registered widget mounts through the registry and releases without a trace.
    ///
    /// This is the traversal P3-3 exists for: each of the 24 entries is mounted by ID
    /// through the one path a pane uses, and released again. A registration whose type no
    /// longer satisfies the contract fails to mount, and a release that leaves a frame
    /// route, a node or a value slot behind fails the counters — both here rather than in
    /// the gallery.
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

    /// The attribute that opens the tests of a source file, which is where its
    /// production half ends.
    const TEST_MODULE_ATTRIBUTE: &str = concat!("#[cfg", "(test)]");
    /// The two spellings a widget hands a message key over in: the constructor, and the
    /// variant it constructs. A widget that writes the variant out by hand names the
    /// same key.
    const NOTICE_MESSAGE_CALLS: &[&str] = &["Notice::message(", "Notice::Message("];

    /// The key of the one notice a widget does not name itself.
    ///
    /// A widget that needs a market loaded hands over the venues it waits for and the
    /// venues it can read at all, and the sentence that names the second set is the
    /// catalogue's own: `venue::market_not_loaded_sentence` renders
    /// `common.market_not_loaded` from them.
    const MARKET_NOT_LOADED_KEY: &str = "common.market_not_loaded";

    /// The number of distinct keys the guard has to read to prove it read anything.
    ///
    /// The widgets hand their notices over under 16 distinct keys today; the bound is
    /// the old guard's canary for "the reader found no call site at all", kept so a
    /// broken scan fails here instead of passing vacuously.
    const MINIMUM_DISTINCT_KEYS: usize = 12;

    /// The crate directories the guard reads, derived from the registered widget IDs.
    ///
    /// This is what replaces the hand-written `WIDGET_NOTICE_SOURCES` list: registration
    /// is what puts a widget's sources under the guard, so a widget added to the
    /// catalogue is covered without a second list to update. The directory is derived
    /// from the ID rather than declared next to it, and [`SHARED_CRATES`] names the one
    /// crate that publishes more than one widget.
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

    /// Every production source file of one widget crate, spelled the way a reader would
    /// open it, with the source read.
    ///
    /// The walk is recursive because a crate that publishes more than one widget keeps
    /// their shared pane a directory below `src`. Tests are skipped: a module that is a
    /// directory keeps its tests in sibling `*_tests.rs` files and the fakes several of
    /// them share in `test_support.rs` (`crates/widgets/AGENTS.md` §2), and neither is
    /// compiled into a widget, so neither hands a notice over — a notice a test writes
    /// for itself is not one a pane draws.
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

    /// The production half of one source file: everything above the test module it
    /// declares.
    ///
    /// The slice is taken at the test module rather than at the first `#[cfg(test)]`,
    /// because the widget sources mark accessors a test drives with that attribute in the
    /// middle of their production half — a slice taken at the first attribute would leave
    /// the notice calls after it unread. A source that declares no test module is read
    /// whole.
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

    /// The message keys the production half of one widget source hands a notice over
    /// under.
    ///
    /// A notice names its key as a literal at every call site, so the literal after the
    /// call is the whole of what it names. The literal is looked for after the call
    /// rather than on the same line, because a call site is written both on one line and
    /// one argument per line. The market-not-loaded notice is the one whose key is not at
    /// the call site: the widget hands over the venues it needs, and the catalogue renders
    /// them through a key of its own.
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

    /// Every key a registered widget hands a notice over under is one every locale
    /// defines and renders.
    ///
    /// The sources are derived from the registry ([`notice_crates`]) rather than listed,
    /// so the widget that hands a notice over is the widget whose key is checked: a
    /// notice that is added — or one whose key is mistyped — is covered by the call site
    /// rather than by a list written beside it.
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

    /// Every widget crate in the tree publishes a registered widget, and every registered
    /// widget's crate exists.
    ///
    /// [`notice_crates`] derives the crate from the ID, which is what puts a widget's
    /// notices under the guard: a crate nothing registers would never be scanned, so a
    /// widget added to the tree without a catalogue entry fails here rather than later,
    /// when a pane cannot be restored into it.
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

use std::rc::Rc;

use app_config::settings::Config;
use bytes::Bytes;
use i18n::{language::codes::LanguageCode, translate::Translate};
use kytrix_service_provider::service::connector::KytrixServiceConnector;
use market_participants_connector::connector::MarketParticipantsConnector;
use tokio::runtime::Handle;
use ui_core::{app::App, node::signal::Signal};
use ui_kit::theme::handle::ThemeHandle;

use crate::frame::FrameRouter;

pub struct WidgetServices {
    pub translate: Translate,
    pub language: Signal<LanguageCode>,
    pub config: Signal<Config>,
    pub theme: ThemeHandle,
    pub market_participants: Option<MarketParticipantsConnector>,
    pub catalog_generation: Signal<u64>,
    pub kytrix_service: Option<KytrixServiceConnector>,
    pub open_url: Rc<dyn Fn(&str)>,
    pub sound: Rc<dyn Fn(&'static str) -> Option<Bytes>>,
    pub reduce_motion: Signal<bool>,
    pub frame_router: Rc<FrameRouter>,
    pub runtime: Handle,
}

impl WidgetServices {
    #[must_use]
    pub fn market_participants(&self) -> Option<&MarketParticipantsConnector> {
        self.market_participants.as_ref()
    }

    #[must_use]
    pub fn kytrix_service(&self) -> Option<&KytrixServiceConnector> {
        self.kytrix_service.as_ref()
    }

    pub fn builder(
        app: &mut App,
        runtime: Handle,
        translate: Translate,
        theme: ThemeHandle,
        frame_router: Rc<FrameRouter>,
    ) -> WidgetServicesBuilder<'_> {
        WidgetServicesBuilder {
            app,
            runtime,
            translate,
            theme,
            frame_router,
            language: LanguageCode::En,
            config: Config::default(),
            catalog_generation: 0,
            reduce_motion: false,
            market_participants: None,
            kytrix_service: None,
            open_url: Rc::new(|_| {}),
            sound: Rc::new(|_| None),
        }
    }
}

pub struct WidgetServicesBuilder<'a> {
    app: &'a mut App,
    runtime: Handle,
    translate: Translate,
    theme: ThemeHandle,
    frame_router: Rc<FrameRouter>,
    language: LanguageCode,
    config: Config,
    catalog_generation: u64,
    reduce_motion: bool,
    market_participants: Option<MarketParticipantsConnector>,
    kytrix_service: Option<KytrixServiceConnector>,
    open_url: Rc<dyn Fn(&str)>,
    sound: Rc<dyn Fn(&'static str) -> Option<Bytes>>,
}

impl WidgetServicesBuilder<'_> {
    #[must_use]
    pub fn language(mut self, language: LanguageCode) -> Self {
        self.language = language;
        self
    }

    #[must_use]
    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    #[must_use]
    pub fn catalog_generation(mut self, generation: u64) -> Self {
        self.catalog_generation = generation;
        self
    }

    #[must_use]
    pub fn reduce_motion(mut self, reduce_motion: bool) -> Self {
        self.reduce_motion = reduce_motion;
        self
    }

    #[must_use]
    pub fn market_participants(
        mut self,
        market_participants: Option<MarketParticipantsConnector>,
    ) -> Self {
        self.market_participants = market_participants;
        self
    }

    #[must_use]
    pub fn kytrix_service(mut self, kytrix_service: Option<KytrixServiceConnector>) -> Self {
        self.kytrix_service = kytrix_service;
        self
    }

    #[must_use]
    pub fn open_url(mut self, open_url: impl Fn(&str) + 'static) -> Self {
        self.open_url = Rc::new(open_url);
        self
    }

    #[must_use]
    pub fn sound(mut self, sound: impl Fn(&'static str) -> Option<Bytes> + 'static) -> Self {
        self.sound = Rc::new(sound);
        self
    }

    #[must_use]
    pub fn build(self) -> WidgetServices {
        let Self {
            app,
            runtime,
            translate,
            theme,
            frame_router,
            language,
            config,
            catalog_generation,
            reduce_motion,
            market_participants,
            kytrix_service,
            open_url,
            sound,
        } = self;
        let scope = app.signal_scope();

        WidgetServices {
            translate,
            language: app.signal_scoped(scope, language),
            config: app.signal_scoped(scope, config),
            theme,
            market_participants,
            catalog_generation: app.signal_scoped(scope, catalog_generation),
            kytrix_service,
            open_url,
            sound,
            reduce_motion: app.signal_scoped(scope, reduce_motion),
            frame_router,
            runtime,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use app_config::settings::{Appearance, Config};
    use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
    use smol_str::{SmolStr, format_smolstr};
    use tokio::runtime::Handle;
    use ui_core::{
        app::App,
        error::EngineError,
        math::px,
        node::{
            builder::NodeBuilder,
            id::NodeId,
            signal::{Signal, SignalScope},
        },
        popup::{Anchor, PlacementSpec, PopupId, PopupSpec, PopupTier},
        text::TextStyle,
    };
    use ui_kit::theme::{appearance::Appearance as ThemeAppearance, handle::ThemeHandle};

    use super::WidgetServices;
    use crate::{
        category::WidgetCategory,
        config::WidgetConfigChange,
        cx::WidgetCx,
        frame::FrameRouter,
        identity::{WidgetId, WidgetInstanceId},
        status::{Notice, WidgetAction, WidgetState, WidgetStatus},
        test_support::WidgetTestApp,
        view::{ReloadHandle, WidgetView},
    };

    const MESSAGES_EN: &str = "common:\n  close: Close\n";
    const MESSAGES_ZH: &str = "common:\n  close: 关闭\n";

    type Opened = Rc<RefCell<Vec<String>>>;

    fn compose() -> (App, WidgetServices, Opened, Rc<FrameRouter>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("the test runtime builds");
        let mut app = App::new(runtime.handle().clone());
        let theme = ThemeHandle::new(&mut app, ThemeAppearance::Light);
        let translate = Translate::new(
            Rc::new(
                i18n::translator::types::I18n::builder()
                    .locale_yaml(LanguageCode::En, MESSAGES_EN)
                    .expect("the English fixture parses")
                    .locale_yaml(LanguageCode::ZhCn, MESSAGES_ZH)
                    .expect("the Chinese fixture parses")
                    .fallback(LanguageCode::En)
                    .build()
                    .expect("the fixture catalogue builds"),
            ),
            LanguageCode::En,
        );
        let frame_router =
            Rc::new(FrameRouter::new(&mut app).expect("the fresh frame slot is free"));

        let opened: Opened = Rc::new(RefCell::new(Vec::new()));
        let record = Rc::clone(&opened);
        let services = WidgetServices::builder(
            &mut app,
            runtime.handle().clone(),
            translate,
            theme,
            Rc::clone(&frame_router),
        )
        .open_url(move |url| record.borrow_mut().push(url.to_owned()))
        .build();

        (app, services, opened, frame_router)
    }

    #[test]
    fn services_compose_without_a_gui_context() {
        let (app, services, opened, frame_router) = compose();

        assert_eq!(t_smol!(services.translate, "common.close"), "Close");
        assert_eq!(*app.read(&services.language), LanguageCode::En);
        assert_eq!(app.read(&services.config).appearance, Appearance::System);
        assert_eq!(
            services.theme.current(&app),
            ThemeAppearance::Light,
            "the injected theme is the one the host built"
        );
        assert!(
            services.market_participants().is_none(),
            "a headless host leaves the directory absent"
        );
        assert_eq!(
            *app.read(&services.catalog_generation),
            0,
            "no catalogue generation has been published yet"
        );
        assert!(
            services.kytrix_service().is_none(),
            "a headless host leaves the service connection absent"
        );
        assert_eq!(
            (services.sound)("sounds/news_notification.wav"),
            None,
            "a host without audio answers None instead of failing"
        );
        assert!(
            !*app.read(&services.reduce_motion),
            "reduced motion starts off unless the application mirrors a setting"
        );
        assert!(
            Rc::ptr_eq(&services.frame_router, &frame_router),
            "the aggregate carries the composition root's one frame router"
        );
        assert!(
            frame_router.installed(),
            "the composition root's router owns the application's frame hook"
        );

        (services.open_url)("https://example.com/market");
        assert_eq!(opened.borrow().as_slice(), ["https://example.com/market"]);
    }

    #[test]
    fn the_builder_composes_every_frozen_slot() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("the test runtime builds");
        let mut app = App::new(runtime.handle().clone());
        let theme = ThemeHandle::new(&mut app, ThemeAppearance::Dark);
        let translate = Translate::new(
            Rc::new(
                i18n::translator::types::I18n::builder()
                    .locale_yaml(LanguageCode::En, MESSAGES_EN)
                    .expect("the English fixture parses")
                    .fallback(LanguageCode::En)
                    .build()
                    .expect("the fixture catalogue builds"),
            ),
            LanguageCode::ZhCn,
        );
        let frame_router =
            Rc::new(FrameRouter::new(&mut app).expect("the fresh frame slot is free"));
        let opened: Opened = Rc::new(RefCell::new(Vec::new()));
        let record = Rc::clone(&opened);
        let mut config = Config::default();
        config.data_time_zone =
            app_config::time_zone::DataTimeZone::from_hours(8).expect("UTC+8 is offered");

        let services = WidgetServices::builder(
            &mut app,
            runtime.handle().clone(),
            translate,
            theme,
            Rc::clone(&frame_router),
        )
        .language(LanguageCode::ZhCn)
        .config(config)
        .catalog_generation(7)
        .reduce_motion(true)
        .open_url(move |url| record.borrow_mut().push(url.to_owned()))
        .sound(|name| {
            (name == "sounds/news_notification.wav").then_some(bytes::Bytes::from_static(b"clip"))
        })
        .build();

        assert_eq!(*app.read(&services.language), LanguageCode::ZhCn);
        assert_eq!(
            app.read_with(&services.config, |config| {
                crate::time_zone::data_time_zone(config).whole_hours()
            }),
            8
        );
        assert_eq!(*app.read(&services.catalog_generation), 7);
        assert!(*app.read(&services.reduce_motion));
        assert_eq!(
            services.theme.current(&app),
            ThemeAppearance::Dark,
            "the theme the host minted is the one the aggregate carries"
        );
        assert_eq!(
            (services.sound)("sounds/news_notification.wav"),
            Some(bytes::Bytes::from_static(b"clip"))
        );
        assert_eq!(
            (services.sound)("sounds/other.wav"),
            None,
            "the seam answers only for the asset it was given"
        );
        (services.open_url)("https://example.com/news");
        assert_eq!(opened.borrow().as_slice(), ["https://example.com/news"]);
    }

    fn open_popup(harness: &mut WidgetTestApp, key: &str, tier: PopupTier) -> PopupId {
        harness.with_cx(|cx| {
            let root = cx.tree().root();
            let trigger = NodeBuilder::boxed()
                .id(format_smolstr!("{key}:trigger"))
                .build(cx.tree_mut(), root)
                .expect("the anchor node builds");
            let window = cx.window();
            let mut spec = PopupSpec::new(Anchor::Node { node: trigger }, PlacementSpec::default());
            spec.tier = tier;
            let key = key.to_owned();
            cx.app_mut()
                .open_popup(
                    window,
                    spec,
                    move |app, window, node| {
                        let Some(target) = app.window_mut(window) else {
                            return Err(EngineError::UnknownWindow { window });
                        };
                        NodeBuilder::boxed()
                            .id(format_smolstr!("{key}:content"))
                            .build(target.tree_mut(), node)
                            .map(|_| ())
                    },
                    None,
                )
                .expect("the popup opens")
        })
    }

    #[test]
    fn a_popup_opened_through_the_widget_context_registers_in_the_window_stack() {
        let mut harness = WidgetTestApp::new();
        let id = open_popup(&mut harness, "test.menu", PopupTier::Auto);

        let stack = harness
            .app()
            .popup_stack(harness.window())
            .expect("the window owns a popup stack");
        assert_eq!(
            stack.entries.len(),
            1,
            "the popup registered in the window's stack"
        );
        assert_eq!(stack.entries[0].id, id);
        assert!(
            harness.tree().get(stack.entries[0].node).is_ok(),
            "the stack names the content node the widget context built"
        );

        harness
            .with_cx(|cx| cx.app_mut().close_popup(id))
            .expect("the popup is open");
        assert!(
            harness
                .app()
                .popup_stack(harness.window())
                .expect("the window owns a popup stack")
                .entries
                .is_empty(),
            "closing the popup unregistered it from the same stack"
        );
        assert!(harness.app().popup_rect(id).is_none());
    }

    #[test]
    fn the_widget_context_exposes_the_hosts_frame_router_and_window() {
        let mut harness = WidgetTestApp::new();
        let host_router = harness.router() as *const FrameRouter;
        harness.with_cx(|cx| {
            assert_eq!(
                cx.frame_router() as *const FrameRouter,
                host_router,
                "the context exposes the host's frame router"
            );
        });

        let menu = open_popup(&mut harness, "test.menu", PopupTier::Auto);
        let tooltip = open_popup(&mut harness, "test.tooltip", PopupTier::Hint);
        let stack = harness
            .app()
            .popup_stack(harness.window())
            .expect("the window owns a popup stack");
        assert_eq!(
            stack
                .entries
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            [menu, tooltip],
            "both popups are listed bottom-up in the window the context was handed"
        );
    }

    #[test]
    fn reduced_motion_is_a_signal_the_application_owns() {
        let (mut app, services, _opened, _frame_router) = compose();

        assert!(app.write(&services.reduce_motion, true));
        assert!(*app.read(&services.reduce_motion));

        assert!(
            !app.write(&services.reduce_motion, true),
            "writing the same value again is deduplicated"
        );
    }

    #[test]
    fn switching_the_language_updates_the_signal_and_the_translator_together() {
        let (mut app, services, _opened, _frame_router) = compose();

        assert_eq!(t_smol!(services.translate, "common.close"), "Close");
        assert!(services.translate.set_language(LanguageCode::ZhCn));
        assert!(
            app.write(&services.language, LanguageCode::ZhCn),
            "the switch changes the signal, which redraws the components that read it"
        );

        assert_eq!(*app.read(&services.language), LanguageCode::ZhCn);
        assert_eq!(
            t_smol!(services.translate, "common.close"),
            "关闭",
            "the next render reads the signal and draws through the translator carrying the \
             same language"
        );
    }

    #[test]
    fn a_settings_change_reaches_the_widget_through_the_signal() {
        let (mut app, services, _opened, _frame_router) = compose();
        let mut config = (*app.read(&services.config)).clone();
        config.data_time_zone =
            app_config::time_zone::DataTimeZone::from_hours(8).expect("UTC+8 is offered");

        assert!(app.write(&services.config, config));
        assert_eq!(
            app.read_with(&services.config, |config| {
                crate::time_zone::data_time_zone(config).whole_hours()
            }),
            8
        );
    }

    #[test]
    fn the_services_signals_live_in_one_scope() {
        let (mut app, services, _opened, _frame_router) = compose();
        let scope: SignalScope = app.signal_scope();
        let signal = app.signal_scoped(scope, 1_u8);

        let released = app.release_scope(scope);

        assert!(released > 0);
        assert_eq!(*app.read(&services.language), LanguageCode::En);
        let _: Signal<u8> = signal;
    }

    #[test]
    fn the_services_carry_the_runtime_the_widget_spawns_on() {
        let (_app, services, _opened, _frame_router) = compose();
        let handle: &Handle = &services.runtime;
        assert!(handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread);
    }

    struct DirectoryConsumer {
        status: WidgetStatus,
        generation: Signal<u64>,
        seen: u64,
        reloads: Signal<u64>,
        ready: bool,
    }

    impl WidgetView for DirectoryConsumer {
        type Config = ();

        const ID: WidgetId = WidgetId::new_static("test.directory_consumer");
        const DISPLAY_NAME: &'static str = "test.directory_consumer";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            status: WidgetStatus,
            _reload: ReloadHandle,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            cx: &mut WidgetCx<'_>,
        ) -> Result<Self, EngineError> {
            let generation = cx.services().catalog_generation;
            let seen = *cx.read(&generation);
            let reloads = cx.signal(0_u64);
            Ok(Self {
                status,
                generation,
                seen,
                reloads,
                ready: false,
            })
        }

        fn load(&mut self, cx: &mut WidgetCx<'_>) {
            let state = if self.ready {
                WidgetState::Ready
            } else {
                WidgetState::waiting(Notice::message("test.waiting"))
            };
            self.status.report(cx.app_mut(), state);
        }

        fn has_content(&self) -> bool {
            self.ready
        }

        fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            let reloads = self.reloads;
            NodeBuilder::text(SmolStr::default(), TextStyle::new(px(13.0)))
                .bind_text(move |read| {
                    let reloads = *read.read(&reloads);
                    format_smolstr!("reloaded {reloads}")
                })
                .build(cx.tree_mut(), parent)?;
            Ok(())
        }

        fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            let generation = *cx.read(&self.generation);
            if generation != self.seen {
                self.seen = generation;
                self.ready = true;
                cx.update(&self.reloads, |reloads| *reloads += 1);
                self.load(cx);
                cx.request_frame()?;
            }
            Ok(())
        }
    }

    #[test]
    fn a_directory_generation_change_reaches_a_widget_waiting_for_it() {
        let mut harness = WidgetTestApp::new();
        let widget = harness.mount::<DirectoryConsumer>(());
        harness.drain();

        assert_eq!(
            widget.status().state(harness.app()),
            WidgetState::waiting(Notice::message("test.waiting"))
        );
        let requests = harness.app().frame_requests();
        let generation = harness.services().catalog_generation;

        assert!(harness.app_mut().write(&generation, 1));

        assert!(
            harness.app().frame_requests() > requests,
            "the surface root carries the directory dependency, so a generation write \
             schedules the frame that runs sync"
        );

        harness.drain();
        harness.drain();

        assert_eq!(widget.status().state(harness.app()), WidgetState::Ready);
        let content = harness
            .find_by_debug_prefix(widget.surface(), "WIDGET_CONTENT")
            .expect("the content slot exists");
        assert!(
            harness.text_of(content).contains("reloaded 1"),
            "the consumer reloaded once and drew the body it became ready with: {:?}",
            harness.text_of(content)
        );
    }

    #[test]
    fn an_unchanged_directory_generation_schedules_no_frame() {
        let mut harness = WidgetTestApp::new();
        let _widget = harness.mount::<DirectoryConsumer>(());
        harness.drain();
        let generation = harness.services().catalog_generation;
        let settled = harness.app().frame_requests();

        assert!(
            !harness.app_mut().write(&generation, 0),
            "the generation did not move"
        );
        harness.drain();

        assert_eq!(
            harness.app().frame_requests(),
            settled,
            "an unchanged generation scheduled no frame"
        );
    }
}

//! The collaborators a widget is handed instead of reading process globals.
//!
//! This is the replacement for the process globals the framework used to reach through
//! its context: a widget renders with the [`Translate`] it was handed, reads the
//! selected language from the [`Signal`] it was handed, reads settings from the
//! [`Signal<Config>`] the composition root keeps in step with the settings manager,
//! draws through the [`ThemeHandle`] it was handed, and drives its streams on the
//! runtime it was handed. Nothing here is installed process-wide and nothing is
//! looked up by type, so a test composes the same aggregate the application does.
//!
//! # The settings signal
//!
//! P3-0 froze this type with a `ConfigManager` field. That manager owns an application-shell
//! entity, so the aggregate could only be built inside that shell — which
//! is exactly what the contract layer is leaving behind. P3-1 freezes the engine
//! shape instead: `config` is the **settings snapshot as a signal**. The composition
//! root owns the `ConfigManager`, writes the signal whenever the manager's snapshot
//! moves, and widgets read either the value
//! ([`WidgetCx::config_ref`](crate::cx::WidgetCx::config_ref)) or the signal (inside a
//! `bind_*` closure that must follow a setting).
//!
//! The framework's surface root reads the signal while it renders, like the language
//! and the directory generation: **one settings write schedules exactly one frame**,
//! and every mounted widget's `sync` runs on it — including a widget that has no body
//! yet, whose own tree therefore has no node that could carry the edge. A widget that
//! follows a setting compares it in `sync` (or derives its drawing from it inside a
//! `bind_*`); it never schedules the frame itself. See `crates/widgets/AGENTS.md` §8.1.
//!
//! # The directory generation
//!
//! [`WidgetServices::catalog_generation`] is the published market-directory generation
//! as a signal. The composition root writes the value of a `CatalogGenerationId`
//! whenever the catalogue publishes a new generation (`0` = nothing published yet),
//! and the framework's surface root reads it while it renders: a write therefore
//! schedules exactly one frame, and every mounted widget's `sync` runs on it —
//! including a widget that is still waiting for the directory and has no body that
//! could read anything. A widget that follows the directory compares the signal with
//! the value it saw in `new` and runs `load` again when it moved
//! (`crates/widgets/AGENTS.md` §8.3); it never reads the connector's
//! `generation_id()` itself.
//!
//! # Why two collaborators are optional
//!
//! Both [`WidgetServices::market_participants`] and
//! [`WidgetServices::kytrix_service`] are application-shell objects: the directory
//! connector takes a `widget_tokio_runtime` runtime, and the service connector owns
//! the process's OAuth session and its transport. Both are constructed by the
//! composition root (the connector through its toolkit-free
//! `KytrixServiceConnector::spawn`, P4-5). The contract layer cannot construct either
//! in a headless test, so both are `Option` (§13.9 R-1):
//!
//! - the application root always supplies them;
//! - a headless test that does not read the directory or the service leaves them
//!   absent, and a widget that needs one maps `None` to its own business
//!   notice/failure (there is no shared `WidgetError` type for it — a widget states
//!   its own).
//!
//! # The application seams a widget cannot read itself
//!
//! Three collaborators are application-layer actions rather than data:
//!
//! - [`WidgetServices::open_url`] — opening an external link is the application's
//!   command (design §11.7); the contract only receives a closure, so it never learns
//!   which browser or shell the platform uses.
//! - [`WidgetServices::sound`] — audio assets and the player stay in the application
//!   (design §9.5); a widget asks for the bytes of a named sound and gets `None` when
//!   this host has no audio, which is the ordinary case in a test.
//! - [`WidgetServices::reduce_motion`] — the application mirrors its accessibility
//!   settings into a signal; a widget that animates reads it and lands on the final
//!   value when it is set. The engine has no accessibility-settings store of its own.
//!
//! # The three composition-root singletons
//!
//! Kit components need three application-level instances that only the composition
//! root may create:
//!
//! - [`WidgetServices::layer`] — the z-order arbitration between overlays
//!   (`base_ui::kit::layer::Layer`). One instance per application, partitioned by
//!   window inside; a second one would make two overlays disagree about who is on
//!   top, and dialogs / popups opened through different stacks would interleave
//!   wrongly.
//! - [`WidgetServices::frame_router`] — the owner of the engine's single
//!   `App::on_frame` slot ([`FrameRouter`]). `AnyWidget::mount` registers its sync
//!   route here and `release` unregisters it; a second router cannot install itself
//!   (the slot is taken) and its routes would silently never run.
//! - [`WidgetServices::animation_router`] — the owner of the engine's single
//!   `App::on_animation_complete` slot (`base_ui::kit::overlay::AnimationRouter`).
//!   Every `Overlay` / `Tooltip` hands its fade-in/fade-out ids to this one router; a
//!   second one would replace the completion callback and the first overlay's fade-out
//!   would never finish (the node would stay on screen).
//!
//! A widget therefore never builds its own — it takes the instance it was handed
//! ([`WidgetCx::layer`](crate::cx::WidgetCx::layer),
//! [`WidgetCx::frame_router`](crate::cx::WidgetCx::frame_router),
//! [`WidgetCx::animation`](crate::cx::WidgetCx::animation)) and passes clones into the
//! kit component it builds. The three fields are `Rc` handles so cloning one into a
//! dialog / selector host shares the composition root's instance rather than copying
//! its state. [`WidgetServices::builder`] is the assembly seam: the composition root
//! (and the test host) names the three singletons once, and everything a widget builds
//! through its context reaches the same instances.
//!
//! The aggregate is deliberately minimal: it gains a field when a widget needs
//! another collaborator, not because one may be needed later. P3-1 freezes this
//! shape.
//!
//! # Language
//!
//! A widget reads [`WidgetServices::language`] where it renders — that read
//! registers the engine dependency which redraws it — and renders its text through
//! [`WidgetServices::translate`]. The translator is pure `i18n` state and observes no
//! signal, so both carry the selected language and the composition root keeps them
//! in step: switching the language sets the translator's language and writes the
//! signal. Set the translator first, so a redraw the write schedules can only draw
//! the new language.
//!
//! # Threading
//!
//! The aggregate is neither `Clone` (it owns a [`Translate`] and a [`ThemeHandle`],
//! both `Rc`-backed and `!Send`) nor `Send`; the host builds it once on the UI
//! thread and hands it out as an `Rc<WidgetServices>`. It must not be captured by a
//! future that requires `Send`, such as the engine's spawn. The three singletons are
//! `Rc` handles for the same reason: they are shared on the UI thread, never across
//! one.
//!
//! # Examples
//!
//! A widget reads the settings snapshot from the services it was handed:
//!
//! ```text
//! // Before: the settings lived in a process global looked up through the context.
//! let config = cx.global::<ConfigManager>().read();
//!
//! // After: the composition root hands the services in, and the widget reads a field
//! // (or a borrowed view of the snapshot, without cloning it per frame).
//! let zone = cx.config_ref(|config| data_time_zone(config));
//! ```
//!
//! The translation context is the other half of the same move — see
//! [`Translate`] and [`t_smol!`](i18n::t_smol).

use std::rc::Rc;

use app_config::settings::Config;
use base_ui::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::handle::ThemeHandle,
};
use bytes::Bytes;
use i18n::{language::codes::LanguageCode, translate::Translate};
use kytrix_service_provider::service::connector::KytrixServiceConnector;
use market_participants_connector::connector::MarketParticipantsConnector;
use tokio::runtime::Handle;
use ui_core::{app::App, node::signal::Signal};

use crate::frame::FrameRouter;

/// The services every widget is given by the composition root.
///
/// See the module documentation for the frozen shape, the language contract, the
/// optional collaborators, and the threading rules.
pub struct WidgetServices {
    /// Renders the interface's own messages in the selected language.
    pub translate: Translate,
    /// The selected language as an engine signal: a component that reads it while it
    /// renders is redrawn when the language changes.
    pub language: Signal<LanguageCode>,
    /// The current settings snapshot as an engine signal.
    ///
    /// The composition root writes it when `ConfigManager`'s snapshot moves; a
    /// `bind_*` closure that reads it follows the setting it reads. The framework's
    /// surface root reads it too, so a write schedules a frame even for a widget
    /// with no body (see the module documentation).
    pub config: Signal<Config>,
    /// The theme service: colours are read through it inside `bind_style` closures.
    pub theme: ThemeHandle,
    /// The market-participants directory, when the host has one.
    ///
    /// The application root always supplies it; a headless test that does not read
    /// the directory leaves it absent. See the module documentation.
    pub market_participants: Option<MarketParticipantsConnector>,
    /// The published market-directory generation, as an engine signal.
    ///
    /// The composition root writes `CatalogGenerationId::get()` whenever a venue
    /// commit publishes a new generation; `0` = nothing published yet. The
    /// framework's surface root reads it while it renders, so a write schedules the
    /// frame that runs every widget's `sync`. A widget that follows the directory
    /// compares it with the value it saw and reloads when it moved; see the module
    /// documentation.
    pub catalog_generation: Signal<u64>,
    /// The Kytrix service connection, when the host has one.
    ///
    /// The application root always supplies it; a headless test leaves it absent.
    /// Widgets that fetch through the service map `None` to their own failure. See
    /// the module documentation.
    pub kytrix_service: Option<KytrixServiceConnector>,
    /// Opens an external link through the application's own command.
    ///
    /// The contract receives the closure and calls it with the URL to open; it never
    /// talks to a browser or a shell itself.
    pub open_url: Rc<dyn Fn(&str)>,
    /// The bytes of one application audio asset, by name; `None` = no audio here.
    ///
    /// The player and the asset source stay in the application (design §9.5); a
    /// widget only says which sound to play.
    pub sound: Rc<dyn Fn(&'static str) -> Option<Bytes>>,
    /// Whether the application asks for reduced motion.
    ///
    /// The composition root mirrors the accessibility setting into this signal; an
    /// animating widget reads it and lands on the final value when it is set.
    pub reduce_motion: Signal<bool>,
    /// The application's one layer stack: who is on top of whom among the window's
    /// overlays.
    ///
    /// The composition root creates it once and mirrors it into every
    /// [`Overlay`](base_ui::kit::overlay::Overlay) and dialog it builds; a widget hands
    /// this instance to the kit components it builds (see the module documentation).
    /// It holds one stack partitioned by window, so a widget never partitions it
    /// itself.
    pub layer: Rc<Layer>,
    /// The application's one frame router: the owner of `App::on_frame`.
    ///
    /// The framework registers each mounted widget's `sync` route here; the kit's
    /// per-frame components (overlays, lists, tables) are ticked by the routes their
    /// owners register. The application's hook already points at this instance, so a
    /// widget never installs another one.
    pub frame_router: Rc<FrameRouter>,
    /// The application's one animation-completion router: the owner of
    /// `App::on_animation_complete`.
    ///
    /// Every overlay / tooltip fade registers its animation here, so the completion
    /// callback has one dispatcher. A second router would replace the engine's single
    /// slot and the first overlay's fade-out would never complete — see the module
    /// documentation.
    pub animation_router: Rc<AnimationRouter>,
    /// The tokio runtime the widget's streams and RPC calls are driven on.
    ///
    /// The composition root hands over the application runtime; the framework's test
    /// host owns a current-thread runtime and drives it through
    /// [`WidgetTestApp::drain_runtime`](crate::test_support::WidgetTestApp::drain_runtime)
    /// / [`run_until_idle`](crate::test_support::WidgetTestApp::run_until_idle).
    pub runtime: Handle,
}

impl WidgetServices {
    /// The market-participants directory, if the host installed one.
    #[must_use]
    pub fn market_participants(&self) -> Option<&MarketParticipantsConnector> {
        self.market_participants.as_ref()
    }

    /// The Kytrix service connection, if the host installed one.
    #[must_use]
    pub fn kytrix_service(&self) -> Option<&KytrixServiceConnector> {
        self.kytrix_service.as_ref()
    }

    /// Starts assembling the aggregate the way the composition root does.
    ///
    /// The three singletons are constructor arguments rather than setters because a
    /// widget's kit components cannot work without the application's instances (see
    /// the module documentation): a caller cannot forget one and get a silently
    /// private layer / router. The four signals are minted by
    /// [`WidgetServicesBuilder::build`] with the documented defaults; the optional
    /// application seams and the signal initial values are builder methods.
    ///
    /// `app` is where the signals are minted (one scope per built aggregate), so the
    /// builder borrows it until [`WidgetServicesBuilder::build`] runs. The caller still
    /// owns the composition-root singletons it passes in — it keeps its own handles for
    /// the frame hook, the layer enforcement and the completion routing.
    pub fn builder(
        app: &mut App,
        runtime: Handle,
        translate: Translate,
        theme: ThemeHandle,
        layer: Rc<Layer>,
        frame_router: Rc<FrameRouter>,
        animation_router: Rc<AnimationRouter>,
    ) -> WidgetServicesBuilder<'_> {
        WidgetServicesBuilder {
            app,
            runtime,
            translate,
            theme,
            layer,
            frame_router,
            animation_router,
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

/// The assembly seam for [`WidgetServices`]: defaults plus the seams a host overrides.
///
/// [`WidgetServices::builder`] documents the shape. Every builder method returns the
/// builder, so a host names only the collaborators it actually provides.
pub struct WidgetServicesBuilder<'a> {
    app: &'a mut App,
    runtime: Handle,
    translate: Translate,
    theme: ThemeHandle,
    layer: Rc<Layer>,
    frame_router: Rc<FrameRouter>,
    animation_router: Rc<AnimationRouter>,
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
    /// The initial language signal value (default: English).
    ///
    /// The host must keep the translator and the signal in step; set the translator
    /// first, then this value, so a redraw the write schedules can only draw the new
    /// language (see the module documentation).
    #[must_use]
    pub fn language(mut self, language: LanguageCode) -> Self {
        self.language = language;
        self
    }

    /// The initial settings snapshot (default: [`Config::default`]).
    #[must_use]
    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// The initial catalogue generation (default `0` = nothing published yet).
    #[must_use]
    pub fn catalog_generation(mut self, generation: u64) -> Self {
        self.catalog_generation = generation;
        self
    }

    /// The initial reduced-motion value (default `false`).
    #[must_use]
    pub fn reduce_motion(mut self, reduce_motion: bool) -> Self {
        self.reduce_motion = reduce_motion;
        self
    }

    /// The market-participants directory, when the host has one.
    #[must_use]
    pub fn market_participants(
        mut self,
        market_participants: Option<MarketParticipantsConnector>,
    ) -> Self {
        self.market_participants = market_participants;
        self
    }

    /// The Kytrix service connection, when the host has one.
    #[must_use]
    pub fn kytrix_service(mut self, kytrix_service: Option<KytrixServiceConnector>) -> Self {
        self.kytrix_service = kytrix_service;
        self
    }

    /// The application's command for opening an external link.
    #[must_use]
    pub fn open_url(mut self, open_url: impl Fn(&str) + 'static) -> Self {
        self.open_url = Rc::new(open_url);
        self
    }

    /// The application's source of audio-asset bytes.
    #[must_use]
    pub fn sound(mut self, sound: impl Fn(&'static str) -> Option<Bytes> + 'static) -> Self {
        self.sound = Rc::new(sound);
        self
    }

    /// Mints the four signals in one scope and returns the aggregate.
    #[must_use]
    pub fn build(self) -> WidgetServices {
        let Self {
            app,
            runtime,
            translate,
            theme,
            layer,
            frame_router,
            animation_router,
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
            layer,
            frame_router,
            animation_router,
            runtime,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use app_config::settings::{Appearance, Config};
    use base_ui::{
        kit::{
            layer::{Band, Layer},
            overlay::{Anchor, AnimationRouter, Overlay, OverlayOptions},
        },
        theme::{appearance::Appearance as ThemeAppearance, handle::ThemeHandle},
    };
    use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
    use smol_str::{SmolStr, format_smolstr};
    use tokio::runtime::Handle;
    use ui_core::{
        app::App,
        error::EngineError,
        math::Point,
        node::{
            builder::NodeBuilder,
            id::NodeId,
            signal::{Signal, SignalScope},
        },
        text::TextStyle,
    };

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

    /// What the test host's `open_url` was called with.
    type Opened = Rc<RefCell<Vec<String>>>;

    /// A headless engine app and a runtime; the services are composed through the same
    /// builder the application root uses, only without the two shell-bound collaborators
    /// (the directory and the Kytrix service). The three composition-root singletons come
    /// back too, so a test can assert the aggregate carries the instances the host built.
    fn compose() -> (
        App,
        WidgetServices,
        Opened,
        Rc<Layer>,
        Rc<FrameRouter>,
        Rc<AnimationRouter>,
    ) {
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
        let layer = Rc::new(Layer::new());
        let frame_router =
            Rc::new(FrameRouter::new(&mut app).expect("the fresh frame slot is free"));
        let animation_router = Rc::new(AnimationRouter::new());

        let opened: Opened = Rc::new(RefCell::new(Vec::new()));
        let record = Rc::clone(&opened);
        let services = WidgetServices::builder(
            &mut app,
            runtime.handle().clone(),
            translate,
            theme,
            Rc::clone(&layer),
            Rc::clone(&frame_router),
            Rc::clone(&animation_router),
        )
        .open_url(move |url| record.borrow_mut().push(url.to_owned()))
        .build();

        (app, services, opened, layer, frame_router, animation_router)
    }

    /// The services are built without any application-shell context: an engine application and a
    /// tokio runtime are the whole host, and every frozen slot is assembled.
    #[test]
    fn services_compose_without_a_gui_context() {
        let (app, services, opened, layer, frame_router, animation_router) = compose();

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
            Rc::ptr_eq(&services.layer, &layer),
            "the aggregate carries the composition root's one layer stack"
        );
        assert!(
            Rc::ptr_eq(&services.frame_router, &frame_router),
            "the aggregate carries the composition root's one frame router"
        );
        assert!(
            Rc::ptr_eq(&services.animation_router, &animation_router),
            "the aggregate carries the composition root's one animation router"
        );
        assert!(
            frame_router.installed(),
            "the composition root's router owns the application's frame hook"
        );
        assert!(
            !animation_router.installed(),
            "the completion callback is installed by the first overlay, not by composing"
        );

        (services.open_url)("https://example.com/market");
        assert_eq!(opened.borrow().as_slice(), ["https://example.com/market"]);
    }

    /// The builder is the assembly seam: the host names the two optional connectors, the four
    /// initial signal values and the application seams, and everything else is the documented
    /// default.
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
        let layer = Rc::new(Layer::new());
        let frame_router =
            Rc::new(FrameRouter::new(&mut app).expect("the fresh frame slot is free"));
        let animation_router = Rc::new(AnimationRouter::new());
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
            Rc::clone(&layer),
            Rc::clone(&frame_router),
            Rc::clone(&animation_router),
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

    /// Builds one closed overlay over the host's singletons, the way a widget's `new` builds the
    /// kit components it owns.
    fn build_overlay(harness: &mut WidgetTestApp, key: &str) -> Overlay {
        harness.with_cx(|cx| {
            let theme = cx.theme().clone();
            let layer = cx.layer().clone();
            let animation = cx.animation().clone();
            Overlay::new(
                cx.app_mut(),
                theme,
                layer,
                animation,
                key,
                OverlayOptions::default(),
            )
        })
    }

    /// Opens `overlay` at a fixed point, with one box of content in its panel.
    fn open_overlay(harness: &mut WidgetTestApp, overlay: &Overlay) -> bool {
        harness
            .with_cx(|cx| {
                let window = cx.window();
                let origin = Anchor::point(Point::new(40.0, 40.0));
                overlay.open(cx.app_mut(), window, origin, |tree, panel| {
                    NodeBuilder::boxed().build(tree, panel).map(|_| ())
                })
            })
            .expect("the overlay opens")
    }

    /// The layer stack is a composition-root singleton: an overlay a widget builds through its
    /// context registers in the host's one stack, so the framework can enforce the z-order.
    #[test]
    fn an_overlay_opened_through_the_widget_context_registers_in_the_layer_stack() {
        let mut harness = WidgetTestApp::new();
        let overlay = build_overlay(&mut harness, "test.menu");

        assert!(open_overlay(&mut harness, &overlay));
        let node = overlay.overlay_node().expect("the overlay is open");
        assert_eq!(
            harness.layer().len(harness.window()),
            1,
            "the overlay registered in the host's layer stack"
        );
        assert!(
            harness.layer().contains_node(harness.window(), node),
            "the host's stack knows the node the overlay built"
        );

        harness.with_cx(|cx| overlay.close_now(cx.app_mut()));
        assert!(
            harness.layer().is_empty(harness.window()),
            "closing the overlay unregistered it from the same stack"
        );
    }

    /// The completion callback is a composition-root singleton too: building an overlay through
    /// the context installs the host's router (a widget-built router would take the engine's
    /// single slot and the other overlays' fade-outs would never finish).
    #[test]
    fn the_animation_router_handed_to_a_widget_owns_the_completion_slot() {
        let mut harness = WidgetTestApp::new();
        assert!(
            !harness.animation().installed(),
            "composing the host installs nothing; the first kit component does"
        );

        let overlay = build_overlay(&mut harness, "test.tooltip");

        assert!(
            harness.animation().installed(),
            "the overlay installed the host's one animation router, not a private one"
        );
        assert!(open_overlay(&mut harness, &overlay));
        harness.with_cx(|cx| overlay.close_now(cx.app_mut()));
        assert_eq!(
            overlay.overlay_node(),
            None,
            "the overlay released its nodes"
        );
    }

    /// The widget context hands out the very instances the host created, and its layer queries
    /// answer for the overlays that were registered through it.
    #[test]
    fn the_widget_context_queries_the_layer_stack_it_was_handed() {
        let mut harness = WidgetTestApp::new();
        let host_layer = harness.layer() as *const Layer;
        let host_router = harness.router() as *const FrameRouter;
        let host_animation = harness.animation() as *const AnimationRouter;
        let menu = build_overlay(&mut harness, "test.menu");
        let tooltip = build_overlay(&mut harness, "test.tooltip");

        harness.with_cx(|cx| {
            assert_eq!(
                cx.layer() as *const Layer,
                host_layer,
                "the context exposes the host's stack, not a copy"
            );
            assert_eq!(
                cx.frame_router() as *const FrameRouter,
                host_router,
                "the context exposes the host's frame router"
            );
            assert_eq!(
                cx.animation() as *const AnimationRouter,
                host_animation,
                "the context exposes the host's animation router"
            );
        });

        assert!(open_overlay(&mut harness, &menu));
        assert!(open_overlay(&mut harness, &tooltip));
        let (bands, top) = harness.with_cx(|cx| {
            let bands: Vec<Band> = cx
                .layer()
                .entries(cx.window())
                .into_iter()
                .map(|entry| entry.band)
                .collect();
            (bands, cx.layer().top(cx.window()).map(|entry| entry.node()))
        });
        assert_eq!(
            bands,
            [Band::Popup, Band::Popup],
            "both overlays are in the popup band, listed bottom-up"
        );
        assert_eq!(
            top,
            tooltip.overlay_node(),
            "the layer query names the most recently registered overlay"
        );
    }

    /// The reduced-motion signal is application state like any other: writing it
    /// wakes the widgets that read it, and a widget observes the new value through
    /// the same signal handle.
    #[test]
    fn reduced_motion_is_a_signal_the_application_owns() {
        let (mut app, services, _opened, _layer, _frame_router, _animation_router) = compose();

        assert!(app.write(&services.reduce_motion, true));
        assert!(*app.read(&services.reduce_motion));

        assert!(
            !app.write(&services.reduce_motion, true),
            "writing the same value again is deduplicated"
        );
    }

    /// Switching the language is a write to the signal a widget reads plus the
    /// matching update of the translator it renders through: after both, the signal
    /// reports the new language and the next render draws the new text. The two are
    /// separate pieces of state, so the composition root is what keeps them
    /// together — this pins the documented order (translator first, then the signal).
    #[test]
    fn switching_the_language_updates_the_signal_and_the_translator_together() {
        let (mut app, services, _opened, _layer, _frame_router, _animation_router) = compose();

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

    /// A setting the widget must follow reaches it through the settings signal; the
    /// borrowed view of the snapshot reads the same value without cloning it.
    #[test]
    fn a_settings_change_reaches_the_widget_through_the_signal() {
        let (mut app, services, _opened, _layer, _frame_router, _animation_router) = compose();
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

    /// The signal handles are minted in one scope so the host can release the whole
    /// aggregate's state at once.
    #[test]
    fn the_services_signals_live_in_one_scope() {
        let (mut app, services, _opened, _layer, _frame_router, _animation_router) = compose();
        let scope: SignalScope = app.signal_scope();
        let signal = app.signal_scoped(scope, 1_u8);

        let released = app.release_scope(scope);

        assert!(released > 0);
        assert_eq!(*app.read(&services.language), LanguageCode::En);
        let _: Signal<u8> = signal;
    }

    /// `Handle` is the runtime half of the aggregate: a widget spawns its streams on
    /// it rather than asking the process for one.
    #[test]
    fn the_services_carry_the_runtime_the_widget_spawns_on() {
        let (_app, services, _opened, _layer, _frame_router, _animation_router) = compose();
        let handle: &Handle = &services.runtime;
        assert!(handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread);
    }

    /// A consumer in the shape `force_order` has: it waits for the directory, reloads
    /// when a generation is published, and only then grows a body. The body reads the
    /// reload counter rather than the generation, so the frame scheduled by the write
    /// has to come from the framework's own dependency.
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
            NodeBuilder::text(SmolStr::default(), TextStyle::new(13.0))
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

    /// The directory reaches a widget through the generation signal. A widget still
    /// waiting for the directory has no body, so nothing it built can register the
    /// dependency: the surface root carries it, the write schedules the frame that
    /// runs `sync`, and the consumer reloads and grows its body.
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

    /// A generation that did not move is not a write, so it schedules no frame: the
    /// directory costs a frame only when it actually published (I3).
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

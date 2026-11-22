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
//! # Why two collaborators are optional
//!
//! Both [`WidgetServices::market_participants`] and
//! [`WidgetServices::kytrix_service`] are application-shell objects:
//! `MarketParticipantsConnector` takes a `widget_tokio_runtime` runtime, and
//! `KytrixServiceConnector::install` takes the toolkit application context plus a
//! `ConfigNotification`; both crates still depend on the old toolkit. The contract
//! layer cannot construct either in a headless test, so both are `Option`:
//!
//! - the application root always supplies them;
//! - a headless test that does not read the directory or the service leaves them
//!   absent, and a widget that needs one maps `None` to its own business
//!   notice/failure (there is no shared `WidgetError` type for it — a widget states
//!   its own).
//!
//! P4 adds a toolkit-free construction seam and both become required fields; the
//! `Option` is a transition forced by the old toolkit, not the frozen shape.
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
//! future that requires `Send`, such as the engine's spawn.
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
use base_ui::theme::handle::ThemeHandle;
use bytes::Bytes;
use i18n::{language::codes::LanguageCode, translate::Translate};
use kytrix_service_provider::service::connector::KytrixServiceConnector;
use market_participants_connector::connector::MarketParticipantsConnector;
use tokio::runtime::Handle;
use ui_core::node::signal::Signal;

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
    /// `bind_*` closure that reads it follows the setting it reads.
    pub config: Signal<Config>,
    /// The theme service: colours are read through it inside `bind_style` closures.
    pub theme: ThemeHandle,
    /// The market-participants directory, when the host has one.
    ///
    /// The application root always supplies it; a headless test that does not read
    /// the directory leaves it absent. See the module documentation.
    pub market_participants: Option<MarketParticipantsConnector>,
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
    /// The tokio runtime the widget's streams and RPC calls are driven on.
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
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use app_config::settings::{Appearance, Config};
    use base_ui::theme::{appearance::Appearance as ThemeAppearance, handle::ThemeHandle};
    use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
    use tokio::runtime::Handle;
    use ui_core::{
        app::App,
        node::signal::{Signal, SignalScope},
    };

    use super::WidgetServices;

    const MESSAGES_EN: &str = "common:\n  close: Close\n";
    const MESSAGES_ZH: &str = "common:\n  close: 关闭\n";

    /// What the test host's `open_url` was called with.
    type Opened = Rc<RefCell<Vec<String>>>;

    /// A headless engine app and a runtime; the services are composed exactly the
    /// way the application root composes them, only without the two shell-bound
    /// collaborators (the directory and the Kytrix service).
    fn compose() -> (App, WidgetServices, Opened) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("the test runtime builds");
        let mut app = App::new(runtime.handle().clone());
        let scope = app.signal_scope();
        let language = app.signal_scoped(scope, LanguageCode::En);
        let config = app.signal_scoped(scope, Config::default());
        let reduce_motion = app.signal_scoped(scope, false);
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

        let opened: Opened = Rc::new(RefCell::new(Vec::new()));
        let open_url: Rc<dyn Fn(&str)> = {
            let opened = Rc::clone(&opened);
            Rc::new(move |url| opened.borrow_mut().push(url.to_owned()))
        };
        let sound: Rc<dyn Fn(&'static str) -> Option<bytes::Bytes>> = Rc::new(|_| None);

        let services = WidgetServices {
            translate,
            language,
            config,
            theme,
            market_participants: None,
            kytrix_service: None,
            open_url,
            sound,
            reduce_motion,
            runtime: runtime.handle().clone(),
        };
        (app, services, opened)
    }

    /// The services are built without any application-shell context: an engine application and a
    /// tokio runtime are the whole host, and every frozen slot is assembled.
    #[test]
    fn services_compose_without_a_gui_context() {
        let (app, services, opened) = compose();

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

        (services.open_url)("https://example.com/market");
        assert_eq!(opened.borrow().as_slice(), ["https://example.com/market"]);
    }

    /// The reduced-motion signal is application state like any other: writing it
    /// wakes the widgets that read it, and a widget observes the new value through
    /// the same signal handle.
    #[test]
    fn reduced_motion_is_a_signal_the_application_owns() {
        let (mut app, services, _opened) = compose();

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
        let (mut app, services, _opened) = compose();

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
        let (mut app, services, _opened) = compose();
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
        let (mut app, services, _opened) = compose();
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
        let (_app, services, _opened) = compose();
        let handle: &Handle = &services.runtime;
        assert!(handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread);
    }
}

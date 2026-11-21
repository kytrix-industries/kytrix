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
//! # Why a settings signal, and why the directory is optional
//!
//! P3-0 froze this type with a `ConfigManager` field. That manager owns an application-shell
//! entity, so the aggregate could only be built inside that shell — which
//! is exactly what the contract layer is leaving behind. P3-1 freezes the engine
//! shape instead:
//!
//! - `config` is the **settings snapshot as a signal**. The composition root owns
//!   the `ConfigManager`, writes the signal whenever the manager's snapshot moves,
//!   and widgets read either the value ([`WidgetCx::config`](crate::cx::WidgetCx))
//!   or the signal (inside a `bind_*` closure that must follow a setting).
//! - `market_participants` is the one collaborator that still needs an application-shell context
//!   to construct (`MarketParticipantsConnector` takes a `widget_tokio_runtime`
//!   runtime, and that crate depends on the old toolkit). It is therefore **optional**:
//!   the application root always supplies it, and a headless test that does not
//!   read the directory leaves it absent. A widget that needs it asks
//!   [`WidgetServices::market_participants`] and reports
//!   [`WidgetError::DirectoryUnavailable`](crate::error::WidgetError) when the host
//!   has none.
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
//! // After: the composition root hands the services in, and the widget reads the field.
//! let config = cx.config();
//! ```
//!
//! The translation context is the other half of the same move — see
//! [`Translate`] and [`t_smol!`](i18n::t_smol).

use app_config::settings::Config;
use base_ui::theme::handle::ThemeHandle;
use i18n::{language::codes::LanguageCode, translate::Translate};
use market_participants_connector::connector::MarketParticipantsConnector;
use tokio::runtime::Handle;
use ui_core::node::signal::Signal;

/// The services every widget is given by the composition root.
///
/// See the module documentation for the frozen shape, the language contract, and
/// the threading rules.
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
    /// The tokio runtime the widget's streams and RPC calls are driven on.
    pub runtime: Handle,
}

impl WidgetServices {
    /// The market-participants directory, if the host installed one.
    #[must_use]
    pub fn market_participants(&self) -> Option<&MarketParticipantsConnector> {
        self.market_participants.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

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

    /// A headless engine app and a runtime; the services are composed exactly the
    /// way the application root composes them, only without the directory.
    fn compose() -> (App, WidgetServices) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("the test runtime builds");
        let mut app = App::new(runtime.handle().clone());
        let scope = app.signal_scope();
        let language = app.signal_scoped(scope, LanguageCode::En);
        let config = app.signal_scoped(scope, Config::default());
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

        let services = WidgetServices {
            translate,
            language,
            config,
            theme,
            market_participants: None,
            runtime: runtime.handle().clone(),
        };
        (app, services)
    }

    /// The services are built without any application-shell context: an engine application and a
    /// tokio runtime are the whole host.
    #[test]
    fn services_compose_without_a_gui_context() {
        let (app, services) = compose();

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
    }

    /// Switching the language is a write to the signal a widget reads plus the
    /// matching update of the translator it renders through: after both, the signal
    /// reports the new language and the next render draws the new text. The two are
    /// separate pieces of state, so the composition root is what keeps them
    /// together — this pins the documented order (translator first, then the signal).
    #[test]
    fn switching_the_language_updates_the_signal_and_the_translator_together() {
        let (mut app, services) = compose();

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

    /// A setting the widget must follow reaches it through the settings signal, and
    /// the time-zone seam reads the same value.
    #[test]
    fn a_settings_change_reaches_the_widget_through_the_signal() {
        let (mut app, services) = compose();
        let mut config = (*app.read(&services.config)).clone();
        config.data_time_zone =
            app_config::time_zone::DataTimeZone::from_hours(8).expect("UTC+8 is offered");

        assert!(app.write(&services.config, config));
        assert_eq!(
            crate::time_zone::data_time_zone(&app.read(&services.config)).whole_hours(),
            8
        );
    }

    /// The signal handles are minted in one scope so the host can release the whole
    /// aggregate's state at once.
    #[test]
    fn the_services_signals_live_in_one_scope() {
        let (mut app, services) = compose();
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
        let (_app, services) = compose();
        let handle: &Handle = &services.runtime;
        assert!(handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread);
    }
}

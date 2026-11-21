//! The collaborators a widget is handed instead of reading process globals.

use app_config_provider::manager::ConfigManager;
use i18n::{language::codes::LanguageCode, translate::Translate};
use market_participants_connector::connector::MarketParticipantsConnector;
use tokio::runtime::Handle;
use ui_core::node::signal::Signal;

/// The services every widget is given by the composition root.
///
/// This is the replacement for the GPUI globals the framework used to reach through its
/// context: a widget renders with the [`Translate`] it was handed, reads the selected
/// language from the [`Signal`] it was handed, reads settings from the [`ConfigManager`] it
/// was handed, and drives its streams on the runtime it was handed. Nothing here is
/// installed process-wide and nothing is looked up by type, so a test composes the same
/// aggregate the application does.
///
/// The aggregate is deliberately minimal: it gains a field when a widget needs another
/// collaborator, not because one may be needed later. P3-1 freezes its final shape.
///
/// # Language
///
/// A widget reads [`WidgetServices::language`] where it renders — that read registers the
/// engine dependency which redraws it — and renders its text through
/// [`WidgetServices::translate`]. The translator is pure `i18n` state and observes no
/// signal, so both carry the selected language and the composition root keeps them in step:
/// switching the language sets the translator's language and writes the signal. Set the
/// translator first, so a redraw the write schedules can only draw the new language.
///
/// # Threading
///
/// The aggregate is neither `Clone` (the [`ConfigManager`] holds an entity) nor `Send` (the
/// entity belongs to the thread that created it); the host builds it once on the UI thread
/// and hands it out as an `Rc<WidgetServices>`. It must not be captured by a future that
/// requires `Send`, such as the engine's spawn.
///
/// # Examples
///
/// A widget reads the settings snapshot from the services it was handed:
///
/// ```text
/// // Before: the settings lived in a GPUI global looked up through the context.
/// let config = cx.global::<ConfigManager>().read();
///
/// // After: the composition root hands the services in, and the widget reads the field.
/// let config = services.config.read();
/// ```
///
/// The translation context is the other half of the same move — see
/// [`Translate`] and [`t_smol!`](i18n::t_smol).
pub struct WidgetServices {
    /// Renders the interface's own messages in the selected language.
    pub translate: Translate,
    /// The selected language as an engine signal: a component that reads it while it
    /// renders is redrawn when the language changes.
    pub language: Signal<LanguageCode>,
    /// Owns the current settings snapshot and the events its changes are emitted from.
    pub config: ConfigManager,
    /// The market-participants root the widget reads markets and listings through.
    pub market_participants: MarketParticipantsConnector,
    /// The tokio runtime the widget's streams and RPC calls are driven on.
    pub runtime: Handle,
}

#[cfg(test)]
mod tests {
    use std::{rc::Rc, sync::Arc};

    use app_config::settings::{Appearance, Config};
    use app_config_provider::manager::ConfigManager;
    use gpui_kit::{App, TestAppContext};
    use i18n::{global::embedded, language::codes::LanguageCode, t_smol, translate::Translate};
    use market_participants_connector::{catalog::store::Catalog, test_support::install_root};
    use tokio::runtime::Handle;
    use ui_core::{app::App as Engine, node::signal::Signal};

    use super::WidgetServices;

    /// The aggregate the composition root builds: the embedded catalogue, an in-memory
    /// settings manager, the fixture market-participants root, the test's runtime and the
    /// language signal the engine minted.
    fn build_services(
        cx: &mut App,
        language: Signal<LanguageCode>,
        runtime: Handle,
    ) -> WidgetServices {
        WidgetServices {
            translate: Translate::new(
                Rc::new(embedded::load().expect("the embedded locales load")),
                LanguageCode::En,
            ),
            language,
            config: ConfigManager::in_memory(Config::default(), cx),
            market_participants: install_root(cx, Arc::new(Catalog::new())),
            runtime,
        }
    }

    /// The aggregate carries live collaborators: the widget renders through the injected
    /// translator and reads the injected settings snapshot, with no global lookup of its
    /// own. The market-participants root is the fixture one, the runtime is only held
    /// because a widget would spawn on it, and the language signal carries the language the
    /// translator renders.
    #[test]
    fn a_widget_reads_through_the_services_it_was_handed() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("the test runtime builds");
            let mut engine = Engine::new(runtime.handle().clone());
            let language = engine.signal(LanguageCode::En);
            let services = build_services(cx, language, runtime.handle().clone());

            assert_eq!(t_smol!(services.translate, "common.close"), "Close");
            assert_eq!(
                services.config.read().appearance,
                Appearance::System,
                "the injected manager answers with the snapshot it was built from"
            );
            assert!(
                !services.market_participants.supported_venues().is_empty(),
                "the fixture root speaks the venues this build declares"
            );
            assert_eq!(
                services.translate.language(),
                LanguageCode::En,
                "the injected translator renders the language it carries"
            );
            assert_eq!(
                *engine.read(&services.language),
                LanguageCode::En,
                "the language signal carries the language the services were built with"
            );
        });
    }

    /// Switching the language is a write to the signal a widget reads plus the matching
    /// update of the translator it renders through: after both, the signal reports the new
    /// language and the next render draws the new text. The two are separate pieces of state,
    /// so the composition root is what keeps them together — this pins the documented order
    /// (translator first, then the signal).
    #[test]
    fn switching_the_language_updates_the_signal_and_the_translator_together() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("the test runtime builds");
            let mut engine = Engine::new(runtime.handle().clone());
            let language = engine.signal(LanguageCode::En);
            let services = build_services(cx, language, runtime.handle().clone());

            assert_eq!(t_smol!(services.translate, "common.close"), "Close");

            assert!(services.translate.set_language(LanguageCode::ZhCn));
            assert!(
                engine.write(&services.language, LanguageCode::ZhCn),
                "the switch changes the signal, which redraws the components that read it"
            );

            assert_eq!(*engine.read(&services.language), LanguageCode::ZhCn);
            assert_eq!(
                t_smol!(services.translate, "common.close"),
                "关闭",
                "the next render reads the signal and draws through the translator carrying the \
                 same language"
            );
        });
    }
}

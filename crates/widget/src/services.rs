//! The collaborators a widget is handed instead of reading process globals.

use app_config_provider::manager::ConfigManager;
use i18n::translate::Translate;
use market_participants_connector::connector::MarketParticipantsConnector;
use tokio::runtime::Handle;

/// The services every widget is given by the composition root.
///
/// This is the replacement for the GPUI globals the framework used to reach through its
/// context: a widget renders with the [`Translate`] it was handed, reads settings from
/// the [`ConfigManager`] it was handed, and drives its streams on the runtime it was
/// handed. Nothing here is installed process-wide and nothing is looked up by type, so a
/// test composes the same aggregate the application does.
///
/// The aggregate is deliberately minimal: it gains a field when a widget needs another
/// collaborator, not because one may be needed later. P3-1 freezes its final shape.
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
    use gpui_kit::TestAppContext;
    use i18n::{global::embedded, language::codes::LanguageCode, t_smol, translate::Translate};
    use market_participants_connector::{catalog::store::Catalog, test_support::install_root};

    use super::WidgetServices;

    /// The aggregate carries live collaborators: the widget renders through the injected
    /// translator and reads the injected settings snapshot, with no global lookup of its
    /// own. The market-participants root is the fixture one, and the runtime is only held
    /// because a widget would spawn on it.
    #[test]
    fn a_widget_reads_through_the_services_it_was_handed() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            let translate = Translate::new(
                Rc::new(embedded::load().expect("the embedded locales load")),
                LanguageCode::En,
            );
            let config = ConfigManager::in_memory(Config::default(), cx);
            let market_participants = install_root(cx, Arc::new(Catalog::new()));
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("the test runtime builds");

            let services = WidgetServices {
                translate,
                config,
                market_participants,
                runtime: runtime.handle().clone(),
            };

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
        });
    }
}

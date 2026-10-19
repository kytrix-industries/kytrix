//! The process-wide projection of the configured language.

use app_config::settings::Language;
use app_config_provider::{events::LanguageChanged, manager::ConfigManager};
use gpui_kit::{App, Global, Subscription};
use i18n::language::codes::LanguageCode;

/// Holds the subscription that keeps `LanguageCode` in step with the settings.
struct LanguageProjection {
    _subscription: Subscription,
}

impl Global for LanguageProjection {}

/// Applies the persisted language once, before the first window is built.
pub fn apply_startup(cx: &mut App) {
    let language = cx.global::<ConfigManager>().read().language;

    cx.set_global(language_code(language));
}

/// Subscribes the language global to `LanguageChanged`.
pub fn install(cx: &mut App) {
    let events = cx.global::<ConfigManager>().events().clone();
    let subscription = cx.subscribe(&events, |_, event: &LanguageChanged, cx| {
        cx.set_global(language_code(event.0));
        cx.refresh_windows();
    });
    cx.set_global(LanguageProjection {
        _subscription: subscription,
    });
}

pub(crate) const fn language_code(language: Language) -> LanguageCode {
    match language {
        Language::ZhCn => LanguageCode::ZhCn,
        Language::En => LanguageCode::En,
    }
}

#[cfg(test)]
mod tests {
    use app_config::settings::Config;
    use gpui_kit::{BorrowAppContext as _, TestAppContext};

    use super::*;

    /// A language write reaches the global the translator reads, without the
    /// writing side touching it.
    ///
    /// The catalogue installs `En`, and the global is then reset to `ZhCn` so the
    /// startup assertion cannot pass on the value the catalogue left behind. The
    /// persisted setting is `En`, so only a startup projection that reads it
    /// installs `En`; the write afterwards then moves the global to `ZhCn`, which
    /// only the subscription does.
    #[gpui_kit::test]
    fn a_language_write_should_update_the_language_global(cx: &mut TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            cx.set_global(LanguageCode::ZhCn);

            let mut config = Config::default();
            config.language = Language::En;
            let manager = ConfigManager::in_memory(config, cx);
            cx.set_global(manager);
            apply_startup(cx);
            install(cx);
        });

        cx.update(|cx| {
            assert_eq!(
                *cx.global::<LanguageCode>(),
                LanguageCode::En,
                "the startup projection should install the persisted language"
            );
        });

        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(|config| config.language = Language::ZhCn, cx);
            });
        });

        cx.update(|cx| {
            assert_eq!(
                *cx.global::<LanguageCode>(),
                LanguageCode::ZhCn,
                "the subscription should follow the written language"
            );
        });
    }
}

//! The top bar's update button.

use base_ui::{
    button::Button, interactive::InteractiveVariantExt as _, size::UnifiedSizable as _,
    tooltip::TooltipExt as _,
};
use gpui_kit::{
    App, Context, Empty, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Window,
};
use i18n::t;

use crate::{dialog::UpdateDialog, global, state::Updater};

/// The element id and debug selector the button is drawn with, which the visual
/// tests look it up by.
const UPDATER_INDICATOR: &str = "UPDATER_INDICATOR";

/// The top bar's update button.
///
/// It is drawn while an update is on offer and at no other time, which is the
/// whole of what this feature says until the user asks for more: there is no
/// "nothing available" state to open and no failed check to report, because an
/// update nobody asked for is not something to interrupt anyone with.
pub struct UpdateIndicator {
    updater: Entity<Updater>,
}

impl UpdateIndicator {
    /// The indicator for the updater the composition root installed.
    ///
    /// # Panics
    ///
    /// Panics when the updater was not installed, which is the contract the
    /// application runtime already has: the composition root installs both before
    /// the first window is built.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            updater: global::entity(cx)
                .expect("the updater must be installed before the window is built"),
        }
    }

    /// Opens the dialog the offered update is read about and installed from.
    fn open(&mut self, window: &mut Window, cx: &mut App) {
        UpdateDialog::show(&self.updater, window, cx);
    }
}

impl Render for UpdateIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(release) = self.updater.read(cx).state().offered() else {
            return Empty.into_any_element();
        };
        let label = t!(cx, "window.updater.available");
        let tooltip = t!(
            cx,
            "window.updater.available_tooltip",
            version = release.version
        );

        // The header row is the window's drag area, so the button takes the
        // pointer explicitly: without it the press starts a window drag instead.
        Button::new(UPDATER_INDICATOR)
            .ghost()
            .medium()
            .occlude()
            .debug_selector(|| UPDATER_INDICATOR.into())
            .aria_label(label.clone())
            .with_tooltip(tooltip)
            .on_click(cx.listener(|this, _, window, cx| this.open(window, cx)))
            .child(label)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use app_config::network::Config;
    use base_ui::{animation::ANIMATION_TICK, layer::Layer};
    use gpui_kit::{
        AppContext as _, Context, IntoElement, Modifiers, Render, Styled as _, TestAppContext,
        VisualTestContext, Window, div,
    };
    use network::transport::hub::{TransportHub, TransportPolicy};
    use tokio::sync::watch;
    use widget_tokio_runtime::runtime::Runtime;

    use super::*;
    use crate::{
        global::{self, UpdaterDeps},
        release::{PlatformKey, PlatformRelease, ReleaseFormat, ReleaseInfo},
        state::UpdaterState,
    };

    /// The notes the dialog draws, which `dialog.rs` names for its own tests:
    /// finding them is how these tests see that the dialog opened.
    const UPDATER_DIALOG_NOTES: &str = "UPDATER_DIALOG_NOTES";

    /// A window that draws the indicator and nothing else.
    struct Harness {
        indicator: Entity<UpdateIndicator>,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.indicator.clone())
        }
    }

    fn release() -> ReleaseInfo {
        ReleaseInfo {
            key: PlatformKey::current().expect("the tests run on a packaged platform"),
            version: semver::Version::new(1, 2, 3),
            notes: None,
            pub_date: None,
            asset: PlatformRelease {
                url: url::Url::parse("https://example.invalid/kytrix-setup.exe").unwrap(),
                signature: smol_str::SmolStr::new_static("signature"),
                format: ReleaseFormat::Nsis,
            },
        }
    }

    /// The updater the composition root would have installed, without a network.
    fn installed(cx: &mut TestAppContext) -> Entity<Updater> {
        cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("the embedded locales should load");
            let (_sender, config) = watch::channel(Arc::new(Config::default()));
            global::install(
                cx,
                UpdaterDeps {
                    transport: Arc::new(TransportHub::new(TransportPolicy::default())),
                    config,
                    runtime: Runtime::for_test(),
                },
            );

            global::entity(cx).expect("the updater that was just installed is the one found")
        })
    }

    /// A window whose root is the layer an application mounts, which is what the
    /// dialog the indicator opens is opened on.
    fn window(cx: &mut TestAppContext) -> &mut VisualTestContext {
        let (_, cx) = cx.add_window_view(|_, cx| {
            Layer::new(cx.new(|cx| Harness {
                indicator: cx.new(UpdateIndicator::new),
            }))
        });
        cx.run_until_parked();

        cx
    }

    /// Lets the dialog's own transition finish and repaints, so that one just
    /// opened is past its first frames.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn the_indicator_should_be_drawn_only_while_an_update_is_offered(cx: &mut TestAppContext) {
        let updater = installed(cx);
        let cx = window(cx);

        assert!(
            cx.debug_bounds(UPDATER_INDICATOR).is_none(),
            "nothing is offered, so the top bar has no button"
        );

        cx.update(|_, cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(UpdaterState::Available(release()), cx);
            });
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(UPDATER_INDICATOR).is_some(),
            "an offered update is what the button is for"
        );
    }

    #[gpui_kit::test]
    fn the_button_should_stay_while_the_last_attempt_failed(cx: &mut TestAppContext) {
        let updater = installed(cx);
        let cx = window(cx);

        cx.update(|_, cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(
                    UpdaterState::failed_for_test(
                        release(),
                        crate::error::UpdaterError::UnsupportedPlatform,
                    ),
                    cx,
                );
            });
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(UPDATER_INDICATOR).is_some(),
            "a failure to download is not a release that stopped existing"
        );
    }

    /// The press that opens the dialog is the indicator's own: it hands the offered
    /// update to the layer the window draws its dialogs on.
    #[gpui_kit::test]
    fn pressing_the_indicator_should_open_the_dialog_for_the_offered_update(
        cx: &mut TestAppContext,
    ) {
        let updater = installed(cx);
        let cx = window(cx);
        cx.update(|_, cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(UpdaterState::Available(release()), cx);
            });
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let target = cx
            .debug_bounds(UPDATER_INDICATOR)
            .expect("an offered update is drawn as the button that opens it")
            .center();
        cx.simulate_click(target, Modifiers::none());
        settle(cx);

        assert!(
            cx.debug_bounds(UPDATER_DIALOG_NOTES).is_some(),
            "pressing the button opens the dialog the update is read about from"
        );
    }
}

//! The dialog an offered update is read about, downloaded through and installed from.
//!
//! One entity draws all of it: the notes before anything is downloaded, the
//! progress while it is, and the invitation to install once it has downloaded. Which
//! of them is drawn is read from the updater, which owns that state — the dialog
//! keeps no copy, it follows the entity that does. Closing is the dialog's own
//! [`DismissEvent`], which is what the layer it was opened on subscribes to.

use base_ui::{
    button::Button,
    interactive::InteractiveVariantExt as _,
    layer::LayerExt as _,
    markdown::view::{Markdown, MarkdownState},
    progress::Progress,
    separator::Separator,
    size::UnifiedSizable as _,
    spinner::Spinner,
    style::theme::ThemeExt as _,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, DismissEvent, Empty, Entity, EventEmitter,
    FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use i18n::{language::codes::LanguageCode, t};
use smol_str::SmolStr;

use crate::{
    install, publish,
    state::{Updater, UpdaterState},
};

/// How wide the panel is.
const DIALOG_WIDTH: f32 = 520.0;

/// How tall the release notes are drawn before they scroll.
const NOTES_MAX_HEIGHT: f32 = 320.0;

/// The debug selectors the dialog's parts are drawn with, for the visual tests.
const UPDATER_DIALOG_NOTES: &str = "UPDATER_DIALOG_NOTES";
const UPDATER_DIALOG_DOWNLOAD: &str = "UPDATER_DIALOG_DOWNLOAD";
const UPDATER_DIALOG_PROGRESS: &str = "UPDATER_DIALOG_PROGRESS";
const UPDATER_DIALOG_DOWNLOADED: &str = "UPDATER_DIALOG_DOWNLOADED";
const UPDATER_DIALOG_INSTALL: &str = "UPDATER_DIALOG_INSTALL";
const UPDATER_DIALOG_RELEASE_PAGE: &str = "UPDATER_DIALOG_RELEASE_PAGE";
const UPDATER_DIALOG_FAILURE: &str = "UPDATER_DIALOG_FAILURE";
const UPDATER_DIALOG_CLOSE: &str = "UPDATER_DIALOG_CLOSE";
const UPDATER_DIALOG_CANCEL: &str = "UPDATER_DIALOG_CANCEL";

/// The dialog the update indicator opens.
pub(crate) struct UpdateDialog {
    updater: Entity<Updater>,
    /// The release notes, parsed once: one release is offered at a time, so the
    /// text does not change for as long as the dialog lives.
    notes: Entity<MarkdownState>,
}

impl EventEmitter<DismissEvent> for UpdateDialog {}

impl UpdateDialog {
    /// Opens the dialog over the window, for the update the updater is offering.
    pub(crate) fn show(
        updater: &Entity<Updater>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let language = *cx.global::<LanguageCode>();
        let notes = updater
            .read(cx)
            .state()
            .release()
            .and_then(|release| release.notes.as_ref())
            .and_then(|notes| notes.in_language(language))
            .cloned();
        let dialog = cx.new(|cx| Self::new(updater.clone(), notes, cx));
        window.dialog(cx, dialog.clone());

        dialog
    }

    fn new(updater: Entity<Updater>, notes: Option<SmolStr>, cx: &mut Context<Self>) -> Self {
        // Progress and outcome both arrive as the updater's state moving, so the
        // dialog follows the entity rather than asking it again on a timer.
        cx.observe(&updater, |_, _, cx| cx.notify()).detach();

        Self {
            updater,
            notes: cx.new(|cx| MarkdownState::new(notes.as_deref().unwrap_or_default(), cx)),
        }
    }

    /// What the state has to say below the title.
    fn body(&self, state: &UpdaterState, cx: &Context<Self>) -> AnyElement {
        match state {
            UpdaterState::Downloading(_) => download_body(state, cx),
            UpdaterState::ReadyToInstall(_) | UpdaterState::Installing(_) => downloaded_body(cx),
            UpdaterState::Failed(_) => self.failure_body(state, cx),
            UpdaterState::Available(_) | UpdaterState::Idle | UpdaterState::Checking => {
                self.notes_body(cx)
            }
        }
    }

    /// The controls the state offers, the primary one last.
    fn actions(&self, state: &UpdaterState, cx: &Context<Self>) -> Vec<AnyElement> {
        match state {
            // Nothing on this platform can replace the running application, so
            // nothing is downloaded for it: the release page is where its update is.
            UpdaterState::Available(_) if !install::supported() => vec![
                self.closing_button(
                    "UpdaterDialogClose",
                    UPDATER_DIALOG_CLOSE,
                    t!(cx, "window.updater.cancel"),
                    None,
                    cx,
                ),
                release_page_button(cx),
            ],
            UpdaterState::Available(_) => vec![
                self.closing_button(
                    "UpdaterDialogCancel",
                    UPDATER_DIALOG_CANCEL,
                    t!(cx, "window.updater.cancel"),
                    None,
                    cx,
                ),
                self.download_button(cx),
            ],
            // Cancelling a download stops it where it is: the bytes it had are
            // discarded, and the release is on offer again.
            UpdaterState::Downloading(_) => vec![self.closing_button(
                "UpdaterDialogCancelDownload",
                UPDATER_DIALOG_CANCEL,
                t!(cx, "window.updater.cancel"),
                Some(Updater::cancel_download),
                cx,
            )],
            UpdaterState::ReadyToInstall(_) => vec![
                self.closing_button(
                    "UpdaterDialogDiscard",
                    UPDATER_DIALOG_CANCEL,
                    t!(cx, "window.updater.cancel"),
                    Some(Updater::discard_download),
                    cx,
                ),
                self.install_button(cx),
            ],
            // The application is being replaced underneath the dialog: there is
            // nothing left to press.
            UpdaterState::Installing(_) => Vec::new(),
            UpdaterState::Failed(_) => vec![self.closing_button(
                "UpdaterDialogDismiss",
                UPDATER_DIALOG_CLOSE,
                t!(cx, "common.close"),
                Some(Updater::dismiss_failure),
                cx,
            )],
            UpdaterState::Idle | UpdaterState::Checking => Vec::new(),
        }
    }

    /// The release notes, and what a platform that cannot install in place is told.
    fn notes_body(&self, cx: &Context<Self>) -> AnyElement {
        let mut body = div().flex().flex_col().gap_2().child(
            div()
                .id("UpdaterDialogNotes")
                .debug_selector(|| UPDATER_DIALOG_NOTES.into())
                .max_h(px(NOTES_MAX_HEIGHT))
                .overflow_y_scroll()
                .child(Markdown::new(&self.notes)),
        );
        if !install::supported() {
            body = body.child(
                div()
                    .text_xs()
                    .child(t!(cx, "window.updater.manual_required")),
            );
        }

        body.into_any_element()
    }

    /// What the last attempt failed with.
    fn failure_body(&self, state: &UpdaterState, cx: &Context<Self>) -> AnyElement {
        let Some(error) = state.failure() else {
            return Empty.into_any_element();
        };
        let color = cx.theme().roles.status.danger.base.hsla;

        div()
            .debug_selector(|| UPDATER_DIALOG_FAILURE.into())
            .text_color(color)
            .child(t!(cx, error.message_key()))
            .into_any_element()
    }

    /// The control that starts the download.
    fn download_button(&self, cx: &Context<Self>) -> AnyElement {
        let label = t!(cx, "window.updater.download");

        Button::new("UpdaterDialogDownload")
            .filled()
            .medium()
            .debug_selector(|| UPDATER_DIALOG_DOWNLOAD.into())
            .aria_label(label.clone())
            .on_click(cx.listener(|this, _, _, cx| {
                let updater = this.updater.clone();
                updater.update(cx, |updater, cx| updater.download(cx));
            }))
            .child(label)
            .into_any_element()
    }

    /// The control that hands the downloaded payload to the installer.
    fn install_button(&self, cx: &Context<Self>) -> AnyElement {
        let label = t!(cx, "window.updater.install");

        Button::new("UpdaterDialogInstall")
            .filled()
            .medium()
            .debug_selector(|| UPDATER_DIALOG_INSTALL.into())
            .aria_label(label.clone())
            .on_click(cx.listener(|this, _, _, cx| {
                let updater = this.updater.clone();
                updater.update(cx, |updater, cx| updater.install(cx));
            }))
            .child(label)
            .into_any_element()
    }

    /// A control that closes the dialog, having asked the updater for whatever
    /// closing means in the state it is drawn in.
    fn closing_button(
        &self,
        id: &'static str,
        selector: &'static str,
        label: SharedString,
        asked: Option<fn(&mut Updater, &mut Context<Updater>)>,
        cx: &Context<Self>,
    ) -> AnyElement {
        Button::new(id)
            .tonal()
            .medium()
            .debug_selector(move || selector.into())
            .aria_label(label.clone())
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(asked) = asked {
                    let updater = this.updater.clone();
                    updater.update(cx, asked);
                }
                cx.emit(DismissEvent);
            }))
            .child(label)
            .into_any_element()
    }
}

impl Render for UpdateDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.updater.read(cx).state();
        let Some(release) = state.release() else {
            return Empty.into_any_element();
        };
        let title = t!(cx, "window.updater.dialog_title", version = release.version);
        let body = self.body(state, cx);
        let actions = self.actions(state, cx);

        div()
            .w(px(DIALOG_WIDTH))
            .flex()
            .flex_col()
            .gap_3()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(body)
            .child(Separator::horizontal())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .children(actions),
            )
            .into_any_element()
    }
}

/// The progress of the download in flight.
///
/// A response that named its length is drawn as a bar against it. One that did
/// not — a chunked answer — has no total to measure against, so what is drawn is
/// the fact that something is happening rather than a fraction of it.
fn download_body(state: &UpdaterState, cx: &Context<UpdateDialog>) -> AnyElement {
    let heading = t!(cx, "window.updater.downloading");
    let Some((downloaded, total)) = state.progress() else {
        return div().child(heading).into_any_element();
    };

    let progress = match total.filter(|total| *total > 0) {
        Some(total) => {
            let percent = (downloaded as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32;
            div()
                .debug_selector(|| UPDATER_DIALOG_PROGRESS.into())
                .flex()
                .flex_col()
                .gap_2()
                .child(Progress::new("UpdaterDownloadProgress").value(percent))
                .child(div().text_xs().child(t!(
                    cx,
                    "window.updater.download_progress",
                    percent = percent as u32
                )))
                .into_any_element()
        }
        None => div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .debug_selector(|| UPDATER_DIALOG_PROGRESS.into())
                    .flex_none()
                    .size_4()
                    .child(Spinner::new("UpdaterDownloadSpinner")),
            )
            .child(div().text_xs().child(heading.clone()))
            .into_any_element(),
    };

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(heading)
        .child(progress)
        .into_any_element()
}

/// What a downloaded payload says.
fn downloaded_body(cx: &Context<UpdateDialog>) -> AnyElement {
    div()
        .debug_selector(|| UPDATER_DIALOG_DOWNLOADED.into())
        .flex()
        .flex_col()
        .gap_2()
        .child(t!(cx, "window.updater.downloaded_message"))
        .child(div().text_xs().child(t!(cx, "window.updater.restart_hint")))
        .into_any_element()
}

/// The control that opens the release page, for a platform that cannot install in place.
fn release_page_button(cx: &Context<UpdateDialog>) -> AnyElement {
    let label = t!(cx, "window.updater.open_release_page");

    Button::new("UpdaterDialogReleasePage")
        .filled()
        .medium()
        .debug_selector(|| UPDATER_DIALOG_RELEASE_PAGE.into())
        .aria_label(label.clone())
        .on_click(|_, _, cx| cx.open_url(publish::RELEASE_PAGE))
        .child(label)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use app_config_provider::manager::ConfigManager;
    use base_ui::{animation::ANIMATION_TICK, layer::Layer, style::theme};
    use gpui_kit::{EmptyView, Modifiers, TestAppContext, VisualTestContext};
    use network::transport::hub::{TransportHub, TransportPolicy};
    use tokio_util::sync::CancellationToken;
    use widget_tokio_runtime::runtime::Runtime;

    use super::*;
    use crate::{
        error::UpdaterError,
        global::{self, UpdaterDeps},
        release::{PlatformKey, PlatformRelease, ReleaseFormat, ReleaseInfo, ReleaseNotes},
        state::UpdaterState,
    };

    fn release() -> ReleaseInfo {
        ReleaseInfo {
            key: PlatformKey::current().expect("the tests run on a packaged platform"),
            version: semver::Version::new(1, 2, 3),
            notes: Some(ReleaseNotes::for_test(&[
                ("en", "## What changed\n\n- everything"),
                ("zh-CN", "## 更新内容\n\n- 全部"),
            ])),
            pub_date: None,
            asset: PlatformRelease {
                url: url::Url::parse("https://example.invalid/kytrix-setup.exe").unwrap(),
                format: ReleaseFormat::Nsis,
            },
        }
    }

    /// The same release with only the notes a test publishes.
    fn release_with_notes(notes: &[(&'static str, &'static str)]) -> ReleaseInfo {
        ReleaseInfo {
            notes: Some(ReleaseNotes::for_test(notes)),
            ..release()
        }
    }

    /// A window whose root is the layer an application mounts, which is what a
    /// dialog is opened on and dismissed from.
    fn harness(cx: &mut TestAppContext) -> (&mut VisualTestContext, Entity<Updater>) {
        cx.update(|cx| {
            theme::install(cx);
            i18n::global::embedded::install(cx).expect("the embedded locales should load");
        });
        let updater = cx.update(|cx| {
            let manager = ConfigManager::in_memory(app_config::settings::Config::default(), cx);
            cx.set_global(manager);
            global::install(
                cx,
                UpdaterDeps {
                    transport: Arc::new(TransportHub::new(TransportPolicy::default())),
                    runtime: Runtime::for_test(),
                },
            );

            global::entity(cx).expect("the updater that was just installed is the one found")
        });
        let (_, cx) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| EmptyView)));
        cx.run_until_parked();

        (cx, updater)
    }

    /// Lets the dialog's own transition finish and repaints, so that one just
    /// opened is past its first frames and one just dismissed is gone rather than
    /// still fading out.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    fn shown(
        cx: &mut VisualTestContext,
        updater: &Entity<Updater>,
        state: UpdaterState,
    ) -> Entity<UpdateDialog> {
        cx.update(|_, cx| {
            updater.update(cx, |updater, cx| updater.set_state_for_test(state, cx));
        });
        let dialog = cx.update(|window, cx| UpdateDialog::show(updater, window, cx));
        settle(cx);

        dialog
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the dialog should draw {selector}"))
            .center();
        cx.simulate_click(target, Modifiers::none());
        settle(cx);
    }

    /// Waits for a dialog that was dismissed to leave the layer.
    ///
    /// The exit transition is drawn over several frames and the layer drops the
    /// dialog on the first one past it, so this waits for that rather than
    /// guessing which frame it is.
    fn dismissed(cx: &mut VisualTestContext, selector: &'static str) {
        for _ in 0..4 {
            if cx.debug_bounds(selector).is_none() {
                return;
            }
            settle(cx);
        }

        panic!("the dialog should have left the layer");
    }

    #[gpui_kit::test]
    fn a_dialog_should_draw_the_notes_and_offer_the_download(cx: &mut TestAppContext) {
        let (cx, updater) = harness(cx);
        shown(cx, &updater, UpdaterState::Available(release()));

        assert!(
            cx.debug_bounds(UPDATER_DIALOG_NOTES).is_some(),
            "the notes are what the dialog is opened for"
        );
        assert!(
            cx.debug_bounds(UPDATER_DIALOG_FAILURE).is_none(),
            "nothing has failed"
        );
        // Which primary action an update has depends on whether this build can
        // install one over itself at all: a package-manager installation cannot be
        // replaced in place, and is sent to the release page instead.
        assert_eq!(
            cx.debug_bounds(UPDATER_DIALOG_DOWNLOAD).is_some(),
            install::supported(),
            "an installable payload is offered as a download"
        );
        assert_eq!(
            cx.debug_bounds(UPDATER_DIALOG_RELEASE_PAGE).is_some(),
            !install::supported(),
            "a build that cannot install one is offered the release page"
        );
    }

    /// The notes are chosen in the language the interface is in when it opens.
    #[gpui_kit::test]
    fn the_dialog_should_draw_the_notes_in_the_language_the_interface_is_in(
        cx: &mut TestAppContext,
    ) {
        let (cx, updater) = harness(cx);
        cx.update(|_, cx| cx.set_global(LanguageCode::ZhCn));
        let dialog = shown(cx, &updater, UpdaterState::Available(release()));

        assert_eq!(
            cx.read(|cx| dialog.read(cx).notes.read(cx).source().to_string()),
            "## 更新内容\n\n- 全部"
        );
    }

    /// A language the release published no notes in is drawn in English.
    #[gpui_kit::test]
    fn the_dialog_should_fall_back_to_the_english_notes(cx: &mut TestAppContext) {
        let (cx, updater) = harness(cx);
        cx.update(|_, cx| cx.set_global(LanguageCode::ZhCn));
        let dialog = shown(
            cx,
            &updater,
            UpdaterState::Available(release_with_notes(&[(
                "en",
                "## What changed\n\n- everything",
            )])),
        );

        assert_eq!(
            cx.read(|cx| dialog.read(cx).notes.read(cx).source().to_string()),
            "## What changed\n\n- everything"
        );
    }

    /// The download is not started here: pressing the control would send a real
    /// request. What is drawn while one is in flight is the same state either way.
    #[gpui_kit::test]
    fn a_download_in_flight_should_be_drawn_as_progress_and_offer_only_cancelling(
        cx: &mut TestAppContext,
    ) {
        let (cx, updater) = harness(cx);
        let cancellation = CancellationToken::new();
        shown(
            cx,
            &updater,
            UpdaterState::downloading_for_test(release(), 4_096, Some(8_192), cancellation.clone()),
        );

        assert!(
            cx.debug_bounds(UPDATER_DIALOG_PROGRESS).is_some(),
            "a download that named its length is drawn against it"
        );
        assert!(
            cx.debug_bounds(UPDATER_DIALOG_DOWNLOAD).is_none(),
            "there is nothing left to start"
        );
        assert!(
            cx.debug_bounds(UPDATER_DIALOG_INSTALL).is_none(),
            "nothing is downloaded yet"
        );

        click(cx, UPDATER_DIALOG_CANCEL);

        assert!(
            cancellation.is_cancelled(),
            "cancelling a download stops the request that is running it"
        );
        assert!(
            updater.read_with(cx, |updater, _| matches!(
                updater.state(),
                UpdaterState::Available(_)
            )),
            "the release is on offer again"
        );
        dismissed(cx, UPDATER_DIALOG_CANCEL);
    }

    #[gpui_kit::test]
    fn a_download_without_a_length_should_be_drawn_as_something_happening(cx: &mut TestAppContext) {
        let (cx, updater) = harness(cx);
        shown(
            cx,
            &updater,
            UpdaterState::downloading_for_test(release(), 4_096, None, CancellationToken::new()),
        );

        assert!(
            cx.debug_bounds(UPDATER_DIALOG_PROGRESS).is_some(),
            "a chunked response is still shown as a download in flight"
        );
        let progress = cx.debug_bounds(UPDATER_DIALOG_PROGRESS).unwrap();
        assert!(
            progress.size.width <= px(16.0),
            "a length that is not known is not drawn as a bar (progress {:?})",
            progress.size
        );
    }

    #[gpui_kit::test]
    fn a_downloaded_payload_should_offer_the_install_and_be_given_up_on_closing(
        cx: &mut TestAppContext,
    ) {
        let (cx, updater) = harness(cx);
        shown(
            cx,
            &updater,
            UpdaterState::ready_for_test(release(), bytes::Bytes::from_static(b"payload")),
        );

        assert!(
            cx.debug_bounds(UPDATER_DIALOG_DOWNLOADED).is_some(),
            "a downloaded payload says so"
        );
        assert!(
            cx.debug_bounds(UPDATER_DIALOG_INSTALL).is_some(),
            "what a downloaded payload is for is installing it"
        );

        click(cx, UPDATER_DIALOG_CANCEL);

        assert!(
            updater.read_with(cx, |updater, _| matches!(
                updater.state(),
                UpdaterState::Available(_)
            )),
            "closing on a downloaded payload drops it and offers the release again"
        );
        dismissed(cx, UPDATER_DIALOG_INSTALL);
    }

    #[gpui_kit::test]
    fn a_failed_attempt_should_be_drawn_where_the_notes_were(cx: &mut TestAppContext) {
        let (cx, updater) = harness(cx);
        shown(
            cx,
            &updater,
            UpdaterState::failed_for_test(release(), UpdaterError::UnsupportedPlatform),
        );

        assert!(
            cx.debug_bounds(UPDATER_DIALOG_FAILURE).is_some(),
            "a failure is what the dialog says instead"
        );
        assert!(
            cx.debug_bounds(UPDATER_DIALOG_DOWNLOAD).is_none(),
            "and it is not an invitation to start over from here"
        );

        click(cx, UPDATER_DIALOG_CLOSE);

        assert!(
            updater.read_with(cx, |updater, _| matches!(
                updater.state(),
                UpdaterState::Available(_)
            )),
            "closing on a failure leaves the release on offer"
        );
        dismissed(cx, UPDATER_DIALOG_FAILURE);
    }
}

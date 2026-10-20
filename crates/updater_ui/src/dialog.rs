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
    scroll_bar::Scrollbar,
    separator::Separator,
    size::UnifiedSizable as _,
    spinner::Spinner,
    style::theme::ThemeExt as _,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, DismissEvent, Empty, Entity, EventEmitter,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, base::ScrollbarMode, div, px,
};
use i18n::{language::codes::LanguageCode, t};
use smol_str::SmolStr;

use crate::{
    install, publish,
    state::{Updater, UpdaterState},
};

/// How wide the panel is while the release notes are read.
const NOTES_WIDTH: f32 = 1040.0;

/// How wide the panel is for everything that is not the notes.
///
/// A download in flight, a payload waiting to be installed and what a failed attempt
/// says are a line or two of text each: drawn at the notes' width they would be a wide
/// band of empty panel around two words.
const STATUS_WIDTH: f32 = 640.0;

/// How tall the release notes are drawn before they scroll.
const NOTES_MAX_HEIGHT: f32 = 640.0;

/// How wide the band the notes' scrollbar is drawn in.
///
/// The scrollbar paints a 4px thumb hard against its own right edge, so the band is
/// what keeps the thumb off the notes' text rather than over it.
const SCROLLBAR_WIDTH: f32 = 8.0;

/// The debug selectors the dialog's parts are drawn with, for the visual tests.
const UPDATER_DIALOG_NOTES: &str = "UPDATER_DIALOG_NOTES";
const UPDATER_DIALOG_SCROLLBAR: &str = "UPDATER_DIALOG_SCROLLBAR";
const UPDATER_DIALOG_DOWNLOAD: &str = "UPDATER_DIALOG_DOWNLOAD";
const UPDATER_DIALOG_PROGRESS: &str = "UPDATER_DIALOG_PROGRESS";
const UPDATER_DIALOG_DOWNLOADED: &str = "UPDATER_DIALOG_DOWNLOADED";
const UPDATER_DIALOG_INSTALL: &str = "UPDATER_DIALOG_INSTALL";
const UPDATER_DIALOG_RELEASE_PAGE: &str = "UPDATER_DIALOG_RELEASE_PAGE";
const UPDATER_DIALOG_FAILURE: &str = "UPDATER_DIALOG_FAILURE";
const UPDATER_DIALOG_CLOSE: &str = "UPDATER_DIALOG_CLOSE";
const UPDATER_DIALOG_CANCEL: &str = "UPDATER_DIALOG_CANCEL";

/// What the dialog draws, named rather than borrowed from the updater.
///
/// The dialog follows the updater for as long as it is open. It is still drawn for the
/// length of its exit transition after it has been dismissed, and by the first of those
/// frames the updater has already moved on — cancelling a download puts the release back
/// on offer, which is the notes again. Reading the live state there would swap the
/// content out from under the fade, so what the last live frame drew is named here and
/// drawn unchanged until the panel is gone.
#[derive(Clone, Copy)]
enum Shown {
    /// The release notes, and what a platform that cannot install in place is told.
    Notes,
    /// A download in flight: what it has accepted, and the size the response named.
    Download(u64, Option<u64>),
    /// The payload is downloaded, waiting for the user.
    Downloaded,
    /// Nothing left to press: the application is being replaced underneath it.
    Installing,
    /// The last attempt failed, with the catalogue key the interface says it with.
    Failure(Option<&'static str>),
}

impl Shown {
    /// What a state draws, or nothing when it is about no release at all.
    fn of(state: &UpdaterState) -> Option<Self> {
        match state {
            UpdaterState::Idle | UpdaterState::Checking => None,
            UpdaterState::Available(_) => Some(Self::Notes),
            // A download in flight is the state itself: what it has accepted and the
            // size it is measured against are both part of it.
            UpdaterState::Downloading(download) => {
                Some(Self::Download(download.accepted(), download.named_total()))
            }
            UpdaterState::ReadyToInstall(_) => Some(Self::Downloaded),
            UpdaterState::Installing(_) => Some(Self::Installing),
            UpdaterState::Failed(_) => Some(Self::Failure(
                state.failure().map(|error| error.message_key()),
            )),
        }
    }
}

/// The dialog the update indicator opens.
pub(crate) struct UpdateDialog {
    updater: Entity<Updater>,
    /// The release notes, parsed once: one release is offered at a time, so the
    /// text does not change for as long as the dialog lives.
    notes: Entity<MarkdownState>,
    /// Where the notes are scrolled to, which is also what the scrollbar beside them
    /// is drawn from.
    notes_scroll: ScrollHandle,
    /// What this dialog was drawing when it was dismissed, kept so that the frames of
    /// its exit transition draw the dialog that left rather than the one that came back.
    frozen: Option<Shown>,
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
            notes_scroll: ScrollHandle::new(),
            frozen: None,
        }
    }

    /// What this frame draws: the live state, or the frame kept while the dialog fades.
    fn shown(&self, cx: &Context<Self>) -> Option<Shown> {
        self.frozen
            .or_else(|| Shown::of(self.updater.read(cx).state()))
    }

    /// Keeps the frame the dialog is drawn as while it leaves.
    ///
    /// Called before the dismissal asks the updater anything, so what is kept is the
    /// dialog the user pressed rather than what closing turned the updater into.
    fn freeze(&mut self, cx: &mut Context<Self>) {
        if self.frozen.is_none() {
            self.frozen = Shown::of(self.updater.read(cx).state());
        }
    }

    /// What the release notes and the state's own text say.
    fn body(&self, shown: Shown, cx: &Context<Self>) -> AnyElement {
        match shown {
            Shown::Notes => self.notes_body(cx),
            Shown::Download(downloaded, total) => download_body(downloaded, total, cx),
            Shown::Downloaded | Shown::Installing => downloaded_body(cx),
            Shown::Failure(key) => self.failure_body(key, cx),
        }
    }

    /// The controls the state offers, the primary one last.
    fn actions(&self, shown: Shown, cx: &Context<Self>) -> Vec<AnyElement> {
        match shown {
            // Nothing on this platform can replace the running application, so
            // nothing is downloaded for it: the release page is where its update is.
            Shown::Notes if !install::supported() => vec![
                self.closing_button(
                    "UpdaterDialogClose",
                    UPDATER_DIALOG_CLOSE,
                    t!(cx, "window.updater.cancel"),
                    None,
                    cx,
                ),
                release_page_button(cx),
            ],
            Shown::Notes => vec![
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
            Shown::Download(..) => vec![self.closing_button(
                "UpdaterDialogCancelDownload",
                UPDATER_DIALOG_CANCEL,
                t!(cx, "window.updater.cancel"),
                Some(Updater::cancel_download),
                cx,
            )],
            Shown::Downloaded => vec![
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
            Shown::Installing => Vec::new(),
            Shown::Failure(_) => vec![self.closing_button(
                "UpdaterDialogDismiss",
                UPDATER_DIALOG_CLOSE,
                t!(cx, "common.close"),
                Some(Updater::dismiss_failure),
                cx,
            )],
        }
    }

    /// The release notes, and what a platform that cannot install in place is told.
    ///
    /// The scrollbar is drawn beside the notes rather than inside them, and always:
    /// the notes are as long as the release wrote them, so whether there is more to
    /// read is not something the reader should have to find by scrolling first. The
    /// notes keep clear of the band it is drawn in, so the thumb never sits over the
    /// text it is measuring.
    fn notes_body(&self, cx: &Context<Self>) -> AnyElement {
        let mut body = div().flex().flex_col().gap_2().child(
            div()
                .relative()
                .flex()
                .flex_col()
                .child(
                    div()
                        .id("UpdaterDialogNotes")
                        .debug_selector(|| UPDATER_DIALOG_NOTES.into())
                        .max_h(px(NOTES_MAX_HEIGHT))
                        .pr(px(SCROLLBAR_WIDTH))
                        .overflow_y_scroll()
                        .track_scroll(&self.notes_scroll)
                        .child(Markdown::new(&self.notes)),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .w(px(SCROLLBAR_WIDTH))
                        .debug_selector(|| UPDATER_DIALOG_SCROLLBAR.into())
                        .child(
                            Scrollbar::vertical(self.notes_scroll.clone())
                                .mode(ScrollbarMode::Always),
                        ),
                ),
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
    fn failure_body(&self, key: Option<&'static str>, cx: &Context<Self>) -> AnyElement {
        let Some(key) = key else {
            return Empty.into_any_element();
        };
        let color = cx.theme().roles.status.danger.base.hsla;

        div()
            .debug_selector(|| UPDATER_DIALOG_FAILURE.into())
            .text_color(color)
            .child(t!(cx, key))
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
                this.freeze(cx);
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
        let Some(shown) = self.shown(cx) else {
            return Empty.into_any_element();
        };
        let width = match shown {
            Shown::Notes => NOTES_WIDTH,
            Shown::Download(..) | Shown::Downloaded | Shown::Installing | Shown::Failure(_) => {
                STATUS_WIDTH
            }
        };
        let body = self.body(shown, cx);
        let actions = self.actions(shown, cx);

        div()
            .w(px(width))
            .flex()
            .flex_col()
            .gap_2()
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
/// A response that named its length is drawn as a bar against it, with the share
/// accepted so far centred under it. One that did not — a chunked answer — has no total
/// to measure against, so what is drawn is the fact that something is happening rather
/// than a fraction of it: a spinner, centred, under the same heading.
fn download_body(downloaded: u64, total: Option<u64>, cx: &Context<UpdateDialog>) -> AnyElement {
    let heading = t!(cx, "window.updater.downloading");

    let progress = match total.filter(|total| *total > 0) {
        Some(total) => {
            let percent = (downloaded as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32;
            div()
                .debug_selector(|| UPDATER_DIALOG_PROGRESS.into())
                .flex()
                .flex_col()
                .gap_2()
                .child(Progress::new("UpdaterDownloadProgress").value(percent))
                .child(div().text_xs().text_center().child(t!(
                    cx,
                    "window.updater.download_progress",
                    percent = percent as u32
                )))
                .into_any_element()
        }
        None => div()
            .flex()
            .justify_center()
            .child(
                div()
                    .debug_selector(|| UPDATER_DIALOG_PROGRESS.into())
                    .flex_none()
                    .size_4()
                    .child(Spinner::new("UpdaterDownloadSpinner")),
            )
            .into_any_element(),
    };

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(div().text_center().child(heading))
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
        .child(
            div()
                .text_center()
                .child(t!(cx, "window.updater.downloaded_message")),
        )
        .child(
            div()
                .text_xs()
                .text_center()
                .child(t!(cx, "window.updater.restart_hint")),
        )
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

    /// The dialog is drawn for the length of its exit transition after it has been
    /// dismissed, and by then the updater has already moved on. What it drew while it
    /// was open is what it has to keep drawing, or the last frames show a different
    /// dialog from the one that is leaving — cancelling a download, for one, puts the
    /// notes back.
    #[gpui_kit::test]
    fn a_dismissed_dialog_should_keep_drawing_the_frame_it_was_dismissed_in(
        cx: &mut TestAppContext,
    ) {
        let (cx, updater) = harness(cx);
        let cancellation = CancellationToken::new();
        shown(
            cx,
            &updater,
            UpdaterState::downloading_for_test(release(), 4_096, Some(8_192), cancellation.clone()),
        );

        // Pressed without letting the transition settle, so the frames that follow are
        // the ones the fade is drawn on.
        let target = cx
            .debug_bounds(UPDATER_DIALOG_CANCEL)
            .expect("a download in flight offers cancelling")
            .center();
        cx.simulate_click(target, Modifiers::none());

        assert!(
            updater.read_with(cx, |updater, _| matches!(
                updater.state(),
                UpdaterState::Available(_)
            )),
            "cancelling offers the release again straight away"
        );
        assert!(
            cx.debug_bounds(UPDATER_DIALOG_PROGRESS).is_some(),
            "the dialog that is still fading is the download it was dismissed in, not the \
             notes the cancellation returned to"
        );
        assert!(
            cx.debug_bounds(UPDATER_DIALOG_NOTES).is_none(),
            "the notes the release went back to waiting behind is not what the fade draws"
        );
    }

    /// The scrollbar beside the notes does not wait for the pointer or for a scroll to
    /// appear: it is drawn for as long as there are notes taller than the box they are
    /// drawn in, which is how the reader is told there is more without scrolling first.
    ///
    /// Nothing here hovers or scrolls, so a scrollbar that appeared on either would not
    /// be drawn at all.
    #[gpui_kit::test]
    fn the_notes_should_be_drawn_with_their_scrollbar_without_being_scrolled(
        cx: &mut TestAppContext,
    ) {
        let (cx, updater) = harness(cx);
        shown(
            cx,
            &updater,
            UpdaterState::Available(release_with_notes(&[(
                "en",
                "## What changed\n\nA paragraph of the release notes, long enough that a few of \
                 them put the notes past the box they are drawn in.\n\nA paragraph of the release \
                 notes, long enough that a few of them put the notes past the box they are drawn \
                 in.\n\nA paragraph of the release notes, long enough that a few of them put the \
                 notes past the box they are drawn in.\n\nA paragraph of the release notes, long \
                 enough that a few of them put the notes past the box they are drawn in.\n\nA \
                 paragraph of the release notes, long enough that a few of them put the notes past \
                 the box they are drawn in.\n\nA paragraph of the release notes, long enough that a \
                 few of them put the notes past the box they are drawn in.\n\nA paragraph of the \
                 release notes, long enough that a few of them put the notes past the box they are \
                 drawn in.\n\nA paragraph of the release notes, long enough that a few of them put \
                 the notes past the box they are drawn in.\n\nA paragraph of the release notes, long \
                 enough that a few of them put the notes past the box they are drawn in.\n\nA \
                 paragraph of the release notes, long enough that a few of them put the notes past \
                 the box they are drawn in.",
            )])),
        );

        let scrollbar = cx
            .debug_bounds(UPDATER_DIALOG_SCROLLBAR)
            .expect("the scrollbar is drawn beside the notes");
        assert!(
            scrollbar.size.height > px(100.),
            "the scrollbar runs the height of the notes it scrolls (scrollbar {scrollbar:?})"
        );
    }

    /// The notes are read in a panel twice the width they used to be, and everything
    /// else in a narrower one: a download's progress and a downloaded payload's two
    /// lines are not worth a wide band of empty panel.
    #[gpui_kit::test]
    fn the_notes_should_be_drawn_wider_than_the_status_the_dialog_turns_into(
        cx: &mut TestAppContext,
    ) {
        let (cx, updater) = harness(cx);
        let notes = shown(cx, &updater, UpdaterState::Available(release()));
        let notes_width = cx
            .debug_bounds(UPDATER_DIALOG_NOTES)
            .expect("the notes are what the dialog is opened for")
            .size
            .width;
        cx.update(|_, cx| {
            notes.update(cx, |_, cx| cx.emit(DismissEvent));
        });
        dismissed(cx, UPDATER_DIALOG_NOTES);

        shown(
            cx,
            &updater,
            UpdaterState::downloading_for_test(
                release(),
                4_096,
                Some(8_192),
                CancellationToken::new(),
            ),
        );
        let download_width = cx
            .debug_bounds(UPDATER_DIALOG_PROGRESS)
            .expect("a download in flight says how far it is")
            .size
            .width;

        assert!(
            notes_width > px(900.),
            "the notes fill the panel, which is 1040 wide (notes {notes_width:?})"
        );
        assert_eq!(
            download_width,
            px(STATUS_WIDTH),
            "a download is drawn in the narrower panel"
        );
    }

    /// A failed attempt is as short a message as a download is, so it is drawn in the
    /// same narrower panel rather than in the notes' width.
    #[gpui_kit::test]
    fn a_failure_should_be_drawn_in_the_narrower_panel(cx: &mut TestAppContext) {
        let (cx, updater) = harness(cx);
        shown(
            cx,
            &updater,
            UpdaterState::failed_for_test(release(), UpdaterError::UnsupportedPlatform),
        );

        let failure = cx
            .debug_bounds(UPDATER_DIALOG_FAILURE)
            .expect("a failure is what the dialog says instead");
        assert_eq!(
            failure.size.width,
            px(STATUS_WIDTH),
            "the failure fills the narrower panel rather than the notes' width"
        );
    }
}

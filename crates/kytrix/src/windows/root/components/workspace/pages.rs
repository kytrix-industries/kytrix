//! The page a workspace draws over its panes: the widget gallery, and the transition it
//! enters and leaves with.
//!
//! The page is not a window dialog and not a peer of the workspace: it belongs to the
//! workspace, covers the whole stage while it is open, and is drawn over the panes. It slides
//! up from the bottom of the stage and fades in, and leaves the same way in reverse. The
//! transition is one value — [`PAGE_TRANSITION_DURATION`] spent moving the page from one
//! stage-height below its place to it — and the page's opacity follows the same value, so the
//! two cannot drift apart.
//!
//! What the page is belongs to whoever opened it. The gallery reports a choice or a
//! cancellation through [`WidgetGalleryEvent`], and this host applies a confirmation to the
//! pane the gallery was opened from, then lets the page leave. A page that is leaving takes no
//! more reports: the choice behind it is already made.
//!
//! Adding another page of the workspace means adding a variant to [`WorkspacePage`] and the
//! way it reports its decision. The host's transition, its blocking of the stage under it, and
//! its cleanup after the page has left do not change.

use std::time::Duration;

use gpui_kit::{
    AppContext as _, Context, ElementId, Empty, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled, Subscription, WeakEntity, Window,
    base::{Easing, Presence, Transition},
    div,
    prelude::FluentBuilder as _,
    relative,
};
use widget::identity::WidgetId;
use widget_gallery_ui::gallery::{WidgetGallery, WidgetGalleryEvent};

use super::stage::Stage;

/// How long the page takes to slide up from the bottom of the workspace and to slide back down.
///
/// The slide and the fade share it: the page is one surface moving and appearing, not two
/// effects running beside each other.
pub(crate) const PAGE_TRANSITION_DURATION: Duration = Duration::from_millis(250);

/// The page that is open over the workspace, while it is open.
struct OpenPage {
    kind: WorkspacePage,
    /// Whether the page has been dismissed and is on its way back down.
    ///
    /// A page that is leaving takes no more reports: the decision behind it is already made,
    /// so a second press while it fades out is a press on a decision that is over.
    closing: bool,
    /// Follows the page. Dropping it would leave a page whose controls do nothing.
    _events: Subscription,
}

/// The pages a workspace can draw over its panes.
enum WorkspacePage {
    /// The page a pane's widget is chosen from.
    WidgetGallery {
        /// The pane whose widget is being chosen.
        pane_id: ElementId,
        /// The stage the pane belongs to, which a confirmation is applied through. It is weak:
        /// the workspace owns the stage, and a page must not be what keeps it alive.
        stage: WeakEntity<Stage>,
        /// The page a confirmation or a cancellation is reported through.
        gallery: Entity<WidgetGallery>,
    },
}

/// The page drawn over a workspace's panes, and whether it is leaving.
pub(super) struct WorkspacePages {
    page: Option<OpenPage>,
}

impl WorkspacePages {
    pub(super) fn new() -> Self {
        Self { page: None }
    }

    /// Whether a page is open, including one that is already on its way out.
    pub(super) fn is_open(&self) -> bool {
        self.page.is_some()
    }

    /// Opens the gallery in which `pane_id`'s widget is chosen.
    ///
    /// A second page over the first would have nothing to choose for, so opening while one is
    /// already open is a no-op. The page is created here rather than while the stage draws: it
    /// makes the field it is searched with, which is an entity, and it takes the caret, so
    /// building it inside a frame would be the re-entrancy a frame must not have.
    pub(super) fn open_widget_gallery(
        &mut self,
        pane_id: ElementId,
        current: WidgetId,
        stage: WeakEntity<Stage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.page.is_some() {
            return;
        }

        let gallery = cx.new(|cx| WidgetGallery::new(Some(current), window, cx));
        let events = cx.subscribe(&gallery, |pages, _, event: &WidgetGalleryEvent, cx| {
            pages.gallery_event(event, cx)
        });
        self.page = Some(OpenPage {
            kind: WorkspacePage::WidgetGallery {
                pane_id,
                stage,
                gallery,
            },
            closing: false,
            _events: events,
        });
        cx.notify();
    }

    /// Applies a confirmed choice and starts the page leaving, or starts it leaving without a
    /// choice.
    ///
    /// The choice is applied immediately rather than when the slide has finished: the page
    /// covers what changes underneath it, and the widget the user asked for is what the frame
    /// that uncovers the pane shows.
    fn gallery_event(&mut self, event: &WidgetGalleryEvent, cx: &mut Context<Self>) {
        let Some(page) = self.page.as_mut() else {
            return;
        };
        if page.closing {
            return;
        }

        if let (
            WidgetGalleryEvent::Confirm(widget_id),
            WorkspacePage::WidgetGallery { pane_id, stage, .. },
        ) = (event, &page.kind)
            && let Some(stage) = stage.upgrade()
        {
            let pane_id = pane_id.clone();
            let widget_id = widget_id.as_str();
            stage.update(cx, |stage, cx| stage.set_widget(&pane_id, widget_id, cx));
        }

        page.closing = true;
        cx.notify();
    }

    /// Takes the page away at once, if there is one.
    ///
    /// This is how the workspace is left — it is replaced by another workspace, or the window
    /// shows the settings page instead — and there is nothing for the page to slide over: the
    /// content under it is not the content it was opened from.
    pub(super) fn close(&mut self, cx: &mut Context<Self>) {
        if self.page.take().is_some() {
            cx.notify();
        }
    }

    /// The gallery a test can inspect while it is open.
    #[cfg(test)]
    pub(super) fn gallery(&self) -> Option<&Entity<WidgetGallery>> {
        match self.page.as_ref().map(|page| &page.kind) {
            Some(WorkspacePage::WidgetGallery { gallery, .. }) => Some(gallery),
            None => None,
        }
    }

    /// The pane the open gallery chooses a widget for.
    #[cfg(test)]
    pub(super) fn pane_id(&self) -> Option<&ElementId> {
        match self.page.as_ref().map(|page| &page.kind) {
            Some(WorkspacePage::WidgetGallery { pane_id, .. }) => Some(pane_id),
            None => None,
        }
    }
}

impl Render for WorkspacePages {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The page grows from one stage-height below its place to it: 0 is entirely below the
        // workspace and 1 is in place. `Presence` owns the transition, including the value a
        // reversal starts from — an interrupted opening turns around from where it was rather
        // than from the end it never reached — and reports the value to draw.
        let present = self.page.as_ref().is_some_and(|page| !page.closing);
        let presence = Presence::new(("workspace-pages", "presence"), present)
            .transition(Transition::new(PAGE_TRANSITION_DURATION).easing(Easing::EaseInOut))
            .sample(window, cx);
        let closing = self.page.as_ref().is_some_and(|page| page.closing);
        // A page that starts leaving before `Presence` has drawn it even once would keep an
        // invisible page over the workspace for the length of the transition: its first absent
        // sample reports `Exiting` at zero progress rather than `Absent`. Zero progress is the
        // page entirely below the workspace — nothing is drawn, but the host still blocks the
        // stage — so it is cleared then too.
        if closing && (!presence.should_render() || presence.progress <= 0.0) {
            self.page = None;
            return Empty.into_any_element();
        }
        let Some(page) = self.page.as_ref() else {
            return Empty.into_any_element();
        };

        let content = match &page.kind {
            WorkspacePage::WidgetGallery { gallery, .. } => gallery.clone().into_any_element(),
        };
        let progress = presence.progress.clamp(0.0, 1.0);

        div()
            .debug_selector(|| "WORKSPACE_PAGES".into())
            .absolute()
            .inset_0()
            .overflow_hidden()
            // The host covers the stage for as long as it is drawn. The page itself may still
            // be below the workspace during the first frames of its way up, and a click on a
            // pane it has not reached must not reach it.
            .occlude()
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .h_full()
                    .top(relative(1.0 - progress))
                    .opacity(progress)
                    // A page on its way out is still drawn, but what it draws no longer acts:
                    // a second click of a double-click cannot run the choice it already made.
                    .when(page.closing, |this| {
                        this.capture_any_mouse_down(|_, _, cx| cx.stop_propagation())
                    })
                    .child(content),
            )
            .into_any_element()
    }
}

//! The page in which a pane's widget is chosen, drawn over the stage.
//!
//! The page is not a window dialog: the workspace shows it inside itself, growing out of
//! the pane whose widget is being chosen and covering the stage while the choice is made.
//! That is what this session holds — the pane it belongs to, the rectangle it grows from,
//! the page itself, and whether it is already on its way out.

use std::time::Duration;

use base_ui::style::theme::ThemeExt;
use gpui_kit::{
    AnyElement, App, Bounds, ElementId, Entity, InteractiveElement, IntoElement, ParentElement,
    Pixels, Styled, Subscription, base::Interpolate, div,
};
use widget_gallery_ui::gallery::{WidgetGallery, WidgetGalleryEvent};

/// How long the page takes to grow out of its pane and to shrink back into it.
pub(super) const MORPH_DURATION: Duration = Duration::from_millis(300);

/// One open picker: the page, the pane it was opened from, and whether it is leaving.
pub(super) struct WidgetPicker {
    /// The pane whose widget is being chosen.
    pane_id: ElementId,
    /// Where the pane was when the page opened, in window coordinates: the rectangle the
    /// page grows out of and shrinks back into while the pane no longer has a measured one.
    origin: Bounds<Pixels>,
    /// The page a confirmation or a cancellation is reported through.
    gallery: Entity<WidgetGallery>,
    /// Whether the choice has been made or cancelled, and the page is shrinking back.
    ///
    /// A session that is closing takes no more reports: the choice behind it is already
    /// made, so a second press while the page fades out is a press on a decision that is
    /// over rather than a new one.
    closing: bool,
    /// Follows the page. Dropping it would leave a page whose buttons do nothing.
    _events: Subscription,
}

impl WidgetPicker {
    pub(super) fn new(
        pane_id: ElementId,
        origin: Bounds<Pixels>,
        gallery: Entity<WidgetGallery>,
        events: Subscription,
    ) -> Self {
        Self {
            pane_id,
            origin,
            gallery,
            closing: false,
            _events: events,
        }
    }

    /// The pane whose widget is being chosen.
    pub(super) fn pane_id(&self) -> &ElementId {
        &self.pane_id
    }

    /// The pane's rectangle when the page opened, for a pane that has none measured now.
    pub(super) fn origin(&self) -> Bounds<Pixels> {
        self.origin
    }

    /// Whether the page has been dismissed and is shrinking back into its pane.
    pub(super) fn is_closing(&self) -> bool {
        self.closing
    }

    /// Handles one report from the page, unless the page has already left: a report that
    /// arrives while the page shrinks back belongs to the decision that dismissed it, and
    /// has nothing left to decide.
    ///
    /// The report is the page's own event: the page and the workspace speak the same two
    /// decisions, and a second type mirroring them here would only be a copy to keep in step.
    pub(super) fn report(&mut self, event: &WidgetGalleryEvent) -> Option<WidgetGalleryEvent> {
        if self.closing {
            return None;
        }

        self.closing = true;

        Some(event.clone())
    }

    #[cfg(test)]
    pub(super) fn gallery(&self) -> &Entity<WidgetGallery> {
        &self.gallery
    }

    /// Draws the page `progress` of the way out of `origin`: the pane's own rectangle at 0,
    /// `stage_bounds` at 1, with the page fading in behind the growing bounds.
    ///
    /// Both rectangles are the caller's current measurement, so a window resized under an
    /// open page still grows it out of the pane and shrinks it back into the pane. A stage
    /// that has not been measured yet has no stage rectangle; the page then keeps its
    /// pane's size, and the next measured frame retargets it.
    pub(super) fn overlay(
        &self,
        progress: f32,
        origin: Bounds<Pixels>,
        stage_bounds: Option<Bounds<Pixels>>,
        cx: &App,
    ) -> AnyElement {
        let progress = progress.clamp(0.0, 1.0);
        let stage_bounds = stage_bounds.unwrap_or(origin);
        let bounds = origin.interpolate(&stage_bounds, progress);
        let offset = bounds.origin - stage_bounds.origin;
        // The ground appears first and the page fades in after it: a page that was already
        // opaque on the first frame would read as a flash rather than as a page growing out
        // of the pane it was opened from. Both curves end at 1, so the settled page is the
        // plain page.
        let backdrop = (progress / 0.25).clamp(0.0, 1.0);
        let content = ((progress - 0.15) / 0.85).clamp(0.0, 1.0);

        div()
            .absolute()
            .left(offset.x)
            .top(offset.y)
            .w(bounds.size.width)
            .h(bounds.size.height)
            .overflow_hidden()
            .occlude()
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().roles.surface.canvas.hsla)
                    .opacity(backdrop),
            )
            .child(
                div()
                    .size_full()
                    .opacity(content)
                    .child(self.gallery.clone()),
            )
            .into_any_element()
    }
}

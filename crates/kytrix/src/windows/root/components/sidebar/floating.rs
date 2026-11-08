//! Whether the floating full panel is showing itself over the page.
//!
//! The panel is not a fourth sidebar state: it is a transient preview the pointer asks for
//! while the sidebar is collapsed — hidden, where the title bar's toggle asks for it, or
//! icon-only, where the rail under the pointer asks for it. It stays up while the pointer is
//! on the thing that asked for it or on the panel itself, and closes a moment after it
//! leaves both. The icon-only rail is drawn inside the rectangle the panel takes, so the
//! pointer that opens it is already on the panel: that state records the panel's hover, and
//! only the toggle needs a region of its own.
//!
//! Two hover regions are tracked separately rather than inferred from listener order: the
//! toggle and the panel. GPUI dispatches a leaving region and an entering one in whichever
//! order the two elements were painted, so a close scheduled by the first listener must see
//! the second region already holding the pointer.
//!
//! A menu an item of the panel opened is not a hover region: it is a context menu owned by
//! the window layer, and the window holds the panel up while the layer has one open. The
//! layer, and the next pointer move after that menu closes, are what decide when the panel
//! goes away; this state machine only knows the pointer on the panel and on the toggle.
//!
//! A press does not hold the preview: clicking one of its controls is not a reason to keep
//! the panel after the pointer leaves. The context menu that press opens is a window-level
//! surface, and the window layer is what holds the panel while the menu is up. An alert the
//! press opens belongs to the window rather than to the panel, so it does not hold the
//! preview either. The next mode change or close forgets the whole state.
//!
//! How the panel comes and goes is the window's business: this state machine says whether it
//! is up, and the window draws the movement that opening and closing ask for.

use std::time::Duration;

/// How long the floating sidebar stays up after the pointer leaves it.
///
/// It is not closed at once because the pointer has to be able to travel from the toggle to
/// the panel, and every point of that journey is on neither end of it.
pub(crate) const FLOATING_CLOSE_DELAY: Duration = Duration::from_millis(220);

/// The floating sidebar's visibility and the hover regions that keep it up.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct FloatingSidebar {
    open: bool,
    toggle_hovered: bool,
    panel_hovered: bool,
}

impl FloatingSidebar {
    /// Whether the panel is drawn.
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Records whether the pointer is on the title bar's toggle.
    ///
    /// Returns whether a close should be scheduled: the pointer left one of the two
    /// regions, and the other does not hold it.
    pub(crate) fn hover_toggle(&mut self, hovered: bool) -> bool {
        self.toggle_hovered = hovered;
        if hovered {
            self.open = true;
        }

        self.should_close()
    }

    /// Records whether the pointer is on the panel itself, with the same answer as
    /// [`Self::hover_toggle`].
    pub(crate) fn hover_panel(&mut self, hovered: bool) -> bool {
        self.panel_hovered = hovered;
        if hovered {
            self.open = true;
        }

        self.should_close()
    }

    /// Closes the panel, reporting whether it was open.
    ///
    /// The hover flags reset with it: the press that dismissed the panel is not a pointer
    /// resting on any of the regions that would keep it up.
    pub(crate) fn dismiss(&mut self) -> bool {
        let open = self.open;
        self.reset();

        open
    }

    /// Whether the pointer has left both regions.
    pub(crate) fn should_close(&self) -> bool {
        self.open && !self.toggle_hovered && !self.panel_hovered
    }

    /// Forgets the panel: it is closed and no region holds the pointer.
    ///
    /// A close that ran out its delay and a mode change both come here: the state a new
    /// opening starts from is the one nothing of the old panel survives in.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::FloatingSidebar;

    /// The pointer on the toggle asks for the panel, and takes it down when it leaves.
    #[test]
    fn hovering_the_toggle_should_open_the_panel_and_leaving_it_should_schedule_the_close() {
        let mut floating = FloatingSidebar::default();

        assert!(!floating.hover_toggle(true), "entering asks for no close");
        assert!(floating.is_open(), "the toggle opens the floating panel");

        assert!(
            floating.hover_toggle(false),
            "leaving the only hovered part has to schedule a close"
        );
        assert!(
            floating.should_close(),
            "nothing holds the panel after the pointer left"
        );
    }

    /// The pointer on the panel itself asks for it as much as the pointer on the toggle
    /// does: the panel is what the user travelled to.
    #[test]
    fn hovering_the_panel_should_open_it() {
        let mut floating = FloatingSidebar::default();

        assert!(!floating.hover_panel(true));
        assert!(floating.is_open());

        assert!(
            floating.hover_panel(false),
            "leaving the panel has to schedule a close"
        );
        assert!(floating.should_close());
    }

    /// The journey from the toggle to the panel has a gap between them; entering the panel
    /// inside the delay calls the close off.
    #[test]
    fn entering_the_panel_should_cancel_the_close_the_toggle_scheduled() {
        let mut floating = FloatingSidebar::default();

        floating.hover_toggle(true);
        assert!(floating.hover_toggle(false), "the gap schedules a close");

        assert!(
            !floating.hover_panel(true),
            "the panel the pointer reached cancels the close"
        );
        assert!(floating.is_open());
        assert!(!floating.should_close());
    }

    /// Moving from the panel back to the toggle dispatches a leave and an enter in the same
    /// mouse move. GPUI may run either listener first; neither order may schedule a close
    /// that survives both listeners.
    #[test]
    fn moving_from_the_panel_to_the_toggle_should_keep_it_open_in_either_event_order() {
        // The panel's `hover(false)` runs first: it schedules the close, and the toggle's
        // `hover(true)` that follows has to call it off.
        let mut panel_first = FloatingSidebar::default();
        panel_first.hover_panel(true);
        assert!(
            panel_first.hover_panel(false),
            "the panel being left schedules the close"
        );
        assert!(
            !panel_first.hover_toggle(true),
            "the toggle under the pointer cancels it"
        );
        assert!(panel_first.is_open());
        assert!(!panel_first.should_close());

        // The toggle's `hover(true)` runs first: the panel's leave that follows must see
        // the toggle as hovered and schedule nothing.
        let mut toggle_first = FloatingSidebar::default();
        toggle_first.hover_panel(true);
        assert!(
            !toggle_first.hover_toggle(true),
            "the toggle under the pointer schedules nothing"
        );
        assert!(
            !toggle_first.hover_panel(false),
            "the panel being left is still held by the toggle"
        );
        assert!(toggle_first.is_open());
        assert!(!toggle_first.should_close());
    }

    /// The other half of the same journey: moving from the toggle onto the panel.
    #[test]
    fn moving_from_the_toggle_to_the_panel_should_keep_it_open_in_either_event_order() {
        // The toggle's `hover(false)` runs first: the leave schedules the close, and the
        // panel's `hover(true)` that follows has to call it off.
        let mut toggle_first = FloatingSidebar::default();
        toggle_first.hover_toggle(true);
        assert!(
            toggle_first.hover_toggle(false),
            "the toggle being left schedules the close"
        );
        assert!(
            !toggle_first.hover_panel(true),
            "the panel under the pointer cancels it"
        );
        assert!(toggle_first.is_open());
        assert!(!toggle_first.should_close());

        // The panel's `hover(true)` runs first: the toggle's leave that follows must see
        // the panel as hovered and schedule nothing.
        let mut panel_first = FloatingSidebar::default();
        panel_first.hover_toggle(true);
        assert!(
            !panel_first.hover_panel(true),
            "the panel under the pointer schedules nothing"
        );
        assert!(
            !panel_first.hover_toggle(false),
            "the toggle being left is still held by the panel"
        );
        assert!(panel_first.is_open());
        assert!(!panel_first.should_close());
    }

    /// A press outside the panel dismisses it, and it can be floated again. The dismissal
    /// leaves no hover behind that would hold the next preview up.
    #[test]
    fn dismissing_should_close_the_panel() {
        let mut floating = FloatingSidebar::default();

        floating.hover_toggle(true);
        floating.hover_panel(true);

        assert!(floating.dismiss(), "the panel that was up reports it");
        assert!(!floating.is_open());
        assert!(!floating.should_close());

        floating.hover_panel(true);
        assert!(floating.is_open());
        assert!(!floating.should_close(), "the panel is up because hovered");
        assert!(
            floating.hover_panel(false),
            "the next leave closes the panel the dismissal made fresh"
        );
    }

    /// A press on the title bar over the open panel is dispatched to the title bar, which
    /// dismisses the panel. The next hover is an ordinary preview that the pointer's leave
    /// closes.
    #[test]
    fn a_title_bar_press_over_the_panel_should_leave_it_closed() {
        let mut floating = FloatingSidebar::default();
        floating.hover_toggle(true);
        assert!(floating.is_open());

        assert!(
            floating.dismiss(),
            "the title bar press dismisses the panel"
        );
        assert!(
            !floating.is_open(),
            "the overlap press leaves the panel closed"
        );

        // The next hover opens the ordinary preview, and the leave after it closes it.
        assert!(!floating.hover_toggle(true));
        assert!(floating.is_open());
        assert!(!floating.should_close());
        assert!(
            floating.hover_toggle(false),
            "the preview the overlap press left behind closes when the pointer leaves"
        );
        assert!(floating.should_close());
    }

    /// The mode change and the close timer both forget the whole state.
    #[test]
    fn resetting_should_close_the_panel_and_forget_its_hover() {
        let mut floating = FloatingSidebar::default();
        floating.hover_toggle(true);
        floating.hover_panel(true);

        floating.reset();
        assert_eq!(floating, FloatingSidebar::default());
        assert!(!floating.is_open());
        assert!(!floating.should_close());

        // Nothing of the old panel keeps a new one up.
        floating.hover_toggle(true);
        assert!(!floating.should_close());
        assert!(
            floating.hover_toggle(false),
            "the new panel is held by its own hover alone"
        );
        assert!(floating.should_close());
    }

    /// A pointer that never opened the panel cannot schedule a close for it.
    #[test]
    fn leaving_before_the_panel_was_opened_should_not_schedule_a_close() {
        let mut floating = FloatingSidebar::default();

        assert!(!floating.hover_toggle(false));
        assert!(!floating.hover_panel(false));
        assert!(!floating.should_close());
    }
}

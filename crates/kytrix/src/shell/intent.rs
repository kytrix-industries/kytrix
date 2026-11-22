//! What a control asks the shell to do, and the queue those requests travel in.
//!
//! The shell's views are built before the shell itself is behind an `Rc`, so a control's
//! click handler cannot call back into the shell: it reports an [`Intent`] and the shell's
//! frame route drains the queue once per frame, at the top of the tick, and applies each
//! request against its own state. That is also what keeps every mutation of the shell in
//! one place (the route), which is what makes the order of "apply the user's command, then
//! lay the window out, then draw it" a property of the code rather than of a callback's
//! arrival order.

use std::collections::VecDeque;

use smol_str::SmolStr;
use ui_core::{interaction::DragPhase, math::Point, window::WindowControl};

use super::workspace::tree::{PaneId, PaneOrientation, SplitId};

/// One request from a control to the shell.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Intent {
    /// Walk the sidebar to its next state and persist it.
    ToggleSidebar,
    /// Show the settings page in the page area.
    ShowSettings,

    /// Switch to the named workspace.
    SelectWorkspace(SmolStr),
    /// Ask for the name of a new workspace.
    AddWorkspace,
    /// Ask for the new name of the named workspace.
    RenameWorkspace(SmolStr),
    /// Ask whether the named workspace should be deleted.
    DeleteWorkspace(SmolStr),
    /// The naming alert's answer: create a workspace with this name.
    CreateWorkspace(SmolStr),
    /// The renaming alert's answer: rename `from` to `to`.
    ApplyRename { from: SmolStr, to: SmolStr },
    /// The delete confirmation's answer: delete this workspace.
    ConfirmDelete(SmolStr),
    /// Minimize the window.
    Minimize,
    /// Toggle the window's maximized state.
    ToggleMaximize,
    /// Close the window.
    CloseWindow,
    /// Split a pane, keeping the new pane on the head or the tail side.
    SplitPane {
        pane: PaneId,
        orientation: PaneOrientation,
        target_on_head: bool,
    },
    /// Close a pane (the last pane of a workspace cannot be closed).
    ClosePane(PaneId),
    /// Open the widget gallery over the workspace for this pane.
    OpenGallery(PaneId),
    /// The gallery's answer: show this widget in the pane.
    ChooseWidget { pane: PaneId, widget: SmolStr },

    /// A split bar's drag moved.
    SplitDrag {
        split: SplitId,
        phase: DragPhase,
        delta: Point,
    },
}

/// The queue a control reports into and the shell drains once per frame.
#[derive(Default)]
pub(crate) struct IntentQueue {
    queue: VecDeque<Intent>,
}

impl Intent {
    /// The window control this intent asks the platform for, when it asks for one.
    pub(crate) const fn window_control(&self) -> Option<WindowControl> {
        match self {
            Self::Minimize => Some(WindowControl::Minimize),
            Self::ToggleMaximize => Some(WindowControl::ToggleMaximize),
            _ => None,
        }
    }
}

impl IntentQueue {
    /// Queues one request.
    pub(crate) fn push(&mut self, intent: Intent) {
        self.queue.push_back(intent);
    }

    /// Takes everything queued so far.
    pub(crate) fn drain(&mut self) -> Vec<Intent> {
        self.queue.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The queue preserves the order the user's controls reported in, and draining empties
    /// it: the next frame sees only new requests.
    #[test]
    fn the_queue_should_preserve_order_and_empty_on_drain() {
        let mut queue = IntentQueue::default();
        queue.push(Intent::ShowSettings);
        queue.push(Intent::ToggleSidebar);

        assert_eq!(
            queue.drain(),
            vec![Intent::ShowSettings, Intent::ToggleSidebar]
        );
        assert!(queue.drain().is_empty());
    }
}

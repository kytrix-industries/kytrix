use std::collections::VecDeque;

use smol_str::SmolStr;
use ui_core::{interaction::DragPhase, math::Point, window::WindowControl};

use super::workspace::tree::{PaneId, PaneOrientation, SplitId};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Intent {
    ToggleSidebar,
    ShowSettings,

    SelectWorkspace(SmolStr),
    AddWorkspace,
    RenameWorkspace(SmolStr),
    DeleteWorkspace(SmolStr),
    CreateWorkspace(SmolStr),
    ApplyRename {
        from: SmolStr,
        to: SmolStr,
    },
    ConfirmDelete(SmolStr),
    Minimize,
    ToggleMaximize,
    CloseWindow,
    SplitPane {
        pane: PaneId,
        orientation: PaneOrientation,
        target_on_head: bool,
    },
    ClosePane(PaneId),
    OpenGallery(PaneId),
    ChooseWidget {
        pane: PaneId,
        widget: SmolStr,
    },

    SplitDrag {
        split: SplitId,
        phase: DragPhase,
        delta: Point,
    },
}

#[derive(Default)]
pub(crate) struct IntentQueue {
    queue: VecDeque<Intent>,
}

impl Intent {
    pub(crate) const fn window_control(&self) -> Option<WindowControl> {
        match self {
            Self::Minimize => Some(WindowControl::Minimize),
            Self::ToggleMaximize => Some(WindowControl::ToggleMaximize),
            _ => None,
        }
    }
}

impl IntentQueue {
    pub(crate) fn push(&mut self, intent: Intent) {
        self.queue.push_back(intent);
    }

    pub(crate) fn drain(&mut self) -> Vec<Intent> {
        self.queue.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

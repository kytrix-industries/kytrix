//! Placeholders for the components the workspace needs but that are not migrated yet.
//!
//! Each type mirrors the API of the component it stands in for, so the workspace call
//! sites keep working unchanged once the real component lands in `base_ui`.
//! They deliberately render no interactive surface: the linkage selector and the
//! separators currently have no effect. [`InstrumentLinkMode`] is not a
//! placeholder — it is the real linkage state the workspace tracks and will move to
//! `ui` with the selector.

use gpui_kit::{App, ElementId, Empty, IntoElement, RenderOnce, Window};

/// How a pane follows another pane's instrument selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum InstrumentLinkMode {
    #[default]
    Unlinked,
    Group(u8),
    FollowMouse,
}

impl InstrumentLinkMode {
    pub(crate) fn group_index(self) -> Option<usize> {
        match self {
            Self::Group(group @ 1..=9) => Some(usize::from(group - 1)),
            _ => None,
        }
    }
}

/// Placeholder for `base_ui::instrument_linker::InstrumentLinker`.
///
/// Renders nothing and never reports a mode change, so the pane linkage slot stays
/// empty until the selector is migrated.
#[derive(IntoElement)]
pub(crate) struct InstrumentLinker;

impl InstrumentLinker {
    pub(crate) fn new(_id: impl Into<ElementId>, _mode: InstrumentLinkMode) -> Self {
        Self
    }

    pub(crate) fn on_change(
        self,
        _handler: impl Fn(InstrumentLinkMode, &mut Window, &mut App) + 'static,
    ) -> Self {
        self
    }
}

impl RenderOnce for InstrumentLinker {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        Empty
    }
}

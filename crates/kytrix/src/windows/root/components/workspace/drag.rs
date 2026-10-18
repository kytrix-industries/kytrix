//! Drag-and-drop of whole panes, including the floating preview and the drop
//! target indicator.

use gpui_kit::*;

use super::{
    pane::{Pane, default_widget},
    stage::Stage,
    tree::{DropTargetPosition, PaneNode},
};

#[derive(Clone)]
struct DragPaneView {
    pane: Pane,
    stage: Entity<Stage>,
    hitbox_offset: Point<Pixels>,
    mouse_position: Point<Pixels>,
}

impl DragPaneView {
    fn new(pane: Pane, stage: Entity<Stage>) -> Self {
        Self {
            pane,
            stage,
            hitbox_offset: Default::default(),
            mouse_position: Default::default(),
        }
    }

    fn position(mut self, hitbox_offset: Point<Pixels>, mouse_position: Point<Pixels>) -> Self {
        self.hitbox_offset = hitbox_offset;
        self.mouse_position = mouse_position;
        self
    }
}

impl Render for DragPaneView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (pane_bounds, stage_bounds) = {
            let tree = self.stage.read(cx).pane_tree();

            let Some(pane_bounds) = tree.pane_bounds(&self.pane.id) else {
                log::error!("drag pane is missing or has no bounds in pane node tree");
                return Empty.into_any_element();
            };
            let Some(stage_bounds) = tree.root().bounds().copied() else {
                log::error!("workspace root has no bounds in pane node tree");
                return Empty.into_any_element();
            };

            (pane_bounds, stage_bounds)
        };

        let mouse_offset_in_pane = self.mouse_position - pane_bounds.origin;
        let pane_offset = self.hitbox_offset - mouse_offset_in_pane;
        let drag_root_origin = window.mouse_position() - self.hitbox_offset;
        let shield_offset = stage_bounds.origin - drag_root_origin;
        let stage = self.stage.clone();

        div()
            .debug_selector(|| "PANE_DRAG_GHOST".into())
            .child(
                div()
                    .relative()
                    .left(pane_offset.x)
                    .top(pane_offset.y)
                    .w(pane_bounds.size.width)
                    .h(pane_bounds.size.height)
                    .child(self.pane.clone()),
            )
            .child(
                div()
                    .absolute()
                    .left(shield_offset.x)
                    .top(shield_offset.y)
                    .w(stage_bounds.size.width)
                    .h(stage_bounds.size.height)
                    .occlude()
                    .on_drop(move |dragged: &PaneDragSource, _, cx| {
                        stage.update(cx, |stage, cx| {
                            stage.commit_drop(dragged, cx);
                        });
                    }),
            )
            .into_any_element()
    }
}

pub(in crate::windows::root) struct PaneDragSource {
    pub pane_id: ElementId,
    stage: Entity<Stage>,
}

impl PaneDragSource {
    fn exists(&self, cx: &App) -> bool {
        self.stage
            .read(cx)
            .pane_tree()
            .root
            .find_pane(&self.pane_id)
            .is_some()
    }

    fn pane(&self, cx: &App) -> Option<Pane> {
        pane_for_drag(&self.pane_id, &self.stage, cx)
    }
}

#[derive(IntoElement)]
pub(in crate::windows::root) struct PaneDragRegion {
    id: ElementId,
    pane_id: ElementId,
    stage: Entity<Stage>,
}

impl PaneDragRegion {
    pub(in crate::windows::root) fn new(
        id: ElementId,
        pane_id: ElementId,
        stage: Entity<Stage>,
    ) -> Self {
        Self { id, pane_id, stage }
    }
}

impl RenderOnce for PaneDragRegion {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let stage = self.stage.clone();
        let drag_source = PaneDragSource {
            pane_id: self.pane_id.clone(),
            stage: stage.clone(),
        };
        if !drag_source.exists(cx) {
            return Empty.into_any_element();
        }

        div()
            .id(self.id)
            .h_full()
            .min_w_0()
            .flex_1()
            .cursor_grab()
            .on_drag(drag_source, start_pane_drag)
            .on_drag_move(move |event: &DragMoveEvent<PaneDragSource>, _, cx| {
                update_pane_drag_preview(event, &stage, cx);
            })
            .into_any_element()
    }
}

fn pane_for_drag(pane_id: &ElementId, stage: &Entity<Stage>, cx: &App) -> Option<Pane> {
    let PaneNode::Leaf { pane, .. } = stage.read(cx).pane_tree().root.find_pane(pane_id)? else {
        return None;
    };
    Some(pane.as_ref().clone())
}

fn start_pane_drag(
    drag_source: &PaneDragSource,
    hitbox_offset: Point<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<DragPaneView> {
    let stage = drag_source.stage.clone();
    let pane = drag_source.pane(cx).unwrap_or_else(|| {
        log::error!(
            "drag source pane `{}` disappeared before drag started",
            drag_source.pane_id
        );
        // The pane the drag started from is gone, so there is no widget left to
        // draw but the one a pane falls back to.
        let widget = default_widget(&drag_source.pane_id, &stage.downgrade(), cx);
        Pane::build(
            drag_source.pane_id.clone(),
            widget,
            stage.clone(),
            super::placeholder::InstrumentLinkMode::Unlinked,
            true,
            false,
            cx,
        )
    });
    let pane = pane.detached_for_drag(stage.clone(), cx);
    let mouse_position = window.mouse_position();
    let preview =
        cx.new(|_| DragPaneView::new(pane, stage.clone()).position(hitbox_offset, mouse_position));
    // The preview is the only copy of the pane from here on: the pane keeps its slot in the
    // layout, but nothing is painted there until the preview is released.
    stage.update(cx, |stage, cx| {
        stage.begin_pane_drag(&drag_source.pane_id, &preview, cx);
    });

    preview
}

fn update_pane_drag_preview(
    event: &DragMoveEvent<PaneDragSource>,
    stage: &Entity<Stage>,
    cx: &mut App,
) {
    let source_pane_id = event.drag(cx).pane_id.clone();

    stage.update(cx, |stage, cx| {
        let mouse_position = event.event.position;
        let drop_target_preview = stage
            .pane_tree()
            .drop_target_leaf_at(mouse_position, &source_pane_id)
            .map(|(node_id, bounds)| (node_id, calc_drop_target_position(&bounds, mouse_position)));

        if stage.drop_target_preview() != drop_target_preview {
            stage.set_drop_target_preview(drop_target_preview);
            cx.notify();
        }
    });
}

fn calc_drop_target_position(
    pane_bounds: &Bounds<Pixels>,
    mouse_position: Point<Pixels>,
) -> DropTargetPosition {
    let left_bounds = bounds(
        pane_bounds.origin,
        size(pane_bounds.size.width / 3.0, pane_bounds.size.height),
    );

    if left_bounds.contains(&mouse_position) {
        return DropTargetPosition::Left;
    }

    let right_bounds = bounds(
        point(
            pane_bounds.origin.x + 2 * pane_bounds.size.width / 3.0,
            pane_bounds.origin.y,
        ),
        size(pane_bounds.size.width / 3.0, pane_bounds.size.height),
    );

    if right_bounds.contains(&mouse_position) {
        return DropTargetPosition::Right;
    }

    let top_bounds = bounds(
        pane_bounds.origin,
        size(pane_bounds.size.width, pane_bounds.size.height / 3.0),
    );

    if top_bounds.contains(&mouse_position) {
        return DropTargetPosition::Top;
    }

    let bottom_bounds = bounds(
        point(
            pane_bounds.origin.x,
            pane_bounds.origin.y + 2 * pane_bounds.size.height / 3.0,
        ),
        size(pane_bounds.size.width, pane_bounds.size.height / 3.0),
    );

    if bottom_bounds.contains(&mouse_position) {
        return DropTargetPosition::Bottom;
    }

    DropTargetPosition::Center
}

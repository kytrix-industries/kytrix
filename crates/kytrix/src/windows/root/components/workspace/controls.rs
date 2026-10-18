//! Pane chrome controls: the widget selector, the split menu, the close button,
//! and the split resize bar.

use base_ui::{
    button::Button,
    dropdown::Dropdown,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    menu::{MenuItem, MenuItemIconAction},
    size::{UnifiedSizable, UnifiedSize},
    style::theme::ThemeExt,
    tooltip::TooltipExt,
};
use gpui_kit::{
    App, AppContext, Context, DragMoveEvent, ElementId, Empty, Entity, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Pixels, Point, Render, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, Window, base::Align, div, prelude::FluentBuilder, px,
};
use i18n::t;
use smol_str::format_smolstr;
use widget_gallery_ui::gallery::show_widget_gallery;

use super::{
    stage::Stage,
    tree::{NodeId, PaneOrientation},
};

/// Width of the instrument linkage slot at the start of a pane's header row.
pub(in crate::windows::root) const CHROME_LINK_WIDTH: Pixels = px(32.0);

/// Item ids of the pane split menu.
const SPLIT_KEEP_LEFT: &str = "split-keep-left";
const SPLIT_KEEP_RIGHT: &str = "split-keep-right";
const SPLIT_KEEP_TOP: &str = "split-keep-top";
const SPLIT_KEEP_BOTTOM: &str = "split-keep-bottom";

#[derive(IntoElement)]
pub(in crate::windows::root) struct PaneWidgetButton {
    id: ElementId,
    pane_id: ElementId,
    stage: Entity<Stage>,
}

impl PaneWidgetButton {
    pub(in crate::windows::root) fn new(pane_id: ElementId, stage: Entity<Stage>) -> Self {
        Self {
            id: (pane_id.clone(), "PaneWidgetButton").into(),
            pane_id,
            stage,
        }
    }
}

impl RenderOnce for PaneWidgetButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        Button::new(self.id)
            .child(Icon::new(IconKind::PuzzlePiece).size_3())
            .ghost()
            .with_unified_size(UnifiedSize::Small)
            .aria_label(t!(cx, "window.pane.select_widget"))
            .with_tooltip(t!(cx, "window.pane.select_widget"))
            .on_click({
                let stage = self.stage;
                let pane_id = self.pane_id;
                move |_, window, cx| {
                    // The gallery opens on what the pane is showing, so the dialog the
                    // user gets is the one that already has their answer in it.
                    let current = stage.read(cx).pane_widget_id(&pane_id, cx);
                    let stage_for_choice = stage.downgrade();
                    let pane_for_choice = pane_id.clone();

                    show_widget_gallery(
                        current,
                        move |widget_id, _, cx| {
                            // The gallery outlives nothing: if the stage has gone the pane it
                            // named has gone with it, so there is nothing left to choose into.
                            let _ = stage_for_choice.update(cx, |stage, cx| {
                                stage.set_widget(&pane_for_choice, widget_id.as_str(), cx);
                            });
                        },
                        window,
                        cx,
                    );
                }
            })
    }
}

#[derive(Clone)]
struct ResizeBarDragInfo {
    resize_bar_id: ElementId,
    position: Point<Pixels>,
}

impl ResizeBarDragInfo {
    fn position(mut self, position: Point<Pixels>) -> ResizeBarDragInfo {
        self.position = position;
        self
    }
}

impl Render for ResizeBarDragInfo {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[derive(IntoElement)]
pub(in crate::windows::root) struct ResizeBar {
    id: ElementId,
    group_id: SharedString,
    stage: Entity<Stage>,
    orientation: PaneOrientation,
    split_id: NodeId,
}

impl ResizeBar {
    pub(in crate::windows::root) fn new(
        parent_id: ElementId,
        stage: Entity<Stage>,
        orientation: PaneOrientation,
        split_id: NodeId,
    ) -> Self {
        let id = ElementId::from((parent_id, format_smolstr!("ResizeBar{split_id}").as_str()));
        let group_id = SharedString::from(format_smolstr!("{id}Hover").as_str());

        Self {
            id,
            group_id,
            stage,
            orientation,
            split_id,
        }
    }
}

impl RenderOnce for ResizeBar {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        // The bar is a hit area, not a painted control: it is invisible until the pointer is on
        // it, where it takes the brand color, and one step deeper while the pointer holds it. Both
        // are the brand scale's own steps, so the two states follow the appearance the way every
        // other control does. Held is latched on the press instead of taken from GPUI's active
        // state, because that state is the click state: GPUI clears it the moment the press
        // becomes the drag this bar starts, which is the whole gesture here. Hover cannot stand in
        // for it either -- a held bar stops following the pointer once the split reaches a pane's
        // minimum width, and the pointer leaves the band with it.
        let hover_color = cx.theme().roles.brand.base.hsla;
        let held_color = cx.theme().roles.brand.active.hsla;
        let held = self.stage.read(cx).held_split() == Some(self.split_id);
        let bar_id = (self.id.clone(), "Bar");

        let line = div()
            .id(bar_id)
            .when_else(
                matches!(self.orientation, PaneOrientation::Horizontal),
                |this| this.w(px(2.5)).h_full(),
                |this| this.h(px(2.5)).w_full(),
            )
            .when(held, |this| this.bg(held_color))
            .when(!held, |this| {
                this.group_hover(self.group_id.clone(), |s| s.bg(hover_color))
            });

        div()
            .id(self.id.clone())
            .size_full()
            .occlude()
            .flex()
            .items_center()
            .when_else(
                matches!(self.orientation, PaneOrientation::Horizontal),
                |this| {
                    this.cursor_ew_resize()
                        .group(self.group_id.clone())
                        .flex_col()
                },
                |this| {
                    this.cursor_ns_resize()
                        .group(self.group_id.clone())
                        .flex_row()
                },
            )
            .child(line)
            .on_mouse_down(MouseButton::Left, {
                let stage = self.stage.clone();
                let split_id = self.split_id;
                move |_, _, cx| {
                    stage.update(cx, |stage, cx| stage.hold_split(split_id, cx));
                }
            })
            .on_mouse_up(MouseButton::Left, {
                let stage = self.stage.clone();
                let split_id = self.split_id;
                move |_, _, cx| {
                    stage.update(cx, |stage, cx| stage.release_split(split_id, cx));
                }
            })
            .on_mouse_up_out(MouseButton::Left, {
                let stage = self.stage.clone();
                let split_id = self.split_id;
                move |_, _, cx| {
                    stage.update(cx, |stage, cx| stage.release_split(split_id, cx));
                }
            })
            .on_drag(
                ResizeBarDragInfo {
                    resize_bar_id: self.id.clone(),
                    position: Default::default(),
                },
                |resize_bar_drag_info, position, _, cx| {
                    cx.new(|_| resize_bar_drag_info.clone().position(position))
                },
            )
            .on_drag_move({
                let id = self.id;
                let stage = self.stage;
                let split_id = self.split_id;
                move |event: &DragMoveEvent<ResizeBarDragInfo>, _, cx| {
                    let Some(resize_bar_drag_info) =
                        event.dragged_item().downcast_ref::<ResizeBarDragInfo>()
                    else {
                        log::error!("downcast drag entity to ResizeBarDragInfo failed");
                        return;
                    };

                    if id != resize_bar_drag_info.resize_bar_id {
                        return;
                    }

                    stage.update(cx, |stage, cx| {
                        stage.resize_split(
                            split_id,
                            resize_bar_drag_info.position,
                            event.event.position,
                            cx,
                        );
                    });
                }
            })
    }
}

#[derive(IntoElement)]
pub(in crate::windows::root) struct PaneSplitButton {
    id: ElementId,
    pane_id: ElementId,
    stage: Entity<Stage>,
}

impl PaneSplitButton {
    pub(in crate::windows::root) fn new(parent_id: ElementId, stage: Entity<Stage>) -> Self {
        Self {
            id: (parent_id.clone(), "PaneSplitButton").into(),
            pane_id: parent_id,
            stage,
        }
    }
}

impl RenderOnce for PaneSplitButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        Dropdown::new(self.id.clone())
            .ghost()
            .child(Icon::new(IconKind::SquareHalf).size_3())
            .small()
            .align(Align::Start)
            .with_tooltip(t!(cx, "window.pane.split"))
            .menu_item(MenuItem::IconAction(MenuItemIconAction::new(
                SPLIT_KEEP_LEFT,
                IconKind::ColumnsPlusRight,
                t!(cx, "window.pane.split_keep_left"),
            )))
            .menu_item(MenuItem::IconAction(MenuItemIconAction::new(
                SPLIT_KEEP_RIGHT,
                IconKind::ColumnsPlusLeft,
                t!(cx, "window.pane.split_keep_right"),
            )))
            .menu_item(MenuItem::IconAction(MenuItemIconAction::new(
                SPLIT_KEEP_TOP,
                IconKind::RowsPlusBottom,
                t!(cx, "window.pane.split_keep_top"),
            )))
            .menu_item(MenuItem::IconAction(MenuItemIconAction::new(
                SPLIT_KEEP_BOTTOM,
                IconKind::RowsPlusTop,
                t!(cx, "window.pane.split_keep_bottom"),
            )))
            .on_menu_item_click({
                let stage = self.stage;
                let pane_id = self.pane_id;
                move |_, item_id, _, cx| {
                    let (orientation, target_on_head) = match item_id.as_ref() {
                        SPLIT_KEEP_LEFT => (PaneOrientation::Horizontal, true),
                        SPLIT_KEEP_RIGHT => (PaneOrientation::Horizontal, false),
                        SPLIT_KEEP_TOP => (PaneOrientation::Vertical, true),
                        SPLIT_KEEP_BOTTOM => (PaneOrientation::Vertical, false),
                        _ => return,
                    };
                    stage.update(cx, |stage, cx| {
                        stage.split_pane(&pane_id, orientation, target_on_head, cx);
                    });
                }
            })
    }
}

#[derive(IntoElement)]
pub(in crate::windows::root) struct PaneCloseButton {
    id: ElementId,
    pane_id: ElementId,
    stage: Entity<Stage>,
}

impl PaneCloseButton {
    pub(in crate::windows::root) fn new(parent_id: ElementId, stage: Entity<Stage>) -> Self {
        Self {
            id: (parent_id.clone(), "PaneCloseButton").into(),
            pane_id: parent_id,
            stage,
        }
    }
}

impl RenderOnce for PaneCloseButton {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        Button::new(self.id)
            .child(Icon::new(IconKind::X).size_3())
            .ghost()
            .with_unified_size(UnifiedSize::Small)
            .aria_label(t!(cx, "window.pane.close"))
            .with_tooltip(t!(cx, "window.pane.close"))
            .on_click({
                let pane_id = self.pane_id;
                move |_, _, cx| {
                    self.stage
                        .update(cx, |stage, cx| stage.close_pane(&pane_id, cx));
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, time::Duration};

    use app_config::workspace::WorkspaceLayout;
    use base_ui::{animation::ANIMATION_TICK, layer::Layer};
    use gpui_kit::{
        App, AppContext, Context, ElementId, Entity, IntoElement, Modifiers, MouseButton,
        ParentElement, Render, RenderOnce as _, Styled, TestAppContext, VisualTestContext,
        WeakEntity, Window, div, point, px, size,
    };
    use widget::{
        category::WidgetCategory,
        config::WidgetConfigChange,
        identity::{WidgetId, WidgetInstanceId},
        status::{WidgetAction, WidgetStatus},
        view::WidgetView,
    };
    use widget_registry::registry::{WidgetRegistration, WidgetRegistry};

    use super::PaneWidgetButton;
    use crate::windows::root::components::workspace::{
        stage::Stage,
        tree::{NodeId, PaneNode, PaneOrientation},
    };

    /// The debug selectors of the gallery's own panel and confirm control.
    ///
    /// The gallery belongs to another crate and proves nothing about itself here, so what a test
    /// outside it can see is its own selectors — and seeing them is what makes the drops below
    /// evidence rather than accidents.
    const GALLERY_PANEL: &str = "WIDGET_GALLERY";
    const GALLERY_CONFIRM: &str = "WIDGET_GALLERY_CONFIRM";

    /// A widget that draws nothing, so the pane's own control and the gallery over it are the only
    /// things a test has to account for.
    struct TestWidget;

    impl WidgetView for TestWidget {
        type Config = ();

        const ID: WidgetId = WidgetId::new_static("kytrix.test.gallery-control");
        const DISPLAY_NAME: &'static str = "widget.names.candlestick";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: Entity<WidgetStatus>,
            _reload: impl Fn(&mut App) + 'static,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut Context<Self>,
        ) -> Self {
            Self
        }

        fn load(&mut self, _cx: &mut Context<Self>) {}

        fn has_content(&self) -> bool {
            false
        }
    }

    impl Render for TestWidget {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn install(cx: &mut App) {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("embedded locales should load");
        widget_registry::catalogue::install(cx).expect("the catalogue should install");
        cx.global_mut::<WidgetRegistry>()
            .register(WidgetRegistration::for_view::<TestWidget>())
            .expect("the test widget should register");
    }

    /// A stage with one pane showing the test widget, and the id of that pane.
    ///
    /// The pane shows a widget because that is what the gallery opens on: a pane showing nothing
    /// opens on a category with nothing selected, and a gallery with nothing selected refuses the
    /// confirmation this test is about.
    fn stage_with_a_widget(cx: &mut TestAppContext) -> (Entity<Stage>, ElementId) {
        cx.update(install);
        let stage =
            cx.update(|cx| cx.new(|cx| Stage::from_layout(1, &WorkspaceLayout::single_pane(), cx)));
        let pane_id = cx.update(|cx| {
            let mut ids = Vec::with_capacity(1);
            stage
                .read(cx)
                .pane_tree()
                .root
                .for_each_pane(&mut |pane| ids.push(pane.id.clone()));
            let pane_id = ids.pop().expect("a single pane workspace holds one pane");

            stage.update(cx, |stage, cx| {
                stage.set_widget(&pane_id, TestWidget::ID.as_str(), cx);
            });

            pane_id
        });

        (stage, pane_id)
    }

    /// A stage with two panes side by side, and the id of the split between them.
    fn split_stage(cx: &mut TestAppContext) -> (Entity<Stage>, NodeId) {
        let (stage, pane_id) = stage_with_a_widget(cx);
        let split_id = cx.update(|cx| {
            stage.update(cx, |stage, cx| {
                stage.split_pane(&pane_id, PaneOrientation::Horizontal, true, cx);
            });
            let PaneNode::Split { id, .. } = &stage.read(cx).pane_tree().root else {
                panic!("splitting the only pane should leave a split at the root");
            };
            *id
        });

        (stage, split_id)
    }

    /// The window root the gallery test draws into: the pane's own widget control, with none of the
    /// pane around it.
    ///
    /// The control is drawn only while `showing` says so. A control holds its stage — the
    /// application's own is drawn out of the stage's tree, and a control that let its workspace go
    /// would be a control reading a workspace that is already gone — so the test takes it out of the
    /// tree once the gallery is up, and what is left that could hold the stage is the dialog.
    struct ButtonHarness {
        pane_id: ElementId,
        stage: WeakEntity<Stage>,
        showing: Rc<Cell<bool>>,
    }

    impl Render for ButtonHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let control = self
                .showing
                .get()
                .then(|| self.stage.upgrade())
                .flatten()
                .map(|stage| PaneWidgetButton::new(self.pane_id.clone(), stage).render(window, cx));

            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .children(control)
        }
    }

    /// Lets the gallery's own fade finish and repaints, so what is drawn is what has settled.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// The gallery is handed a weak handle on the stage, so opening it is not what keeps the
    /// workspace alive, and a choice confirmed after the workspace is gone lands nowhere.
    ///
    /// The pane's control is drawn from the stage's own tree in the application, and the handle the
    /// control captures is the one the gallery keeps for as long as it is open: kept strongly, a
    /// workspace the user closed would stay alive — panes, streams and buffers — behind its dialog.
    #[gpui_kit::test]
    fn an_open_gallery_should_not_keep_the_stage_alive(cx: &mut TestAppContext) {
        let (stage, pane_id) = stage_with_a_widget(cx);
        let weak = stage.downgrade();
        let showing = Rc::new(Cell::new(true));
        let harness = cx.update(|cx| {
            cx.new(|_| ButtonHarness {
                pane_id,
                stage: weak.clone(),
                showing: showing.clone(),
            })
        });
        let (_, cx) = cx.add_window_view({
            let harness = harness.clone();
            move |_, _| Layer::new(harness)
        });
        cx.simulate_resize(size(px(1000.0), px(800.0)));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "nothing is open before the pane's control is clicked"
        );
        cx.simulate_click(point(px(500.0), px(400.0)), Modifiers::none());
        settle(cx);
        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_some(),
            "the pane's own control should have opened the gallery"
        );

        // The control leaves the tree with the stage: the gallery is left as the only thing that
        // could still be holding what the pane's own control was drawing.
        drop(stage);
        cx.update(|_, cx| {
            harness.update(cx, |_, cx| {
                showing.set(false);
                cx.notify();
            });
        });
        settle(cx);
        assert!(
            weak.upgrade().is_none(),
            "an open gallery should not be what keeps the workspace it was opened from alive"
        );

        // Confirming is the callback the control handed over, run with nothing behind it: the
        // gallery closes on a choice into a workspace that is already gone.
        let confirm = cx
            .debug_bounds(GALLERY_CONFIRM)
            .expect("the gallery should offer a way to confirm");
        cx.simulate_click(confirm.center(), Modifiers::none());
        settle(cx);
        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "confirming should have closed the gallery once the exit transition was over"
        );
        assert!(
            weak.upgrade().is_none(),
            "a confirmation made after the workspace was released should not have brought it back"
        );
    }

    /// The brand highlight a press puts on a split bar has to outlast the press becoming a drag.
    ///
    /// This is the regression: the bar painted its held state from GPUI's `group_active`, which is
    /// the click state, and GPUI clears that state the moment a press passes the 2px drag
    /// threshold. The highlight therefore went out on the first movement, with the button still
    /// down and the gesture still in progress.
    #[gpui_kit::test]
    fn a_pressed_split_bar_stays_lit_while_it_is_dragged(cx: &mut TestAppContext) {
        let (stage, split_id) = split_stage(cx);

        let (_, cx) = cx.add_window_view({
            let stage = stage.clone();
            move |_, _| Layer::new(stage)
        });
        cx.simulate_resize(size(px(1000.0), px(800.0)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        // The head child starts with half of the 996px the split has after the bar, so the 4px bar
        // straddles 500 and the point below is on it.
        cx.simulate_mouse_down(
            point(px(500.0), px(400.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).held_split()),
            Some(split_id),
            "a press has to light the bar"
        );

        // Past the threshold the press becomes a drag: the point where the click state GPUI used
        // to paint from went out and took the highlight with it.
        cx.simulate_mouse_move(point(px(520.0), px(400.0)), None, Modifiers::none());
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).held_split()),
            Some(split_id),
            "starting the drag must not put the bar out"
        );

        // The drag moves the split with the pointer, which is the resize the bar exists for.
        cx.simulate_mouse_move(point(px(700.0), px(400.0)), None, Modifiers::none());
        let fraction = cx.update(|_, cx| match &stage.read(cx).pane_tree().root {
            PaneNode::Split { head_fraction, .. } => *head_fraction,
            PaneNode::Leaf { .. } => panic!("the stage is a split"),
        });
        assert!(
            fraction > 0.5,
            "the drag should have widened the head pane, not merely started"
        );
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).held_split()),
            Some(split_id),
            "the bar has to stay lit for the whole drag"
        );

        cx.simulate_mouse_up(
            point(px(700.0), px(400.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).held_split()),
            None,
            "releasing the button lets the bar go"
        );
    }

    /// A bar the pointer has left still has to let go when the button comes back up.
    ///
    /// A held bar stops following the pointer once the split reaches a pane's minimum width, so
    /// the release then lands away from the bar. Reading the release off the bar's own hover
    /// would leave it lit for good.
    #[gpui_kit::test]
    fn a_split_bar_lets_go_of_a_release_away_from_it(cx: &mut TestAppContext) {
        let (stage, split_id) = split_stage(cx);

        let (_, cx) = cx.add_window_view({
            let stage = stage.clone();
            move |_, _| Layer::new(stage)
        });
        cx.simulate_resize(size(px(1000.0), px(800.0)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        cx.simulate_mouse_down(
            point(px(500.0), px(400.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        // Starting the drag first, so the move below is a drag move that resizes the split.
        cx.simulate_mouse_move(point(px(520.0), px(400.0)), None, Modifiers::none());
        // Past the head pane's 120px minimum, the bar cannot follow any further: it stops at 120
        // while the pointer runs out to 10.
        cx.simulate_mouse_move(point(px(10.0), px(400.0)), None, Modifiers::none());
        let fraction = cx.update(|_, cx| match &stage.read(cx).pane_tree().root {
            PaneNode::Split { head_fraction, .. } => *head_fraction,
            PaneNode::Leaf { .. } => panic!("the stage is a split"),
        });
        assert!(
            fraction < 0.2,
            "the bar has to stop at the head pane's minimum width, not follow the pointer"
        );
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).held_split()),
            Some(split_id)
        );

        cx.simulate_mouse_up(
            point(px(10.0), px(400.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).held_split()),
            None,
            "a release away from the bar still has to let it go"
        );
    }
}

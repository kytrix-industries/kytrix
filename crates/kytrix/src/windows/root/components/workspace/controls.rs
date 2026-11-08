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

use super::{
    stage::Stage,
    tree::{NodeId, PaneOrientation},
};

/// The debug selector of the pane's Widget control.
const PANE_WIDGET_BUTTON: &str = "PANE_WIDGET_BUTTON";

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
        // The page covers the pane's header while it is open, so a hint about this control
        // would be a popup painted over the page: the tooltip is withdrawn with the control's
        // job, and the page's own controls carry theirs.
        let tooltip = (!self.stage.read(cx).is_widget_gallery_open())
            .then(|| t!(cx, "window.pane.select_widget"));
        let button = Button::new(self.id)
            .debug_selector(|| PANE_WIDGET_BUTTON.into())
            .child(Icon::new(IconKind::PuzzlePiece).size_3())
            .ghost()
            .with_unified_size(UnifiedSize::Small)
            .aria_label(t!(cx, "window.pane.select_widget"))
            .on_click({
                let stage = self.stage;
                let pane_id = self.pane_id;
                move |_, window, cx| {
                    stage.update(cx, |stage, cx| {
                        stage.open_widget_gallery(&pane_id, window, cx);
                    });
                }
            });
        match tooltip {
            Some(tooltip) => button.with_tooltip(tooltip),
            None => button,
        }
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
    use std::time::Duration;

    use app_config::workspace::WorkspaceLayout;
    use base_ui::layer::Layer;
    use gpui_kit::{
        App, AppContext, Bounds, Context, ElementId, Entity, IntoElement, Modifiers, MouseButton,
        Pixels, Render, TestAppContext, VisualTestContext, Window, div, point, px, size,
    };
    use widget::{
        category::WidgetCategory,
        config::WidgetConfigChange,
        identity::{WidgetId, WidgetInstanceId},
        status::{WidgetAction, WidgetStatus},
        view::WidgetView,
    };
    use widget_registry::registry::{WidgetRegistration, WidgetRegistry};

    use crate::windows::root::components::workspace::{
        stage::Stage,
        tree::{NodeId, PaneNode, PaneOrientation},
        widget_picker::MORPH_DURATION,
    };

    /// The debug selectors of the page this test opens, and of its way out.
    const GALLERY_PANEL: &str = "WIDGET_GALLERY";
    const GALLERY_CANCEL: &str = "WIDGET_GALLERY_CANCEL";

    /// A widget that draws nothing, so the pane's own control and the page over it are the only
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
        gpui_kit::base::init(cx);
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("embedded locales should load");
        widget_registry::catalogue::install(cx).expect("the catalogue should install");
        cx.global_mut::<WidgetRegistry>()
            .register(WidgetRegistration::for_view::<TestWidget>())
            .expect("the test widget should register");
    }

    /// A stage with one pane showing the test widget, and the id of that pane.
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

    /// A stage with two panes side by side, the id of the split between them, and the id of
    /// each pane, head first.
    fn split_stage(cx: &mut TestAppContext) -> (Entity<Stage>, NodeId, [ElementId; 2]) {
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
        let panes = cx.update(|cx| {
            let mut ids = Vec::with_capacity(2);
            stage
                .read(cx)
                .pane_tree()
                .root
                .for_each_pane(&mut |pane| ids.push(pane.id.clone()));

            ids.try_into()
                .expect("splitting the only pane should leave two panes")
        });

        (stage, split_id, panes)
    }

    /// Lets the page's morph finish and repaints, so what is drawn is what has settled.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(MORPH_DURATION + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// The bounds the stage has measured for `pane_id`.
    fn pane_bounds(
        cx: &mut VisualTestContext,
        stage: &Entity<Stage>,
        pane_id: &ElementId,
    ) -> Bounds<Pixels> {
        cx.update(|_, cx| {
            stage
                .read(cx)
                .pane_tree()
                .pane_bounds(pane_id)
                .expect("the frame that drew the stage should have measured its panes")
        })
    }

    /// The bounds the stage has measured for its root.
    fn stage_bounds(cx: &mut VisualTestContext, stage: &Entity<Stage>) -> Bounds<Pixels> {
        cx.update(|_, cx| {
            stage
                .read(cx)
                .pane_tree()
                .root()
                .bounds()
                .copied()
                .expect("the frame that drew the stage should have measured it")
        })
    }

    /// Asserts two rectangles are the same to within a device pixel: the page is placed from
    /// the pane's measured rectangle, and both are snapped by the layout.
    fn assert_same_bounds(actual: Bounds<Pixels>, expected: Bounds<Pixels>, what: &str) {
        let distance = |a: Pixels, b: Pixels| (f32::from(a) - f32::from(b)).abs();
        assert!(
            distance(actual.origin.x, expected.origin.x) <= 1.0
                && distance(actual.origin.y, expected.origin.y) <= 1.0
                && distance(actual.size.width, expected.size.width) <= 1.0
                && distance(actual.size.height, expected.size.height) <= 1.0,
            "{what}: drew {actual:?}, expected {expected:?}"
        );
    }

    /// The pane's Widget control opens the page inside the workspace, and the page starts as the
    /// rectangle of the pane whose control was clicked rather than as the whole workspace: it is
    /// opened from the pane, and the growth out of it is what says so.
    ///
    /// The pane is one of two, so its rectangle really is smaller than the stage's: a page that
    /// opened at the stage's own rectangle cannot pass this test.
    ///
    /// Backing out leaves the pane as it was and takes the page away once it has shrunk back.
    #[gpui_kit::test]
    fn clicking_the_widget_control_opens_the_picker_from_the_pane(cx: &mut TestAppContext) {
        let (stage, _, panes) = split_stage(cx);
        let (_, cx) = cx.add_window_view({
            let stage = stage.clone();
            move |_, _| Layer::new(stage)
        });
        cx.simulate_resize(size(px(1000.0), px(800.0)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "nothing is open before the pane's control is clicked"
        );
        let stage_rect = stage_bounds(cx, &stage);
        let button = cx
            .debug_bounds(super::PANE_WIDGET_BUTTON)
            .expect("the panes' headers should draw their Widget controls");
        // Every pane draws the same control selector, so which pane owns the box that was
        // drawn is told by the pane whose rectangle holds it.
        let (pane_id, pane) = panes
            .iter()
            .map(|pane_id| (pane_id, pane_bounds(cx, &stage, pane_id)))
            .find(|(_, bounds)| bounds.contains(&button.center()))
            .expect("the control that was drawn should sit in one of the panes");
        let before = cx.update(|_, cx| stage.read(cx).pane_widget_id(pane_id, cx));
        assert!(
            pane.size.width < stage_rect.size.width,
            "the fixture needs a pane narrower than the stage, or the page would be the stage \
             anyway: pane {pane:?}, stage {stage_rect:?}"
        );

        cx.simulate_click(button.center(), Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let opened = cx
            .debug_bounds(GALLERY_PANEL)
            .expect("the pane's own control should have opened the page");
        assert_same_bounds(
            opened,
            pane,
            "the page should open on the rectangle of the pane it was opened from",
        );
        assert!(
            opened.size.width < stage_rect.size.width,
            "the opened page must be the pane it was opened from, not the whole stage: \
             opened {opened:?}, stage {stage_rect:?}"
        );

        settle(cx);
        let cancel = cx
            .debug_bounds(GALLERY_CANCEL)
            .expect("the settled page should offer its way out")
            .center();
        cx.simulate_click(cancel, Modifiers::none());
        settle(cx);

        assert!(
            cx.debug_bounds(GALLERY_PANEL).is_none(),
            "backing out should take the page away once it has shrunk back"
        );
        assert_eq!(
            cx.update(|_, cx| stage.read(cx).pane_widget_id(pane_id, cx)),
            before,
            "backing out should leave the pane showing the widget it was showing"
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
        let (stage, split_id, _) = split_stage(cx);

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
        let (stage, split_id, _) = split_stage(cx);

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

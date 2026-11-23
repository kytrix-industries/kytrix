use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use ui_core::{
    app::App, interaction::Overlay as EngineOverlay, node::id::NodeId, window::WindowId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Band {
    Dialog,
    Alert,
    Popup,
    ContextMenu,
    Tooltip,
}

impl Band {
    #[must_use]
    pub const fn order(self) -> u8 {
        match self {
            Self::Dialog => 10,
            Self::Alert => 20,
            Self::Popup => 30,
            Self::ContextMenu => 40,
            Self::Tooltip => 50,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LayerSlot {
    #[default]
    Exclusive,
    Nested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub band: Band,
    pub overlay: EngineOverlay,
    pub modal: bool,
    pub sequence: u64,
}

impl Entry {
    #[must_use]
    pub fn node(self) -> NodeId {
        self.overlay.node()
    }

    #[must_use]
    pub fn window(self) -> WindowId {
        self.overlay.window()
    }
}

struct LayerInner {
    entries: RefCell<Vec<Entry>>,
    next_sequence: Cell<u64>,
}

#[derive(Clone)]
pub struct Layer {
    inner: Rc<LayerInner>,
}

impl Default for Layer {
    fn default() -> Self {
        Self::new()
    }
}

impl Layer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Rc::new(LayerInner {
                entries: RefCell::new(Vec::new()),
                next_sequence: Cell::new(0),
            }),
        }
    }

    pub fn register(&self, app: &mut App, band: Band, overlay: EngineOverlay, modal: bool) -> u64 {
        self.register_with(app, band, overlay, modal, LayerSlot::Exclusive)
    }

    pub(crate) fn register_with(
        &self,
        app: &mut App,
        band: Band,
        overlay: EngineOverlay,
        modal: bool,
        slot: LayerSlot,
    ) -> u64 {
        let window = overlay.window();
        if matches!(band, Band::Dialog | Band::Alert) {
            while self.top_of(window, Band::ContextMenu).is_some() {
                let _ = self.close_top_of(app, window, Band::ContextMenu);
            }
        }
        if slot == LayerSlot::Exclusive && matches!(band, Band::Alert | Band::ContextMenu) {
            while self.top_of(window, band).is_some() {
                let _ = self.close_top_of(app, window, band);
            }
        }
        let sequence = self.inner.next_sequence.get();
        self.inner.next_sequence.set(sequence + 1);
        self.inner.entries.borrow_mut().push(Entry {
            band,
            overlay,
            modal,
            sequence,
        });
        sequence
    }

    pub fn remove(&self, overlay: &EngineOverlay) -> bool {
        let mut entries = self.inner.entries.borrow_mut();
        let before = entries.len();
        entries.retain(|entry| entry.overlay != *overlay);
        entries.len() != before
    }

    #[must_use]
    pub fn len(&self, window: WindowId) -> usize {
        self.inner
            .entries
            .borrow()
            .iter()
            .filter(|entry| entry.window() == window)
            .count()
    }

    #[must_use]
    pub fn is_empty(&self, window: WindowId) -> bool {
        self.len(window) == 0
    }

    #[must_use]
    pub fn entries(&self, window: WindowId) -> Vec<Entry> {
        let mut entries: Vec<Entry> = self
            .inner
            .entries
            .borrow()
            .iter()
            .copied()
            .filter(|entry| entry.window() == window)
            .collect();
        entries.sort_unstable_by_key(|entry| (entry.band, entry.sequence));
        entries
    }

    #[must_use]
    pub fn top(&self, window: WindowId) -> Option<Entry> {
        self.entries(window).pop()
    }

    #[must_use]
    pub fn top_of(&self, window: WindowId, band: Band) -> Option<Entry> {
        self.entries(window)
            .into_iter()
            .rev()
            .find(|entry| entry.band == band)
    }

    #[must_use]
    pub fn modal_scope(&self, window: WindowId) -> Option<Entry> {
        self.entries(window)
            .into_iter()
            .rev()
            .find(|entry| entry.modal)
    }

    #[must_use]
    pub fn contains_node(&self, window: WindowId, node: NodeId) -> bool {
        self.entries(window)
            .iter()
            .any(|entry| entry.node() == node)
    }

    #[must_use]
    pub fn position(&self, window: WindowId, overlay: &EngineOverlay) -> Option<usize> {
        self.entries(window)
            .iter()
            .position(|entry| entry.overlay == *overlay)
    }

    pub fn enforce(&self, app: &mut App, window: WindowId) -> bool {
        let current: Vec<NodeId> = app
            .overlays(window)
            .into_iter()
            .map(|(node, _)| node)
            .collect();
        self.prune(window, &current);
        let desired: Vec<NodeId> = self
            .entries(window)
            .iter()
            .map(|entry| entry.node())
            .collect();
        if desired.is_empty() {
            return false;
        }
        let current_registered: Vec<NodeId> = current
            .into_iter()
            .filter(|node| desired.contains(node))
            .collect();
        if current_registered == desired {
            return false;
        }
        for entry in self.entries(window) {
            app.bring_overlay_to_front(&entry.overlay);
        }
        true
    }

    pub fn close_top(&self, app: &mut App, window: WindowId) -> bool {
        let Some(entry) = self.top(window) else {
            return false;
        };
        let closed = app.close_overlay(&entry.overlay);
        self.remove(&entry.overlay);
        closed
    }

    pub fn close_top_of(&self, app: &mut App, window: WindowId, band: Band) -> bool {
        let Some(entry) = self.top_of(window, band) else {
            return false;
        };
        let closed = app.close_overlay(&entry.overlay);
        self.remove(&entry.overlay);
        closed
    }

    fn prune(&self, window: WindowId, current: &[NodeId]) {
        self.inner
            .entries
            .borrow_mut()
            .retain(|entry| entry.window() != window || current.contains(&entry.node()));
    }
}

#[cfg(test)]
mod tests {

    use super::Band;

    #[test]
    fn bands_order_from_dialog_to_tooltip() {
        let bands = [
            Band::Dialog,
            Band::Alert,
            Band::Popup,
            Band::ContextMenu,
            Band::Tooltip,
        ];
        for pair in bands.windows(2) {
            assert!(
                pair[0].order() < pair[1].order(),
                "{:?} 必须在 {:?} 之下",
                pair[0],
                pair[1]
            );
        }
        assert!(Band::Alert.order() > Band::Dialog.order());
        assert!(Band::ContextMenu.order() > Band::Popup.order());
        assert!(Band::Tooltip.order() > Band::ContextMenu.order());
        assert!(Band::Popup.order() > Band::Alert.order());
        assert_eq!(Band::Dialog.order(), 10);
    }
}

#[cfg(test)]
mod ui_tests {

    use ui_core::{
        event::{Action, InputEvent, Modifiers, PointerButton},
        interaction::{OverlayDismiss, OverlayOptions, OverlayPlacement},
        math::{Point, Size},
        node::{builder::NodeBuilder, id::NodeId},
        test_support::app::TestApp,
        window::{WindowId, WindowOptions},
    };

    use super::{Band, Layer};
    use crate::style_builder::{PaintBuilder, StyleBuilder};

    const PANEL: Size = Size::new(120.0, 80.0);

    fn harness() -> (TestApp, WindowId, NodeId) {
        let app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        (app, window, root)
    }

    fn overlay_at(
        app: &mut TestApp,
        window: WindowId,
        origin: Point,
        dismiss: OverlayDismiss,
        trap_focus: bool,
    ) -> ui_core::interaction::Overlay {
        let overlay = app
            .app_mut()
            .create_overlay(
                window,
                OverlayOptions {
                    placement: OverlayPlacement::Point(origin),
                    dismiss,
                    trap_focus,
                    restore_focus: false,
                    ..OverlayOptions::default()
                },
            )
            .expect("建浮层");
        let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
        NodeBuilder::boxed()
            .id("layer-panel")
            .style(
                StyleBuilder::new()
                    .width(PANEL.width)
                    .height(PANEL.height)
                    .build(),
            )
            .paint(
                PaintBuilder::new()
                    .background(ui_core::paint::color::Color::WHITE)
                    .build(),
            )
            .build(tree, overlay.node())
            .expect("面板");
        overlay
    }

    fn ordered_nodes(app: &TestApp, window: WindowId) -> Vec<NodeId> {
        app.app()
            .overlays(window)
            .into_iter()
            .map(|(node, _)| node)
            .collect()
    }

    fn button(
        app: &mut TestApp,
        window: WindowId,
        parent: NodeId,
        key: &str,
    ) -> (NodeId, ui_core::node::signal::Signal<u32>) {
        let clicks = app.app_mut().signal(0u32);
        let action = Action::Custom(smol_str::format_smolstr!("{key}.click"));
        let node = {
            let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
            NodeBuilder::boxed()
                .id(key.to_owned())
                .style(StyleBuilder::new().width(40.0).height(20.0).build())
                .paint(
                    PaintBuilder::new()
                        .background(ui_core::paint::color::Color::BLACK)
                        .build(),
                )
                .on_click(action.clone())
                .build(tree, parent)
                .expect("按钮")
        };
        app.app_mut().on_action(action, move |ctx| {
            ctx.update(&clicks, |count| *count += 1);
        });
        (node, clicks)
    }

    fn center(app: &TestApp, node: NodeId) -> Point {
        let bounds = app.probe().bounds(node).expect("节点必须有布局盒");
        Point::new(
            bounds.origin.x + bounds.size.width * 0.5,
            bounds.origin.y + bounds.size.height * 0.5,
        )
    }

    fn click(app: &mut TestApp, at: Point) {
        let _ = app.inject(InputEvent::pointer_down(
            at,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = app.inject(InputEvent::pointer_up(
            at,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
    }

    fn node_exists(app: &TestApp, window: WindowId, node: NodeId) -> bool {
        app.app()
            .window(window)
            .expect("窗口")
            .tree()
            .get(node)
            .is_ok()
    }

    #[test]
    fn a_dialog_opened_after_an_alert_stays_below_it() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let alert = overlay_at(
            &mut app,
            window,
            Point::new(20.0, 20.0),
            OverlayDismiss::Explicit,
            true,
        );
        let dialog = overlay_at(
            &mut app,
            window,
            Point::new(20.0, 20.0),
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Alert, alert, true);
        layer.register(app.app_mut(), Band::Dialog, dialog, true);
        let _ = app.drain().expect("首帧");

        assert_eq!(
            ordered_nodes(&app, window),
            vec![alert.node(), dialog.node()],
            "创建序：dialog 在 alert 之上"
        );

        assert!(layer.enforce(app.app_mut(), window), "层带失配必须重排");
        let _ = app.drain().expect("布局");
        assert_eq!(
            ordered_nodes(&app, window),
            vec![dialog.node(), alert.node()],
            "层带：alert 必须在 dialog 之上"
        );
        let tree = app.app().window(window).expect("窗口").tree();
        let hit = tree
            .hit_test(Point::new(40.0, 40.0))
            .map(|hit| hit.node)
            .expect("重叠处必有命中");
        let mut cursor = Some(hit);
        let mut in_alert = false;
        while let Some(current) = cursor {
            if current == alert.node() {
                in_alert = true;
                break;
            }
            cursor = tree.get(current).ok().and_then(|target| target.parent());
        }
        assert!(
            in_alert,
            "重叠处命中 alert（最上层）的子树，实际命中 {hit:?}"
        );
    }

    #[test]
    fn the_order_within_a_band_is_the_registration_order() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let first = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            false,
        );
        let second = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            false,
        );
        layer.register(app.app_mut(), Band::Dialog, first, false);
        layer.register(app.app_mut(), Band::Dialog, second, false);
        let _ = app.drain().expect("首帧");

        app.app_mut().bring_overlay_to_front(&first);
        let _ = app.drain().expect("帧");
        assert_eq!(
            ordered_nodes(&app, window),
            vec![second.node(), first.node()]
        );

        assert!(layer.enforce(app.app_mut(), window));
        let _ = app.drain().expect("帧");
        assert_eq!(
            ordered_nodes(&app, window),
            vec![first.node(), second.node()],
            "同层带按登记序（后登记 = 更上）"
        );
    }

    #[test]
    fn enforce_is_a_no_op_when_the_stack_already_matches_and_requests_no_frame() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let lower = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            false,
        );
        let upper = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            false,
        );
        layer.register(app.app_mut(), Band::Dialog, lower, false);
        layer.register(app.app_mut(), Band::Popup, upper, false);
        let _ = app.drain().expect("首帧");

        assert!(!layer.enforce(app.app_mut(), window), "已对齐必须零工作");
        assert!(!layer.enforce(app.app_mut(), window), "重复调用幂等");
        let outcome = app
            .app_mut()
            .frame(window)
            .expect("窗口")
            .expect("帧不失败");
        assert_eq!(
            outcome,
            ui_core::window::FrameOutcome::Idle,
            "静默检查不得请求帧"
        );
    }

    #[test]
    fn nested_layers_keep_their_bands() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let alert = overlay_at(
            &mut app,
            window,
            Point::new(10.0, 10.0),
            OverlayDismiss::Explicit,
            true,
        );
        let popup = overlay_at(
            &mut app,
            window,
            Point::new(10.0, 10.0),
            OverlayDismiss::OutsideClick,
            false,
        );
        let dialog = overlay_at(
            &mut app,
            window,
            Point::new(10.0, 10.0),
            OverlayDismiss::Explicit,
            true,
        );
        let submenu = overlay_at(
            &mut app,
            window,
            Point::new(10.0, 10.0),
            OverlayDismiss::OutsideClick,
            false,
        );
        let tooltip = overlay_at(
            &mut app,
            window,
            Point::new(10.0, 10.0),
            OverlayDismiss::Explicit,
            false,
        );
        layer.register(app.app_mut(), Band::Alert, alert, true);
        layer.register(app.app_mut(), Band::Popup, popup, false);
        layer.register(app.app_mut(), Band::Dialog, dialog, true);
        layer.register(app.app_mut(), Band::ContextMenu, submenu, false);
        layer.register(app.app_mut(), Band::Tooltip, tooltip, false);
        let _ = app.drain().expect("首帧");
        assert!(layer.enforce(app.app_mut(), window));
        let _ = app.drain().expect("帧");

        assert_eq!(
            ordered_nodes(&app, window),
            vec![
                dialog.node(),
                alert.node(),
                popup.node(),
                submenu.node(),
                tooltip.node()
            ],
            "自下而上必须是 Dialog < Alert < Popup < ContextMenu < Tooltip"
        );
        assert_eq!(
            layer.modal_scope(window).map(|entry| entry.node()),
            Some(alert.node()),
            "焦点作用域根是最上层模态（alert 恒高于 dialog），弹出层不改变它"
        );
    }

    #[test]
    fn removing_an_entry_keeps_the_remaining_order() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let lower = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            false,
        );
        let upper = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            false,
        );
        layer.register(app.app_mut(), Band::Dialog, lower, false);
        layer.register(app.app_mut(), Band::Dialog, upper, false);
        assert!(layer.remove(&lower));
        assert!(!layer.remove(&lower), "重复移除返回 false");
        assert_eq!(layer.len(window), 1);
        assert_eq!(
            layer.top(window).map(|entry| entry.node()),
            Some(upper.node())
        );
    }

    #[test]
    fn closing_the_top_layer_promotes_the_one_below() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let lower = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        let upper = overlay_at(
            &mut app,
            window,
            Point::new(5.0, 5.0),
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Dialog, lower, true);
        layer.register(app.app_mut(), Band::Dialog, upper, true);
        let _ = app.drain().expect("首帧");

        assert_eq!(
            layer.modal_scope(window).map(|entry| entry.node()),
            Some(upper.node())
        );
        assert!(layer.close_top(app.app_mut(), window));
        let _ = app.drain().expect("帧");

        assert_eq!(
            ordered_nodes(&app, window),
            vec![lower.node()],
            "关闭上层后只剩下层"
        );
        assert_eq!(
            layer.modal_scope(window).map(|entry| entry.node()),
            Some(lower.node()),
            "焦点作用域回落到下一个模态"
        );
    }

    #[test]
    fn closing_a_band_leaves_the_other_bands_alone() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let alert = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        let dialog = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        let menu = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::OutsideClick,
            false,
        );
        layer.register(app.app_mut(), Band::Alert, alert, true);
        layer.register(app.app_mut(), Band::Dialog, dialog, true);
        layer.register(app.app_mut(), Band::ContextMenu, menu, false);
        let _ = app.drain().expect("首帧");

        assert!(
            layer.close_top_of(app.app_mut(), window, Band::Alert),
            "关掉 alert"
        );
        let _ = app.drain().expect("帧");
        assert_eq!(layer.len(window), 2);
        assert_eq!(layer.top_of(window, Band::Alert), None);
        assert!(layer.top_of(window, Band::Dialog).is_some());
        assert!(layer.top_of(window, Band::ContextMenu).is_some());
        assert!(
            !node_exists(&app, window, alert.node()),
            "被关闭的 alert 节点必须已卸载"
        );
        assert!(node_exists(&app, window, dialog.node()));
        assert!(node_exists(&app, window, menu.node()));
    }

    #[test]
    fn a_non_modal_layer_does_not_block_the_pointer() {
        let (mut app, window, root) = harness();
        let layer = Layer::new();
        let (button_node, clicks) = button(&mut app, window, root, "background");
        let popup = overlay_at(
            &mut app,
            window,
            Point::new(300.0, 300.0),
            OverlayDismiss::Explicit,
            false,
        );
        layer.register(app.app_mut(), Band::Popup, popup, false);
        let _ = app.drain().expect("首帧");

        let at = center(&app, button_node);
        click(&mut app, at);
        let _ = app.drain().expect("帧");
        assert_eq!(*app.app().read(&clicks), 1, "非模态层之外的下层控件可点");
        assert_eq!(
            ordered_nodes(&app, window),
            vec![popup.node()],
            "浮层未被误关"
        );
    }

    #[test]
    fn a_modal_layer_swallows_presses_outside_its_panel() {
        let (mut app, window, root) = harness();
        let layer = Layer::new();
        let (button_node, clicks) = button(&mut app, window, root, "background");
        let modal = overlay_at(
            &mut app,
            window,
            Point::new(300.0, 300.0),
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Dialog, modal, true);
        let _ = app.drain().expect("首帧");

        let at = center(&app, button_node);
        click(&mut app, at);
        let _ = app.drain().expect("帧");
        assert_eq!(
            *app.app().read(&clicks),
            0,
            "模态作用域外的按下不得触发下层动作"
        );
        assert!(layer.contains_node(window, modal.node()));
    }

    #[test]
    fn a_shared_layer_is_partitioned_by_window() {
        let (mut app, window_a, _root_a) = harness();
        let layer = Layer::new();
        let window_b = app.app_mut().create_window(WindowOptions::default());
        let alert_a = overlay_at(
            &mut app,
            window_a,
            Point::new(20.0, 20.0),
            OverlayDismiss::Explicit,
            true,
        );
        let dialog_b = overlay_at(
            &mut app,
            window_b,
            Point::new(20.0, 20.0),
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Alert, alert_a, true);
        layer.register(app.app_mut(), Band::Dialog, dialog_b, true);
        let order_b = ordered_nodes(&app, window_b);

        assert_eq!(layer.len(window_a), 1);
        assert_eq!(layer.len(window_b), 1);
        assert!(
            !layer.enforce(app.app_mut(), window_a),
            "窗口 A 已对齐；窗口 B 的登记不参与 A 的判定（否则永不收敛）"
        );
        assert!(!layer.enforce(app.app_mut(), window_b), "窗口 B 同样已对齐");
        assert_eq!(
            ordered_nodes(&app, window_a),
            vec![alert_a.node()],
            "A 的 root 顺序不变"
        );
        assert_eq!(
            ordered_nodes(&app, window_b),
            order_b,
            "A 的 enforce 不得重排 B 的 root"
        );
        assert_eq!(
            layer.modal_scope(window_b).map(|entry| entry.node()),
            Some(dialog_b.node()),
            "modal_scope 按窗口分区"
        );
        assert_eq!(
            layer.modal_scope(window_a).map(|entry| entry.node()),
            Some(alert_a.node())
        );
    }

    #[test]
    fn enforce_prunes_entries_whose_overlay_was_closed_elsewhere() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let first = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        let second = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Dialog, first, true);
        layer.register(app.app_mut(), Band::Dialog, second, true);
        let _ = app.drain().expect("首帧");
        assert_eq!(
            layer.modal_scope(window).map(|entry| entry.node()),
            Some(second.node())
        );
        assert!(app.app_mut().close_overlay(&second), "引擎侧卸载最上层模态");
        assert_eq!(
            layer.len(window),
            2,
            "登记暂时还在（等下一轮 sync / enforce）"
        );

        assert!(
            !layer.enforce(app.app_mut(), window),
            "剪除死登记后已对齐 ⇒ 零工作（不得每轮重排）"
        );
        assert_eq!(layer.len(window), 1, "死登记必须被剪除");
        assert_eq!(
            layer.modal_scope(window).map(|entry| entry.node()),
            Some(first.node()),
            "modal_scope 必须回落到仍然存活的模态（不得对已死节点说谎）"
        );
        assert!(!layer.enforce(app.app_mut(), window), "此后稳态零工作");
    }

    #[test]
    fn registering_a_dialog_or_alert_dismisses_the_context_menu() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let menu = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::OutsideClick,
            false,
        );
        layer.register(app.app_mut(), Band::ContextMenu, menu, false);
        let dialog = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Dialog, dialog, true);
        assert_eq!(layer.len(window), 1, "菜单被替换掉");
        assert!(layer.top_of(window, Band::ContextMenu).is_none());
        assert!(!node_exists(&app, window, menu.node()), "菜单节点已卸载");

        let menu = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::OutsideClick,
            false,
        );
        layer.register(app.app_mut(), Band::ContextMenu, menu, false);
        let alert = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Alert, alert, true);
        assert!(layer.top_of(window, Band::ContextMenu).is_none());
        assert!(
            !node_exists(&app, window, menu.node()),
            "alert 打开也收起菜单"
        );
    }

    #[test]
    fn a_second_alert_replaces_the_first_and_a_second_menu_replaces_the_first() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let first_alert = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Alert, first_alert, true);
        let second_alert = overlay_at(
            &mut app,
            window,
            Point::new(10.0, 10.0),
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Alert, second_alert, true);
        assert_eq!(
            layer.top_of(window, Band::Alert).map(|entry| entry.node()),
            Some(second_alert.node())
        );
        assert_eq!(layer.len(window), 1, "alert 槽位至多一个");
        assert!(
            !node_exists(&app, window, first_alert.node()),
            "旧 alert 已卸载"
        );

        let first_menu = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::OutsideClick,
            false,
        );
        layer.register(app.app_mut(), Band::ContextMenu, first_menu, false);
        let second_menu = overlay_at(
            &mut app,
            window,
            Point::new(10.0, 10.0),
            OverlayDismiss::OutsideClick,
            false,
        );
        layer.register(app.app_mut(), Band::ContextMenu, second_menu, false);
        assert_eq!(layer.len(window), 2, "alert + 最新菜单");
        assert!(
            !node_exists(&app, window, first_menu.node()),
            "旧菜单已卸载"
        );
        assert!(node_exists(&app, window, second_menu.node()));
    }
}

pub mod animation;
pub mod drag;
pub mod focus;
pub mod overlay;
pub mod scroll;
pub mod virtual_list;

use std::time::{Duration, Instant};

use crate::{
    a11y::A11yOverride,
    app::App,
    event::{Action, ActionSource, Modifiers, PointerButton},
    layout::hit_test::HitResult,
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{color::Color, style::Cursor},
    window::WindowId,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FocusScope {
    focused: Option<NodeId>,
}

impl FocusScope {
    #[must_use]
    pub fn focused(&self) -> Option<NodeId> {
        self.focused
    }

    #[must_use]
    pub fn is_focused(&self, node: NodeId) -> bool {
        self.focused == Some(node)
    }

    pub(crate) fn set(&mut self, node: Option<NodeId>) -> bool {
        if self.focused == node {
            return false;
        }
        self.focused = node;
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PressRecord {
    pub node: Option<NodeId>,
    pub position: LogicalPoint,
    pub button: PointerButton,
    pub at: Instant,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DragSession {
    pub node: NodeId,
    pub action: Action,
    pub origin: LogicalPoint,
    pub current: LogicalPoint,
    pub phase: DragPhase,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingDrag {
    pub node: NodeId,
    pub action: Action,
    pub origin: LogicalPoint,
}

impl DragSession {
    #[must_use]
    pub fn delta(&self) -> LogicalPoint {
        LogicalPoint::new(
            self.current.x - self.origin.x,
            self.current.y - self.origin.y,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragPhase {
    Start,
    Move,
    End,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DragInfo {
    pub origin: LogicalPoint,
    pub current: LogicalPoint,
    pub delta: LogicalPoint,
    pub phase: DragPhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DragState {
    window: WindowId,
    node: NodeId,
}

impl DragState {
    pub(crate) fn new(window: WindowId, node: NodeId) -> Self {
        Self { window, node }
    }

    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PointerState {
    pub hovered: Option<NodeId>,
    pub cursor: Cursor,
    pub press: Option<PressRecord>,
    pub pending_drag: Option<PendingDrag>,
    pub drag: Option<DragSession>,
    pub capture: Option<NodeId>,
    pub last_hit: Option<HitResult>,
    pub modifiers: Modifiers,
    pub text_edit: Option<crate::text_edit::TextPointerPress>,
}

impl PointerState {
    pub(crate) fn new() -> Self {
        Self {
            hovered: None,
            cursor: Cursor::Default,
            press: None,
            pending_drag: None,
            drag: None,
            capture: None,
            last_hit: None,
            modifiers: Modifiers::empty(),
            text_edit: None,
        }
    }

    pub(crate) fn end_drag(&mut self) -> Option<DragSession> {
        self.capture = None;
        self.pending_drag = None;
        self.drag.take()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollAxis {
    Vertical,
    Horizontal,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollConfig {
    pub axis: ScrollAxis,
    pub line_height: LogicalPx,
}

impl Default for ScrollConfig {
    fn default() -> Self {
        Self {
            axis: ScrollAxis::Vertical,
            line_height: px(48.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScrollHandle {
    window: WindowId,
    node: NodeId,
}

impl ScrollHandle {
    pub(crate) fn new(window: WindowId, node: NodeId) -> Self {
        Self { window, node }
    }

    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollState {
    pub offset: LogicalPoint,
    pub content: LogicalSize,
    pub viewport: LogicalSize,
    pub axis: ScrollAxis,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScrollSlot {
    pub offset: LogicalPoint,
    pub content: LogicalSize,
    pub axis: ScrollAxis,
    pub line_height: LogicalPx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VirtualList {
    window: WindowId,
    node: NodeId,
}

impl VirtualList {
    pub(crate) fn new(window: WindowId, node: NodeId) -> Self {
        Self { window, node }
    }

    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VirtualListConfig {
    pub extent: LogicalPx,
    pub extents: Option<Box<[LogicalPx]>>,
    pub overscan: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemAlign {
    Start,
    Center,
    End,
    Nearest,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VirtualListState {
    pub count: usize,
    pub first: usize,
    pub last: usize,
    pub mounted: usize,
    pub offset: LogicalPoint,
    pub viewport: LogicalSize,
}

pub(crate) struct VirtualSlot {
    pub extent: LogicalPx,
    pub extents: Option<Box<[LogicalPx]>>,
    prefix: Vec<LogicalPx>,
    pub overscan: usize,
    pub count: usize,
    pub builder: Option<Box<dyn FnMut(usize) -> NodeBuilder>>,
    pub mounted: Vec<(usize, NodeId)>,
}

impl VirtualSlot {
    pub(crate) fn new(extent: LogicalPx, overscan: usize) -> Self {
        Self {
            extent,
            extents: None,
            prefix: Vec::new(),
            overscan,
            count: 0,
            builder: None,
            mounted: Vec::new(),
        }
    }

    pub(crate) fn set_extents(&mut self, extents: Box<[LogicalPx]>) {
        self.prefix.clear();
        self.prefix.reserve(extents.len() + 1);
        self.prefix.push(LogicalPx::ZERO);
        let mut total = LogicalPx::ZERO;
        for &height in &extents {
            total += height;
            self.prefix.push(total);
        }
        self.extents = Some(extents);
    }

    pub(crate) fn clear_extents(&mut self) {
        self.extents = None;
        self.prefix.clear();
    }

    pub(crate) fn active_extents(&self) -> Option<&[LogicalPx]> {
        self.extents
            .as_deref()
            .filter(|extents| extents.len() == self.count)
    }

    pub(crate) fn prefix(&self) -> &[LogicalPx] {
        &self.prefix
    }
}

impl std::fmt::Debug for VirtualSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualSlot")
            .field("extent", &self.extent)
            .field("extents", &self.extents.as_deref().map(<[LogicalPx]>::len))
            .field("prefix", &self.prefix.len())
            .field("overscan", &self.overscan)
            .field("count", &self.count)
            .field("builder", &self.builder.as_ref().map(|_| "<closure>"))
            .field("mounted", &self.mounted)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayDismiss {
    OutsideClick,
    Explicit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorSide {
    Below,
    Above,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OverlayPlacement {
    Point(LogicalPoint),
    Anchor {
        node: NodeId,
        side: AnchorSide,
        gap: f32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverlayOptions {
    pub placement: OverlayPlacement,
    pub dismiss: OverlayDismiss,
    pub trap_focus: bool,
    pub restore_focus: bool,
    pub a11y: A11yOverride,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            placement: OverlayPlacement::Point(LogicalPoint::ZERO),
            dismiss: OverlayDismiss::OutsideClick,
            trap_focus: false,
            restore_focus: false,
            a11y: A11yOverride::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Overlay {
    window: WindowId,
    node: NodeId,
}

impl Overlay {
    pub(crate) fn new(window: WindowId, node: NodeId) -> Self {
        Self { window, node }
    }

    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OverlaySlot {
    pub dismiss: OverlayDismiss,
    pub placement: OverlayPlacement,
    pub trap_focus: bool,
    pub opener: Option<NodeId>,
    pub restore_focus: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AnimationId {
    window: WindowId,
    generation: u32,
}

impl AnimationId {
    pub(crate) fn new(window: WindowId, generation: u32) -> Self {
        Self { window, generation }
    }

    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    pub(crate) fn generation(&self) -> u32 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Easing {
    Linear,
    EaseOutCubic,
}

impl Easing {
    #[must_use]
    pub fn sample(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        if t >= 1.0 {
            return 1.0;
        }
        match self {
            Self::Linear => t,
            Self::EaseOutCubic => 1.0 - (1.0 - t).powi(3),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimationSpec {
    pub duration: Duration,
    pub easing: Easing,
}

impl AnimationSpec {
    #[must_use]
    pub fn short() -> Self {
        Self {
            duration: Duration::from_millis(120),
            easing: Easing::EaseOutCubic,
        }
    }
}

#[derive(Clone, Debug)]
pub enum AnimatedValue {
    Scalar {
        target: crate::node::signal::Transient<f32>,
        to: f32,
    },
    Color {
        target: crate::node::signal::Transient<Color>,
        to: Color,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputThresholds {
    pub double_click_time: Duration,
    pub double_click_distance: f32,
    pub drag_distance: f32,
}

impl Default for InputThresholds {
    fn default() -> Self {
        Self::platform_default()
    }
}

impl InputThresholds {
    #[must_use]
    pub const fn platform_default() -> Self {
        #[cfg(windows)]
        {
            Self {
                double_click_time: Duration::from_millis(500),
                double_click_distance: 4.0,
                drag_distance: 4.0,
            }
        }
        #[cfg(target_os = "macos")]
        {
            Self {
                double_click_time: Duration::from_millis(500),
                double_click_distance: 4.0,
                drag_distance: 4.0,
            }
        }
        #[cfg(target_os = "linux")]
        {
            Self {
                double_click_time: Duration::from_millis(400),
                double_click_distance: 4.0,
                drag_distance: 4.0,
            }
        }
    }
}

pub struct ActionCtx<'a> {
    app: &'a mut App,
    window: WindowId,
    node: NodeId,
    hit: Option<HitResult>,
    modifiers: Modifiers,
    source: ActionSource,
}

impl<'a> ActionCtx<'a> {
    pub(crate) fn new(
        app: &'a mut App,
        window: WindowId,
        node: NodeId,
        hit: Option<HitResult>,
        modifiers: Modifiers,
        source: ActionSource,
    ) -> Self {
        Self {
            app,
            window,
            node,
            hit,
            modifiers,
            source,
        }
    }

    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }

    #[must_use]
    pub fn hit(&self) -> Option<HitResult> {
        self.hit
    }

    #[must_use]
    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    #[must_use]
    pub fn source(&self) -> ActionSource {
        self.source
    }

    pub fn read<T: 'static>(
        &self,
        signal: &crate::node::signal::Signal<T>,
    ) -> crate::node::signal::SignalRef<T> {
        self.app.read(signal)
    }

    pub fn write<T: Clone + PartialEq + 'static>(
        &mut self,
        signal: &crate::node::signal::Signal<T>,
        value: T,
    ) -> bool {
        self.app.write(signal, value)
    }

    pub fn update<T: 'static>(
        &mut self,
        signal: &crate::node::signal::Signal<T>,
        update: impl FnOnce(&mut T),
    ) -> bool {
        self.app.update(signal, update)
    }

    pub fn app_mut(&mut self) -> &mut App {
        self.app
    }
}

pub(crate) struct NodeInteraction {
    pub action: Option<Action>,
    pub drag: Option<Action>,
    pub scroll: Option<ScrollSlot>,
    pub virtual_list: Option<VirtualSlot>,
    pub overlay: Option<OverlaySlot>,
    pub focus_ring: Option<crate::paint::style::FocusRing>,
}

impl NodeInteraction {
    pub(crate) fn new() -> Self {
        Self {
            action: None,
            drag: None,
            scroll: None,
            virtual_list: None,
            overlay: None,
            focus_ring: None,
        }
    }
}

impl std::fmt::Debug for NodeInteraction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeInteraction")
            .field("action", &self.action)
            .field("drag", &self.drag)
            .field("scroll", &self.scroll)
            .field("virtual_list", &self.virtual_list)
            .field("overlay", &self.overlay)
            .field("focus_ring", &self.focus_ring)
            .finish()
    }
}

pub(crate) fn resolve_placement(
    tree: &crate::node::tree::NodeTree,
    placement: OverlayPlacement,
    size: crate::math::LogicalSize,
) -> LogicalPoint {
    match placement {
        OverlayPlacement::Point(origin) => origin,
        OverlayPlacement::Anchor { node, side, gap } => {
            let bounds = tree.get(node).map_or_else(
                |_| LogicalRect::default(),
                |target| scroll::to_screen(tree, node, target.layout().bounds),
            );
            match side {
                AnchorSide::Below => LogicalPoint::new(bounds.origin.x, bounds.max().y + gap),
                AnchorSide::Above => {
                    LogicalPoint::new(bounds.origin.x, bounds.origin.y - gap - size.height)
                }
                AnchorSide::Left => {
                    LogicalPoint::new(bounds.origin.x - gap - size.width, bounds.origin.y)
                }
                AnchorSide::Right => LogicalPoint::new(bounds.max().x + gap, bounds.origin.y),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Easing, FocusScope, InputThresholds, ScrollAxis, ScrollConfig, ScrollSlot};
    use crate::{
        math::{LogicalPoint, px},
        node::id::NodeId,
    };

    #[test]
    fn focus_scope_reports_and_dedupes() {
        let mut scope = FocusScope::default();
        let node = NodeId::for_test(1, 1);

        assert_eq!(scope.focused(), None);
        assert!(scope.set(Some(node)));
        assert_eq!(scope.focused(), Some(node));
        assert!(scope.is_focused(node));
        assert!(!scope.set(Some(node)), "同值必须返回 false（不标脏）");
        assert!(scope.set(None));
        assert_eq!(scope.focused(), None);
    }

    #[test]
    fn easing_is_monotonic_and_exact_at_the_end() {
        let mut last = -1.0;
        for step in 0..=10 {
            let value = Easing::EaseOutCubic.sample(step as f32 / 10.0);
            assert!(value >= last, "缓动必须单调");
            assert!((0.0..=1.0).contains(&value));
            last = value;
        }
        assert_eq!(Easing::EaseOutCubic.sample(1.0), 1.0);
        assert_eq!(Easing::Linear.sample(0.5), 0.5);
    }

    #[test]
    fn thresholds_default_matches_the_platform_defaults() {
        let thresholds = InputThresholds::default();
        let platform = InputThresholds::platform_default();

        assert_eq!(thresholds, platform);
        assert_eq!(thresholds.double_click_distance, 4.0);
        assert_eq!(thresholds.drag_distance, 4.0);
        assert!(
            matches!(thresholds.double_click_time.as_millis(), 400 | 500),
            "双击窗口必须是平台缺省之一（实际 {:?}）",
            thresholds.double_click_time
        );
    }

    #[test]
    fn scroll_config_defaults_to_vertical() {
        let config = ScrollConfig::default();
        let slot = ScrollSlot {
            offset: LogicalPoint::ZERO,
            content: crate::math::LogicalSize::new(px(100.0), px(200.0)),
            axis: config.axis,
            line_height: config.line_height,
        };

        assert_eq!(config.axis, ScrollAxis::Vertical);
        assert_eq!(config.line_height, px(48.0));
        assert_eq!(slot.offset, LogicalPoint::ZERO);
    }
}

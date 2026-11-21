//! 通用锚定浮层（旧 `crate::overlay` + 旧 `crate::layer` 的浮层部分的引擎化移植）。
//!
//! 本模块是 dropdown / menu / context_menu / select / tooltip / dialog / color_picker 的共同底座：
//! 一个属于触发器的面板——**放在哪**、**怎么淡入淡出**、**什么时候消失**、**焦点与遮罩怎么算**。
//! 「什么时候打开」不归它管（点击 / 悬停 / 键盘是触发器的职责），调用方把打开动作接到
//! [`Overlay::open`] 上即可。
//!
//! # 旧实现的事实与差距（评估 §1.6 / G6）
//!
//! 旧 `Overlay` 做四件事：`on_prepaint` 拿触发器实测包围盒 → `Positioner::side(..).margin(4)
//! .offset(4)` 求位置（引擎只有 4 向 `AnchorSide`，**没有** align / flip / clamp）→ `Presence`
//! 淡入淡出（引擎只有 `Transient` + `App::animate`）→ `on_mouse_down_out` 关面板（引擎有
//! `OverlayDismiss::OutsideClick`）。
//!
//! 新引擎已具备：浮层节点（root 末尾子节点 = 绘制序末尾 = 命中优先）、4 向
//! `OverlayPlacement`、`trap_focus`（焦点作用域）、`restore_focus`（关闭还给 opener）、
//! `OutsideClick`（点击外部关闭，整次按下消费）、默认 Esc → `Action::Dismiss`。
//! 本模块补齐引擎**没有**的部分，并把引擎能力包装成可复用 API：
//!
//! | 能力 | 落点 |
//! |---|---|
//! | 12 向 placement（4 边 × 3 对齐） | [`Placement`] + [`resolve_origin`]（纯函数） |
//! | flip（首选边放不下时翻到对边） | [`PlacementConfig::flip`] + [`resolve_origin`] |
//! | clamp（视口内，含视口边距） | [`PlacementConfig::margin`] + [`resolve_origin`] |
//! | 锚点几何读取（含滚动容器偏移） | [`anchor_rect`]（`Window::tree` 的布局盒 − 祖先滚动偏移） |
//! | 面板实测后纠位 | [`Overlay::place`] / [`Overlay::sync`]（measure-then-place） |
//! | 固定 z 层带 | [`crate::kit::layer::Band`] + [`crate::kit::layer::Layer`] |
//! | modal 遮罩（密度可调） | [`Scrim`]（浮层的**子节点**，`interactive: false` 命中穿透） |
//! | `Transient` 淡入淡出 | [`Overlay::progress`] + `App::animate`（不建帧循环） |
//! | a11y 角色（Dialog / Menu / Tooltip） | [`OverlayA11y`] |
//!
//! ## 与引擎语义的差异（已接受的取舍，见 spec-delta）
//!
//! - **点击外部 / Esc 关闭是即时的**：引擎在按下 / `Dismiss` 处理里直接卸载浮层节点，没有
//!   「先淡出再走」的中间态。程序化 [`Overlay::close`] 才有淡出（淡出结束由 [`Overlay::sync`]
//!   收尾）。旧实现在两者上都会淡出。
//! - **淡出是逐图元 alpha，不是组不透明度**（引擎 G6 未实现组不透明度）：`StylePatch::opacity`
//!   作用在浮层壳（面板背景 / 边框 / 遮罩）的每个图元上，内容子节点不跟着淡。内容需要
//!   跟随淡出时读 [`Overlay::progress`] 自己缩放透明度（参考 `animation::popup_should_render`）。
//! - **遮罩是浮层自己的子节点**：与浮层同生命周期（引擎的「点击外部关闭」只移除浮层节点），
//!   负偏移铺满窗口；因为 `interactive: false`，命中会落到背景节点 ⇒ 引擎的
//!   `outside_click_target` 判定「点在浮层外」，于是「点遮罩 = 点外部」自然成立。
//! - **入场位置可能晚一帧**：浮层首帧不知道自己的尺寸（引擎只有 `OverlayPlacement::Anchor`
//!   的 4 向纠位，没有应用层 12 向 placement 的自动再求值），所以首帧按零尺寸落位、
//!   [`Overlay::sync`] 在下一帧按实测尺寸纠位（最多两帧收敛，收敛后零工作，I3）。
//!
//! # 用法（组合根）
//!
//! ```ignore
//! let layer = Layer::new();
//! let menu = Overlay::new(app, theme.clone(), layer.clone(), "menu", OverlayOptions {
//!     band: Band::ContextMenu,
//!     ..OverlayOptions::default()
//! });
//! // 触发器的 on_click（或业务回调）里：
//! menu.open(app, window, Anchor::node(trigger), |tree, panel| {
//!     NodeBuilder::text("…", text_style).build(tree, panel)?;
//!     Ok(())
//! })?;
//! // 组合根的 on_frame 钩子里（与 list/table/tree 的 sync 同一个钩子）：
//! menu.sync(app);
//! ```
//!
//! # 生命周期
//!
//! 浮层的信号 / 瞬时值归自有 `SignalScope`（[`Overlay::release`] 释放）；浮层节点归引擎
//! （`App::close_overlay` 卸载并恢复焦点）。动态创建 / 销毁的浮层在卸载后调用一次 `release`。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    error::EngineError,
    interaction::{
        AnimatedValue, AnimationId, AnimationSpec, Easing, Overlay as EngineOverlay,
        OverlayDismiss, OverlayOptions as EngineOverlayOptions, OverlayPlacement,
    },
    math::{Point, Rect, Size},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope, Transient},
        tree::NodeTree,
    },
    paint::{
        color::Color,
        style::{ShadowSpec, Style, StylePatch},
    },
    window::WindowId,
};

use crate::{
    kit::layer::{Band, Layer},
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{components::ComponentTokens, handle::ThemeHandle, role::Role},
};

// ── 定位（纯逻辑） ─────────────────────────────────────────────────────────

/// 浮层相对锚点的**边**（旧 `Placement` 的四个方向）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// 锚点上方。
    Top,
    /// 锚点下方。
    Bottom,
    /// 锚点左侧。
    Left,
    /// 锚点右侧。
    Right,
}

impl Side {
    /// 对边（flip 的目标）。
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

/// 浮层在**另一条轴**上相对锚点的对齐（旧 `Align`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Alignment {
    /// 贴锚点的起始边（LTR 下是左边 / 上边）。
    Start,
    /// 居中。
    Center,
    /// 贴锚点的结束边。
    End,
}

/// 12 向定位：4 条边 × 3 种对齐 = 12 种（旧 `Placement` + `Align` 的组合）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Placement {
    /// 首选边。
    pub side: Side,
    /// 另一条轴的对齐。
    pub align: Alignment,
}

impl Placement {
    /// 构造。
    #[must_use]
    pub const fn new(side: Side, align: Alignment) -> Self {
        Self { side, align }
    }

    /// 上方 + 起始对齐。
    pub const TOP_START: Self = Self::new(Side::Top, Alignment::Start);
    /// 上方 + 居中。
    pub const TOP: Self = Self::new(Side::Top, Alignment::Center);
    /// 上方 + 结束对齐。
    pub const TOP_END: Self = Self::new(Side::Top, Alignment::End);
    /// 下方 + 起始对齐（旧 popup 默认：`Positioner::side(Bottom)` 左对齐）。
    pub const BOTTOM_START: Self = Self::new(Side::Bottom, Alignment::Start);
    /// 下方 + 居中。
    pub const BOTTOM: Self = Self::new(Side::Bottom, Alignment::Center);
    /// 下方 + 结束对齐。
    pub const BOTTOM_END: Self = Self::new(Side::Bottom, Alignment::End);
    /// 左侧 + 起始对齐。
    pub const LEFT_START: Self = Self::new(Side::Left, Alignment::Start);
    /// 左侧 + 居中。
    pub const LEFT: Self = Self::new(Side::Left, Alignment::Center);
    /// 左侧 + 结束对齐。
    pub const LEFT_END: Self = Self::new(Side::Left, Alignment::End);
    /// 右侧 + 起始对齐。
    pub const RIGHT_START: Self = Self::new(Side::Right, Alignment::Start);
    /// 右侧 + 居中。
    pub const RIGHT: Self = Self::new(Side::Right, Alignment::Center);
    /// 右侧 + 结束对齐。
    pub const RIGHT_END: Self = Self::new(Side::Right, Alignment::End);
}

/// 定位参数（旧 `Positioner` 的 `margin(4).offset(4)` 的引擎化）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacementConfig {
    /// 浮层与锚点之间的间隙（逻辑像素）。
    pub gap: f32,
    /// 浮层与视口边之间的最小距离（clamp 用的边距）。
    pub margin: f32,
    /// 首选边放不下时是否翻到对边。
    pub flip: bool,
}

impl Default for PlacementConfig {
    fn default() -> Self {
        // 旧实现：`Positioner::side(..).margin(4.).offset(4.)`。
        Self {
            gap: 4.0,
            margin: 4.0,
            flip: true,
        }
    }
}

/// 把锚点包围盒 / 浮层尺寸 / 视口解成浮层左上角（**12 向 + flip + clamp** 的唯一实现）。
///
/// 语义（与 CSS anchor positioning / Floating UI 的 `flip` + `shift` 同型）：
///
/// 1. 先按首选边的「另一条轴对齐」算出候选位置；
/// 2. `flip` 时若候选位置在视口（内缩 `margin`）里放不下，就翻到对边——对边完整放得下才翻；
///    两边都放不下时比较可用空间，翻到空间更大的一边（仍由下一步 clamp 兜底）；
/// 3. 最后沿两条轴 clamp 进 `[margin, 视口 - margin - 浮层尺寸]`；浮层比视口还大时贴 `margin`
///    （与 CSS `position: fixed` 的 overscroll 行为一致：不把浮层推到负坐标）。
///
/// 非有限输入（NaN / ±∞）返回 [`Point::ZERO`]（与引擎 `hit_test` 的「非有限 ⇒ 不命中」同向的
/// 安全兜底：宁可落在左上角也不 panic）。
#[must_use]
pub fn resolve_origin(
    anchor: Rect,
    panel: Size,
    viewport: Size,
    placement: Placement,
    config: PlacementConfig,
) -> Point {
    if !anchor.is_finite()
        || !panel.width.is_finite()
        || !panel.height.is_finite()
        || !viewport.width.is_finite()
        || !viewport.height.is_finite()
    {
        return Point::ZERO;
    }
    let gap = config.gap.max(0.0);
    let margin = config.margin.max(0.0);
    let mut side = placement.side;
    let preferred = candidate(anchor, panel, placement, gap);
    if config.flip && !fits(preferred, viewport, margin) {
        let opposite = side.opposite();
        let alternative = candidate(
            anchor,
            panel,
            Placement::new(opposite, placement.align),
            gap,
        );
        if fits(alternative, viewport, margin)
            || available(anchor, opposite, viewport, gap, margin)
                > available(anchor, side, viewport, gap, margin)
        {
            side = opposite;
        }
    }
    let placed = candidate(anchor, panel, Placement::new(side, placement.align), gap);
    Point::new(
        clamp_axis(
            placed.origin.x,
            margin,
            viewport.width - margin - panel.width,
        ),
        clamp_axis(
            placed.origin.y,
            margin,
            viewport.height - margin - panel.height,
        ),
    )
}

/// 按边与对齐算候选矩形（未 clamp）。
fn candidate(anchor: Rect, panel: Size, placement: Placement, gap: f32) -> Rect {
    let (x, y) = match placement.side {
        Side::Bottom => (
            aligned_start(
                anchor.origin.x,
                anchor.size.width,
                panel.width,
                placement.align,
            ),
            anchor.max().y + gap,
        ),
        Side::Top => (
            aligned_start(
                anchor.origin.x,
                anchor.size.width,
                panel.width,
                placement.align,
            ),
            anchor.origin.y - gap - panel.height,
        ),
        Side::Right => (
            anchor.max().x + gap,
            aligned_start(
                anchor.origin.y,
                anchor.size.height,
                panel.height,
                placement.align,
            ),
        ),
        Side::Left => (
            anchor.origin.x - gap - panel.width,
            aligned_start(
                anchor.origin.y,
                anchor.size.height,
                panel.height,
                placement.align,
            ),
        ),
    };
    Rect::new(x, y, panel.width, panel.height)
}

/// 交叉轴对齐：`start` = 锚点起始边，`center` = 锚点中心 − 浮层一半，`end` = 锚点结束边 − 浮层尺寸。
fn aligned_start(anchor_origin: f32, anchor_size: f32, panel_size: f32, align: Alignment) -> f32 {
    match align {
        Alignment::Start => anchor_origin,
        Alignment::Center => anchor_origin + (anchor_size - panel_size) * 0.5,
        Alignment::End => anchor_origin + anchor_size - panel_size,
    }
}

/// 候选矩形是否完整落在视口内（四边内缩 `margin`，留 0.5 px 浮点容差）。
fn fits(rect: Rect, viewport: Size, margin: f32) -> bool {
    let max = rect.max();
    rect.origin.x >= margin - 0.5
        && rect.origin.y >= margin - 0.5
        && max.x <= viewport.width - margin + 0.5
        && max.y <= viewport.height - margin + 0.5
}

/// 某条边方向上「锚点边到视口边」的可用空间（flip 的次级判据）。
fn available(anchor: Rect, side: Side, viewport: Size, gap: f32, margin: f32) -> f32 {
    match side {
        Side::Bottom => viewport.height - margin - anchor.max().y - gap,
        Side::Top => anchor.origin.y - margin - gap,
        Side::Right => viewport.width - margin - anchor.max().x - gap,
        Side::Left => anchor.origin.x - margin - gap,
    }
}

/// 单轴 clamp：`max < min`（浮层比视口大）时贴 `min`。
fn clamp_axis(value: f32, min: f32, max: f32) -> f32 {
    if max < min {
        min
    } else {
        value.clamp(min, max)
    }
}

// ── 锚点 ──────────────────────────────────────────────────────────────────

/// 浮层依附的几何。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Anchor {
    /// 锚点节点：布局盒从引擎树读，**减去祖先滚动偏移**（滚动容器里的触发器跟随滚动）。
    Node(NodeId),
    /// 固定矩形（窗口根坐标；context_menu 在指针处打开等）。
    Fixed(Rect),
}

impl Anchor {
    /// 依附某节点。
    #[must_use]
    pub const fn node(node: NodeId) -> Self {
        Self::Node(node)
    }

    /// 依附一个固定点（零尺寸矩形）。
    #[must_use]
    pub const fn point(point: Point) -> Self {
        Self::Fixed(Rect::new(point.x, point.y, 0.0, 0.0))
    }

    /// 依附一个固定矩形。
    #[must_use]
    pub const fn rect(rect: Rect) -> Self {
        Self::Fixed(rect)
    }
}

/// 读锚点的**屏幕**包围盒（窗口根坐标）。
///
/// 节点锚点：`NodeTree` 的布局盒是**内容坐标**；浮层挂在 root 上，必须与屏幕上看到的锚点对齐，
/// 因此减去全部**祖先**滚动容器的偏移（与引擎 `interaction::scroll::to_screen` 同一定义；
/// 锚点自身是滚动容器时它的偏移只作用于子树，不减）。滚动条拖动 / 列表滚动后同一函数给出
/// 新位置，[`Overlay::sync`] 据此让浮层跟随。
///
/// 返回 `None`：窗口不存在 / 节点已卸载。
#[must_use]
pub fn anchor_rect(app: &App, window: WindowId, anchor: Anchor) -> Option<Rect> {
    match anchor {
        Anchor::Fixed(rect) => Some(rect),
        Anchor::Node(node) => {
            let tree = app.window(window)?.tree();
            let bounds = tree.get(node).ok()?.layout().bounds;
            let mut offset = Point::ZERO;
            let mut cursor = tree.get(node).ok()?.parent();
            while let Some(current) = cursor {
                if let Some(handle) = app.scroll_handle(window, current)
                    && let Some(state) = app.scroll_state(&handle)
                {
                    offset = Point::new(offset.x + state.offset.x, offset.y + state.offset.y);
                }
                cursor = tree.get(current).ok()?.parent();
            }
            Some(bounds.translate(Point::new(-offset.x, -offset.y)))
        }
    }
}

// ── 参数 ──────────────────────────────────────────────────────────────────

/// 关闭策略（旧 `on_mouse_down_out` / 显式关闭的二分）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DismissPolicy {
    /// 点击浮层外部即关闭（并消费该次按下）。
    OutsideClick,
    /// 只能显式关闭（[`Overlay::close`] / `Action::Dismiss`）。
    Explicit,
}

impl DismissPolicy {
    fn engine(self) -> OverlayDismiss {
        match self {
            Self::OutsideClick => OverlayDismiss::OutsideClick,
            Self::Explicit => OverlayDismiss::Explicit,
        }
    }
}

/// 浮层壳（面板）的表面画法：背景角色 + 圆角 + 内边距 + 阴影。
///
/// 背景是**主题角色**（由 `bind_style` 读主题信号解析）：深浅色切换只失效读过它的节点。
/// 阴影是静态 `PaintSpec`（引擎只有一个 `ShadowSpec`；旧实现的菜单两层阴影在这里合并为一层）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlaySurface {
    /// 背景语义角色。
    pub background: Role,
    /// 圆角（逻辑像素）。
    pub corner_radius: f32,
    /// 面板内边距（逻辑像素）。
    pub padding: f32,
    /// 阴影（`None` = 不画）。
    pub shadow: Option<ShadowSpec>,
}

impl OverlaySurface {
    /// 菜单 / 下拉面板（旧 `menu.rs`：`bg SurfaceOverlay`、`rounded_sm`、`p_1`、菜单阴影）。
    #[must_use]
    pub fn menu() -> Self {
        let tokens = ComponentTokens::standard();
        Self {
            background: Role::SurfaceOverlay,
            corner_radius: 4.0,
            padding: 4.0,
            shadow: Some(ShadowSpec {
                offset: Point::new(0.0, 3.0),
                blur: 6.0,
                color: Color::BLACK.with_opacity(tokens.shadow.menu_alpha),
            }),
        }
    }

    /// 对话框面板（旧 `dialog.rs`：`bg SurfaceOverlay`、`rounded_sm`、`p_2`、对话框阴影）。
    #[must_use]
    pub fn dialog() -> Self {
        let tokens = ComponentTokens::standard();
        Self {
            background: Role::SurfaceOverlay,
            corner_radius: 4.0,
            padding: 8.0,
            shadow: Some(ShadowSpec {
                offset: Point::new(0.0, 4.0),
                blur: 12.0,
                color: Color::BLACK.with_opacity(tokens.shadow.dialog_alpha),
            }),
        }
    }

    /// tooltip 面板（旧 `tooltip.rs`：`bg SurfaceInverse`、`text FgOnInverse`、`rounded_sm`、`p_1`，
    /// 无阴影）。
    #[must_use]
    pub fn tooltip() -> Self {
        Self {
            background: Role::SurfaceInverse,
            corner_radius: 4.0,
            padding: 4.0,
            shadow: None,
        }
    }
}

/// 模态遮罩（旧 dialog 的 `backdrop`）。
///
/// 遮罩是浮层的子节点：铺满窗口、`interactive: false`（命中穿透到背景）、按 `density` 缩放
/// 主题 `Role::Scrim` 的 alpha。点击遮罩 = 点击浮层外部（引擎 `outside_click_target` 的命中
/// 落在背景上），因此 `OutsideClick` 策略的模态浮层点遮罩即关闭。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scrim {
    /// 密度乘数（`0` = 全透明，`1` = 主题默认 alpha；clamp 到 `[0, 1]`）。
    pub density: f32,
}

impl Default for Scrim {
    fn default() -> Self {
        Self { density: 1.0 }
    }
}

impl Scrim {
    /// 指定密度的遮罩。
    #[must_use]
    pub const fn new(density: f32) -> Self {
        Self { density }
    }
}

/// 浮层的 a11y 声明（角色 + 可访问名）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverlayA11y {
    /// 覆盖角色（对话框 `Dialog` / 菜单 `Menu` / 提示 `Tooltip`）。
    pub role: Option<A11yRole>,
    /// 可访问名。
    pub label: Option<SmolStr>,
}

impl OverlayA11y {
    /// 对话框语义（模态）。
    #[must_use]
    pub fn dialog(label: impl Into<SmolStr>) -> Self {
        Self {
            role: Some(A11yRole::Dialog),
            label: Some(label.into()),
        }
    }

    /// 菜单语义。
    #[must_use]
    pub fn menu(label: impl Into<SmolStr>) -> Self {
        Self {
            role: Some(A11yRole::Menu),
            label: Some(label.into()),
        }
    }

    /// tooltip 语义。
    #[must_use]
    pub fn tooltip(label: impl Into<SmolStr>) -> Self {
        Self {
            role: Some(A11yRole::Tooltip),
            label: Some(label.into()),
        }
    }
}

/// 浮层参数。
#[derive(Clone, Debug, PartialEq)]
pub struct OverlayOptions {
    /// 定位（12 向 + 首选边）。
    pub placement: Placement,
    /// 与锚点的间隙。
    pub gap: f32,
    /// 视口边距（clamp）。
    pub margin: f32,
    /// 首选边放不下时是否翻到对边。
    pub flip: bool,
    /// 关闭策略。
    pub dismiss: DismissPolicy,
    /// 是否模态（`trap_focus`：焦点作用域 + 浮层外按下被消费）。
    pub modal: bool,
    /// 关闭时是否把焦点还给打开它的节点。
    pub restore_focus: bool,
    /// 固定层带（与其它浮层的 z 次序）。
    pub band: Band,
    /// 面板表面。
    pub surface: OverlaySurface,
    /// 模态遮罩（只在 `modal` 时生效；`None` = 模态但无遮罩）。
    pub scrim: Option<Scrim>,
    /// a11y 角色与可访问名。
    pub a11y: OverlayA11y,
    /// 是否播放淡入淡出（`false` = 立即显示 / 立即卸载）。
    pub fade: bool,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            placement: Placement::BOTTOM_START,
            gap: 4.0,
            margin: 4.0,
            flip: true,
            dismiss: DismissPolicy::OutsideClick,
            modal: false,
            restore_focus: true,
            band: Band::Popup,
            surface: OverlaySurface::menu(),
            scrim: None,
            a11y: OverlayA11y::default(),
            fade: true,
        }
    }
}

impl OverlayOptions {
    fn placement_config(&self) -> PlacementConfig {
        PlacementConfig {
            gap: self.gap,
            margin: self.margin,
            flip: self.flip,
        }
    }
}

// ── 浮层 ──────────────────────────────────────────────────────────────────

/// 一次打开留下的状态（`RefCell`：句柄 `Clone` 后共享，方法都取 `&self`）。
#[derive(Default)]
struct OverlayState {
    window: Option<WindowId>,
    handle: Option<EngineOverlay>,
    node: Option<NodeId>,
    panel: Option<NodeId>,
    scrim: Option<NodeId>,
    anchor: Option<Anchor>,
    /// 上次求值的面板左上角（`None` = 还没有求过）。
    origin: Option<Point>,
    /// 遮罩上次写下的几何（`(原点, 视口)`；避免每帧写 style 造成布局抖动）。
    scrim_geometry: Option<(Point, Size)>,
    /// 是否正在淡出（`sync` 在进度归零后真正卸载）。
    closing: bool,
}

struct OverlayInner {
    key: SmolStr,
    theme: ThemeHandle,
    layer: Layer,
    options: OverlayOptions,
    scope: SignalScope,
    /// 是否打开（触发器可以用 `bind_style` 读它画按下态）。
    open: Signal<bool>,
    /// 淡入淡出进度（`0` = 全透明，`1` = 不透明；逐图元 alpha）。
    fade: Transient<f32>,
    fade_animation: Cell<Option<AnimationId>>,
    state: RefCell<OverlayState>,
}

/// 锚定浮层句柄（`Rc` 浅拷贝；可以 move 进 `on_action` 闭包）。
#[derive(Clone)]
pub struct Overlay {
    inner: Rc<OverlayInner>,
}

impl Overlay {
    /// 建浮层状态（关闭态）。之后用 [`Overlay::open`] 打开。
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        layer: Layer,
        key: impl Into<SmolStr>,
        options: OverlayOptions,
    ) -> Self {
        let key = key.into();
        let scope = app.signal_scope();
        Self {
            inner: Rc::new(OverlayInner {
                open: app.signal_scoped(scope, false),
                fade: app.transient_scoped(scope, 0.0),
                key,
                theme,
                layer,
                options,
                scope,
                fade_animation: Cell::new(None),
                state: RefCell::new(OverlayState::default()),
            }),
        }
    }

    /// 浮层键（调试 id 前缀）。
    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// 参数（只读）。
    #[must_use]
    pub fn options(&self) -> &OverlayOptions {
        &self.inner.options
    }

    /// 打开状态信号（`bind_style` / `bind_a11y` 可读；触发器画按下态用）。
    #[must_use]
    pub fn open_state(&self) -> Signal<bool> {
        self.inner.open
    }

    /// 是否打开（应用语义；引擎侧被点击外部 / Esc 关闭时，下一次 [`Overlay::sync`] 之前仍为
    /// `true` —— 这是「状态在帧边界更新」的引擎模型，不是两套真相）。
    #[must_use]
    pub fn is_open(&self, app: &App) -> bool {
        *app.read(&self.inner.open)
    }

    /// 浮层面板节点（打开时存在；内容建在这里）。
    #[must_use]
    pub fn panel_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().panel
    }

    /// 引擎浮层节点（`App::place_overlay` / 命中诊断用）。
    #[must_use]
    pub fn overlay_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().node
    }

    /// 模态遮罩节点（非模态 / 未配置时为 `None`）。
    #[must_use]
    pub fn scrim_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().scrim
    }

    /// 淡入淡出进度（`0` = 全透明，`1` = 不透明）。
    #[must_use]
    pub fn progress(&self, app: &App) -> f32 {
        *app.read_transient(&self.inner.fade)
    }

    /// 打开浮层并建内容；返回 `true` = 本次真的打开了，`false` = 本来就开着（只更新锚点）。
    ///
    /// `content` 在**面板节点**里建子节点（顺序即绘制序）：面板提供背景 / 圆角 / 内边距 / a11y
    /// 角色，内容只负责内部结构。内容建完立即登记图层带并启动淡入；尺寸要等布局，定位由
    /// [`Overlay::sync`] 纠位。
    ///
    /// # Errors
    /// `UnknownWindow`（窗口已关闭）；`content` 抛出的建树错误（`StaleNode` 等）原样传播
    /// （此时浮层节点已卸载，不会留下半成品）。
    pub fn open(
        &self,
        app: &mut App,
        window: WindowId,
        anchor: Anchor,
        content: impl FnOnce(&mut NodeTree, NodeId) -> Result<(), EngineError>,
    ) -> Result<bool, EngineError> {
        if app.window(window).is_none() {
            return Err(EngineError::UnknownWindow { window });
        }
        // 已打开：同窗只换锚点（context_menu 换位置 / select 重新对齐），异窗先关旧窗。
        let existing = self.inner.state.borrow().handle;
        if let Some(handle) = existing {
            if handle.window() == window {
                let (panel, scrim, was_closing) = {
                    let mut state = self.inner.state.borrow_mut();
                    state.anchor = Some(anchor);
                    let was_closing = std::mem::take(&mut state.closing);
                    (state.panel, state.scrim, was_closing)
                };
                // 正在淡出时重新打开 = 撤销关闭并淡回不透明（快速移开又移回指针的提示场景）。
                if was_closing && let Some(panel) = panel {
                    self.start_fade(app, window, panel, scrim, 1.0);
                }
                self.place(app);
                return Ok(false);
            }
            self.close_now(app);
        }
        let viewport = app
            .window(window)
            .map_or(Size::ZERO, |target| target.size());
        let initial_origin = anchor_rect(app, window, anchor).map_or(Point::ZERO, |rect| {
            resolve_origin(
                rect,
                Size::ZERO,
                viewport,
                self.inner.options.placement,
                self.inner.options.placement_config(),
            )
        });
        let engine_overlay = app.create_overlay(
            window,
            EngineOverlayOptions {
                placement: OverlayPlacement::Point(initial_origin),
                dismiss: self.inner.options.dismiss.engine(),
                trap_focus: self.inner.options.modal,
                restore_focus: self.inner.options.restore_focus,
            },
        )?;
        let engine_node = engine_overlay.node();

        let built = {
            let tree = app.window_mut(window).expect("窗口刚检查过").tree_mut();
            let scrim = if self.inner.options.modal
                && let Some(scrim) = self.inner.options.scrim
            {
                Some(self.build_scrim(tree, engine_node, viewport, initial_origin, scrim)?)
            } else {
                None
            };
            let panel = self.build_panel(tree, engine_node)?;
            match content(tree, panel) {
                Ok(()) => Ok((panel, scrim)),
                Err(error) => Err(error),
            }
        };
        let (panel, scrim) = match built {
            Ok(built) => built,
            Err(error) => {
                // 内容建树失败：把刚铸出的浮层节点卸载，不留半成品。
                let _ = app.close_overlay(&engine_overlay);
                return Err(error);
            }
        };
        {
            let mut state = self.inner.state.borrow_mut();
            state.window = Some(window);
            state.handle = Some(engine_overlay);
            state.node = Some(engine_node);
            state.panel = Some(panel);
            state.scrim = scrim;
            state.anchor = Some(anchor);
            state.origin = Some(initial_origin);
            state.scrim_geometry = scrim.map(|_| (initial_origin, viewport));
            state.closing = false;
        }
        self.inner.layer.register(
            self.inner.options.band,
            engine_overlay,
            self.inner.options.modal,
        );
        let _ = app.write(&self.inner.open, true);
        self.start_fade(app, window, panel, scrim, 1.0);
        // 首帧按零尺寸落位，布局后由 `sync` 纠位；这里先让遮罩几何正确。
        let _ = self.sync_scrim(app, window, initial_origin, viewport);
        Ok(true)
    }

    /// 按锚点重新求值一次位置（锚点移动后调用；[`Overlay::sync`] 每帧也调）。
    ///
    /// 返回 `true` = 位置（或遮罩几何）真的变了。位置差 `< 0.5 px` 视为未变 —— 稳态不写
    /// inset、不请求帧（I3）。
    pub fn place(&self, app: &mut App) -> bool {
        let (window, handle, node, anchor) = {
            let state = self.inner.state.borrow();
            let (Some(window), Some(handle), Some(node), Some(anchor)) =
                (state.window, state.handle, state.node, state.anchor)
            else {
                return false;
            };
            (window, handle, node, anchor)
        };
        let Some(anchor_rect) = anchor_rect(app, window, anchor) else {
            // 锚点节点被卸载：浮层没有可依附的东西，直接关闭（旧 dropdown 触发器消失的同义）。
            self.close_now(app);
            return true;
        };
        let Some(viewport) = app.window(window).map(|target| target.size()) else {
            return false;
        };
        let Some(panel_size) = app
            .window(window)
            .and_then(|target| target.tree().get(node).ok())
            .map(|target| target.layout().bounds.size)
        else {
            return false;
        };
        let origin = resolve_origin(
            anchor_rect,
            panel_size,
            viewport,
            self.inner.options.placement,
            self.inner.options.placement_config(),
        );
        let mut changed = false;
        let previous = self.inner.state.borrow().origin;
        let moved = previous.is_none_or(|previous| {
            (previous.x - origin.x).abs() >= 0.5 || (previous.y - origin.y).abs() >= 0.5
        });
        if moved {
            changed |= app.place_overlay(&handle, OverlayPlacement::Point(origin));
            self.inner.state.borrow_mut().origin = Some(origin);
        }
        changed |= self.sync_scrim(app, window, origin, viewport);
        changed
    }

    /// 帧同步（组合根的 `on_frame` 钩子里调用；测试直接调）：外部关闭探测 → 淡出收尾 →
    /// 层带排序 → 跟随锚点。返回 `true` = 本帧做了工作（写 inset / 重排 / 卸载）。
    ///
    /// **没有自注册帧源**：与 `list` / `table` / `tree` 的 sync 同一约定（引擎 `App::on_frame`
    /// 只有一个钩子槽）。
    pub fn sync(&self, app: &mut App) -> bool {
        let (window, handle) = {
            let state = self.inner.state.borrow();
            let (Some(window), Some(handle)) = (state.window, state.handle) else {
                return false;
            };
            (window, handle)
        };
        // ① 引擎侧已卸载（点击外部 / Esc / `Dismiss` / 外部 `close_overlay`）。
        let alive = app
            .window(window)
            .is_some_and(|target| target.tree().get(handle.node()).is_ok());
        if !alive {
            self.finish_close(app);
            return true;
        }
        // ② 淡出结束 ⇒ 真正卸载（淡出完成由动画时钟给出，sync 只做收尾）。
        if self.inner.state.borrow().closing {
            if self.progress(app) <= 0.0 {
                self.close_now(app);
                return true;
            }
            return false;
        }
        // ③ 层带排序（只有失配才重排）+ ④ 锚点跟随。
        let mut changed = self.inner.layer.enforce(app, window);
        changed |= self.place(app);
        changed
    }

    /// 关闭浮层。开启了淡出时先播淡出（节点保留到进度归零，由 [`Overlay::sync`] 卸载）；
    /// 否则立即卸载。返回是否有浮层被关。
    pub fn close(&self, app: &mut App) -> bool {
        if self.inner.state.borrow().handle.is_none() {
            return false;
        }
        let closing = self.inner.state.borrow().closing;
        if !self.inner.options.fade || closing || self.progress(app) <= 0.0 {
            return self.close_now(app);
        }
        self.inner.state.borrow_mut().closing = true;
        let (Some(window), Some(panel), scrim) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().panel,
            self.inner.state.borrow().scrim,
        ) else {
            return self.close_now(app);
        };
        self.start_fade(app, window, panel, scrim, 0.0);
        true
    }

    /// 立即卸载（不播淡出）：引擎卸载浮层节点（连同它的遮罩子节点）+ 恢复焦点 + 移除图层登记。
    /// 返回是否有浮层被关。
    pub fn close_now(&self, app: &mut App) -> bool {
        let handle = self.inner.state.borrow_mut().handle.take();
        let Some(handle) = handle else {
            return false;
        };
        self.inner.layer.remove(&handle);
        self.cancel_fade(app);
        let closed = app.close_overlay(&handle);
        self.finish_close(app);
        closed
    }

    /// 释放浮层的信号作用域（动态创建的浮层在节点卸载后调用一次；幂等）。
    pub fn release(&self, app: &mut App) {
        self.close_now(app);
        let _ = app.release_scope(self.inner.scope);
    }

    /// 引擎卸载后的状态收尾（不碰引擎树：节点已经没了）。
    fn finish_close(&self, app: &mut App) {
        self.cancel_fade(app);
        {
            let mut state = self.inner.state.borrow_mut();
            state.window = None;
            state.handle = None;
            state.node = None;
            state.panel = None;
            state.scrim = None;
            state.anchor = None;
            state.origin = None;
            state.scrim_geometry = None;
            state.closing = false;
        }
        let _ = app.write(&self.inner.open, false);
        app.write_transient(&self.inner.fade, 0.0);
    }

    /// 建模态遮罩：铺满窗口、`interactive: false`、alpha = 主题 Scrim × density × 淡入进度。
    fn build_scrim(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        viewport: Size,
        origin: Point,
        scrim: Scrim,
    ) -> Result<NodeId, EngineError> {
        let theme = self.inner.theme.clone();
        let fade = self.inner.fade;
        let density = if scrim.density.is_finite() {
            scrim.density.clamp(0.0, 1.0)
        } else {
            1.0
        };
        NodeBuilder::boxed()
            .id(format_smolstr!("{}:scrim", self.inner.key))
            .style(scrim_style(origin, viewport))
            .bind_style(move |cx| {
                // 读主题信号（深浅切换失效）+ 淡入进度（动画驱动）；遮罩不可命中 ⇒ 命中穿透背景，
                // 于是「点遮罩」在引擎眼里就是「点浮层外部」。
                let progress = *cx.read_transient(&fade);
                StylePatch {
                    background: Some(theme.role(cx, Role::Scrim)),
                    opacity: Some(density * progress),
                    interactive: Some(false),
                    ..StylePatch::default()
                }
            })
            .a11y(A11yOverride {
                role: Some(A11yRole::Group),
                hidden: true,
                ..A11yOverride::default()
            })
            .build(tree, parent)
    }

    /// 建面板：背景 / 圆角 / 内边距 / 阴影 + a11y 角色；内容建在它里面。
    fn build_panel(&self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        let theme = self.inner.theme.clone();
        let fade = self.inner.fade;
        let surface = self.inner.options.surface;
        let mut paint = PaintBuilder::new().rounded(surface.corner_radius);
        if let Some(shadow) = surface.shadow {
            paint = paint.shadow(shadow);
        }
        NodeBuilder::boxed()
            .id(format_smolstr!("{}:panel", self.inner.key))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .padding(surface.padding)
                    .build(),
            )
            .paint(paint.build())
            .bind_style(move |cx| StylePatch {
                background: Some(theme.role(cx, surface.background)),
                opacity: Some(*cx.read_transient(&fade)),
                ..StylePatch::default()
            })
            .a11y(A11yOverride {
                role: self.inner.options.a11y.role,
                label: self.inner.options.a11y.label.clone(),
                modal: self.inner.options.modal,
                ..A11yOverride::default()
            })
            .build(tree, parent)
    }

    /// 让遮罩几何跟上浮层原点 / 视口（只有变化才写 style，I3）。
    fn sync_scrim(&self, app: &mut App, window: WindowId, origin: Point, viewport: Size) -> bool {
        let scrim = self.inner.state.borrow().scrim;
        let Some(scrim) = scrim else {
            return false;
        };
        {
            let mut state = self.inner.state.borrow_mut();
            if state.scrim_geometry == Some((origin, viewport)) {
                return false;
            }
            state.scrim_geometry = Some((origin, viewport));
        }
        let Some(tree) = app.window_mut(window).map(|target| target.tree_mut()) else {
            return false;
        };
        tree.set_style(scrim, scrim_style(origin, viewport)).is_ok()
    }

    /// 注册淡入 / 淡出动画（`nodes` = 引擎每帧重算并重绘的节点集合）。
    fn start_fade(
        &self,
        app: &mut App,
        window: WindowId,
        panel: NodeId,
        scrim: Option<NodeId>,
        to: f32,
    ) {
        let fade = self.inner.fade;
        if !self.inner.options.fade {
            app.write_transient(&fade, to);
            return;
        }
        let mut nodes = Vec::with_capacity(2);
        nodes.push(panel);
        nodes.extend(scrim);
        let spec = AnimationSpec {
            duration: ComponentTokens::standard().animation_tick,
            easing: Easing::EaseOutCubic,
        };
        match app.animate(
            window,
            AnimatedValue::Scalar { target: fade, to },
            &nodes,
            spec,
        ) {
            Ok(id) => self.inner.fade_animation.set(Some(id)),
            // 窗口已关：直接落值（不会再有帧）。
            Err(_) => app.write_transient(&fade, to),
        }
    }

    /// 取消在飞的淡入 / 淡出动画（卸载前必须做，否则动画会继续给死节点标脏）。
    fn cancel_fade(&self, app: &mut App) {
        if let Some(id) = self.inner.fade_animation.take() {
            let _ = app.cancel_animation(&id);
        }
    }
}

/// 遮罩的静态几何：相对浮层节点（其原点 = `origin`）负偏移铺满窗口。
fn scrim_style(origin: Point, viewport: Size) -> Style {
    StyleBuilder::new()
        .absolute()
        .left(-origin.x)
        .top(-origin.y)
        .width(viewport.width)
        .height(viewport.height)
        .build()
}

#[cfg(test)]
mod tests {
    //! **纯逻辑**：12 向 placement / flip / clamp / 非有限兜底。不需要窗口。

    use ui_core::math::{Point, Rect, Size};

    use super::{Alignment, Placement, PlacementConfig, Side, resolve_origin};

    const VIEWPORT: Size = Size::new(800.0, 600.0);
    const PANEL: Size = Size::new(120.0, 80.0);
    /// 锚点：x 300..400、y 200..240（视口中央，12 向都放得下）。
    const ANCHOR: Rect = Rect::new(300.0, 200.0, 100.0, 40.0);

    fn config(flip: bool) -> PlacementConfig {
        PlacementConfig {
            gap: 4.0,
            margin: 4.0,
            flip,
        }
    }

    /// 12 向：4 条边 × 3 对齐，逐项手算期望（gap = 4、面板 120×80）。
    #[test]
    fn the_twelve_placements_line_up_with_the_anchor() {
        let cfg = config(false);
        let cases = [
            // 边 + 对齐：期望左上角
            (
                Placement::new(Side::Bottom, Alignment::Start),
                (300.0, 244.0),
            ),
            (
                Placement::new(Side::Bottom, Alignment::Center),
                (290.0, 244.0),
            ),
            (Placement::new(Side::Bottom, Alignment::End), (280.0, 244.0)),
            (Placement::new(Side::Top, Alignment::Start), (300.0, 116.0)),
            (Placement::new(Side::Top, Alignment::Center), (290.0, 116.0)),
            (Placement::new(Side::Top, Alignment::End), (280.0, 116.0)),
            (
                Placement::new(Side::Right, Alignment::Start),
                (404.0, 200.0),
            ),
            (
                Placement::new(Side::Right, Alignment::Center),
                (404.0, 180.0),
            ),
            (Placement::new(Side::Right, Alignment::End), (404.0, 160.0)),
            (Placement::new(Side::Left, Alignment::Start), (176.0, 200.0)),
            (
                Placement::new(Side::Left, Alignment::Center),
                (176.0, 180.0),
            ),
            (Placement::new(Side::Left, Alignment::End), (176.0, 160.0)),
        ];
        for (placement, (x, y)) in cases {
            assert_eq!(
                resolve_origin(ANCHOR, PANEL, VIEWPORT, placement, cfg),
                Point::new(x, y),
                "{placement:?} 的落点漂移"
            );
        }
    }

    /// flip：首选边放不下、对边放得下 ⇒ 翻到对边。
    #[test]
    fn flip_moves_to_the_opposite_side_when_the_preferred_side_does_not_fit() {
        let cfg = config(true);
        // 锚点贴底：Bottom 的 544..624 超出视口，翻到 Top（500−4−80 = 416）。
        let bottom = Rect::new(300.0, 500.0, 100.0, 40.0);
        assert_eq!(
            resolve_origin(bottom, PANEL, VIEWPORT, Placement::BOTTOM_START, cfg),
            Point::new(300.0, 416.0)
        );
        // 锚点贴右：Right 的 804..924 超出视口，翻到 Left（700−4−120 = 576）。
        let right = Rect::new(700.0, 200.0, 100.0, 40.0);
        assert_eq!(
            resolve_origin(right, PANEL, VIEWPORT, Placement::RIGHT_START, cfg),
            Point::new(576.0, 200.0)
        );
        // 上下都放不下时翻到空间更大的一边：视口 200 高，锚点 y 120..160；
        // Bottom 空间 = 200−4−160−4 = 32、Top 空间 = 120−4−4 = 112 ⇒ 翻到 Top。
        let tight = Rect::new(300.0, 120.0, 100.0, 40.0);
        assert_eq!(
            resolve_origin(
                tight,
                PANEL,
                Size::new(800.0, 200.0),
                Placement::BOTTOM_START,
                cfg
            ),
            Point::new(300.0, 36.0)
        );
    }

    /// flip 可关：关掉后保持首选边，交给 clamp 兜底（旧 `Positioner` 不翻转的行为）。
    #[test]
    fn flip_can_be_disabled() {
        let cfg = config(false);
        let bottom = Rect::new(300.0, 500.0, 100.0, 40.0);
        assert_eq!(
            resolve_origin(bottom, PANEL, VIEWPORT, Placement::BOTTOM_START, cfg),
            Point::new(300.0, 516.0),
            "关掉 flip 后 Bottom 被 clamp 到视口底边"
        );
    }

    /// clamp：右下角 / 左上角 / 比视口还大的面板都被夹在视口（内缩 margin）内。
    #[test]
    fn clamping_keeps_the_panel_inside_the_viewport() {
        let cfg = config(false);
        let corner = Rect::new(790.0, 590.0, 10.0, 10.0);
        assert_eq!(
            resolve_origin(corner, PANEL, VIEWPORT, Placement::BOTTOM_END, cfg),
            Point::new(800.0 - 4.0 - PANEL.width, 600.0 - 4.0 - PANEL.height)
        );
        let top_left = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert_eq!(
            resolve_origin(top_left, PANEL, VIEWPORT, Placement::TOP_START, cfg),
            Point::new(4.0, 4.0)
        );
        // 面板比视口大：贴 margin，不产生负坐标。
        assert_eq!(
            resolve_origin(
                ANCHOR,
                Size::new(900.0, 700.0),
                VIEWPORT,
                Placement::BOTTOM_START,
                cfg
            ),
            Point::new(4.0, 4.0)
        );
    }

    /// 非有限输入不 panic、不产生 NaN 落点：回退原点；负 gap / margin 按 0 处理。
    #[test]
    fn non_finite_inputs_and_negative_parameters_are_safe() {
        let cfg = PlacementConfig::default();
        assert_eq!(
            resolve_origin(
                Rect::new(f32::NAN, 0.0, 10.0, 10.0),
                PANEL,
                VIEWPORT,
                Placement::BOTTOM_START,
                cfg
            ),
            Point::ZERO
        );
        assert_eq!(
            resolve_origin(
                ANCHOR,
                Size::new(f32::INFINITY, 10.0),
                VIEWPORT,
                Placement::BOTTOM_START,
                cfg
            ),
            Point::ZERO
        );
        let negative = PlacementConfig {
            gap: -5.0,
            margin: -5.0,
            flip: true,
        };
        assert_eq!(
            resolve_origin(ANCHOR, PANEL, VIEWPORT, Placement::BOTTOM_START, negative),
            Point::new(300.0, 240.0),
            "负间隙 / 负边距按 0 处理"
        );
    }
}

#[cfg(test)]
mod ui_tests {
    //! 引擎树上的行为：打开 / 纠位 / 点击外部 / Esc / 焦点陷阱与恢复 / 遮罩 / 淡入淡出 / I3 /
    //! 滚动容器内锚点跟随 / a11y 角色。

    use std::time::Duration;

    use smol_str::format_smolstr;
    use ui_core::{
        a11y::A11yRole,
        event::{Action, InputEvent, KeyCode, Modifiers, PointerButton},
        math::{Point, Rect, Size},
        node::{builder::NodeBuilder, id::NodeId},
        test_support::app::TestApp,
        window::{FrameOutcome, WindowId},
    };

    use super::{
        Anchor, DismissPolicy, Overlay, OverlayA11y, OverlayOptions, OverlaySurface, Scrim,
    };
    use crate::{
        kit::layer::Layer,
        style_builder::{PaintBuilder, StyleBuilder},
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    /// 视口（`TestApp` 默认 800×600）。
    const VIEWPORT: Size = Size::new(800.0, 600.0);
    /// 锚点按钮的几何。
    const ANCHOR: Rect = Rect::new(300.0, 200.0, 100.0, 40.0);
    /// 面板内容的几何（面板 = 内容 + 4 px 内边距 ⇒ 128×88）。
    const CONTENT: Size = Size::new(120.0, 80.0);

    /// 面板内容：固定尺寸的实心盒子（决定面板尺寸 = 内容 + 内边距）。
    fn build_content(
        tree: &mut ui_core::node::tree::NodeTree,
        panel: NodeId,
    ) -> Result<(), ui_core::error::EngineError> {
        NodeBuilder::boxed()
            .id("content")
            .style(
                StyleBuilder::new()
                    .width(CONTENT.width)
                    .height(CONTENT.height)
                    .build(),
            )
            .paint(
                PaintBuilder::new()
                    .background(ui_core::paint::color::Color::WHITE)
                    .build(),
            )
            .build(tree, panel)?;
        Ok(())
    }

    struct Harness {
        app: TestApp,
        window: WindowId,
        root: NodeId,
        theme: ThemeHandle,
    }

    impl Harness {
        fn new() -> Self {
            let mut app = TestApp::new();
            let window = app.window_id();
            let root = app.app().window(window).expect("窗口").tree().root();
            let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
            Self {
                app,
                window,
                root,
                theme,
            }
        }

        fn overlay(&mut self, key: &str, options: OverlayOptions) -> Overlay {
            Overlay::new(
                self.app.app_mut(),
                self.theme.clone(),
                Layer::new(),
                key,
                options,
            )
        }

        fn anchor(&mut self) -> NodeId {
            self.node("anchor", ANCHOR, None)
        }

        fn node(&mut self, id: &str, rect: Rect, action: Option<Action>) -> NodeId {
            let mut builder = NodeBuilder::boxed().id(id.to_owned()).layout_override(rect);
            if let Some(action) = action {
                builder = builder.on_click(action);
            }
            builder
                .build(
                    self.app
                        .app_mut()
                        .window_mut(self.window)
                        .expect("窗口")
                        .tree_mut(),
                    self.root,
                )
                .expect("节点")
        }

        /// 内容：一个固定尺寸的实心盒子（面板的流动子节点决定面板尺寸）。
        fn open(&mut self, overlay: &Overlay, anchor: Anchor) -> bool {
            let opened = overlay
                .open(self.app.app_mut(), self.window, anchor, build_content)
                .expect("打开浮层");
            self.settle(overlay);
            opened
        }

        fn settle(&mut self, overlay: &Overlay) {
            for _ in 0..3 {
                let _ = self.app.drain().expect("帧");
                let _ = overlay.sync(self.app.app_mut());
            }
            let _ = self.app.drain().expect("帧");
        }

        fn bounds(&self, node: NodeId) -> Rect {
            self.app.probe().bounds(node).expect("布局盒")
        }

        fn click(&mut self, at: Point) {
            let _ = self.app.inject(InputEvent::pointer_down(
                at,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let _ = self.app.inject(InputEvent::pointer_up(
                at,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let _ = self.app.drain().expect("帧");
        }
    }

    fn options() -> OverlayOptions {
        OverlayOptions {
            fade: false,
            ..OverlayOptions::default()
        }
    }

    #[test]
    fn the_panel_opens_below_its_anchor_after_layout_settles() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay("menu", options());
        assert!(harness.open(&overlay, Anchor::node(anchor)));

        let panel = harness.bounds(overlay.overlay_node().expect("浮层节点"));
        assert_eq!(
            panel.origin,
            Point::new(ANCHOR.origin.x, ANCHOR.max().y + 4.0)
        );
        assert_eq!(
            panel.size,
            Size::new(CONTENT.width + 8.0, CONTENT.height + 8.0),
            "面板尺寸 = 内容 + 两侧 4 px 内边距"
        );
        assert!(overlay.is_open(harness.app.app()));
        assert_eq!(
            overlay.progress(harness.app.app()),
            1.0,
            "关掉淡出后立即不透明"
        );
    }

    #[test]
    fn a_panel_near_the_bottom_flips_above_the_anchor() {
        // 集成 flip：锚点在窗口底部，Bottom 放不下 ⇒ 面板上翻并底边贴锚点顶部 − gap。
        let mut harness = Harness::new();
        let anchor = harness.node("anchor", Rect::new(300.0, 560.0, 100.0, 40.0), None);
        let overlay = harness.overlay("menu", options());
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        let panel = harness.bounds(overlay.overlay_node().expect("浮层节点"));
        assert!(
            (panel.max().y - (560.0 - 4.0)).abs() < 0.5,
            "翻到上方后底边应贴锚点顶部 − gap：{panel:?}"
        );
    }

    #[test]
    fn a_press_outside_closes_the_overlay_and_updates_the_state() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay("menu", options());
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        let node = overlay.overlay_node().expect("浮层节点");

        // 点在浮层外（视口左上角）：引擎在按下时卸载浮层节点。
        harness.click(Point::new(10.0, 10.0));
        let _ = overlay.sync(harness.app.app_mut());
        assert!(!overlay.is_open(harness.app.app()), "外部点击必须关闭浮层");
        assert!(overlay.overlay_node().is_none());
        assert!(
            harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree()
                .get(node)
                .is_err(),
            "浮层节点已卸载"
        );
    }

    #[test]
    fn escape_closes_the_overlay() {
        // 引擎默认绑定 Escape → `Action::Dismiss`（关最上层浮层，含 Explicit）。
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "menu",
            OverlayOptions {
                dismiss: DismissPolicy::Explicit,
                fade: false,
                ..OverlayOptions::default()
            },
        );
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::Escape, Modifiers::empty()));
        let _ = harness.app.drain().expect("帧");
        let _ = overlay.sync(harness.app.app_mut());
        assert!(!overlay.is_open(harness.app.app()), "Esc 关闭最上层浮层");
        assert!(overlay.overlay_node().is_none());
    }

    #[test]
    fn a_modal_overlay_traps_focus_and_restores_it_on_close() {
        let mut harness = Harness::new();
        let trigger = Action::Custom(format_smolstr!("trigger.click"));
        let trigger_node = harness.node("trigger", ANCHOR, Some(trigger));
        harness
            .app
            .app_mut()
            .set_focus(harness.window, Some(trigger_node));
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(trigger_node),
            "前置：焦点在触发器上"
        );

        // 模态对话框：内容里有一个可聚焦按钮（另一个独立动作）。
        let overlay = harness.overlay(
            "dialog",
            OverlayOptions {
                modal: true,
                scrim: Some(Scrim::default()),
                dismiss: DismissPolicy::Explicit,
                restore_focus: true,
                surface: OverlaySurface::dialog(),
                a11y: OverlayA11y::dialog("示例对话框"),
                fade: false,
                ..OverlayOptions::default()
            },
        );
        let inner = Action::Custom(format_smolstr!("inner.click"));
        let opened = overlay
            .open(
                harness.app.app_mut(),
                harness.window,
                Anchor::node(trigger_node),
                move |tree, panel| {
                    NodeBuilder::boxed()
                        .id("inner")
                        .style(StyleBuilder::new().width(80.0).height(24.0).build())
                        .on_click(inner)
                        .build(tree, panel)?;
                    Ok(())
                },
            )
            .expect("打开对话框");
        assert!(opened);
        harness.settle(&overlay);

        // Tab（`focus_next`）只在模态内循环：焦点从模态外的触发器移进对话框里的按钮
        // （`validate_focus` 在焦点操作入口把作用域外的焦点修进作用域；只有这一个候选时
        // `focus_next` 的返回值是 `None`，焦点本身已经落位）。
        let _ = harness.app.app_mut().focus_next(harness.window);
        let focused = harness.app.app().focused(harness.window);
        assert_eq!(
            focused.map(|node| harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree()
                .debug_id(node)
                .map(str::to_owned)),
            Some(Some("inner".to_owned())),
            "焦点作用域把 Tab 限制在模态内"
        );

        // 关闭（不播淡出）⇒ 焦点回到打开它的触发器。
        overlay.close_now(harness.app.app_mut());
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(trigger_node),
            "关闭后焦点必须还给 opener"
        );
    }

    #[test]
    fn a_modal_overlay_draws_a_scrim_and_a_plain_one_does_not() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let plain = harness.overlay("plain", options());
        assert!(harness.open(&plain, Anchor::node(anchor)));
        assert!(plain.scrim_node().is_none(), "非模态浮层没有遮罩");

        let modal = harness.overlay(
            "modal",
            OverlayOptions {
                modal: true,
                scrim: Some(Scrim::new(0.5)),
                dismiss: DismissPolicy::Explicit,
                fade: false,
                ..OverlayOptions::default()
            },
        );
        assert!(
            modal
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    |_, _| Ok(())
                )
                .expect("打开模态")
        );
        harness.settle(&modal);
        let scrim = modal.scrim_node().expect("模态浮层有遮罩");
        let bounds = harness.bounds(scrim);
        assert_eq!(bounds.origin, Point::ZERO, "遮罩铺满窗口");
        assert_eq!(bounds.size, VIEWPORT, "遮罩覆盖整个视口");
    }

    #[test]
    fn clicking_the_scrim_dismisses_an_outside_click_overlay() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "dialog",
            OverlayOptions {
                modal: true,
                scrim: Some(Scrim::default()),
                surface: OverlaySurface::dialog(),
                fade: false,
                ..OverlayOptions::default()
            },
        );
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        assert!(overlay.scrim_node().is_some());
        // 遮罩不可命中 ⇒ 命中落到背景 ⇒ 引擎判定「点在浮层外」⇒ 关闭。
        harness.click(Point::new(20.0, 20.0));
        let _ = overlay.sync(harness.app.app_mut());
        assert!(!overlay.is_open(harness.app.app()), "点遮罩 = 点外部");
    }

    #[test]
    fn closing_fades_out_before_the_panel_is_removed() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "menu",
            OverlayOptions {
                fade: true,
                ..options()
            },
        );
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        // 淡入：动画时钟推进后进度到 1。
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("帧");
        assert_eq!(overlay.progress(harness.app.app()), 1.0, "淡入完成");
        let node = overlay.overlay_node().expect("浮层节点");

        // 程序化关闭：先淡出，节点此刻仍在树里。
        assert!(overlay.close(harness.app.app_mut()));
        assert!(
            overlay.is_open(harness.app.app()),
            "关闭中仍然打开（淡出未结束）"
        );
        let alive = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .get(node)
            .is_ok();
        assert!(alive, "淡出期间节点保留");

        // 淡出结束：sync 收尾卸载。
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("帧");
        assert!(overlay.sync(harness.app.app_mut()), "同步完成收尾");
        assert!(!overlay.is_open(harness.app.app()), "淡出结束后关闭");
        assert!(overlay.overlay_node().is_none());
        let alive = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .get(node)
            .is_ok();
        assert!(!alive, "淡出结束后节点卸载");
        assert_eq!(overlay.progress(harness.app.app()), 0.0);
    }

    #[test]
    fn a_settled_overlay_requests_no_more_frames() {
        // I3：定位收敛 + 动画结束后，sync 零工作、下一帧 Idle。
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay("menu", options());
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        assert!(!overlay.sync(harness.app.app_mut()), "稳态 sync 必须零工作");
        let outcome = harness
            .app
            .app_mut()
            .frame(harness.window)
            .expect("窗口")
            .expect("帧");
        assert_eq!(outcome, FrameOutcome::Idle, "稳态不得持续出帧");

        overlay.close_now(harness.app.app_mut());
        let _ = harness.app.drain().expect("卸载帧");
        let outcome = harness
            .app
            .app_mut()
            .frame(harness.window)
            .expect("窗口")
            .expect("帧");
        assert_eq!(outcome, FrameOutcome::Idle, "关闭后不得有残留帧");
    }

    #[test]
    fn the_overlay_follows_its_anchor_inside_a_scrolling_container() {
        // 锚点在滚动容器里：滚动后锚点的屏幕位置变化 ⇒ `anchor_rect` 减祖先偏移，浮层跟随。
        let mut harness = Harness::new();
        let viewport_node = {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("scroller")
                .style(
                    StyleBuilder::new()
                        .absolute()
                        .left(0.0)
                        .top(0.0)
                        .width(300.0)
                        .height(200.0)
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, harness.root)
                .expect("滚动容器")
        };
        let scroll_handle = harness
            .app
            .app_mut()
            .make_scroll(
                harness.window,
                viewport_node,
                ui_core::interaction::ScrollConfig::default(),
            )
            .expect("滚动容器登记");
        harness
            .app
            .app_mut()
            .set_scroll_content(&scroll_handle, Size::new(300.0, 1000.0))
            .expect("内容尺寸");
        let anchor = {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("anchor")
                .layout_override(Rect::new(40.0, 400.0, 100.0, 40.0))
                .build(tree, viewport_node)
                .expect("锚点")
        };
        let overlay = harness.overlay("menu", options());
        assert!(
            overlay
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    |_, _| Ok(())
                )
                .expect("打开")
        );
        harness.settle(&overlay);
        // 锚点内容坐标 y=400、容器未滚动 ⇒ 屏幕位置 444（旧实现按内容坐标落位，会偏 300）。
        let before = harness.bounds(overlay.overlay_node().expect("浮层节点"));
        assert!((before.origin.y - 444.0).abs() < 0.5, "未滚动：{before:?}");

        harness
            .app
            .app_mut()
            .scroll_to(&scroll_handle, Point::new(0.0, 300.0))
            .expect("滚动");
        harness.settle(&overlay);
        let after = harness.bounds(overlay.overlay_node().expect("浮层节点"));
        assert!(
            (after.origin.y - (before.origin.y - 300.0)).abs() < 0.5,
            "滚动 300 后浮层跟随锚点上移：before={before:?} after={after:?}"
        );
    }

    #[test]
    fn an_anchor_that_disappears_closes_the_overlay() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay("menu", options());
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            tree.remove(anchor).expect("移除锚点");
        }
        let _ = harness.app.drain().expect("帧");
        assert!(overlay.sync(harness.app.app_mut()), "sync 必须发现锚点消失");
        assert!(!overlay.is_open(harness.app.app()), "锚点消失 ⇒ 浮层关闭");
    }

    #[test]
    fn the_panel_declares_its_a11y_role() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "dialog",
            OverlayOptions {
                modal: true,
                scrim: Some(Scrim::default()),
                surface: OverlaySurface::dialog(),
                a11y: OverlayA11y::dialog("确认"),
                fade: false,
                ..OverlayOptions::default()
            },
        );
        // a11y 捕获按帧交付：打开后只出这一帧，快照里才有全树。
        assert!(
            overlay
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    build_content
                )
                .expect("打开")
        );
        let _ = harness.app.drain().expect("帧");
        let panel = overlay.panel_node().expect("面板");
        let snapshot = harness.app.a11y().expect("a11y 快照");
        let record = snapshot.node_for(panel).expect("面板 a11y");
        assert_eq!(record.role, A11yRole::Dialog);
        assert_eq!(record.label_text(), Some("确认"));
        assert!(record.modal, "模态对话框必须声明 modal");
    }

    #[test]
    fn a_surface_change_only_affects_dye_not_geometry() {
        // 三种面板表面（菜单 / 对话框 / tooltip）只改画法，不改定位与内容尺寸。
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        for (key, surface) in [
            ("menu", OverlaySurface::menu()),
            ("dialog", OverlaySurface::dialog()),
            ("tooltip", OverlaySurface::tooltip()),
        ] {
            let overlay = harness.overlay(
                key,
                OverlayOptions {
                    surface,
                    fade: false,
                    dismiss: DismissPolicy::Explicit,
                    ..OverlayOptions::default()
                },
            );
            assert!(harness.open(&overlay, Anchor::node(anchor)));
            let panel = harness.bounds(overlay.overlay_node().expect("浮层节点"));
            assert_eq!(
                panel.size,
                Size::new(
                    CONTENT.width + surface.padding * 2.0,
                    CONTENT.height + surface.padding * 2.0
                ),
                "{key} 面板尺寸只由内边距决定"
            );
            overlay.close_now(harness.app.app_mut());
            let _ = harness.app.drain().expect("帧");
        }
    }
}

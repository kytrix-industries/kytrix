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
//! | flip（**主轴**放不下时翻到对边，横轴交给 clamp） | [`PlacementConfig::flip`] + [`resolve_origin`] |
//! | clamp（视口内，含视口边距） | [`PlacementConfig::margin`] + [`resolve_origin`] |
//! | 锚点几何读取（含滚动容器偏移） | [`anchor_rect`]（`Window::tree` 的布局盒 − 祖先滚动偏移） |
//! | 面板实测后纠位 | [`Overlay::place`] / [`Overlay::sync`]（measure-then-place） |
//! | 固定 z 层带 | [`crate::kit::layer::Band`] + [`crate::kit::layer::Layer`] |
//! | modal 遮罩（密度可调） | [`Scrim`]（浮层的**子节点**，`interactive: false` 命中穿透） |
//! | `Transient` 淡入淡出 | [`Overlay::progress`] + `App::animate`（不建帧循环） |
//! | 淡出收尾（真实泵序下可观察） | [`AnimationRouter`]（E10 `App::on_animation_complete` 的 kit 单槽路由） |
//! | a11y 角色（Dialog / AlertDialog / Menu / Tooltip） | [`OverlayA11y`] → 引擎 `OverlayOptions::a11y`（E12） |
//!
//! ## 动画完成路由（E10，单槽）
//!
//! 引擎只有一个 `App::on_animation_complete` 槽（后注册覆盖先注册）。overlay 与 tooltip 的淡入
//! 淡出完成**必须共用同一个路由**：[`AnimationRouter`]。组合根建一个 router（与 [`Layer`] 一样
//! 是应用级唯一），传给每一个 [`Overlay::new`] / `Tooltip::new`；router 在第一次
//! `Overlay::new` 时安装引擎回调，之后按 `AnimationId` 把终值事件分发给对应的浮层句柄。
//! **注册者裁决（B3）**：kit 的 [`AnimationRouter`] 是应用**唯一**的完成回调注册者。应用
//! （kytrix 组合根）**不得**再调 `App::on_animation_complete` —— 引擎是单槽，应用注册会静默
//! 覆盖 kit 路由，表现为所有浮层淡出都收不了尾（E10-R02）。应用若要观测「自己的动画什么时候
//! 结束」，当前实现面只有 `App::on_frame` + `App::animations_running`；需要多消费者时**扩展
//! kit 路由**（在本模块给它加订阅入口），不新增引擎 API、也不允许绕开路由各自注册。
//!
//! ## 与引擎语义的差异（已接受的取舍，见 spec-delta）
//!
//! - **点击外部 / Esc 关闭是即时的**：引擎在按下 / `Dismiss` 处理里直接卸载浮层节点，没有
//!   「先淡出再走」的中间态。程序化 [`Overlay::close`] 才有淡出；淡出到终值的同一帧由
//!   [`AnimationRouter`] 的完成回调卸载节点（E10：不再靠下一轮 `sync` 观测 `progress == 0`）。
//!   旧实现在两者上都会淡出。
//! - **淡出是整组不透明度（E13 接入）**：kit 在引擎浮层包装节点下建一个**组根**（`{key}:group`，
//!   面板与遮罩都是它的子节点），`StylePatch::opacity` 只绑在组根上；引擎把有效不透明度
//!   **级联乘进整棵子树的每个图元**（父 × 子，含文本 / 图片 / 阴影 / 边框），因此一次
//!   `App::animate` 只登记组根即可整组淡入淡出，内容子节点不需要自己读 [`Overlay::progress`]。
//!   遮罩的「密度」是它自己的补丁值，与组根的淡入进度相乘。与 CSS `opacity` 的差异见 E13
//!   spec-delta R2（没有离屏「隔离组」：重叠子图元直接混合，视觉上略实）。
//! - **阴影是多层解析阴影（E13）**：[`OverlaySurface::shadows`] 按旧 GPUI 主题回填——菜单 =
//!   `(0,1,2,0) + (0,3,6,-2)` 双层，对话框 = `(0,25,50,-12)` 大偏移单层；tooltip 无阴影。
//! - **遮罩是浮层自己的子节点**：与浮层同生命周期（引擎的「点击外部关闭」只移除浮层节点），
//!   负偏移铺满窗口；因为 `interactive: false`，命中会落到背景节点 ⇒ 引擎的
//!   `outside_click_target` 判定「点在浮层外」，于是「点遮罩 = 点外部」自然成立。
//! - **首帧不显示**：浮层刚建出来时还不知道自己的尺寸（引擎只有 `OverlayPlacement::Anchor`
//!   的 4 向纠位，没有应用层 12 向 placement 的自动再求值），首帧若直接画会落在「零尺寸
//!   候选位置」上跳一下。面板 / 遮罩在首次实测（[`Overlay::place`] 读到非零尺寸）之前
//!   `StylePatch.visible = false`，位置由 [`Overlay::sync`] 在下一帧按实测尺寸纠位后一起显示
//!   （最多两帧收敛，收敛后零工作，I3）；`fade: false` 因此也没有跳变。
//! - **参数非法是强类型错误**：`gap` / `margin` 为 NaN / ±∞ 时 [`Overlay::open`] 返回
//!   [`OverlayError::PlacementConfig`]（纯函数 [`resolve_origin`] 对同样的输入回退
//!   [`Point::ZERO`]，两者是「不 panic 的兜底」与「调用方配置错误」两个不同层）。
//!
//! # 用法（组合根）
//!
//! ```ignore
//! let layer = Layer::new();
//! let router = AnimationRouter::new();   // 应用级唯一：overlay / tooltip 共用一个完成路由
//! let menu = Overlay::new(app, theme.clone(), layer.clone(), router.clone(), "menu", OverlayOptions {
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
    fmt,
    rc::{Rc, Weak},
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::{AnimationCompletion, AnimationEndKind, App},
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
        style::{ShadowSpec, Shadows, Style, StylePatch},
    },
    window::{FrameIndex, WindowId},
};

use crate::{
    kit::layer::{Band, Layer, LayerSlot},
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{components::ComponentTokens, handle::ThemeHandle, role::Role},
};

// ── 动画完成路由（E10 单槽的 kit 统一入口） ───────────────────────────────

/// kit 层唯一的动画完成路由（overlay / tooltip 的淡入淡出共用）。
///
/// **kit 是应用唯一的完成回调注册者（B3 裁决）**。引擎 `App::on_animation_complete` 是单槽
/// （重复注册替换，E10-R02 裁决），因此：
///
/// - 应用（kytrix 组合根）建一个 `AnimationRouter`（与 [`Layer`] 一样是应用级唯一），传给每个
///   [`Overlay::new`] 与 `Tooltip::new`；
/// - 第一次 `Overlay::new` 时 router 把 kit 的完成回调装进当前 `App`（此后该 App 的完成槽由 kit
///   拥有）；**应用不得再调 `App::on_animation_complete`** —— 会静默覆盖 kit 路由，表现是所有
///   浮层淡出都收不了尾；应用需要观测自己的动画时用 `App::on_frame` +
///   `App::animations_running`；
/// - 多消费者（应用也想要完成事件）**扩展本路由**（在这里加订阅入口并统一分发），不新增引擎
///   API、也不允许调用方各自注册；
/// - 每次淡入 / 淡出注册动画后按 `AnimationId` 登记对应浮层句柄；终值事件到达时**按 id 分发**
///   （不是 FIFO —— 陈旧 `Replaced` 事件绝不能摘掉别的浮层的登记），收到事件即摘除登记（一次性）。
///
/// 分发只处理 `Finished`（`Cancelled` / `Replaced` / `Released` 只摘登记、不动浮层状态）：
/// 淡出的 `Finished` 由 [`Overlay`] 在**产生终值的同一帧**收尾（`close_now`）。「下一轮 `sync`
/// 观测 `progress == 0` 再收尾」的旧路径在真实泵序下不可观察（P2-4b R01），本批已删除。
#[derive(Clone, Default)]
pub struct AnimationRouter {
    inner: Rc<RefCell<AnimationRouterState>>,
}

#[derive(Default)]
struct AnimationRouterState {
    /// 是否已把 kit 的回包装进某个 `App`（router 是应用级唯一，首次 `Overlay::new` 安装）。
    installed: bool,
    /// 在飞的淡入淡出动画（`AnimationId` → 浮层句柄；句柄已释放时自动丢弃）。
    consumers: Vec<(AnimationId, Weak<OverlayInner>)>,
}

impl AnimationRouter {
    /// 建一个路由（组合根唯一；与 [`Layer`] 同一个生命周期）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 把 kit 的统一完成回调装进 `app`（幂等：同一 router 只装一次）。
    fn install(&self, app: &mut App) {
        if self.inner.borrow().installed {
            return;
        }
        self.inner.borrow_mut().installed = true;
        let router = self.clone();
        app.on_animation_complete(move |app, completion| router.dispatch(app, completion));
    }

    /// 登记一条在飞的淡入淡出动画（同一 id 不会重复登记）。
    fn track(&self, id: AnimationId, overlay: &Rc<OverlayInner>) {
        let mut state = self.inner.borrow_mut();
        state.consumers.retain(|(tracked, _)| *tracked != id);
        state.consumers.push((id, Rc::downgrade(overlay)));
    }

    /// 摘除一条登记（动画被取消 / 替换 / 浮层卸载时调用）。
    fn untrack(&self, id: Option<AnimationId>) {
        let Some(id) = id else {
            return;
        };
        self.inner
            .borrow_mut()
            .consumers
            .retain(|(tracked, _)| *tracked != id);
    }

    /// 按 `AnimationId` 分发一条完成事件给对应的浮层句柄。
    fn dispatch(&self, app: &mut App, completion: AnimationCompletion) {
        // 先摘登记再调用：回调里可能重新注册动画 / 卸载浮层（不持有 `RefCell` 借用）。
        let consumer = {
            let mut state = self.inner.borrow_mut();
            state
                .consumers
                .iter()
                .position(|(tracked, _)| *tracked == completion.id)
                .map(|index| state.consumers.swap_remove(index).1)
        };
        if let Some(inner) = consumer.and_then(|weak| weak.upgrade()) {
            Overlay { inner }.animation_completed(app, completion);
        }
    }
}

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

impl PlacementConfig {
    /// 校验参数：`gap` / `margin` 必须有限（NaN / ±∞ 会让落点也是非有限值）。
    ///
    /// 纯函数 [`resolve_origin`] 对同样的输入回退 [`Point::ZERO`]（不 panic 的安全兜底）；
    /// [`Overlay::open`] 用本方法把「调用方配置错误」变成强类型错误，而不是悄悄落回 (0, 0)。
    fn validate(self) -> Result<(), OverlayError> {
        if self.gap.is_finite() && self.margin.is_finite() {
            Ok(())
        } else {
            Err(OverlayError::PlacementConfig {
                gap: self.gap,
                margin: self.margin,
            })
        }
    }
}

/// 浮层的强类型错误。
#[derive(Debug)]
pub enum OverlayError {
    /// 定位参数非法：`gap` / `margin` 必须有限。
    PlacementConfig {
        /// 原始间隙。
        gap: f32,
        /// 原始视口边距。
        margin: f32,
    },
    /// 引擎错误（窗口关闭 / 建树失败等）原样传播。
    Engine(EngineError),
}

impl From<EngineError> for OverlayError {
    fn from(error: EngineError) -> Self {
        Self::Engine(error)
    }
}

impl fmt::Display for OverlayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlacementConfig { gap, margin } => write!(
                formatter,
                "invalid overlay placement config: gap {gap} / margin {margin} must be finite"
            ),
            Self::Engine(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for OverlayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PlacementConfig { .. } => None,
            Self::Engine(error) => Some(error),
        }
    }
}

/// 把锚点包围盒 / 浮层尺寸 / 视口解成浮层左上角（**12 向 + flip + clamp** 的唯一实现）。
///
/// 语义（与旧 `gpui_base::Positioner` 的 `side` + `margin` + `offset` 同型，并补上 12 向组合）：
///
/// 1. 先按首选边的「另一条轴对齐」算出候选位置；
/// 2. `flip` 时**只看主轴**：首选边在主轴方向放不下（Bottom 只看锚点下方空间够不够浮层高度，
///    Top / Left / Right 对称），才考虑对边 —— 对边主轴放得下就翻；两边都放不下时比较主轴
///    可用空间，翻到空间更大的一边。**横轴永不触发 flip**（旧 `Positioner` 的语义）：
///    横轴越界由下一步 clamp 平移解决，否则「下方放得下、只是左右稍越界」的菜单 / 提示会
///    莫名其妙跑到上方；
/// 3. 最后沿两条轴 clamp 进 `[margin, 视口 - margin - 浮层尺寸]`；浮层比视口还大时贴 `margin`
///    （与 CSS `position: fixed` 的 overscroll 行为一致：不把浮层推到负坐标）。
///
/// 非有限输入（NaN / ±∞，含锚点 / 尺寸 / 视口 / `gap` / `margin`）返回 [`Point::ZERO`]（与引擎
/// `hit_test` 的「非有限 ⇒ 不命中」同向的安全兜底：宁可落在左上角也不 panic）。调用方配置错误
/// 走 [`Overlay::open`] 的强类型错误（[`OverlayError::PlacementConfig`]），不依赖本兜底。
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
        || !config.gap.is_finite()
        || !config.margin.is_finite()
    {
        return Point::ZERO;
    }
    let gap = config.gap.max(0.0);
    let margin = config.margin.max(0.0);
    let mut side = placement.side;
    if config.flip && !main_axis_fits(anchor, panel, side, viewport, gap, margin) {
        let opposite = side.opposite();
        if main_axis_fits(anchor, panel, opposite, viewport, gap, margin)
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

/// 首选边在**主轴**方向是否放得下（旧 `Positioner::side` 的判据；横轴不参与，0.5 px 容差）。
fn main_axis_fits(
    anchor: Rect,
    panel: Size,
    side: Side,
    viewport: Size,
    gap: f32,
    margin: f32,
) -> bool {
    const TOLERANCE: f32 = 0.5;
    match side {
        Side::Bottom => anchor.max().y + gap + panel.height <= viewport.height - margin + TOLERANCE,
        Side::Top => anchor.origin.y - gap - panel.height >= margin - TOLERANCE,
        Side::Right => anchor.max().x + gap + panel.width <= viewport.width - margin + TOLERANCE,
        Side::Left => anchor.origin.x - gap - panel.width >= margin - TOLERANCE,
    }
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
/// 阴影是静态 `PaintSpec.shadows`（E13 多层阴影；数值按旧 GPUI 主题回填，见各构造器）。
#[derive(Clone, Debug, PartialEq)]
pub struct OverlaySurface {
    /// 背景语义角色。
    pub background: Role,
    /// 圆角（逻辑像素）。
    pub corner_radius: f32,
    /// 面板内边距（逻辑像素）。
    pub padding: f32,
    /// 多层阴影（**按序绘制**：数组序 = 绘制序；空 = 不画）。
    ///
    /// E13 的 [`Shadows`] 至多 `MAX_SHADOWS` 层，单层 = 一个 shadow 图元（额外一次 draw）；
    /// 旧菜单双层 / 对话框大偏移按旧值原样回填。
    pub shadows: Shadows,
}

impl OverlaySurface {
    /// 菜单 / 下拉面板（旧 `menu.rs`：`bg SurfaceOverlay`、`rounded_sm`、`p_1`、菜单阴影）。
    ///
    /// 旧菜单是**双层**阴影 `(0,1,2,0) + (0,3,6,-2)`（第二层负 spread = 紧凑接触阴影），
    /// E13 之前引擎只有一个 `ShadowSpec`，曾被合并成单层 `(0,3,6)`；这里按旧值还原。
    #[must_use]
    pub fn menu() -> Self {
        let tokens = ComponentTokens::standard();
        let color = Color::BLACK.with_opacity(tokens.shadow.menu_alpha);
        Self {
            background: Role::SurfaceOverlay,
            corner_radius: 4.0,
            padding: 4.0,
            shadows: Shadows::new([
                ShadowSpec {
                    offset: Point::new(0.0, 1.0),
                    blur: 2.0,
                    spread: 0.0,
                    color,
                },
                ShadowSpec {
                    offset: Point::new(0.0, 3.0),
                    blur: 6.0,
                    spread: -2.0,
                    color,
                },
            ]),
        }
    }

    /// 对话框面板（旧 `dialog.rs`：`bg SurfaceOverlay`、`rounded_sm`、`p_2`、对话框阴影）。
    ///
    /// 旧值是单个**大偏移**阴影 `(0,25,50,-12)`（浮起感来自大位移 + 大模糊，负 spread 收边）；
    /// E13 之前被压成 `(0,4,12)`，这里按旧值回填。
    #[must_use]
    pub fn dialog() -> Self {
        let tokens = ComponentTokens::standard();
        Self {
            background: Role::SurfaceOverlay,
            corner_radius: 4.0,
            padding: 8.0,
            shadows: Shadows::from(ShadowSpec {
                offset: Point::new(0.0, 25.0),
                blur: 50.0,
                spread: -12.0,
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
            shadows: Shadows::default(),
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
///
/// E12 之后引擎**不再**给浮层包装节点自动推导 `Dialog`（无显式声明 = `GenericContainer`），
/// 因此角色必须由调用方显式声明：`dialog()` / `alert_dialog()` / `menu()` / `tooltip()` 分别
/// 对应旧语义里的对话框 / 警告对话框 / 菜单 / 提示。声明被映射到引擎
/// `OverlayOptions::a11y`（浮层**包装节点**），面板 / 内容不再重复声明同一语义（E12-R1：
/// 两层同名角色会让 AT 读两遍）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverlayA11y {
    /// 覆盖角色（对话框 `Dialog` / 警告对话框 `AlertDialog` / 菜单 `Menu` / 提示 `Tooltip`）。
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

    /// 警告对话框语义（需要用户确认的模态；AT 立即播报）。
    #[must_use]
    pub fn alert_dialog(label: impl Into<SmolStr>) -> Self {
        Self {
            role: Some(A11yRole::AlertDialog),
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
    /// 层带槽位规则（`ContextMenu` / `Alert` 的独占槽位是否适用；子菜单用 `Nested`）。
    pub slot: LayerSlot,
    /// 面板表面。
    pub surface: OverlaySurface,
    /// 模态遮罩（只在 `modal` 时生效；`None` = 模态但无遮罩）。
    pub scrim: Option<Scrim>,
    /// a11y 角色与可访问名（声明在引擎浮层**包装节点**上；E12 之前靠引擎自动 `Dialog`，
    /// 现在必须显式给，否则包装是 `GenericContainer`）。
    ///
    /// 运行期可用 [`Overlay::set_a11y`] 覆盖下一次 `open` 的声明（tooltip 的内容变化重建）。
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
            slot: LayerSlot::Exclusive,
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
    /// 组根（引擎浮层包装节点下唯一的一层 kit 容器；面板 / 遮罩是它的子节点）。
    ///
    /// E13：淡入淡出的 `App::animate` **只登记这一个节点**，`opacity` 补丁也只绑在它上面 ——
    /// 子节点（含内容 / 文本 / 图片 / 阴影）靠引擎的组不透明度级联跟随。
    group: Option<NodeId>,
    panel: Option<NodeId>,
    scrim: Option<NodeId>,
    anchor: Option<Anchor>,
    /// 上次求值的面板左上角（`None` = 还没有求过）。
    origin: Option<Point>,
    /// 打开时窗口的提交序号：只有**至少一帧之后**才揭开面板（首帧按零尺寸落位，见 `place`）。
    frame_at_open: Option<FrameIndex>,
    /// 遮罩上次写下的几何（`(原点, 视口)`；避免每帧写 style 造成布局抖动）。
    scrim_geometry: Option<(Point, Size)>,
    /// 是否正在淡出（E10 完成回调在淡出终值的同一帧调 `close_now` 卸载）。
    closing: bool,
}

struct OverlayInner {
    key: SmolStr,
    theme: ThemeHandle,
    layer: Layer,
    /// kit 的动画完成路由（overlay / tooltip 共用；单槽，不得各自注册）。
    router: AnimationRouter,
    options: OverlayOptions,
    scope: SignalScope,
    /// 是否打开（触发器可以用 `bind_style` 读它画按下态）。
    open: Signal<bool>,
    /// 面板 / 遮罩是否已过首次实测、可以显示（首帧按零尺寸落位，直接画会跳位）。
    ///
    /// 用 `Signal`（不是 `Transient`）：写它要失效组根的 `bind_style`（`visible`）并请求一帧，
    /// 这正是「测量后显示」那一步。稳态恒为 `true`、不再写。
    revealed: Signal<bool>,
    /// 淡入淡出进度（`0` = 全透明，`1` = 不透明）；E13 组不透明度：只乘在组根上，整棵子树跟随。
    fade: Transient<f32>,
    fade_animation: Cell<Option<AnimationId>>,
    /// 下一次 `open` 写到引擎浮层**包装节点**上的 a11y 声明（tooltip 每次按内容刷新）。
    a11y: RefCell<OverlayA11y>,
    state: RefCell<OverlayState>,
}

/// 锚定浮层句柄（`Rc` 浅拷贝；可以 move 进 `on_action` 闭包）。
#[derive(Clone)]
pub struct Overlay {
    inner: Rc<OverlayInner>,
}

impl Overlay {
    /// 建浮层状态（关闭态）。之后用 [`Overlay::open`] 打开。
    ///
    /// `router` 必须是组合根的共享 [`AnimationRouter`]（overlay / tooltip 共用一个单槽路由；
    /// 每个实例各建一个会让后建的覆盖先建的完成回调，淡出收不了尾）。
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
        key: impl Into<SmolStr>,
        options: OverlayOptions,
    ) -> Self {
        let key = key.into();
        let scope = app.signal_scope();
        router.install(app);
        let a11y = options.a11y.clone();
        Self {
            inner: Rc::new(OverlayInner {
                open: app.signal_scoped(scope, false),
                revealed: app.signal_scoped(scope, false),
                fade: app.transient_scoped(scope, 0.0),
                key,
                theme,
                layer,
                router,
                options,
                scope,
                fade_animation: Cell::new(None),
                a11y: RefCell::new(a11y),
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

    /// 覆盖**下一次 `open`** 写到引擎浮层包装节点上的 a11y 声明。
    ///
    /// tooltip 的可访问名来自当前内容（`set_content` 可变），在每次 `show` 前用本方法刷新；
    /// 已经打开的浮层不重建（tooltip 的内容变化走「关闭 → 按新内容重开」，见 `Tooltip`）。
    pub(crate) fn set_a11y(&self, a11y: OverlayA11y) {
        *self.inner.a11y.borrow_mut() = a11y;
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
    /// 节点结构（E13）：引擎浮层**包装节点**（a11y 角色）→ kit **组根**（组不透明度 / 可见性，
    /// 淡入淡出只登记它）→ 遮罩 + 面板（背景 / 圆角 / 内边距 / 阴影）→ 内容。`content` 在
    /// **面板节点**里建子节点（顺序即绘制序），内容只负责内部结构。内容建完立即登记图层带并
    /// 启动淡入；尺寸要等布局，定位由 [`Overlay::sync`] 纠位。
    ///
    /// 「已打开」的判据是**引擎里节点还活着**：引擎侧已被关闭（点外部 / Esc / `Dismiss`）但
    /// [`Overlay::sync`] 还没收尾时，句柄仍是 `Some`，这里做存活探测后走完整重开，不把这次
    /// 打开静默丢掉（快速「点外部关闭 → 再点触发器」会走到）。
    ///
    /// 锚点节点已卸载（[`Anchor::Node`] 读不到布局盒）时返回 `Ok(false)`：没有可依附的东西，
    /// 不建浮层（旧 dropdown 触发器消失的同义）。
    ///
    /// # Errors
    /// [`OverlayError::PlacementConfig`]（`gap` / `margin` 非有限）；[`OverlayError::Engine`]：
    /// `UnknownWindow`（窗口已关闭）/ `content` 抛出的建树错误（`StaleNode` 等，此时浮层节点
    /// 已卸载，不会留下半成品）。
    pub fn open(
        &self,
        app: &mut App,
        window: WindowId,
        anchor: Anchor,
        content: impl FnOnce(&mut NodeTree, NodeId) -> Result<(), EngineError>,
    ) -> Result<bool, OverlayError> {
        self.inner.options.placement_config().validate()?;
        if app.window(window).is_none() {
            return Err(EngineError::UnknownWindow { window }.into());
        }
        // 已打开：同窗只换锚点（context_menu 换位置 / select 重新对齐），异窗先关旧窗。
        let existing = self.inner.state.borrow().handle;
        if let Some(handle) = existing {
            if handle.window() == window {
                if self.is_alive(app, window) {
                    let (group, was_closing) = {
                        let mut state = self.inner.state.borrow_mut();
                        state.anchor = Some(anchor);
                        let was_closing = std::mem::take(&mut state.closing);
                        (state.group, was_closing)
                    };
                    // 正在淡出时重新打开 = 撤销关闭并淡回不透明（快速移开又移回指针的提示场景）。
                    if was_closing && let Some(group) = group {
                        self.start_fade(app, window, group, 1.0);
                    }
                    self.place(app);
                    return Ok(false);
                }
                // 引擎已关闭但还没 sync：死句柄走完整重开（R06）。
                self.finish_close(app);
            } else {
                self.close_now(app);
            }
        }
        let viewport = app
            .window(window)
            .map_or(Size::ZERO, |target| target.size());
        let Some(anchor_rect) = anchor_rect(app, window, anchor) else {
            return Ok(false);
        };
        let initial_origin = resolve_origin(
            anchor_rect,
            Size::ZERO,
            viewport,
            self.inner.options.placement,
            self.inner.options.placement_config(),
        );
        // E12：角色 / 可访问名显式写在引擎浮层**包装节点**上（无声明 = GenericContainer）；
        // 面板 / 内容不再重复声明同一语义（否则 AT 树里多一层同名角色，E12-R1）。
        let a11y = self.inner.a11y.borrow().clone();
        let engine_overlay = app.create_overlay(
            window,
            EngineOverlayOptions {
                placement: OverlayPlacement::Point(initial_origin),
                dismiss: self.inner.options.dismiss.engine(),
                trap_focus: self.inner.options.modal,
                restore_focus: self.inner.options.restore_focus,
                a11y: A11yOverride {
                    role: a11y.role,
                    label: a11y.label,
                    ..A11yOverride::default()
                },
            },
        )?;
        let engine_node = engine_overlay.node();

        let built = {
            let tree = app.window_mut(window).expect("窗口刚检查过").tree_mut();
            // E13：组根是淡入淡出的唯一动画目标；遮罩 / 面板 / 内容都是它的后代 ⇒ 整组跟随。
            let group = self.build_group(tree, engine_node)?;
            let scrim = if self.inner.options.modal
                && let Some(scrim) = self.inner.options.scrim
            {
                Some(self.build_scrim(tree, group, viewport, initial_origin, scrim)?)
            } else {
                None
            };
            let panel = self.build_panel(tree, group)?;
            match content(tree, panel) {
                Ok(()) => Ok((group, panel, scrim)),
                Err(error) => Err(error),
            }
        };
        let (group, panel, scrim) = match built {
            Ok(built) => built,
            Err(error) => {
                // 内容建树失败：把刚铸出的浮层节点卸载，不留半成品。
                let _ = app.close_overlay(&engine_overlay);
                return Err(error.into());
            }
        };
        let frame_at_open = app.window(window).map(|target| target.frame_index());
        {
            let mut state = self.inner.state.borrow_mut();
            state.window = Some(window);
            state.handle = Some(engine_overlay);
            state.node = Some(engine_node);
            state.group = Some(group);
            state.panel = Some(panel);
            state.scrim = scrim;
            state.anchor = Some(anchor);
            state.origin = Some(initial_origin);
            state.frame_at_open = frame_at_open;
            state.scrim_geometry = scrim.map(|_| (initial_origin, viewport));
            state.closing = false;
        }
        self.inner.layer.register_with(
            app,
            self.inner.options.band,
            engine_overlay,
            self.inner.options.modal,
            self.inner.options.slot,
        );
        let _ = app.write(&self.inner.open, true);
        self.start_fade(app, window, group, 1.0);
        // 首帧按零尺寸落位：面板 / 遮罩在首次实测前不可见（`place` 揭开），这里先让遮罩几何正确。
        let _ = self.sync_scrim(app, window, initial_origin, viewport);
        // 首帧层带排序：新浮层可能属于更低的层带（alert 之上的 dialog），等下一次 sync 会让第一
        // 帧的 z 序是错的（R14）。
        let _ = self.inner.layer.enforce(app, window);
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
        // 揭开面板：打开后至少过了一帧（布局已跑），此处的实测尺寸才可信。首帧按零尺寸落位时
        // 一直不可见，`fade: false` 因此也不跳位（R14）；帧序号判据不依赖面板尺寸，空内容 /
        // 零内边距的面板同样会被揭开。
        let frame_at_open = self.inner.state.borrow().frame_at_open;
        let measured = frame_at_open.is_none_or(|opened| {
            app.window(window)
                .is_some_and(|target| target.frame_index() != opened)
        });
        if !*app.read(&self.inner.revealed) && measured {
            let _ = app.write(&self.inner.revealed, true);
            if !self.inner.options.fade {
                app.write_transient(&self.inner.fade, 1.0);
            }
            changed = true;
        }
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

    /// 帧同步（组合根的 `on_frame` 钩子里调用；测试直接调）：外部关闭探测 → 层带排序 →
    /// 跟随锚点。返回 `true` = 本帧做了工作（写 inset / 重排 / 卸载）。
    ///
    /// **没有自注册帧源**：与 `list` / `table` / `tree` 的 sync 同一约定（引擎 `App::on_frame`
    /// 只有一个钩子槽）。
    ///
    /// 淡出收尾**不在这里**：程序化关闭的淡出到终值时，E10 完成回调（[`AnimationRouter`]）
    /// 在产生终值的同一帧调用 [`Overlay::close_now`]；本方法不再观测 `progress == 0`
    /// （泵序是「钩子 → 帧」，动画终值在钩子之后写入，靠 sync 观测会漏掉最后一帧 —— R01）。
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
        // ② 淡出中：节点保留到 E10 完成回调收尾，这里不做层带重排 / 定位。
        if self.inner.state.borrow().closing {
            return false;
        }
        // ③ 层带排序（只有失配才重排）+ ④ 锚点跟随。
        let mut changed = self.inner.layer.enforce(app, window);
        changed |= self.place(app);
        changed
    }

    /// 关闭浮层。开启了淡出时先播淡出：节点保留到淡出终值，由 E10 完成回调
    /// （[`AnimationRouter`]）在**产生终值的同一帧**调 [`Overlay::close_now`] 卸载 —— 不是由
    /// [`Overlay::sync`] 观测进度归零来收尾（真实泵序下不会再有下一轮钩子，P2-4b R01）。
    /// 未开启淡出 / 进度已在 0 时立即卸载。返回是否有浮层被关（已经关闭 / 已经在淡出 ⇒ `true`，
    /// 幂等）。
    ///
    /// **幂等**：淡出中再次 `close` 是 no-op，不会跳过淡出直接卸载（旧实现 `Presence` 同义）；
    /// 需要立刻消失时显式用 [`Overlay::close_now`]。
    pub fn close(&self, app: &mut App) -> bool {
        if self.inner.state.borrow().handle.is_none() {
            return false;
        }
        // 已经在淡出：第二次关闭不升级成立即卸载（闪断），也不重启动画（重入）。
        if self.inner.state.borrow().closing {
            return true;
        }
        if !self.inner.options.fade || self.progress(app) <= 0.0 {
            return self.close_now(app);
        }
        self.inner.state.borrow_mut().closing = true;
        let (Some(window), Some(group)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().group,
        ) else {
            return self.close_now(app);
        };
        self.start_fade(app, window, group, 0.0);
        true
    }

    /// 立即卸载（不播淡出）：引擎卸载浮层节点（连同它的遮罩子节点）+ 恢复焦点 + 移除图层登记。
    /// 返回是否有浮层被关。
    pub fn close_now(&self, app: &mut App) -> bool {
        let Some(handle) = self.inner.state.borrow().handle else {
            return false;
        };
        self.cancel_fade(app);
        let closed = app.close_overlay(&handle);
        // 状态收尾（含 Layer 登记移除）只有 `finish_close` 一处（R03）。
        self.finish_close(app);
        closed
    }

    /// 释放浮层的信号作用域（动态创建的浮层在节点卸载后调用一次；幂等）。
    pub fn release(&self, app: &mut App) {
        self.close_now(app);
        let _ = app.release_scope(self.inner.scope);
    }

    /// 引擎卸载后的状态收尾（不碰引擎树：节点已经没了）。
    ///
    /// **关闭路径的唯一收尾**：程序化关闭、淡出收尾、外部卸载探测（[`Overlay::sync`]）都走这里。
    /// 它负责把 Layer 登记一起摘掉 —— 否则引擎侧关闭（点外部 / Esc / `Dismiss`）后 `desired`
    /// 永远失配，只要有第二个浮层存活就每轮 `enforce` 重排 + 出帧（R03）。
    fn finish_close(&self, app: &mut App) {
        self.cancel_fade(app);
        let handle = self.inner.state.borrow_mut().handle.take();
        if let Some(handle) = handle {
            self.inner.layer.remove(&handle);
        }
        {
            let mut state = self.inner.state.borrow_mut();
            state.window = None;
            state.handle = None;
            state.node = None;
            state.group = None;
            state.panel = None;
            state.scrim = None;
            state.anchor = None;
            state.origin = None;
            state.frame_at_open = None;
            state.scrim_geometry = None;
            state.closing = false;
        }
        let _ = app.write(&self.inner.open, false);
        let _ = app.write(&self.inner.revealed, false);
        app.write_transient(&self.inner.fade, 0.0);
    }

    /// 引擎侧浮层节点是否还活着（外部关闭后句柄仍为 `Some`，直到下一次 [`Overlay::sync`]）。
    fn is_alive(&self, app: &App, window: WindowId) -> bool {
        let Some(handle) = self.inner.state.borrow().handle else {
            return false;
        };
        app.window(window)
            .is_some_and(|target| target.tree().get(handle.node()).is_ok())
    }

    /// 建**组根**（E13）：引擎浮层包装节点的唯一 kit 子节点，面板 / 遮罩 / 内容都是它的后代。
    ///
    /// `opacity` = 淡入淡出进度（组不透明度：引擎把它级联乘进整棵子树），`visible` = 首次实测
    /// 是否已揭开（首帧按零尺寸落位，直接画会跳位）。淡入淡出的 `App::animate` **只登记本节点**：
    /// 引擎的组不透明度复用戳会让子树的 lane 逐级失配重建，不需要逐个子节点登记。
    fn build_group(&self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        let fade = self.inner.fade;
        let revealed = self.inner.revealed;
        NodeBuilder::boxed()
            .id(format_smolstr!("{}:group", self.inner.key))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .bind_style(move |cx| StylePatch {
                opacity: Some(*cx.read_transient(&fade)),
                visible: Some(*cx.read(&revealed)),
                ..StylePatch::default()
            })
            .build(tree, parent)
    }

    /// 建模态遮罩：铺满窗口、`interactive: false`、alpha = 主题 Scrim × density。
    ///
    /// 淡入进度不在这里乘：遮罩是组根的子节点，组根的组不透明度已经级联乘到它身上（E13）。
    fn build_scrim(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        viewport: Size,
        origin: Point,
        scrim: Scrim,
    ) -> Result<NodeId, EngineError> {
        let theme = self.inner.theme.clone();
        let density = if scrim.density.is_finite() {
            scrim.density.clamp(0.0, 1.0)
        } else {
            1.0
        };
        NodeBuilder::boxed()
            .id(format_smolstr!("{}:scrim", self.inner.key))
            .style(scrim_style(origin, viewport))
            .bind_style(move |cx| {
                // 读主题信号（深浅切换失效）；遮罩不可命中 ⇒ 命中穿透背景，于是「点遮罩」在引擎
                // 眼里就是「点浮层外部」。
                StylePatch {
                    background: Some(theme.role(cx, Role::Scrim)),
                    opacity: Some(density),
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

    /// 建面板：背景 / 圆角 / 内边距 / 多层阴影；内容建在它里面。
    ///
    /// a11y 角色不在这里声明：它属于**引擎浮层包装节点**（E12 之后角色必须显式声明，
    /// 面板保持 `GenericContainer`，AT 不会多读一层同名角色）。淡入淡出 / 首帧隐藏由组根负责
    /// （E13：面板不再自己绑 `opacity` / `visible`，否则会与组根重复调暗）。
    fn build_panel(&self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        let theme = self.inner.theme.clone();
        let surface = self.inner.options.surface.clone();
        let mut paint = PaintBuilder::new().rounded(surface.corner_radius).build();
        paint.shadows = surface.shadows.clone();
        let background = surface.background;
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
            .paint(paint)
            .bind_style(move |cx| StylePatch {
                background: Some(theme.role(cx, background)),
                ..StylePatch::default()
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

    /// 注册淡入 / 淡出动画。
    ///
    /// **只登记组根**（E13）：`nodes` 里的一个节点就够 —— 引擎把组根的有效不透明度级联乘进
    /// 整棵子树，并因 `paint_opacity` 复用戳失配重建子树的 lane（面板 / 遮罩 / 内容一起淡）。
    fn start_fade(&self, app: &mut App, window: WindowId, group: NodeId, to: f32) {
        let fade = self.inner.fade;
        if !self.inner.options.fade {
            // `fade: false` 也不在首帧显示：面板要等 `place` 读到实测尺寸才「揭开」，否则会先
            // 出现在零尺寸候选位置再跳到正确位置（可见跳变）。已经显示过的重入直接落值。
            let revealed = *app.read(&self.inner.revealed);
            app.write_transient(&fade, if revealed { to } else { 0.0 });
            return;
        }
        let nodes = [group];
        let spec = AnimationSpec {
            duration: ComponentTokens::standard().animation_tick,
            easing: Easing::EaseOutCubic,
        };
        // 同一目标上的新动画会替换旧槽（旧动画报 `Replaced`）：先摘旧登记，再登记新的。
        // 完成事件按 id 分发，过期事件在 [`AnimationRouter::dispatch`] 里找不到句柄即丢弃。
        self.inner.router.untrack(self.inner.fade_animation.take());
        match app.animate(
            window,
            AnimatedValue::Scalar { target: fade, to },
            &nodes,
            spec,
        ) {
            Ok(id) => {
                self.inner.fade_animation.set(Some(id));
                self.inner.router.track(id, &self.inner);
            }
            // 窗口已关：直接落值（不会再有帧）。
            Err(_) => app.write_transient(&fade, to),
        }
    }

    /// 取消在飞的淡入 / 淡出动画（卸载前必须做，否则动画会继续给死节点标脏）。
    fn cancel_fade(&self, app: &mut App) {
        let animation = self.inner.fade_animation.take();
        self.inner.router.untrack(animation);
        if let Some(id) = animation {
            let _ = app.cancel_animation(&id);
        }
    }

    /// 淡入 / 淡出动画的终值事件（由 [`AnimationRouter`] 按 `AnimationId` 分发）。
    ///
    /// `Finished` + 正在淡出 ⇒ **同一帧内**卸载浮层（E10 完成回调在绘制之前派发，节点不会
    /// 多画一帧终态）；淡入完成 / 取消 / 替换只清动画句柄，不改浮层状态。
    fn animation_completed(&self, app: &mut App, completion: AnimationCompletion) {
        // 过期事件（已被更晚的动画替换）不碰状态。
        if self.inner.fade_animation.get() != Some(completion.id) {
            return;
        }
        self.inner.fade_animation.set(None);
        if completion.kind != AnimationEndKind::Finished {
            return;
        }
        if self.inner.state.borrow().closing {
            self.close_now(app);
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

    /// 非有限 `gap` / `margin` 也走兜底（R18）：不能产出 `Point(inf, inf)`。
    #[test]
    fn non_finite_config_falls_back_to_the_origin() {
        for broken in [
            PlacementConfig {
                gap: f32::INFINITY,
                margin: 4.0,
                flip: true,
            },
            PlacementConfig {
                gap: 4.0,
                margin: f32::NAN,
                flip: true,
            },
        ] {
            assert_eq!(
                resolve_origin(ANCHOR, PANEL, VIEWPORT, Placement::BOTTOM_START, broken),
                Point::ZERO,
                "非有限定位参数必须回退原点：{broken:?}"
            );
        }
    }

    /// R08：横轴越界**不触发**主轴翻转（旧 `Positioner` 只按主轴 flip，横轴交给 clamp）。
    #[test]
    fn cross_axis_overflow_does_not_flip_for_all_twelve_placements() {
        let cfg = config(true);
        // 锚点贴左右边（主轴放得下、横轴越界）或贴上下边（Right/Left 的横轴越界）。
        let cases = [
            (
                Rect::new(760.0, 200.0, 40.0, 20.0),
                Placement::BOTTOM_START,
                (676.0, 224.0),
            ),
            (
                Rect::new(760.0, 200.0, 40.0, 20.0),
                Placement::BOTTOM,
                (676.0, 224.0),
            ),
            (
                Rect::new(10.0, 200.0, 40.0, 20.0),
                Placement::BOTTOM_END,
                (4.0, 224.0),
            ),
            (
                Rect::new(760.0, 300.0, 40.0, 20.0),
                Placement::TOP_START,
                (676.0, 216.0),
            ),
            (
                Rect::new(760.0, 300.0, 40.0, 20.0),
                Placement::TOP,
                (676.0, 216.0),
            ),
            (
                Rect::new(10.0, 300.0, 40.0, 20.0),
                Placement::TOP_END,
                (4.0, 216.0),
            ),
            (
                Rect::new(200.0, 590.0, 40.0, 20.0),
                Placement::RIGHT_START,
                (244.0, 516.0),
            ),
            (
                Rect::new(200.0, 590.0, 40.0, 20.0),
                Placement::RIGHT,
                (244.0, 516.0),
            ),
            (
                Rect::new(200.0, 590.0, 40.0, 20.0),
                Placement::RIGHT_END,
                (244.0, 516.0),
            ),
            (
                Rect::new(300.0, 590.0, 40.0, 20.0),
                Placement::LEFT_START,
                (176.0, 516.0),
            ),
            (
                Rect::new(300.0, 590.0, 40.0, 20.0),
                Placement::LEFT,
                (176.0, 516.0),
            ),
            (
                Rect::new(300.0, 590.0, 40.0, 20.0),
                Placement::LEFT_END,
                (176.0, 516.0),
            ),
        ];
        for (anchor, placement, (x, y)) in cases {
            assert_eq!(
                resolve_origin(anchor, PANEL, VIEWPORT, placement, cfg),
                Point::new(x, y),
                "{placement:?} 横轴越界只 clamp、不翻转（anchor {anchor:?}）"
            );
        }
    }

    /// R08：主轴放不下时翻到对边（12 向逐一）。
    #[test]
    fn main_axis_overflow_flips_for_all_twelve_placements() {
        let cfg = config(true);
        let narrow = Rect::new(340.0, 590.0, 100.0, 20.0);
        let low = Rect::new(340.0, 0.0, 100.0, 20.0);
        let right = Rect::new(760.0, 260.0, 40.0, 40.0);
        let left = Rect::new(0.0, 260.0, 40.0, 40.0);
        let cases = [
            (narrow, Placement::BOTTOM_START, (340.0, 506.0)),
            (narrow, Placement::BOTTOM, (330.0, 506.0)),
            (narrow, Placement::BOTTOM_END, (320.0, 506.0)),
            (low, Placement::TOP_START, (340.0, 24.0)),
            (low, Placement::TOP, (330.0, 24.0)),
            (low, Placement::TOP_END, (320.0, 24.0)),
            (right, Placement::RIGHT_START, (636.0, 260.0)),
            (right, Placement::RIGHT, (636.0, 240.0)),
            (right, Placement::RIGHT_END, (636.0, 220.0)),
            (left, Placement::LEFT_START, (44.0, 260.0)),
            (left, Placement::LEFT, (44.0, 240.0)),
            (left, Placement::LEFT_END, (44.0, 220.0)),
        ];
        for (anchor, placement, (x, y)) in cases {
            assert_eq!(
                resolve_origin(anchor, PANEL, VIEWPORT, placement, cfg),
                Point::new(x, y),
                "{placement:?} 主轴放不下必须翻到对边（anchor {anchor:?}）"
            );
        }
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
        paint::color::Color,
        test_support::app::TestApp,
        window::{FrameOutcome, WindowId},
    };

    use super::{
        Anchor, AnimationRouter, DismissPolicy, Overlay, OverlayA11y, OverlayError, OverlayOptions,
        OverlaySurface, Scrim,
    };
    use crate::{
        kit::layer::Layer,
        style_builder::{PaintBuilder, StyleBuilder},
        theme::{appearance::Appearance, components::ComponentTokens, handle::ThemeHandle},
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
        /// 组合根共享的 Layer（多个浮层必须登记到同一张层带表）。
        layer: Layer,
        /// 组合根共享的动画完成路由（overlay / tooltip 单槽共用）。
        router: AnimationRouter,
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
                layer: Layer::new(),
                router: AnimationRouter::new(),
            }
        }

        fn overlay(&mut self, key: &str, options: OverlayOptions) -> Overlay {
            Overlay::new(
                self.app.app_mut(),
                self.theme.clone(),
                self.layer.clone(),
                self.router.clone(),
                key,
                options,
            )
        }

        /// 真实泵序的一轮：钩子（`Overlay::sync`）在帧之前 —— 与组合根 `on_frame` 同一位置。
        /// 淡出收尾的 E10 完成回调在**帧内**（tick 之后）派发，因此断言必须紧跟 `drain`。
        fn round(&mut self, by: Duration, overlay: &Overlay) {
            self.app.advance(by);
            let _ = overlay.sync(self.app.app_mut());
            let _ = self.app.drain().expect("帧");
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
    fn closing_fades_out_and_a_finished_animation_unloads_in_the_same_frame() {
        // R01（E10）：泵序是「钩子 → 帧」，淡出的最后一 tick 在钩子之后把 progress 写成 0。
        // 旧实现靠 `sync` 观测 `progress == 0` 收尾 ⇒ 真实泵序下永远等不到。现在由
        // `AnimationRouter` 的完成回调在**产生终值的同一帧内**卸载。
        //
        // B2（帧精确）：断言绑在「`animations_running` 变 false 的那一轮 `drain` 之后」——
        // 这一轮的钩子已经跑过（`sync` 在帧之前），**不补 sync**。旧 P0 形态（完成回调不卸载、
        // 等下一轮 sync 观测终值）在这里必红：完成帧后节点仍在。
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
        // 淡入：时钟推进 + 帧内 tick（完成回调只清动画句柄，不关闭）。
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("帧");
        assert_eq!(overlay.progress(harness.app.app()), 1.0, "淡入完成");
        let node = overlay.overlay_node().expect("浮层节点");

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

        let mut unloaded_in_the_completion_frame = false;
        for _ in 0..20 {
            harness.app.advance(Duration::from_millis(16));
            // 真实泵序的钩子步骤（组合根 `on_frame` 的位置）：在帧之前。
            let _ = overlay.sync(harness.app.app_mut());
            let _ = harness.app.drain().expect("帧");
            if !harness.app.app().animations_running(harness.window) {
                assert!(
                    overlay.overlay_node().is_none(),
                    "动画完成的那一轮 drain 之后节点必须已卸载（完成回调同帧收尾；\
                     等下一轮 sync 的旧形态会在这里变红）"
                );
                unloaded_in_the_completion_frame = true;
                break;
            }
        }
        assert!(
            unloaded_in_the_completion_frame,
            "淡出必须在 20 轮内走完并触发完成回调"
        );
        assert!(!overlay.is_open(harness.app.app()), "淡出结束后关闭");
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
        let outcome = harness
            .app
            .app_mut()
            .frame(harness.window)
            .expect("窗口")
            .expect("帧");
        assert_eq!(outcome, FrameOutcome::Idle, "收尾后不得残留出帧");
    }

    #[test]
    fn two_overlays_share_one_animation_completion_route() {
        // E10 单槽（E10-R02）：每个 Overlay 各自注册 `on_animation_complete` 时后注册者会静默
        // 覆盖先注册者，先关闭的浮层淡出永远收不了尾。共享 `AnimationRouter` 后两条淡出都必须
        // 收到完成事件并卸载。
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let first = harness.overlay(
            "first",
            OverlayOptions {
                dismiss: DismissPolicy::Explicit,
                fade: true,
                ..options()
            },
        );
        let second = harness.overlay(
            "second",
            OverlayOptions {
                dismiss: DismissPolicy::Explicit,
                fade: true,
                ..options()
            },
        );
        assert!(harness.open(&first, Anchor::node(anchor)));
        assert!(harness.open(&second, Anchor::node(anchor)));
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        assert_eq!(first.progress(harness.app.app()), 1.0, "淡入完成");
        assert_eq!(second.progress(harness.app.app()), 1.0, "淡入完成");

        assert!(first.close(harness.app.app_mut()));
        assert!(second.close(harness.app.app_mut()));
        for _ in 0..16 {
            harness.round(Duration::from_millis(16), &first);
            if first.overlay_node().is_none() && second.overlay_node().is_none() {
                break;
            }
        }
        assert!(!first.is_open(harness.app.app()), "第一个浮层已收尾");
        assert!(!second.is_open(harness.app.app()), "第二个浮层已收尾");
        assert!(first.overlay_node().is_none());
        assert!(second.overlay_node().is_none());
    }

    #[test]
    fn an_interleaved_replaced_completion_is_dispatched_by_animation_id() {
        // B4：分发必须**按 `AnimationId` 认消费者**，不能 FIFO。场景：A 淡出中重开会给 A 的旧
        // 动画报 `Replaced`，此时 B 也在淡出；FIFO 会把这条陈旧事件消费到 A 的新登记上，B 的
        // `Finished` 再错配给 A —— 结果 B 永远收不了尾（M5a 变异在本测试下必红）。
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let first = harness.overlay(
            "first",
            OverlayOptions {
                dismiss: DismissPolicy::Explicit,
                fade: true,
                ..options()
            },
        );
        let second = harness.overlay(
            "second",
            OverlayOptions {
                dismiss: DismissPolicy::Explicit,
                fade: true,
                ..options()
            },
        );
        assert!(harness.open(&first, Anchor::node(anchor)));
        assert!(harness.open(&second, Anchor::node(anchor)));
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        assert_eq!(first.progress(harness.app.app()), 1.0, "前置：A 淡入完成");
        assert_eq!(second.progress(harness.app.app()), 1.0, "前置：B 淡入完成");

        // A 开始淡出，走 32 ms（真实泵序：钩子 → 帧）。
        assert!(first.close(harness.app.app_mut()));
        harness.app.advance(Duration::from_millis(32));
        let _ = first.sync(harness.app.app_mut());
        let _ = second.sync(harness.app.app_mut());
        let _ = harness.app.drain().expect("淡出帧");
        assert!(
            first.overlay_node().is_some(),
            "前置：A 还在淡出窗口内（节点未卸载）"
        );

        // A 在淡出窗口内重开：旧动画 `Replaced` + 新动画登记（同窗重入只换锚点、撤销关闭）。
        assert!(
            !first
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    build_content,
                )
                .expect("A 重开"),
            "同窗重入返回 false（只换锚点）"
        );
        assert!(first.is_open(harness.app.app()));
        // B 紧接着关闭：两条在飞动画的登记顺序 ≠ 完成事件顺序（中间夹着陈旧的 `Replaced`）。
        assert!(second.close(harness.app.app_mut()));

        let mut settled = false;
        for _ in 0..20 {
            harness.app.advance(Duration::from_millis(16));
            let _ = first.sync(harness.app.app_mut());
            let _ = second.sync(harness.app.app_mut());
            let _ = harness.app.drain().expect("帧");
            if !harness.app.app().animations_running(harness.window) {
                settled = true;
                break;
            }
        }
        assert!(settled, "两条在飞动画必须在 20 轮内走完");
        assert!(
            first.overlay_node().is_some(),
            "重开的 A 不得被陈旧的 `Replaced` 事件摘掉（FIFO 分发会摘错消费者）"
        );
        assert!(first.is_open(harness.app.app()));
        assert_eq!(
            first.progress(harness.app.app()),
            1.0,
            "A 淡回不透明并留在屏上"
        );
        assert!(
            second.overlay_node().is_none(),
            "B 的完成事件必须命中 B 自己（FIFO 会把 A 的登记摘掉、让 B 永远收不了尾）"
        );
        assert!(!second.is_open(harness.app.app()), "B 已收尾");
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
    fn the_overlay_wrapper_declares_its_a11y_role() {
        // R12/E12：引擎浮层包装节点不再自动推导 `Dialog`（无声明 = GenericContainer）；角色/
        // 可访问名由 kit 的 `OverlayA11y` 显式写到包装节点，面板保持通用容器（不重复声明）。
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
        let wrapper = overlay.overlay_node().expect("浮层包装节点");
        let panel = overlay.panel_node().expect("面板");
        let snapshot = harness.app.a11y().expect("a11y 快照");
        let record = snapshot.node_for(wrapper).expect("包装节点 a11y");
        assert_eq!(record.role, A11yRole::Dialog, "包装节点必须显式声明 Dialog");
        assert_eq!(record.label_text(), Some("确认"));
        assert!(record.modal, "模态对话框必须声明 modal");
        let panel_record = snapshot.node_for(panel).expect("面板 a11y");
        assert_eq!(
            panel_record.role,
            A11yRole::GenericContainer,
            "面板不得重复声明同一角色（否则 AT 读两层对话框）"
        );
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
            let padding = surface.padding;
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
                    CONTENT.width + padding * 2.0,
                    CONTENT.height + padding * 2.0
                ),
                "{key} 面板尺寸只由内边距决定"
            );
            overlay.close_now(harness.app.app_mut());
            let _ = harness.app.drain().expect("帧");
        }
    }

    /// E13：菜单双层 / 对话框大偏移的阴影按旧 GPUI 主题的精确值回填（R15 / P2-4b4）。
    ///
    /// 旧值：菜单 `(0,1,2,0)+(0,3,6,-2)`（第二层负 spread）、dialog `(0,25,50,-12)`；
    /// tooltip 无阴影。`Shadows` 的数组序 = 绘制序（先下后上）。
    #[test]
    fn surfaces_cast_the_legacy_multi_layer_shadows() {
        let tokens = ComponentTokens::standard();
        let menu = OverlaySurface::menu();
        let layers: Vec<_> = menu
            .shadows
            .iter()
            .map(|shadow| (shadow.offset, shadow.blur, shadow.spread, shadow.color))
            .collect();
        assert_eq!(
            layers,
            vec![
                (
                    Point::new(0.0, 1.0),
                    2.0,
                    0.0,
                    Color::BLACK.with_opacity(tokens.shadow.menu_alpha),
                ),
                (
                    Point::new(0.0, 3.0),
                    6.0,
                    -2.0,
                    Color::BLACK.with_opacity(tokens.shadow.menu_alpha),
                ),
            ],
            "菜单必须是旧实现的双层接触阴影（含负 spread）"
        );

        let dialog = OverlaySurface::dialog();
        let layers: Vec<_> = dialog
            .shadows
            .iter()
            .map(|shadow| (shadow.offset, shadow.blur, shadow.spread))
            .collect();
        assert_eq!(
            layers,
            vec![(Point::new(0.0, 25.0), 50.0, -12.0)],
            "对话框必须是旧实现的大偏移单层阴影"
        );
        assert_eq!(
            dialog.shadows.iter().next().map(|shadow| shadow.color),
            Some(Color::BLACK.with_opacity(tokens.shadow.dialog_alpha)),
            "对话框阴影色 = 主题 dialog alpha"
        );

        assert!(
            OverlaySurface::tooltip().shadows.is_empty(),
            "tooltip 旧实现没有阴影"
        );
    }

    /// E13：浮层的节点结构是「包装节点 → 组根 → 遮罩 + 面板 → 内容」。
    ///
    /// 淡入淡出只登记组根：面板 / 遮罩 / 内容都在它的子树里，靠引擎的组不透明度级联跟随。
    /// 结构断言保证「组根真的包住整组」（若面板 / 遮罩仍挂在包装节点下，组根的淡出就够不到它们）。
    #[test]
    fn the_fade_group_root_holds_the_scrim_panel_and_content() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "dialog",
            OverlayOptions {
                modal: true,
                scrim: Some(Scrim::default()),
                surface: OverlaySurface::dialog(),
                a11y: OverlayA11y::dialog("确认"),
                fade: true,
                ..OverlayOptions::default()
            },
        );
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        let wrapper = overlay.overlay_node().expect("包装节点");
        let panel = overlay.panel_node().expect("面板");
        let scrim = overlay.scrim_node().expect("遮罩");
        let tree = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree();
        let children = tree.get(wrapper).expect("包装节点").children();
        assert_eq!(
            children.len(),
            1,
            "包装节点下只能有一层 kit 组根，实际 {children:?}"
        );
        let group = children[0];
        assert_eq!(
            tree.get(group).expect("组根").children(),
            [scrim, panel],
            "遮罩（先画）与面板（后画）都必须是组根的子节点"
        );
        assert_eq!(tree.get(panel).expect("面板").parent(), Some(group));
        assert_eq!(tree.get(group).expect("组根").parent(), Some(wrapper));
        assert!(
            tree.debug_id(group)
                .is_some_and(|id| id.ends_with(":group")),
            "组根 id 带 `:group` 后缀（调试 / 断言用）"
        );
    }

    /// E13：淡出是**整组不透明度** —— 面板背景与内容子节点在同一次动画里一起变暗。
    ///
    /// 真实 GPU 读回（WARP / lavapipe / Metal）：淡出到 `progress = p` 时
    /// - 面板 padding 区域（只有面板背景）的 alpha = `p`；
    /// - 内容盒子像素（面板背景 × 内容 quad 两层）的 alpha = `p + p(1 − p)`（E13 R2：没有
    ///   离屏「隔离组」，重叠图元在目标上直接混合）。
    ///
    /// 变异守卫：只让壳（面板背景）淡、内容不淡 ⇒ 内容像素停在 255，本测试变红。
    #[test]
    fn the_fade_dims_the_whole_group_in_the_readback() {
        let _guard = ui_core::test_support::snapshot::gpu_guard();
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        // tooltip 表面没有阴影：读回里没有额外的遮挡层，alpha 好算。
        let overlay = harness.overlay(
            "fade",
            OverlayOptions {
                surface: OverlaySurface::tooltip(),
                a11y: OverlayA11y::tooltip("提示"),
                fade: true,
                dismiss: DismissPolicy::Explicit,
                ..OverlayOptions::default()
            },
        );
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        assert_eq!(overlay.progress(harness.app.app()), 1.0, "前置：淡入完成");

        // 关掉并走到淡出中段（progress ∈ [0.4, 0.6]），每轮都是真实泵序（钩子 → 帧）。
        assert!(overlay.close(harness.app.app_mut()));
        let mut progress = 1.0;
        for _ in 0..20 {
            harness.app.advance(Duration::from_millis(8));
            let _ = overlay.sync(harness.app.app_mut());
            let _ = harness.app.drain().expect("淡出帧");
            progress = overlay.progress(harness.app.app());
            if (0.4..=0.6).contains(&progress) {
                break;
            }
        }
        assert!(
            (0.4..=0.6).contains(&progress),
            "前置：必须取到淡出中段，实际 progress = {progress}"
        );
        let expected_shell = (progress * 255.0).round() as i32;
        let expected_content = (progress * (2.0 - progress) * 255.0).round() as i32;

        let panel = harness.bounds(overlay.overlay_node().expect("浮层节点"));
        let padding = overlay.options().surface.padding;
        // 上边 padding 带的中点：避开四角圆角，也不压在内容盒子上。
        let padding_x = (panel.origin.x + panel.size.width * 0.5) as u32;
        let padding_y = (panel.origin.y + padding * 0.5) as u32;
        let content_x = (panel.origin.x + padding + CONTENT.width * 0.5) as u32;
        let content_y = (panel.origin.y + padding + CONTENT.height * 0.5) as u32;
        let image = harness
            .app
            .read_pixels()
            .unwrap_or_else(|error| panic!("离屏读回失败：{error:?}"));
        let shell_alpha =
            i32::from(ui_core::test_support::pixels::pixel(&image, padding_x, padding_y)[3]);
        let content_alpha =
            i32::from(ui_core::test_support::pixels::pixel(&image, content_x, content_y)[3]);
        assert!(
            (shell_alpha - expected_shell).abs() <= 8,
            "面板背景 alpha 应为组不透明度 {progress:.3}×255≈{expected_shell}，实测 {shell_alpha}"
        );
        assert!(
            (content_alpha - expected_content).abs() <= 8,
            "内容像素必须随组根一起淡：应是 p+p(1−p)≈{expected_content}，实测 {content_alpha}\
             （只淡壳的实现会停在 255）"
        );
        assert!(
            content_alpha < 240,
            "内容像素没有变暗 ⇒ 组不透明度没有级联到子节点"
        );
    }

    #[test]
    fn an_engine_side_close_cleans_the_layer_entry_and_stops_enforcing() {
        // R03：点外部 / Esc 是引擎直接卸载节点。关闭路径必须摘掉 Layer 登记；否则只要还有
        // 第二个浮层存活，`enforce` 每轮都判定失配并重排 + 出帧（I3 破坏）。
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let background = harness.overlay(
            "background",
            OverlayOptions {
                dismiss: DismissPolicy::Explicit,
                ..options()
            },
        );
        let foreground = harness.overlay("foreground", options());
        assert!(harness.open(&background, Anchor::node(anchor)));
        assert!(harness.open(&foreground, Anchor::node(anchor)));
        assert_eq!(harness.layer.len(harness.window), 2);

        // 点外部关掉最上层（foreground）：引擎立即卸载节点，kit 下一次 sync 才收尾。
        harness.click(Point::new(10.0, 10.0));
        assert!(
            foreground.sync(harness.app.app_mut()),
            "sync 必须发现引擎侧关闭"
        );
        assert!(!foreground.is_open(harness.app.app()));
        assert_eq!(
            harness.layer.len(harness.window),
            1,
            "关闭路径必须摘掉 Layer 登记（否则登记泄漏）"
        );
        assert!(
            !background.sync(harness.app.app_mut()),
            "稳态：没有登记失配 ⇒ enforce 零重排"
        );
        let outcome = harness
            .app
            .app_mut()
            .frame(harness.window)
            .expect("窗口")
            .expect("帧");
        assert_eq!(
            outcome,
            FrameOutcome::Idle,
            "关闭收尾后不得每轮出帧（R03 的空转）"
        );
    }

    #[test]
    fn reopening_after_an_engine_side_close_builds_a_new_overlay() {
        // R06：引擎已关（节点没了）但 kit 还没 sync 时重开，不能返回「本来就开着」把这次
        // 打开丢掉。
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "menu",
            OverlayOptions {
                dismiss: DismissPolicy::Explicit,
                ..options()
            },
        );
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        let old_node = overlay.overlay_node().expect("浮层节点");
        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::Escape, Modifiers::empty()));
        let _ = harness.app.drain().expect("帧");
        assert!(
            harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree()
                .get(old_node)
                .is_err(),
            "Esc 已卸载引擎节点（kit 尚未 sync）"
        );
        assert!(
            overlay.is_open(harness.app.app()),
            "sync 之前 kit 状态仍是打开"
        );

        let reopened = overlay
            .open(
                harness.app.app_mut(),
                harness.window,
                Anchor::node(anchor),
                build_content,
            )
            .expect("重开");
        assert!(reopened, "引擎已关闭时重开必须真的打开，不能静默丢掉");
        let new_node = overlay.overlay_node().expect("新浮层节点");
        assert_ne!(new_node, old_node, "重开建的是新浮层");
        assert!(overlay.is_open(harness.app.app()));
        assert_eq!(
            harness.layer.len(harness.window),
            1,
            "旧登记被替换，不得重复登记"
        );
    }

    #[test]
    fn closing_twice_during_the_fade_is_idempotent() {
        // R10：淡出中第二次 close 不得跳过淡出直接卸载（闪断），也不得重入。
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
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("帧");
        let _ = overlay.sync(harness.app.app_mut());
        assert_eq!(overlay.progress(harness.app.app()), 1.0, "淡入完成");
        let node = overlay.overlay_node().expect("浮层节点");

        assert!(overlay.close(harness.app.app_mut()), "第一次关闭");
        assert!(
            overlay.close(harness.app.app_mut()),
            "第二次关闭幂等返回 true"
        );
        assert!(
            overlay.is_open(harness.app.app()),
            "仍在关闭中（is_open 要到淡出收尾才 false）"
        );
        assert!(
            harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree()
                .get(node)
                .is_ok(),
            "第二次 close 不得跳过淡出直接卸载节点"
        );

        // 淡出到终值时由 E10 完成回调收尾（真实泵序：钩子 → 帧）；第二次 close 没有重启动画。
        for _ in 0..16 {
            harness.round(Duration::from_millis(16), &overlay);
            if overlay.overlay_node().is_none() {
                break;
            }
        }
        assert!(!overlay.is_open(harness.app.app()));
        assert!(
            harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree()
                .get(node)
                .is_err(),
            "节点在淡出结束后卸载"
        );
        assert!(
            !overlay.close(harness.app.app_mut()),
            "已关闭后再关返回 false"
        );
    }

    #[test]
    fn a_fade_less_panel_is_hidden_on_its_first_frame() {
        // R14：首帧按零尺寸落位。`fade: false` 时不能直接画出来（可见跳位），必须先隐藏，
        // 等 `sync` 读到实测尺寸、纠位后一起显示。
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay("menu", options());
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
        assert_eq!(
            overlay.progress(harness.app.app()),
            0.0,
            "首帧不显示（等待实测）"
        );
        let _ = harness.app.drain().expect("首帧");
        assert_eq!(
            overlay.progress(harness.app.app()),
            0.0,
            "首次 sync 之前仍不显示"
        );

        assert!(overlay.sync(harness.app.app_mut()), "首次实测后揭开面板");
        assert_eq!(
            overlay.progress(harness.app.app()),
            1.0,
            "fade=false 揭开即不透明"
        );
        let _ = harness.app.drain().expect("第二帧");
        let panel = harness.bounds(overlay.overlay_node().expect("浮层节点"));
        assert_eq!(
            panel.origin,
            Point::new(ANCHOR.origin.x, ANCHOR.max().y + 4.0),
            "第二帧在实测后的正确位置"
        );
    }

    #[test]
    fn the_wrapper_declares_menu_dialog_and_alert_dialog_roles_and_the_scrim_is_hidden() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let anchor = harness.anchor();
        let menu = harness.overlay(
            "menu",
            OverlayOptions {
                a11y: OverlayA11y::menu("文件"),
                ..options()
            },
        );
        // a11y 捕获按帧交付：打开后只出这一帧，快照里才有全树（`harness.open` 的 settle 会把
        // 帧记录推到「没有 a11y 工作」的后续帧，所以这里直接开 + drain）。
        assert!(
            menu.open(
                harness.app.app_mut(),
                harness.window,
                Anchor::node(anchor),
                build_content
            )
            .expect("打开菜单")
        );
        let _ = harness.app.drain().expect("帧");
        let menu_wrapper = menu.overlay_node().expect("菜单包装节点");
        let snapshot = harness.app.a11y().expect("a11y 快照");
        let record = snapshot.node_for(menu_wrapper).expect("菜单包装 a11y");
        assert_eq!(record.role, A11yRole::Menu, "菜单包装节点必须声明 Menu");
        assert_eq!(record.label_text(), Some("文件"));

        let modal = harness.overlay(
            "dialog",
            OverlayOptions {
                modal: true,
                scrim: Some(Scrim::default()),
                surface: OverlaySurface::dialog(),
                a11y: OverlayA11y::dialog("确认"),
                dismiss: DismissPolicy::Explicit,
                ..options()
            },
        );
        assert!(
            modal
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    build_content
                )
                .expect("打开模态")
        );
        let _ = harness.app.drain().expect("帧");
        let modal_wrapper = modal.overlay_node().expect("对话框包装节点");
        let scrim = modal.scrim_node().expect("模态遮罩");
        let snapshot = harness.app.a11y().expect("a11y 快照");
        let record = snapshot.node_for(modal_wrapper).expect("对话框包装 a11y");
        assert_eq!(record.role, A11yRole::Dialog);
        assert_eq!(record.label_text(), Some("确认"));
        let record = snapshot.node_for(scrim).expect("遮罩 a11y");
        assert_eq!(record.role, A11yRole::Group, "遮罩是装饰组");
        assert!(record.hidden, "遮罩必须对 AT 隐藏");

        // AlertDialog（E12 新增角色）：确认类模态显式声明，AT 立即播报。
        let alert = harness.overlay(
            "alert",
            OverlayOptions {
                modal: true,
                scrim: Some(Scrim::default()),
                surface: OverlaySurface::dialog(),
                a11y: OverlayA11y::alert_dialog("删除？"),
                dismiss: DismissPolicy::Explicit,
                ..options()
            },
        );
        assert!(
            alert
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    build_content
                )
                .expect("打开警告对话框")
        );
        let _ = harness.app.drain().expect("帧");
        let alert_wrapper = alert.overlay_node().expect("警告对话框包装节点");
        let snapshot = harness.app.a11y().expect("a11y 快照");
        let record = snapshot.node_for(alert_wrapper).expect("警告对话框 a11y");
        assert_eq!(record.role, A11yRole::AlertDialog);
        assert_eq!(record.label_text(), Some("删除？"));
        assert!(record.modal, "警告对话框是模态");
    }

    #[test]
    fn a_missing_anchor_does_not_open_an_overlay() {
        // R19：节点锚点的几何读不到（已卸载）时不建浮层，不在 (0,0) 建一个再等下一帧关。
        let mut harness = Harness::new();
        let anchor = harness.anchor();
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
        let overlay = harness.overlay("menu", options());
        assert!(
            !overlay
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    build_content
                )
                .expect("锚点失效不报错，只是不开"),
            "锚点失效 ⇒ open 返回 false"
        );
        assert!(overlay.overlay_node().is_none(), "不得建浮层节点");
        assert!(!overlay.is_open(harness.app.app()));
        assert_eq!(harness.layer.len(harness.window), 0);
    }

    #[test]
    fn an_invalid_placement_config_is_a_typed_error() {
        // R18：非有限 gap / margin 是调用方配置错误，必须走强类型错误，而不是悄悄落到 (0,0)。
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "menu",
            OverlayOptions {
                gap: f32::INFINITY,
                ..options()
            },
        );
        let error = overlay
            .open(
                harness.app.app_mut(),
                harness.window,
                Anchor::node(anchor),
                build_content,
            )
            .expect_err("非有限 gap 必须报错");
        assert!(
            matches!(error, OverlayError::PlacementConfig { gap, .. } if gap.is_infinite()),
            "必须是强类型 PlacementConfig 错误，实际 {error:?}"
        );
        assert!(overlay.overlay_node().is_none(), "配置错误不得建节点");
        assert_eq!(harness.layer.len(harness.window), 0);
    }
}

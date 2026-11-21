//! 模态对话框容器（旧 `crate::dialog` + `crate::alert` 浮层壳的引擎化重建，P2-4d1）。
//!
//! 旧实现分两层：`crate::dialog::Dialog` 只画「全窗遮罩 + 居中面板 + 进出场淡入淡出 +
//! 退场自摘」，内容是一个任意 view；`crate::alert::Alert` 是标题 / 消息 / 输入 / 按钮行的内容
//! 面板，由 `LayerExt::alert` 挂到窗口层的 alert 槽（恒高于所有 dialog）。本模块把**容器**接管
//! 过来，内容仍可来自 [`crate::kit::alert`]（校验 / 输入 / 按钮归 alert，容器 / 模态 / 焦点 /
//! 遮罩 / 动效 / a11y 归 dialog）：
//!
//! | 职责 | 落点 |
//! |---|---|
//! | 遮罩 + 居中面板 + z 层带 | [`crate::kit::overlay::Overlay`]（`Band::Dialog`；alert 形状用 `Band::Alert`，保留旧「alert 恒高于 dialog、至多一个」槽位语义） |
//! | 模态（焦点陷阱 / 恢复 / 背景不可交互） | 引擎 `OverlayOptions::{trap_focus, restore_focus}`（`modal: true`）；本模块不自行推导模态作用域 |
//! | Esc / 点击外部策略 | 引擎默认键绑定 Esc → `Dismiss`（关闭最上层浮层）；外部点击按 [`DismissPolicy`]（默认 `Explicit` = 旧 dialog「内容自己决定何时关闭」；alert 形状同旧 alert） |
//! | 入场 / 退场动画 | [`crate::kit::overlay::Overlay`] 的 `Transient` + `App::animate`：E13 之后 `StylePatch.opacity` 是**组不透明度**，面板根一调光整棵内容跟着淡；E10 完成回调（[`crate::kit::overlay::AnimationRouter`]）在终值同一帧卸载 |
//! | a11y | 引擎浮层**包装节点**显式 `Dialog` / `AlertDialog` + 可访问名（标题）；`aria-modal` 跟 `trap_focus`，遮罩 `hidden`（E12） |
//! | 标题 / 关闭按钮 / 按钮行 / 确认取消回调 | 本模块（面板内容里建，见 [`DialogBuilder`]） |
//! | 居中 | `sync` 按面板实测尺寸把浮层锚点重算成「面板中心 = 窗口中心」（覆盖旧 `justify_center` + `items_center`） |
//!
//! # 与旧实现的差异（登记见 `spec-deltas/kit-dialog.md`）
//!
//! - **关闭路径**：旧 `Dialog` 只由内容 `DismissEvent` 关闭，没有 Esc / 点外部；新实现有显式
//!   `confirm` / `cancel` / 关闭按钮 / [`Dialog::close`]，Esc 由引擎关闭最上层浮层，点外部默认
//!   **不关**（[`DismissPolicy::Explicit`]）——`OutsideClick` 由调用方显式开启。Esc / 点外部是
//!   引擎的**立即卸载**（无退场动画），程序化关闭才有淡出（与 [`crate::kit::overlay`] 同一差异）。
//! - **关闭中重开**：淡出窗口内再调 [`Dialog::open`] 按旧语义「关闭完成后才能再开」——先把上一次
//!   关闭收尾（派发一次 `on_close`），再按新内容重建；不静默丢弃打开请求。
//! - **Esc 无法禁用（引擎差异）**：旧 dialog / alert 都**不可能**被 Esc 关掉；新实现的 Esc 是引擎
//!   默认键绑定（对任意 [`DismissPolicy`] 都关闭最上层浮层），kit 层既不注册也不改写它 ⇒ 当前
//!   引擎下做不出「必须显式确认、不能被 Esc 关掉」的模态。kit 不新增 Esc 入口（保持旧语义面），
//!   差异登记为引擎候选（`OverlayOptions` 的关闭请求钩子 / 可禁用默认绑定，E 候选）。
//! - **内容所有权**：旧 `Entity<AnyView>` 由 layer 强引用、退场结束才释放；新实现是节点树 +
//!   `SignalScope`，卸载节点不等于释放应用层信号，调用方在对话框生命周期结束时调
//!   [`Dialog::release`]（与 `overlay` / `alert` / `button` 的释放契约一致）。
//! - **alert 内容根不再是对话框**（P2-4a3 修 P2-4d1 审查 D6）：`kit::alert` 的内容根声明
//!   `GenericContainer`，对话框角色只由本模块的包装节点声明（E12-R1），AT 不会读两层对话框。
//! - **alert 内容按钮的关闭中守卫**（P2-4a3 修 D2 的 alert 同类）：alert 的 OK / Cancel 不是本
//!   模块的 handler；`build_alert` 把容器的「关闭中」状态注入 `AlertBuilder::closing_guard`，
//!   淡出窗口内的第二次点击不再跑 alert 的用户动作（旧 dialog 用 `capture_any_mouse_down` 吞按下）。
//! - **视觉**：面板阴影用旧值 `(0, 25, 50, -12)`（[`OverlaySurface::dialog`]，E13
//!   `ShadowSpec.spread`）；Dark 外观的 1 px `line.divider` 边缘、旧实现的 8 px 上滑入场与长标题
//!   右侧留白未复刻（像素基线批次）。
//! - **关闭按钮**：普通对话框右上角图标按钮（默认在，可 [`DialogBuilder::close_label`] 给 a11y
//!   名）；**alert 形状恒不建**（旧 alert 没有关闭按钮，X 也无法绕过 alert 的 `cancel_action`）。
//!   关闭按钮与 Esc / 点外部同义（[`DialogClose::Dismissed`]），不触发确认 / 取消回调。
//! - **alert 的 Esc 语义**：引擎立即卸载并报 `Dismissed`，**不跑** alert 的 `cancel_action`
//!   （引擎没有应用层可拦截的关闭钩子）；alert 形状的用户关闭路径只有 OK / Cancel。
//!
//! # 用法（组合根）
//!
//! ```ignore
//! let layer = Layer::new();
//! let router = AnimationRouter::new();          // 应用级唯一（与 overlay / tooltip 共用）
//! let dialog = DialogBuilder::new("Delete file")
//!     .confirm("Delete", move |app| { /* … */ })
//!     .cancel("Keep", |_app| {})
//!     .build(app, theme.clone(), layer.clone(), router.clone(), "delete");
//! // 组合根 on_frame 钩子里（与各组件 sync 同一钩子）：
//! dialog.sync(app)?;
//! // 打开（首帧按零尺寸落位，sync 读到实测尺寸后居中并揭开）：
//! dialog.open(app, window)?;
//! ```
//!
//! alert 形状：内容面板由 [`crate::kit::alert::AlertBuilder`] 建，dialog 只提供容器：
//!
//! ```ignore
//! let dialog = DialogBuilder::alert("Delete file", move |dialog, _theme| {
//!     let dialog = dialog.clone();
//!     AlertBuilder::new().ok_cancel("Delete file", "This cannot be undone.")
//!         .ok_action(move |_, app| { dialog.close(app); })
//! })
//! .build(app, theme, layer, router, "delete-alert");
//! dialog.open(app, window)?;
//! ```
//!
//! # 生命周期
//!
//! 信号 / 瞬时值归自有 `SignalScope`（[`Dialog::release`] 释放）；面板节点归引擎（引擎浮层卸载
//! 时整棵子树消失）。关闭完成后 [`Dialog::is_open`] 变 `false`，关闭原因经
//! [`DialogBuilder::on_close`] 回调派发一次；重开走 [`Dialog::open`]（重建内容，旧内容在重开前
//! 释放）。

use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::{Rc, Weak},
};

use smol_str::{SmolStr, format_smolstr};
use taffy::style::{LengthPercentageAuto, Position};
use ui_core::{
    app::App,
    error::EngineError,
    interaction::ActionCtx,
    math::Point,
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::style::StylePatch,
    text::{TextAlign, TextStyle},
    window::WindowId,
};

use crate::{
    kit::{
        alert::{Alert, AlertBuilder},
        button::{Button, stretch_button},
        icon::IconKind,
        layer::Band,
        overlay::{
            Anchor, AnimationRouter, DismissPolicy, Overlay, OverlayA11y, OverlayError,
            OverlayOptions, OverlaySurface, Placement, Scrim,
        },
        size::UnifiedSizable,
    },
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};

/// 对话框内容宽度（旧 alert 的 `.w_96()` = 384 px；通用对话框默认同宽）。
const CONTENT_WIDTH: f32 = 384.0;

/// 内容列间距（旧 dialog `.gap_2()` / alert `.gap_2()` = 8 px）。
const CONTENT_GAP: f32 = 8.0;

/// 标题字号 / 字重（旧 alert 的标题 20 px、700）。
const TITLE_SIZE: f32 = 20.0;

/// 对话框的 a11y 角色（旧语义的 general dialog / alert dialog 二分）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogRole {
    /// 普通模态对话框（`Dialog`）。
    Dialog,
    /// 需要用户确认的模态提示（`AlertDialog`，AT 立即播报）。
    AlertDialog,
}

/// 关闭原因（[`DialogBuilder::on_close`] 的载荷；主动关闭 / Esc / 点外部同义）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogClose {
    /// 确认按钮。
    Confirmed,
    /// 取消按钮。
    Cancelled,
    /// 关闭按钮 / Esc / 点外部 / 程序化 [`Dialog::close`]。
    Dismissed,
}

/// 强类型错误。
#[derive(Debug)]
pub enum DialogError {
    /// 浮层 / 定位错误（窗口关闭、建树失败、非法定位参数）。
    Overlay(OverlayError),
    /// 引擎错误（建内容节点 / 同步按钮）。
    Engine(EngineError),
    /// 浮层节点不存在（打开中途被卸载的防御面；正常路径不会走到）。
    NotOpen,
}

impl From<OverlayError> for DialogError {
    fn from(error: OverlayError) -> Self {
        Self::Overlay(error)
    }
}

impl From<EngineError> for DialogError {
    fn from(error: EngineError) -> Self {
        Self::Engine(error)
    }
}

impl fmt::Display for DialogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overlay(error) => error.fmt(formatter),
            Self::Engine(error) => error.fmt(formatter),
            Self::NotOpen => formatter.write_str("dialog overlay is not open"),
        }
    }
}

impl std::error::Error for DialogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Overlay(error) => Some(error),
            Self::Engine(error) => Some(error),
            Self::NotOpen => None,
        }
    }
}

/// 自定义内容：在面板的内容列里建节点（持有 `Theme` 的调用方可以在这里挂任意 `kit` 控件）。
pub type DialogContent = Rc<dyn Fn(&mut App, WindowId, NodeId) -> Result<(), EngineError>>;

/// alert 内容源：每次打开按当前 dialog 句柄造一个 `AlertBuilder`（回调里可以关闭对话框）。
pub type DialogAlertSource = Rc<dyn Fn(&Dialog, &ThemeHandle) -> AlertBuilder>;

/// 确认 / 取消回调。
pub type DialogAction = Rc<dyn Fn(&mut App)>;

/// 关闭回调（[`DialogClose`] 为原因；每次关闭派发一次）。
pub type DialogCloseCallback = Rc<dyn Fn(DialogClose, &mut App)>;

/// 对话框配置（builder 内部值；对外只经 [`DialogBuilder`] 的方法设置）。
#[derive(Debug)]
struct DialogConfig {
    title: SmolStr,
    role: DialogRole,
    width: f32,
    header: bool,
    close_button: bool,
    close_label: SmolStr,
    dismiss: DismissPolicy,
    fade: bool,
}

impl DialogConfig {
    fn new(title: impl Into<SmolStr>) -> Self {
        Self {
            title: title.into(),
            role: DialogRole::Dialog,
            width: CONTENT_WIDTH,
            header: true,
            close_button: true,
            close_label: SmolStr::new_static("Close"),
            dismiss: DismissPolicy::Explicit,
            fade: true,
        }
    }

    fn band(&self) -> Band {
        match self.role {
            DialogRole::Dialog => Band::Dialog,
            // 旧 alert 槽：恒高于所有 dialog、至多一个（`Layer::register` 的单例替换语义）。
            DialogRole::AlertDialog => Band::Alert,
        }
    }

    fn a11y(&self) -> OverlayA11y {
        match self.role {
            DialogRole::Dialog => OverlayA11y::dialog(self.title.clone()),
            DialogRole::AlertDialog => OverlayA11y::alert_dialog(self.title.clone()),
        }
    }
}

/// 打开时的运行时状态（`RefCell`：句柄 `Clone` 后共享）。
#[derive(Default)]
struct DialogState {
    window: Option<WindowId>,
    /// 内容列（标题下的正文容器；alert 也挂在这里）。
    body: Option<NodeId>,
}

/// [`Dialog::begin_close`] 的结果：用户回调只在 `Started` 这一次跑（淡出中双击不得重复执行）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseStart {
    /// 本次调用真的开始了关闭（写原因 + 淡出 / 立即卸载）。
    Started,
    /// 已经在关闭（淡出中）：幂等，不重启动画，也不再跑用户回调。
    AlreadyClosing,
    /// 对话框不处于打开态（已关闭 / 从未打开）。
    NotOpen,
}

struct DialogInner {
    overlay: Overlay,
    config: DialogConfig,
    theme: ThemeHandle,
    scope: SignalScope,
    /// 对话框状态（`true` 从打开到退场结束；`is_open` 读它）。
    open: Signal<bool>,
    /// 是否已经开始程序化关闭（淡出中；Esc / 点外部是引擎立即卸载，不置位）。
    closing: Cell<bool>,
    /// 本次关闭的原因（开始关闭时写入；引擎侧关闭不写 ⇒ 默认 `Dismissed`）。
    reason: Cell<Option<DialogClose>>,
    on_close: RefCell<Option<DialogCloseCallback>>,
    content: RefCell<Option<DialogContent>>,
    alert_source: RefCell<Option<DialogAlertSource>>,
    confirm: Option<(SmolStr, DialogAction)>,
    cancel: Option<(SmolStr, DialogAction)>,
    confirm_button: RefCell<Option<Button>>,
    cancel_button: RefCell<Option<Button>>,
    close_button: RefCell<Option<Button>>,
    alert: RefCell<Option<Alert>>,
    state: RefCell<DialogState>,
}

/// 对话框句柄（`Rc` 浅拷贝；可以 move 进按钮回调）。
#[derive(Clone)]
pub struct Dialog {
    inner: Rc<DialogInner>,
}

/// 对话框 builder（标题 / 内容 / 按钮 / 关闭策略都选好之后 `build`）。
pub struct DialogBuilder {
    config: DialogConfig,
    content: Option<DialogContent>,
    alert_source: Option<DialogAlertSource>,
    confirm: Option<(SmolStr, DialogAction)>,
    cancel: Option<(SmolStr, DialogAction)>,
    on_close: Option<DialogCloseCallback>,
}

impl DialogBuilder {
    /// 普通对话框（`Dialog` 角色，标题行 + 可选关闭按钮 + 内容列 + 按钮行）。
    #[must_use]
    pub fn new(title: impl Into<SmolStr>) -> Self {
        Self {
            config: DialogConfig::new(title),
            content: None,
            alert_source: None,
            confirm: None,
            cancel: None,
            on_close: None,
        }
    }

    /// alert 形状：内容面板由 `build` 造 [`AlertBuilder`]，dialog 只做容器（`AlertDialog` 角色、
    /// `Band::Alert` 槽、无标题行 —— alert 自己画标题、**无关闭按钮**）。
    ///
    /// alert 形状没有关闭按钮：旧 alert 只有 OK / Cancel；X 与 Esc 同义（`Dismissed`）却无法
    /// 等价成 alert 的 `cancel_action`（旧用户关闭路径都会跑对应动作），保留它就会绕过用户语义。
    /// Esc 仍由引擎关闭（`Dismissed`，不跑 `cancel_action`，见模块文档）。
    ///
    /// 回调每次打开都会被调用：alert 的 OK / Cancel 动作里可以拿传入的 [`Dialog`] 句柄关闭对话框
    ///（`AlertBuilder` 的校验失败不会调用 OK 动作，因此「校验通过才关」自然成立）。
    #[must_use]
    pub fn alert(
        title: impl Into<SmolStr>,
        build: impl Fn(&Dialog, &ThemeHandle) -> AlertBuilder + 'static,
    ) -> Self {
        let mut builder = Self::new(title);
        builder.config.role = DialogRole::AlertDialog;
        builder.config.header = false;
        builder.alert_source = Some(Rc::new(build));
        builder
    }

    /// 自定义内容（在标题下、按钮行上的内容列里建节点）。
    #[must_use]
    pub fn content(
        mut self,
        build: impl Fn(&mut App, WindowId, NodeId) -> Result<(), EngineError> + 'static,
    ) -> Self {
        self.content = Some(Rc::new(build));
        self
    }

    /// 确认按钮（主按钮；点击先开始关闭，再跑回调）。
    #[must_use]
    pub fn confirm(
        mut self,
        label: impl Into<SmolStr>,
        action: impl Fn(&mut App) + 'static,
    ) -> Self {
        self.confirm = Some((label.into(), Rc::new(action)));
        self
    }

    /// 取消按钮（次按钮；点击先开始关闭，再跑回调）。
    #[must_use]
    pub fn cancel(
        mut self,
        label: impl Into<SmolStr>,
        action: impl Fn(&mut App) + 'static,
    ) -> Self {
        self.cancel = Some((label.into(), Rc::new(action)));
        self
    }

    /// 标题行（默认在，alert 形状默认不在；内容自己画标题时关掉）。
    #[must_use]
    pub fn header(mut self, shown: bool) -> Self {
        self.config.header = shown;
        self
    }

    /// 右上角关闭按钮（普通对话框默认在；**alert 形状恒不建**，见 [`DialogBuilder::alert`]）。
    ///
    /// 关闭 = [`DialogClose::Dismissed`]，不触发确认 / 取消回调。
    #[must_use]
    pub fn close_button(mut self, shown: bool) -> Self {
        self.config.close_button = shown;
        self
    }

    /// 关闭按钮的 a11y 可访问名（icon-only 按钮在 AT 里需要名字；默认 `Close`）。
    ///
    /// 文案由调用方给（i18n 在调用侧解析，例如 `close_label(t!(cx, "dialog.close"))`）；只影响
    /// 会建关闭按钮的普通对话框。
    #[must_use]
    pub fn close_label(mut self, label: impl Into<SmolStr>) -> Self {
        self.config.close_label = label.into();
        self
    }

    /// 内容宽度（默认 384 px；非有限 / 非正值保持默认）。
    #[must_use]
    pub fn width(mut self, width: f32) -> Self {
        if width.is_finite() && width > 0.0 {
            self.config.width = width;
        }
        self
    }

    /// 关闭策略（默认 [`DismissPolicy::Explicit`] = 点外部不关，旧 dialog / alert 语义）。
    ///
    /// 只影响**点外部**：Esc 是引擎默认键绑定（对任意策略都关闭最上层浮层），kit 层无法禁用或
    /// 改写 —— 「必须显式确认、不能被 Esc 关掉」的模态在当前引擎下做不到（见模块文档）。
    #[must_use]
    pub fn dismiss(mut self, dismiss: DismissPolicy) -> Self {
        self.config.dismiss = dismiss;
        self
    }

    /// 是否播放淡入淡出（默认 `true`；`false` = 立即显示 / 立即关闭）。
    #[must_use]
    pub fn fade(mut self, fade: bool) -> Self {
        self.config.fade = fade;
        self
    }

    /// 关闭回调（每完成一次关闭派发一次，载荷是关闭原因）。
    #[must_use]
    pub fn on_close(mut self, callback: impl Fn(DialogClose, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(callback));
        self
    }

    /// 建对话框（关闭态；之后 [`Dialog::open`] 打开）。
    ///
    /// `layer` / `router` 必须是组合根的共享实例（与其它浮层同一张层带表、同一个 E10 完成路由），
    /// 否则 alert 槽位与淡出收尾都不成立。
    #[must_use]
    pub fn build(
        self,
        app: &mut App,
        theme: ThemeHandle,
        layer: crate::kit::layer::Layer,
        router: AnimationRouter,
        key: impl Into<SmolStr>,
    ) -> Dialog {
        let key = key.into();
        let scope = app.signal_scope();
        let open = app.signal_scoped(scope, false);
        let overlay = Overlay::new(
            app,
            theme.clone(),
            layer,
            router,
            key,
            OverlayOptions {
                // 居中不是「锚定在某个 12 向位置上」：`sync` 每帧按面板实测尺寸把锚点重算成
                // 「面板中心 = 窗口中心」的等价矩形（`TOP_START` + gap/margin 0 + 不翻边）。
                placement: Placement::TOP_START,
                gap: 0.0,
                margin: 0.0,
                flip: false,
                dismiss: self.config.dismiss,
                modal: true,
                restore_focus: true,
                band: self.config.band(),
                surface: OverlaySurface::dialog(),
                scrim: Some(Scrim::default()),
                a11y: self.config.a11y(),
                fade: self.config.fade,
            },
        );
        Dialog {
            inner: Rc::new(DialogInner {
                overlay,
                config: self.config,
                theme,
                scope,
                open,
                closing: Cell::new(false),
                reason: Cell::new(None),
                on_close: RefCell::new(self.on_close),
                content: RefCell::new(self.content),
                alert_source: RefCell::new(self.alert_source),
                confirm: self.confirm,
                cancel: self.cancel,
                confirm_button: RefCell::new(None),
                cancel_button: RefCell::new(None),
                close_button: RefCell::new(None),
                alert: RefCell::new(None),
                state: RefCell::new(DialogState::default()),
            }),
        }
    }
}

impl Dialog {
    /// 打开对话框（建浮层 + 面板内容；首帧按零尺寸落位，[`Dialog::sync`] 读到实测尺寸后居中并
    /// 揭开）。
    ///
    /// **幂等**：真的处于打开态（未在关闭、浮层还在）时返回 `Ok(false)`。**关闭中重开**（淡出窗口
    /// 内 / 引擎侧已卸载但状态还没 sync 收尾）按旧语义「关闭完成后才能再开」处理：先把上一次关闭
    /// 收尾（派发一次 `on_close`，原因取已记录的那一个），再按新内容重建，返回 `Ok(true)`。
    ///
    /// # Errors
    /// [`DialogError::Overlay`]：窗口已关闭 / 建浮层失败；[`DialogError::Engine`]：内容节点 /
    /// alert 面板建树失败（此时浮层已卸载，不留半成品）。
    pub fn open(&self, app: &mut App, window: WindowId) -> Result<bool, DialogError> {
        if *app.read(&self.inner.open) {
            if !self.inner.closing.get() && self.inner.overlay.is_open(app) {
                return Ok(false);
            }
            // 关闭中 / 引擎侧已卸载：收尾上一次关闭（`on_close` 派发一次），再重建内容。
            self.inner.overlay.close_now(app);
            self.finalize(app);
        }
        // 引擎侧残留（状态异常）：先彻底关掉旧的，再按新的一次打开。
        if self.inner.overlay.is_open(app) {
            self.inner.overlay.close_now(app);
        }
        self.release_content(app);
        let viewport = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .size();
        let anchor = Anchor::point(Point::new(viewport.width * 0.5, viewport.height * 0.5));
        let opened = self.inner.overlay.open(
            app,
            window,
            anchor,
            |_tree: &mut NodeTree, _panel: NodeId| -> Result<(), EngineError> { Ok(()) },
        )?;
        if !opened {
            return Ok(false);
        }
        self.inner.state.borrow_mut().window = Some(window);
        let built = self
            .build_content(app, window)
            .and_then(|()| self.build_alert(app, window));
        if let Err(error) = built {
            self.inner.overlay.close_now(app);
            self.release_content(app);
            self.inner.state.borrow_mut().window = None;
            return Err(error);
        }
        self.inner.reason.set(None);
        self.inner.closing.set(false);
        let _ = app.write(&self.inner.open, true);
        Ok(true)
    }

    /// 程序化关闭（淡出；`fade = false` 时浮层立即卸载）。返回是否有对话框被关（已关闭 ⇒ `false`；
    /// 淡出中再次关闭幂等返回 `true`，但不会重启动画，也不会再跑用户回调）。
    ///
    /// 关闭原因记 [`DialogClose::Dismissed`]；状态收尾在 [`Dialog::sync`]（`is_open` 变 `false`
    /// 并派发 `on_close`），与引擎侧关闭（Esc / 点外部）同一模型。Esc / 点外部由引擎直接卸载节点，
    /// 原因同样是 `Dismissed`。
    pub fn close(&self, app: &mut App) -> bool {
        !matches!(
            self.begin_close(app, DialogClose::Dismissed),
            CloseStart::NotOpen
        )
    }

    /// 立即关闭（不播淡出；已关闭 ⇒ `false`）。
    ///
    /// 已记录的原因（确认 / 取消 / 关闭按钮先开始的关闭）保留：混合路径（退场中再 `close_now`）
    /// 不得把 `Confirmed` / `Cancelled` 改写成 `Dismissed`。
    pub fn close_now(&self, app: &mut App) -> bool {
        if !*app.read(&self.inner.open) {
            return false;
        }
        self.inner.closing.set(true);
        if self.inner.reason.get().is_none() {
            self.inner.reason.set(Some(DialogClose::Dismissed));
        }
        self.inner.overlay.close_now(app);
        self.finalize(app);
        true
    }

    /// 帧同步（组合根 `on_frame` 钩子里调用；测试直接调）：居中纠位 → 浮层同步（层带 / 外部
    /// 关闭探测 / 淡出收尾）→ 按钮 / alert 同步 → 关闭收尾（派发 `on_close`）。
    ///
    /// 返回 `true` = 本帧做了工作。稳态返回 `false` 且不请求帧（I3）。
    ///
    /// # Errors
    /// 按钮 / alert 同步的引擎错误原样传播。
    pub fn sync(&self, app: &mut App) -> Result<bool, DialogError> {
        if !*app.read(&self.inner.open) {
            return Ok(false);
        }
        let mut changed = self.recenter(app);
        changed |= self.inner.overlay.sync(app);
        for button in [
            self.inner.confirm_button.borrow().clone(),
            self.inner.cancel_button.borrow().clone(),
            self.inner.close_button.borrow().clone(),
        ]
        .into_iter()
        .flatten()
        {
            changed |= button.sync(app)?;
        }
        if let Some(alert) = self.inner.alert.borrow().as_ref() {
            changed |= alert.sync(app)?;
        }
        if !self.inner.overlay.is_open(app) {
            self.finalize(app);
            changed = true;
        }
        Ok(changed)
    }

    /// 是否打开（含退场淡出中；引擎侧已卸载但还没 sync 时仍为 `true` —— 状态在帧边界更新）。
    #[must_use]
    pub fn is_open(&self, app: &App) -> bool {
        *app.read(&self.inner.open)
    }

    /// 是否正在程序化关闭（淡出中）。
    #[must_use]
    pub fn is_closing(&self) -> bool {
        self.inner.closing.get()
    }

    /// 标题（也是包装节点的 a11y 可访问名）。
    #[must_use]
    pub fn title(&self) -> &str {
        &self.inner.config.title
    }

    /// 浮层键（调试 id 前缀）。
    #[must_use]
    pub fn key(&self) -> &str {
        self.inner.overlay.key()
    }

    /// 打开状态信号（外部触发器画按下态 / 绑定 a11y 用）。
    #[must_use]
    pub fn open_state(&self) -> Signal<bool> {
        self.inner.overlay.open_state()
    }

    /// 引擎浮层包装节点（a11y 角色 / `App::modal_scope` 的落点）。
    #[must_use]
    pub fn overlay_node(&self) -> Option<NodeId> {
        self.inner.overlay.overlay_node()
    }

    /// 面板节点（背景 / 圆角 / 内边距 / 阴影）。
    #[must_use]
    pub fn panel_node(&self) -> Option<NodeId> {
        self.inner.overlay.panel_node()
    }

    /// 内容列（标题下、按钮行上的正文容器）。
    #[must_use]
    pub fn body_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().body
    }

    /// 确认按钮（没有配置时 `None`）。
    #[must_use]
    pub fn confirm_button(&self) -> Option<Button> {
        self.inner.confirm_button.borrow().clone()
    }

    /// 取消按钮（没有配置时 `None`）。
    #[must_use]
    pub fn cancel_button(&self) -> Option<Button> {
        self.inner.cancel_button.borrow().clone()
    }

    /// 关闭按钮（没有配置时 `None`）。
    #[must_use]
    pub fn close_button(&self) -> Option<Button> {
        self.inner.close_button.borrow().clone()
    }

    /// 只读访问 alert 内容（没有 alert 形状 / 还没打开时返回 `None`）。
    #[must_use]
    pub fn with_alert<R>(&self, read: impl FnOnce(&Alert) -> R) -> Option<R> {
        self.inner.alert.borrow().as_ref().map(read)
    }

    /// 释放组件（浮层 / 按钮 / alert 的信号作用域与 Action handler；幂等）。
    ///
    /// **打开中调用 = 放弃关闭收尾**：浮层立即卸载，但**不派发** `on_close`（没有「关闭完成」这件
    /// 事）；需要关闭事件时先 [`Dialog::close_now`]。释放后句柄不可再用（`sync` / `open` 会读已
    /// 回收的信号而 panic），与 `overlay` / `alert` / `button` 的释放契约一致。
    pub fn release(&self, app: &mut App) {
        self.inner.overlay.release(app);
        self.release_content(app);
        self.inner.closing.set(false);
        let _ = self.inner.reason.take();
        let _ = app.write(&self.inner.open, false);
        self.inner.state.borrow_mut().window = None;
        let _ = app.release_scope(self.inner.scope);
    }

    // ── 内部 ──────────────────────────────────────────────────────────────

    /// 建面板内容（标题行 / 内容列 / 按钮行 / 关闭按钮）。内容列记进 state，alert 随后挂进去。
    ///
    /// **关闭按钮最后建**：引擎的绘制序 / 命中优先序就是子节点序（倒序命中），而它绝对定位在内容
    /// 列右上角。`header(false)`（alert 形状 / 无标题行）时内容列从面板顶端开始铺满整宽，先建的
    /// 关闭按钮会被内容整块盖住、永远收不到命中（P2-4d1 审查 D1：alert 形状的「死控件」）。
    fn build_content(&self, app: &mut App, window: WindowId) -> Result<(), DialogError> {
        let panel = self
            .inner
            .overlay
            .panel_node()
            .ok_or(DialogError::NotOpen)?;
        let key = self.inner.overlay.key();
        let root = NodeBuilder::boxed()
            .id(format_smolstr!("{key}:content"))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .relative()
                    .width(self.inner.config.width)
                    .gap(CONTENT_GAP)
                    .build(),
            )
            .build(tree_mut(app, window)?, panel)?;
        if self.inner.config.header && !self.inner.config.title.is_empty() {
            let title_theme = self.inner.theme.clone();
            NodeBuilder::text(self.inner.config.title.clone(), {
                let mut style = TextStyle::new(TITLE_SIZE);
                style.weight = 700;
                style.alignment = TextAlign::Center;
                style
            })
            .style(StyleBuilder::new().width_full().build())
            .bind_style(move |cx| StylePatch {
                foreground: Some(title_theme.role(cx, Role::FgDefault)),
                ..StylePatch::default()
            })
            .build(tree_mut(app, window)?, root)?;
        }
        let body = NodeBuilder::boxed()
            .id(format_smolstr!("{key}:body"))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .width_full()
                    .gap(CONTENT_GAP)
                    .build(),
            )
            .build(tree_mut(app, window)?, root)?;
        self.inner.state.borrow_mut().body = Some(body);
        let content = self.inner.content.borrow().clone();
        if let Some(content) = content {
            content(app, window, body)?;
        }
        if self.inner.confirm.is_some() || self.inner.cancel.is_some() {
            self.build_button_row(app, window, root, key)?;
        }
        // alert 形状恒不建关闭按钮（旧 alert 没有 X；X 也无法走 alert 的 `cancel_action`）。
        if self.inner.config.close_button && matches!(self.inner.config.role, DialogRole::Dialog) {
            let weak = Rc::downgrade(&self.inner);
            let button = Button::new(
                app,
                self.inner.theme.clone(),
                format_smolstr!("{key}:close"),
            )
            .icon(IconKind::X)
            .a11y_label(self.inner.config.close_label.clone())
            .ghost()
            .small()
            .on_click(move |ctx: &mut ActionCtx<'_>| {
                if let Some(inner) = weak.upgrade() {
                    let dialog = Dialog { inner };
                    let _ = dialog.begin_close(ctx.app_mut(), DialogClose::Dismissed);
                }
            });
            let node = button.build(app, window, root)?;
            position_close_button(app, window, node)?;
            *self.inner.close_button.borrow_mut() = Some(button);
        }
        Ok(())
    }

    /// 按钮行：确认（主）/ 取消（次）等分铺满面板（旧 alert 的 `flex_1 + justify_center`）。
    fn build_button_row(
        &self,
        app: &mut App,
        window: WindowId,
        root: NodeId,
        key: &str,
    ) -> Result<(), DialogError> {
        let row = NodeBuilder::boxed()
            .id(format_smolstr!("{key}:buttons"))
            .style(StyleBuilder::new().flex_row().width_full().build())
            .build(tree_mut(app, window)?, root)?;
        let paired = self.inner.confirm.is_some() && self.inner.cancel.is_some();
        if let Some((label, action)) = &self.inner.confirm {
            let weak = Rc::downgrade(&self.inner);
            let action = action.clone();
            let button = Button::new(app, self.inner.theme.clone(), format_smolstr!("{key}:ok"))
                .label(label.clone())
                .on_click(move |ctx: &mut ActionCtx<'_>| {
                    if confirm_started(&weak, ctx.app_mut(), DialogClose::Confirmed) {
                        action(ctx.app_mut());
                    }
                });
            let button = if paired { button.joined(0, 2) } else { button };
            let node = button.build(app, window, row)?;
            stretch_button(app, window, node)?;
            *self.inner.confirm_button.borrow_mut() = Some(button);
        }
        if let Some((label, action)) = &self.inner.cancel {
            let weak = Rc::downgrade(&self.inner);
            let action = action.clone();
            let button = Button::new(
                app,
                self.inner.theme.clone(),
                format_smolstr!("{key}:cancel"),
            )
            .secondary()
            .label(label.clone())
            .joined(1, 2)
            .on_click(move |ctx: &mut ActionCtx<'_>| {
                if confirm_started(&weak, ctx.app_mut(), DialogClose::Cancelled) {
                    action(ctx.app_mut());
                }
            });
            let node = button.build(app, window, row)?;
            stretch_button(app, window, node)?;
            *self.inner.cancel_button.borrow_mut() = Some(button);
        }
        Ok(())
    }

    /// 把 alert 内容面板挂进内容列（alert 形状）。
    fn build_alert(&self, app: &mut App, window: WindowId) -> Result<(), DialogError> {
        let source = self.inner.alert_source.borrow().clone();
        let Some(source) = source else {
            return Ok(());
        };
        let body = self
            .inner
            .state
            .borrow()
            .body
            .ok_or(DialogError::Engine(EngineError::UnknownWindow { window }))?;
        let builder = source(self, &self.inner.theme);
        // alert 的内容按钮不是 dialog 的 handler：把容器的「关闭中」状态注入 alert，淡出窗口内的
        // 第二次点击不再跑 alert 的 OK / Cancel 动作（P2-4d1 审查 D2；P2-4a3 修 alert 内容）。
        let weak = Rc::downgrade(&self.inner);
        let builder =
            builder.closing_guard(move || weak.upgrade().is_some_and(|inner| inner.closing.get()));
        let alert = builder.build(app, window, body, &self.inner.theme)?;
        *self.inner.alert.borrow_mut() = Some(alert);
        Ok(())
    }

    /// 居中：把浮层锚点重算成「面板中心 = 窗口中心」的等价固定矩形。
    ///
    /// 浮层（`Overlay`）的定位语义是「相对锚点 + 12 向 placement」；对话框没有锚点，只有
    /// 「在窗口里居中」。`TOP_START` + `gap/margin 0` + `flip false` 下，原点 = 锚点 − 面板尺寸，
    /// 因此把锚点取成 `(面板中心 x, 面板中心 y + 面板高度)`，就得到居中的原点；面板比视口大时由
    /// 浮层的 clamp 兜底（贴左上角，不产生负坐标）。
    fn recenter(&self, app: &mut App) -> bool {
        if self.inner.closing.get() {
            return false;
        }
        let (Some(window), Some(node)) = (
            self.inner.state.borrow().window,
            self.inner.overlay.overlay_node(),
        ) else {
            return false;
        };
        let Some(viewport) = app.window(window).map(|target| target.size()) else {
            return false;
        };
        let Some(bounds) = app
            .window(window)
            .and_then(|target| target.tree().get(node).ok())
            .map(|target| target.layout().bounds)
        else {
            return false;
        };
        let desired = Point::new(
            ((viewport.width - bounds.size.width) * 0.5).max(0.0),
            ((viewport.height - bounds.size.height) * 0.5).max(0.0),
        );
        if (bounds.origin.x - desired.x).abs() < 0.5 && (bounds.origin.y - desired.y).abs() < 0.5 {
            return false;
        }
        let anchor = Anchor::point(Point::new(desired.x, desired.y + bounds.size.height));
        self.inner
            .overlay
            .open(
                app,
                window,
                anchor,
                |_tree: &mut NodeTree, _panel: NodeId| -> Result<(), EngineError> { Ok(()) },
            )
            .is_ok()
    }

    /// 开始程序化关闭（幂等）：写原因、播淡出。见 [`CloseStart`] 的三个结果。
    ///
    /// 状态收尾统一在 [`Dialog::sync`]（或立即关闭的 [`Dialog::close_now`]）里做 —— 引擎侧关闭
    /// 与程序化关闭走同一个 `finalize` 点，`on_close` 的时序不随 `fade` 配置而变化。
    fn begin_close(&self, app: &mut App, reason: DialogClose) -> CloseStart {
        if !*app.read(&self.inner.open) {
            return CloseStart::NotOpen;
        }
        if self.inner.closing.get() {
            return CloseStart::AlreadyClosing;
        }
        self.inner.closing.set(true);
        self.inner.reason.set(Some(reason));
        self.inner.overlay.close(app);
        CloseStart::Started
    }

    /// 关闭收尾：清状态、派发关闭回调（每完成一次关闭一次）。
    fn finalize(&self, app: &mut App) {
        if !*app.read(&self.inner.open) {
            return;
        }
        let reason = self.inner.reason.take().unwrap_or(DialogClose::Dismissed);
        self.inner.closing.set(false);
        let _ = app.write(&self.inner.open, false);
        self.inner.state.borrow_mut().window = None;
        let callback = self.inner.on_close.borrow().clone();
        if let Some(callback) = callback {
            callback(reason, app);
        }
    }

    /// 释放内容侧组件（按钮 / alert）；重开前与 [`Dialog::release`] 共用。
    fn release_content(&self, app: &mut App) {
        for button in [
            self.inner.confirm_button.borrow_mut().take(),
            self.inner.cancel_button.borrow_mut().take(),
            self.inner.close_button.borrow_mut().take(),
        ]
        .into_iter()
        .flatten()
        {
            button.release(app);
        }
        if let Some(alert) = self.inner.alert.borrow_mut().take() {
            alert.release(app);
        }
    }
}

// ── 免费函数 ──────────────────────────────────────────────────────────────

/// 确认 / 取消按钮点击的关闭守卫：返回 `true` = 本次点击真的开始了关闭（调用方的回调跟着跑）。
///
/// 淡出窗口内第二次点击时 [`Dialog::begin_close`] 返回 [`CloseStart::AlreadyClosing`]：忽略这次
/// 点击，避免删除类确认动作被执行两次（旧 dialog 用 `capture_any_mouse_down` 吞掉按下；P2-4d1
/// 审查 D2）。句柄已被释放（`weak` 升不起来）时没有可关闭的对话框，但点击仍按旧行为跑回调。
fn confirm_started(weak: &Weak<DialogInner>, app: &mut App, reason: DialogClose) -> bool {
    match weak.upgrade() {
        Some(inner) => matches!(
            (Dialog { inner }).begin_close(app, reason),
            CloseStart::Started
        ),
        None => true,
    }
}

/// 节点树入口（窗口不存在是强类型错误）。
fn tree_mut(app: &mut App, window: WindowId) -> Result<&mut NodeTree, EngineError> {
    app.window_mut(window)
        .map(|target| target.tree_mut())
        .ok_or(EngineError::UnknownWindow { window })
}

/// 关闭按钮贴内容列右上角（标题居中时它不参与标题行的宽度分配）。
fn position_close_button(app: &mut App, window: WindowId, node: NodeId) -> Result<(), EngineError> {
    let tree = tree_mut(app, window)?;
    let mut style = tree.get(node)?.style().clone();
    style.inner.position = Position::Absolute;
    style.inner.inset.top = LengthPercentageAuto::length(0.0);
    style.inner.inset.right = LengthPercentageAuto::length(0.0);
    tree.set_style(node, style)
}

#[cfg(test)]
mod tests {
    //! 引擎树上的行为：居中 / 内容与按钮 / 确认取消回调 / 关闭原因 / Esc / 点外部 / 模态（焦点
    //! 陷阱与恢复、背景不可交互）/ E13 组淡入淡出 / E10 完成帧收尾 / a11y / I3 / alert 协作 /
    //! 层带（dialog 栈、alert 槽、context menu 让位）。

    use std::{cell::Cell, rc::Rc, time::Duration};

    use smol_str::format_smolstr;
    use ui_core::{
        a11y::A11yRole,
        app::App,
        event::{Action, InputEvent, KeyCode, Modifiers, PointerButton},
        math::{Point, Rect, Size},
        node::{builder::NodeBuilder, id::NodeId, signal::Signal},
        paint::{
            color::Color,
            primitive::{Primitive, QuadPaint},
        },
        test_support::app::TestApp,
        window::{FrameOutcome, WindowId},
    };

    use super::{Dialog, DialogBuilder, DialogClose};
    use crate::{
        kit::{
            alert::AlertBuilder,
            layer::{Band, Layer},
            overlay::{Anchor, AnimationRouter, DismissPolicy, Overlay, OverlayOptions, Placement},
            test_support::{a11y_of, full_a11y},
        },
        style_builder::{PaintBuilder, StyleBuilder},
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    /// `TestApp` 默认视口。
    const VIEWPORT: Size = Size::new(800.0, 600.0);

    struct Harness {
        app: TestApp,
        window: WindowId,
        root: NodeId,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
    }

    impl Harness {
        fn new() -> Self {
            let app = TestApp::new();
            let window = app.window_id();
            let root = app.app().window(window).expect("窗口").tree().root();
            let mut theme_app = app;
            let theme = ThemeHandle::new(theme_app.app_mut(), Appearance::Light);
            Self {
                app: theme_app,
                window,
                root,
                theme,
                layer: Layer::new(),
                router: AnimationRouter::new(),
            }
        }

        fn build(&mut self, builder: DialogBuilder, key: &str) -> Dialog {
            builder.build(
                self.app.app_mut(),
                self.theme.clone(),
                self.layer.clone(),
                self.router.clone(),
                key,
            )
        }

        /// 建 + 打开 + 收敛（面板实测后居中、揭开）。
        fn dialog(&mut self, builder: DialogBuilder, key: &str) -> Dialog {
            let dialog = self.build(builder, key);
            assert!(
                dialog
                    .open(self.app.app_mut(), self.window)
                    .expect("打开对话框"),
                "首次打开必须真的打开"
            );
            self.settle(&dialog);
            dialog
        }

        /// 收敛：几轮 `sync` + 帧（居中纠位 / 首帧揭开 / 淡入都需要至少两帧）。
        fn settle(&mut self, dialog: &Dialog) {
            for _ in 0..4 {
                let _ = dialog.sync(self.app.app_mut()).expect("同步");
                let _ = self.app.drain().expect("帧");
            }
            let _ = dialog.sync(self.app.app_mut()).expect("同步");
        }

        fn bounds(&self, node: NodeId) -> Rect {
            self.app.probe().bounds(node).expect("布局盒")
        }

        fn center(&self, node: NodeId) -> Point {
            let bounds = self.bounds(node);
            Point::new(
                bounds.origin.x + bounds.size.width * 0.5,
                bounds.origin.y + bounds.size.height * 0.5,
            )
        }

        fn click_node(&mut self, node: NodeId) {
            let point = self.center(node);
            self.click_at(point);
        }

        fn click_at(&mut self, point: Point) {
            let _ = self.app.inject(InputEvent::pointer_down(
                point,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let _ = self.app.inject(InputEvent::pointer_up(
                point,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let _ = self.app.drain().expect("帧");
        }

        fn escape(&mut self) {
            let _ = self
                .app
                .inject(InputEvent::key_down(KeyCode::Escape, Modifiers::empty()));
            let _ = self.app.drain().expect("帧");
        }

        fn tree_has(&self, node: NodeId) -> bool {
            self.app
                .app()
                .window(self.window)
                .expect("窗口")
                .tree()
                .get(node)
                .is_ok()
        }

        fn background_with_actions(&mut self) -> (NodeId, Signal<u32>) {
            let clicks = self.app.app_mut().signal(0u32);
            let action = Action::Custom(format_smolstr!("background.click"));
            let node = {
                let tree = self
                    .app
                    .app_mut()
                    .window_mut(self.window)
                    .expect("窗口")
                    .tree_mut();
                NodeBuilder::boxed()
                    .id("background")
                    .layout_override(Rect::new(0.0, 0.0, VIEWPORT.width, VIEWPORT.height))
                    .paint(PaintBuilder::new().background(Color::WHITE).build())
                    .on_click(action.clone())
                    .build(tree, self.root)
                    .expect("背景")
            };
            let clicks_for_handler = clicks;
            self.app.app_mut().on_action(action, move |ctx| {
                ctx.update(&clicks_for_handler, |count| *count += 1);
            });
            (node, clicks)
        }

        fn focused_debug_id(&self) -> Option<String> {
            self.app.app().focused(self.window).and_then(|node| {
                self.app
                    .app()
                    .window(self.window)
                    .expect("窗口")
                    .tree()
                    .debug_id(node)
                    .map(str::to_owned)
            })
        }
    }

    /// 固定尺寸的内容盒子（正文内容；用于尺寸 / 淡入淡出 / 命中断言）。
    fn content_box(
        width: f32,
        height: f32,
    ) -> impl Fn(&mut App, WindowId, NodeId) -> Result<(), ui_core::error::EngineError> {
        let tracked = Rc::new(std::cell::RefCell::new(None));
        content_box_tracked(width, height, tracked)
    }

    /// 同上，并把建出来的节点句柄写到 `tracked`（淡入淡出测试要按**内容盒**自身的几何读
    /// 图元 alpha；内容列容器比内容盒宽）。
    fn content_box_tracked(
        width: f32,
        height: f32,
        tracked: Rc<std::cell::RefCell<Option<NodeId>>>,
    ) -> impl Fn(&mut App, WindowId, NodeId) -> Result<(), ui_core::error::EngineError> {
        move |app: &mut App, window: WindowId, parent: NodeId| {
            let tree = app
                .window_mut(window)
                .ok_or(ui_core::error::EngineError::UnknownWindow { window })?
                .tree_mut();
            let node = NodeBuilder::boxed()
                .id("body-box")
                .style(StyleBuilder::new().width(width).height(height).build())
                .paint(PaintBuilder::new().background(Color::WHITE).build())
                .build(tree, parent)?;
            *tracked.borrow_mut() = Some(node);
            Ok(())
        }
    }

    fn color_alpha(color: Color) -> f32 {
        match color {
            Color::Srgb { alpha, .. }
            | Color::LinearSrgb { alpha, .. }
            | Color::DisplayP3 { alpha, .. }
            | Color::Oklab { alpha, .. }
            | Color::Oklch { alpha, .. } => alpha,
        }
    }

    /// 本帧场景里 `bounds` 处实色 quad 的 alpha（填充色不是 Solid 时 `None`）。
    fn quad_alpha(app: &App, window: WindowId, bounds: Rect) -> Option<f32> {
        app.window(window)?
            .scene()
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Quad(quad) if quad.bounds == bounds => match &quad.paint {
                    QuadPaint::Solid { fill, .. } => Some(color_alpha(*fill)),
                    QuadPaint::Gradient(_) => None,
                },
                _ => None,
            })
    }

    /// 本帧场景里第一段字形的 alpha（标题文字）。
    fn first_glyph_alpha(app: &App, window: WindowId) -> Option<f32> {
        app.window(window)?
            .scene()
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::GlyphRun(run) => Some(color_alpha(run.color)),
                _ => None,
            })
    }

    /// 本帧里覆盖整个视口、alpha 为 1 的实色 quad（背景节点；遮罩 alpha < 1，可区分）。
    fn opaque_background_alpha(app: &App, window: WindowId) -> Option<f32> {
        app.window(window)?
            .scene()
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Quad(quad) if quad.bounds.size == VIEWPORT => match &quad.paint {
                    QuadPaint::Solid { fill, .. } if color_alpha(*fill) >= 0.999 => {
                        Some(color_alpha(*fill))
                    }
                    _ => None,
                },
                _ => None,
            })
    }

    fn record_close(flag: Rc<Cell<Option<DialogClose>>>) -> impl Fn(DialogClose, &mut App) {
        move |reason, _app| flag.set(Some(reason))
    }

    #[test]
    fn the_panel_is_centered_in_the_viewport() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Centered")
                .fade(false)
                .content(content_box(120.0, 40.0)),
            "centered",
        );
        let panel = dialog.panel_node().expect("面板");
        let bounds = harness.bounds(panel);
        assert!(
            (bounds.origin.x - (VIEWPORT.width - bounds.size.width) * 0.5).abs() < 0.5,
            "面板水平居中：{bounds:?}"
        );
        assert!(
            (bounds.origin.y - (VIEWPORT.height - bounds.size.height) * 0.5).abs() < 0.5,
            "面板垂直居中：{bounds:?}"
        );
        assert_eq!(
            bounds.size.width,
            384.0 + 16.0,
            "面板宽度 = 内容宽度 384 + 两侧各 8 px 内边距"
        );
    }

    #[test]
    fn opening_builds_the_title_body_and_button_row() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Save changes")
                .fade(false)
                .content(content_box(100.0, 30.0))
                .confirm("Save", |_| {})
                .cancel("Cancel", |_| {}),
            "layout",
        );
        assert_eq!(dialog.title(), "Save changes");
        let panel = harness.bounds(dialog.panel_node().expect("面板"));
        let body = harness.bounds(dialog.body_node().expect("内容列"));
        let ok = harness.bounds(
            dialog
                .confirm_button()
                .expect("确认按钮")
                .node()
                .expect("ok 节点"),
        );
        let cancel = harness.bounds(
            dialog
                .cancel_button()
                .expect("取消按钮")
                .node()
                .expect("cancel 节点"),
        );
        let close = harness.bounds(
            dialog
                .close_button()
                .expect("关闭按钮")
                .node()
                .expect("close 节点"),
        );
        for (name, bounds) in [("内容列", body), ("确认", ok), ("取消", cancel)] {
            assert!(
                bounds.origin.x >= panel.origin.x && bounds.max().x <= panel.max().x + 0.5,
                "{name} 必须在面板内：panel={panel:?} {name}={bounds:?}"
            );
        }
        assert!(
            ok.size.width > 0.0 && cancel.size.width > 0.0 && ok.origin.y == cancel.origin.y,
            "按钮行上的两个按钮等分同一行"
        );
        assert!(
            (ok.size.width - cancel.size.width).abs() < 0.5,
            "确认 / 取消必须等分按钮行：{ok:?} / {cancel:?}"
        );
        assert!(
            close.origin.x > panel.origin.x + panel.size.width * 0.5,
            "关闭按钮在内容列右上角：{close:?}"
        );
        assert_eq!(body.size.height, 30.0, "内容列高度 = 自定义内容高度");
    }

    #[test]
    fn confirm_runs_its_action_and_reports_confirmed() {
        let mut harness = Harness::new();
        let clicks = Rc::new(Cell::new(0u32));
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Confirm")
                .fade(false)
                .confirm("OK", {
                    let clicks = clicks.clone();
                    move |_| clicks.set(clicks.get() + 1)
                })
                .on_close(record_close(reason.clone())),
            "confirm",
        );
        let ok = dialog
            .confirm_button()
            .expect("确认按钮")
            .node()
            .expect("节点");
        harness.click_node(ok);
        assert_eq!(clicks.get(), 1, "确认按钮必须跑它的回调");
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(!dialog.is_open(harness.app.app()), "fade=false 立即关闭");
        assert_eq!(reason.get(), Some(DialogClose::Confirmed), "关闭原因");
        assert!(dialog.overlay_node().is_none(), "关闭后浮层节点卸载");
    }

    #[test]
    fn cancel_runs_its_action_and_reports_cancelled() {
        let mut harness = Harness::new();
        let clicks = Rc::new(Cell::new(0u32));
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Cancel")
                .fade(false)
                .confirm("OK", |_| panic!("取消不得跑确认回调"))
                .cancel("Cancel", {
                    let clicks = clicks.clone();
                    move |_| clicks.set(clicks.get() + 1)
                })
                .on_close(record_close(reason.clone())),
            "cancel",
        );
        let cancel = dialog
            .cancel_button()
            .expect("取消按钮")
            .node()
            .expect("节点");
        harness.click_node(cancel);
        assert_eq!(clicks.get(), 1);
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(reason.get(), Some(DialogClose::Cancelled));
    }

    #[test]
    fn the_close_button_dismisses_without_running_confirm_or_cancel() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Close")
                .fade(false)
                .confirm("OK", |_| panic!("关闭按钮不得跑确认回调"))
                .cancel("Cancel", |_| panic!("关闭按钮不得跑取消回调"))
                .on_close(record_close(reason.clone())),
            "close-button",
        );
        let close = dialog
            .close_button()
            .expect("关闭按钮")
            .node()
            .expect("节点");
        harness.click_node(close);
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
    }

    #[test]
    fn escape_closes_the_dialog_and_reports_dismissed() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Escape").on_close(record_close(reason.clone())),
            "escape",
        );
        let node = dialog.overlay_node().expect("浮层节点");
        harness.escape();
        assert!(
            !harness.tree_has(node),
            "引擎默认绑定 Esc → Dismiss：节点立即卸载"
        );
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(
            reason.get(),
            Some(DialogClose::Dismissed),
            "Esc 是 Dismissed"
        );
        assert_eq!(
            harness.app.app().modal_scope(harness.window),
            None,
            "关闭后不再有模态作用域"
        );
    }

    #[test]
    fn a_press_outside_neither_closes_nor_reaches_the_background() {
        let mut harness = Harness::new();
        let (_background, clicks) = harness.background_with_actions();
        let reason = Rc::new(Cell::new(None));
        // 默认 `Explicit` = 旧 dialog / alert 语义：点外部不关。
        let dialog = harness.dialog(
            DialogBuilder::new("Explicit")
                .fade(false)
                .on_close(record_close(reason.clone())),
            "explicit",
        );
        harness.click_at(Point::new(10.0, 10.0));
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        assert!(
            dialog.is_open(harness.app.app()),
            "Explicit 不得被外部点击关闭"
        );
        assert_eq!(
            *harness.app.app().read(&clicks),
            0,
            "模态必须消费背景上的按下"
        );
        assert_eq!(reason.get(), None, "没关闭就没有关闭回调");
    }

    #[test]
    fn an_outside_click_policy_closes_the_dialog() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Outside")
                .fade(false)
                .dismiss(DismissPolicy::OutsideClick)
                .on_close(record_close(reason.clone())),
            "outside",
        );
        harness.click_at(Point::new(10.0, 10.0));
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        assert!(
            !dialog.is_open(harness.app.app()),
            "OutsideClick 点遮罩即关闭"
        );
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
    }

    #[test]
    fn the_dialog_traps_focus_and_restores_it_on_close() {
        let mut harness = Harness::new();
        let trigger_action = Action::Custom(format_smolstr!("trigger.click"));
        let trigger = {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("trigger")
                .layout_override(Rect::new(10.0, 10.0, 100.0, 40.0))
                .on_click(trigger_action)
                .build(tree, harness.root)
                .expect("触发器")
        };
        harness
            .app
            .app_mut()
            .set_focus(harness.window, Some(trigger));
        let dialog = harness.dialog(
            DialogBuilder::new("Focus")
                .fade(false)
                .confirm("OK", |_| {})
                .content(content_box(80.0, 20.0)),
            "focus",
        );
        // Tab 只在模态作用域内循环：焦点从模态外移进对话框。
        let _ = harness.app.app_mut().focus_next(harness.window);
        let focused = harness.focused_debug_id().expect("焦点必须在对话框内");
        assert!(
            focused.contains(":ok") || focused.contains(":cancel") || focused.contains(":close"),
            "焦点必须落在对话框的可聚焦节点上，实际 {focused}"
        );
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        dialog.close_now(harness.app.app_mut());
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(trigger),
            "关闭后焦点必须还给 opener"
        );
    }

    #[test]
    fn the_dialog_is_the_modal_scope_and_the_background_stays_inert() {
        let mut harness = Harness::new();
        let (_background, clicks) = harness.background_with_actions();
        let dialog = harness.dialog(DialogBuilder::new("Modal").fade(false), "modal");
        assert_eq!(
            harness.app.app().modal_scope(harness.window),
            dialog.overlay_node(),
            "最上层模态作用域 = 对话框浮层包装节点"
        );
        harness.click_at(Point::new(20.0, 20.0));
        assert_eq!(*harness.app.app().read(&clicks), 0, "模态期间背景不可交互");
    }

    #[test]
    fn the_dialog_group_fades_together_with_its_content() {
        let mut harness = Harness::new();
        let (_background, _clicks) = harness.background_with_actions();
        let tracked = Rc::new(std::cell::RefCell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Fade")
                .content(content_box_tracked(120.0, 40.0, tracked.clone()))
                .close_button(false),
            "fade",
        );
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        let panel = harness.bounds(dialog.panel_node().expect("面板"));
        let content_node = tracked.borrow().expect("内容盒节点");
        let body = harness.bounds(content_node);
        let full_panel = quad_alpha(harness.app.app(), harness.window, panel).expect("面板背景");
        let full_body = quad_alpha(harness.app.app(), harness.window, body).expect("内容盒");
        let full_title = first_glyph_alpha(harness.app.app(), harness.window).expect("标题字形");
        let full_background = opaque_background_alpha(harness.app.app(), harness.window)
            .expect("背景节点的不透明 quad");
        assert!((full_panel - 1.0).abs() < 0.01, "淡入完成后面板不透明");
        assert!((full_body - 1.0).abs() < 0.01, "淡入完成后内容不透明");

        assert!(dialog.close(harness.app.app_mut()), "开始退场");
        harness.app.advance(Duration::from_millis(75));
        let _ = harness.app.drain().expect("退场帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        let panel_progress =
            quad_alpha(harness.app.app(), harness.window, panel).expect("面板背景");
        let body_progress = quad_alpha(harness.app.app(), harness.window, body).expect("内容盒");
        let title_progress =
            first_glyph_alpha(harness.app.app(), harness.window).expect("标题字形");
        let background_progress =
            opaque_background_alpha(harness.app.app(), harness.window).expect("背景 quad");
        assert!(
            panel_progress < full_panel * 0.99,
            "退场中面板必须变淡：{panel_progress} / {full_panel}"
        );
        assert!(
            (body_progress / full_body - panel_progress / full_panel).abs() < 0.02,
            "E13 组不透明度：内容子节点必须跟面板同比例变淡（body {body_progress} / panel {panel_progress}）"
        );
        assert!(
            (title_progress / full_title - panel_progress / full_panel).abs() < 0.02,
            "文字也必须跟组淡出（title {title_progress} / panel {panel_progress}）"
        );
        assert!(
            (background_progress - full_background).abs() < 0.001,
            "对话框外的背景不参与淡出：{background_progress} / {full_background}"
        );
    }

    #[test]
    fn a_finished_fade_unloads_the_panel_in_the_same_frame() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("E10").on_close(record_close(reason.clone())),
            "e10",
        );
        let node = dialog.overlay_node().expect("浮层节点");
        // 先让淡入完成（引擎时钟必须真的走过一个 animation tick，否则关闭会走「进度已为 0」的
        // 立即卸载路径）。
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        assert!(dialog.close(harness.app.app_mut()), "开始退场");
        let mut unloaded = false;
        for _ in 0..24 {
            harness.app.advance(Duration::from_millis(16));
            let _ = dialog.sync(harness.app.app_mut()).expect("同步");
            let _ = harness.app.drain().expect("帧");
            if !harness.tree_has(node) {
                unloaded = true;
                break;
            }
        }
        assert!(
            unloaded,
            "淡出到终值时 E10 完成回调必须在同一帧卸载浮层节点"
        );
        assert!(
            dialog.is_open(harness.app.app()),
            "卸载发生在帧内；kit 状态在下一次 sync 才收尾"
        );
        assert_eq!(reason.get(), None, "收尾前不派发关闭回调");
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
    }

    #[test]
    fn the_wrapper_declares_the_dialog_role_label_and_modal() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dialog = harness.dialog(DialogBuilder::new("Delete file").fade(false), "a11y");
        let wrapper = dialog.overlay_node().expect("包装节点");
        let panel = dialog.panel_node().expect("面板");
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot.node_for(wrapper).expect("包装节点 a11y");
        assert_eq!(record.role, A11yRole::Dialog, "显式 Dialog 角色");
        assert_eq!(record.label_text(), Some("Delete file"), "可访问名 = 标题");
        assert!(record.modal, "模态对话框必须声明 modal");
        assert_eq!(
            snapshot.node_for(panel).expect("面板 a11y").role,
            A11yRole::GenericContainer,
            "面板不得重复声明 Dialog"
        );
        let scrim = snapshot
            .nodes()
            .iter()
            .find(|record| record.role == A11yRole::Group)
            .expect("遮罩在 a11y 树里");
        assert!(scrim.hidden, "遮罩必须 hidden（纯装饰）");
    }

    #[test]
    fn an_alert_dialog_declares_the_alert_dialog_role() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dialog = harness.dialog(
            DialogBuilder::alert("Delete file", |_dialog, _theme| {
                AlertBuilder::new().ok_cancel("Delete file", "This cannot be undone.")
            })
            .fade(false),
            "alert-role",
        );
        let wrapper = dialog.overlay_node().expect("包装节点");
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot.node_for(wrapper).expect("包装节点 a11y");
        assert_eq!(record.role, A11yRole::AlertDialog);
        assert_eq!(record.label_text(), Some("Delete file"));
        assert!(record.modal);
        assert!(
            dialog
                .with_alert(|alert| alert.title().to_owned())
                .is_some()
        );
    }

    #[test]
    fn an_alert_content_panel_is_mounted_and_its_ok_action_can_close_the_dialog() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let ok_runs = Rc::new(Cell::new(0u32));
        let ok_runs_for_builder = ok_runs.clone();
        let dialog = harness.dialog(
            DialogBuilder::alert("Delete file", move |dialog, _theme| {
                let dialog = dialog.clone();
                let ok_runs = ok_runs_for_builder.clone();
                AlertBuilder::new()
                    .ok_cancel("Delete file", "This cannot be undone.")
                    .ok_action(move |_, app| {
                        ok_runs.set(ok_runs.get() + 1);
                        dialog.close(app);
                    })
            })
            .fade(false)
            .on_close(record_close(reason.clone())),
            "alert",
        );
        assert!(
            dialog
                .with_alert(|alert| alert.ok_button().node())
                .flatten()
                .is_some(),
            "alert 面板必须挂进对话框内容列"
        );
        let ok = dialog
            .with_alert(|alert| alert.ok_button().node())
            .flatten()
            .expect("alert OK 节点");
        harness.click_node(ok);
        assert_eq!(ok_runs.get(), 1, "alert 的 OK 动作必须运行");
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
        assert!(
            dialog.with_alert(|_| ()).is_some(),
            "关闭后 alert 句柄保留到 release（应用层信号生命周期）"
        );
    }

    #[test]
    fn an_alert_that_rejects_keeps_the_dialog_open_and_runs_nothing() {
        // alert 负责内容与校验：校验失败时它**不调用** OK 动作，因此容器也不会关闭。
        let mut harness = Harness::new();
        let ok_runs = Rc::new(Cell::new(0u32));
        let ok_runs_for_builder = ok_runs.clone();
        let dialog = harness.dialog(
            DialogBuilder::alert("Rename", move |dialog, _theme| {
                let dialog = dialog.clone();
                let ok_runs = ok_runs_for_builder.clone();
                AlertBuilder::new()
                    .ok("Rename", "Enter a name.")
                    .validate(|value| {
                        if value.is_empty() {
                            Err(smol_str::SmolStr::new_static("a name is required"))
                        } else {
                            Ok(())
                        }
                    })
                    .ok_action(move |_, app| {
                        ok_runs.set(ok_runs.get() + 1);
                        dialog.close(app);
                    })
            })
            .fade(false),
            "alert-validation",
        );
        let ok = dialog
            .with_alert(|alert| alert.ok_button().node())
            .flatten()
            .expect("alert OK 节点");
        harness.click_node(ok);
        assert_eq!(ok_runs.get(), 0, "校验失败不得调用 OK 动作");
        assert!(
            dialog.is_open(harness.app.app()),
            "alert 没有请求关闭 ⇒ 对话框保持打开"
        );
        assert!(
            dialog
                .with_alert(|alert| alert.has_error(harness.app.app()))
                .unwrap_or(false),
            "拒绝原因留在 alert 自己的错误状态里"
        );
        dialog.close_now(harness.app.app_mut());
        assert!(!dialog.is_open(harness.app.app()));
    }

    #[test]
    fn a_settled_dialog_requests_no_frames() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Idle")
                .fade(false)
                .content(content_box(40.0, 20.0)),
            "idle",
        );
        assert!(
            !dialog.sync(harness.app.app_mut()).expect("同步"),
            "稳态 sync 返回 false（零写入）"
        );
        let outcome = harness
            .app
            .app_mut()
            .frame(harness.window)
            .expect("窗口")
            .expect("帧");
        assert_eq!(outcome, FrameOutcome::Idle, "稳态不请求帧（I3）");
    }

    #[test]
    fn closing_twice_is_idempotent_and_notifies_once() {
        let mut harness = Harness::new();
        let closes = Rc::new(Cell::new(0u32));
        let dialog = harness.dialog(
            DialogBuilder::new("Twice").on_close({
                let closes = closes.clone();
                move |_reason, _app| closes.set(closes.get() + 1)
            }),
            "twice",
        );
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        assert!(dialog.close(harness.app.app_mut()));
        assert!(
            dialog.close(harness.app.app_mut()),
            "淡出中再次关闭幂等返回 true"
        );
        for _ in 0..20 {
            harness.app.advance(Duration::from_millis(16));
            let _ = dialog.sync(harness.app.app_mut()).expect("同步");
            let _ = harness.app.drain().expect("帧");
            if !dialog.is_open(harness.app.app()) {
                break;
            }
        }
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(closes.get(), 1, "关闭回调每完成一次关闭只派发一次");
    }

    #[test]
    fn reopening_rebuilds_the_panel_and_keeps_the_callbacks() {
        let mut harness = Harness::new();
        let clicks = Rc::new(Cell::new(0u32));
        let dialog = harness.dialog(
            DialogBuilder::new("Reopen").fade(false).confirm("OK", {
                let clicks = clicks.clone();
                move |_| clicks.set(clicks.get() + 1)
            }),
            "reopen",
        );
        let first = dialog.panel_node().expect("首个面板");
        dialog.close_now(harness.app.app_mut());
        assert!(!harness.tree_has(first));
        assert!(
            dialog
                .open(harness.app.app_mut(), harness.window)
                .expect("重开")
        );
        harness.settle(&dialog);
        let second = dialog.panel_node().expect("新面板");
        assert_ne!(first, second, "重开必须重建面板");
        assert!(harness.tree_has(second));
        let ok = dialog
            .confirm_button()
            .expect("确认按钮")
            .node()
            .expect("节点");
        harness.click_node(ok);
        assert_eq!(clicks.get(), 1, "重开后回调仍然可用");
    }

    #[test]
    fn a_second_dialog_stacks_above_the_first_and_only_the_top_one_closes() {
        let mut harness = Harness::new();
        let first_clicks = Rc::new(Cell::new(0u32));
        let first_reason = Rc::new(Cell::new(None));
        let first = harness.build(
            DialogBuilder::new("First")
                .fade(false)
                .confirm("OK", {
                    let clicks = first_clicks.clone();
                    move |_| clicks.set(clicks.get() + 1)
                })
                .on_close(record_close(first_reason.clone())),
            "first",
        );
        assert!(
            first
                .open(harness.app.app_mut(), harness.window)
                .expect("打开第一层")
        );
        harness.settle(&first);
        let second_clicks = Rc::new(Cell::new(0u32));
        let second_reason = Rc::new(Cell::new(None));
        let second = harness.build(
            DialogBuilder::new("Second")
                .fade(false)
                .confirm("OK", {
                    let clicks = second_clicks.clone();
                    move |_| clicks.set(clicks.get() + 1)
                })
                .on_close(record_close(second_reason.clone())),
            "second",
        );
        assert!(
            second
                .open(harness.app.app_mut(), harness.window)
                .expect("打开第二层")
        );
        harness.settle(&second);
        assert_eq!(harness.layer.len(harness.window), 2, "同带可叠多层");
        assert_eq!(
            harness.app.app().modal_scope(harness.window),
            second.overlay_node(),
            "最上层模态 = 第二层"
        );
        let ok = second.confirm_button().expect("确认").node().expect("节点");
        harness.click_node(ok);
        let _ = second.sync(harness.app.app_mut()).expect("收尾");
        assert_eq!(second_clicks.get(), 1, "只跑顶层内容自己的回调");
        assert_eq!(first_clicks.get(), 0, "下层内容不得被穿越点击");
        assert!(!second.is_open(harness.app.app()));
        assert_eq!(second_reason.get(), Some(DialogClose::Confirmed));
        assert!(first.is_open(harness.app.app()), "下层保持打开");
        assert_eq!(first_reason.get(), None);
        // 下层随后自己关闭：each content dismisses only its own dialog。
        let first_ok = first.confirm_button().expect("确认").node().expect("节点");
        harness.click_node(first_ok);
        let _ = first.sync(harness.app.app_mut()).expect("收尾");
        assert_eq!(first_clicks.get(), 1);
        assert!(!first.is_open(harness.app.app()));
        assert_eq!(first_reason.get(), Some(DialogClose::Confirmed));
    }

    #[test]
    fn opening_a_dialog_dismisses_an_open_context_menu() {
        let mut harness = Harness::new();
        let menu = Overlay::new(
            harness.app.app_mut(),
            harness.theme.clone(),
            harness.layer.clone(),
            harness.router.clone(),
            "menu",
            OverlayOptions {
                placement: Placement::TOP_START,
                band: Band::ContextMenu,
                fade: false,
                ..OverlayOptions::default()
            },
        );
        assert!(
            menu.open(
                harness.app.app_mut(),
                harness.window,
                Anchor::point(Point::new(40.0, 40.0)),
                |_, _| Ok(())
            )
            .expect("打开菜单")
        );
        let menu_node = menu.overlay_node().expect("菜单节点");
        let _ = harness.app.drain().expect("帧");
        assert_eq!(harness.layer.len(harness.window), 1);

        let dialog = harness.dialog(DialogBuilder::new("Dialog").fade(false), "take-menu");
        assert!(
            !harness.tree_has(menu_node),
            "打开对话框必须先收起 context menu（旧 layer 语义）"
        );
        assert_eq!(harness.layer.len(harness.window), 1);
        assert!(dialog.is_open(harness.app.app()));
    }

    #[test]
    fn an_alert_dialog_replaces_the_previous_alert_slot() {
        let mut harness = Harness::new();
        let first_reason = Rc::new(Cell::new(None));
        let first = harness.build(
            DialogBuilder::alert("First alert", |_dialog, _theme| {
                AlertBuilder::new().ok("First alert", "One at a time.")
            })
            .fade(false)
            .on_close(record_close(first_reason.clone())),
            "alert-a",
        );
        assert!(
            first
                .open(harness.app.app_mut(), harness.window)
                .expect("打开第一个 alert")
        );
        harness.settle(&first);
        let first_node = first.overlay_node().expect("第一个 alert 节点");

        let second = harness.build(
            DialogBuilder::alert("Second alert", |_dialog, _theme| {
                AlertBuilder::new().ok("Second alert", "Replaces the first.")
            })
            .fade(false),
            "alert-b",
        );
        assert!(
            second
                .open(harness.app.app_mut(), harness.window)
                .expect("打开第二个 alert")
        );
        harness.settle(&second);
        let _ = first
            .sync(harness.app.app_mut())
            .expect("第一个 alert 收尾");
        assert!(
            !harness.tree_has(first_node),
            "alert 槽至多一个：后开的替换先开的"
        );
        assert_eq!(harness.layer.len(harness.window), 1);
        assert!(!first.is_open(harness.app.app()));
        assert_eq!(first_reason.get(), Some(DialogClose::Dismissed));
    }

    #[test]
    fn sync_on_a_closed_dialog_is_a_no_op() {
        let mut harness = Harness::new();
        let dialog = harness.build(DialogBuilder::new("Closed").fade(false), "closed");
        assert!(
            !dialog.sync(harness.app.app_mut()).expect("同步"),
            "未打开的对话框不做事"
        );
        assert!(dialog.overlay_node().is_none());
        dialog.release(harness.app.app_mut());
    }

    // ── P2-4d1 审查修复的回归测试（D1/D2/D3/D5/D8/D9/D10/D11）──────────────────────

    /// D1：`header(false)`（无标题行）时关闭按钮必须真的可点 —— 它最后建，命中序在内容之上。
    #[test]
    fn the_close_button_is_hittable_when_the_header_is_hidden() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Headerless")
                .header(false)
                .fade(false)
                .content(content_box(320.0, 40.0))
                .confirm("OK", |_| panic!("关闭按钮不得跑确认回调"))
                .cancel("Cancel", |_| panic!("关闭按钮不得跑取消回调"))
                .on_close(record_close(reason.clone())),
            "headerless-close",
        );
        let close = dialog
            .close_button()
            .expect("关闭按钮")
            .node()
            .expect("关闭按钮节点");
        harness.click_node(close);
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(
            !dialog.is_open(harness.app.app()),
            "无标题行时内容列从面板顶端开始：关闭按钮必须最后建才不被盖住（D1）"
        );
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
    }

    /// D5：icon-only 关闭按钮必须有独立 a11y 名（默认 `Close`，可本地化覆盖）。
    #[test]
    fn the_close_button_declares_an_a11y_name() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dialog = harness.dialog(DialogBuilder::new("Named").fade(false), "close-a11y");
        let close = dialog
            .close_button()
            .expect("关闭按钮")
            .node()
            .expect("关闭按钮节点");
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot.node_for(close).expect("关闭按钮必须在 a11y 树里");
        assert_eq!(record.role, A11yRole::Button);
        assert_eq!(record.label_text(), Some("Close"), "默认可访问名");

        let localized = harness.build(
            DialogBuilder::new("Named custom")
                .fade(false)
                .close_label("关闭"),
            "close-a11y-custom",
        );
        assert!(
            localized
                .open(harness.app.app_mut(), harness.window)
                .expect("打开")
        );
        harness.settle(&localized);
        let localized_close = localized
            .close_button()
            .expect("关闭按钮")
            .node()
            .expect("关闭按钮节点");
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot
            .node_for(localized_close)
            .expect("本地化关闭按钮在 a11y 树里");
        assert_eq!(record.label_text(), Some("关闭"), "调用方可以给本地化名字");
    }

    /// D11（审查 D3）：alert 形状没有关闭按钮 —— 旧 alert 只有 OK / Cancel，X 会绕过
    /// `cancel_action`；显式 `close_button(true)` 也不生效（alert 形状恒无 X）。
    #[test]
    fn an_alert_dialog_never_builds_a_close_button() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::alert("No close", |_dialog, _theme| {
                AlertBuilder::new().ok_cancel("No close", "Old alerts have no close button.")
            })
            .close_button(true)
            .fade(false),
            "alert-no-close",
        );
        assert!(
            dialog.close_button().is_none(),
            "alert 形状恒不建关闭按钮（旧 alert 语义；X 无法路由到 cancel_action）"
        );
    }

    /// D11（审查 D3）：Esc 由引擎立即卸载 alert（`Dismissed`），不跑 alert 的 `cancel_action`。
    #[test]
    fn escape_closes_an_alert_without_running_its_actions() {
        let mut harness = Harness::new();
        let ok_runs = Rc::new(Cell::new(0u32));
        let cancel_runs = Rc::new(Cell::new(0u32));
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::alert("Esc alert", {
                let ok_runs = ok_runs.clone();
                let cancel_runs = cancel_runs.clone();
                move |_dialog, _theme| {
                    let ok_counter = ok_runs.clone();
                    let cancel_counter = cancel_runs.clone();
                    AlertBuilder::new()
                        .ok_cancel("Esc alert", "Esc closes it.")
                        .ok_action(move |_, _| ok_counter.set(ok_counter.get() + 1))
                        .cancel_action(move |_| cancel_counter.set(cancel_counter.get() + 1))
                }
            })
            .on_close(record_close(reason.clone())),
            "alert-esc",
        );
        harness.escape();
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(
            reason.get(),
            Some(DialogClose::Dismissed),
            "Esc = 引擎默认 Dismiss"
        );
        assert_eq!(ok_runs.get(), 0);
        assert_eq!(
            cancel_runs.get(),
            0,
            "Esc 是引擎立即卸载：不跑 alert 的 cancel_action（无应用层关闭钩子）"
        );
    }

    /// D2：退场淡出窗口内第二次点击不得再跑用户回调（旧 dialog 用 capture 阶段吞掉按下）。
    #[test]
    fn a_second_click_during_the_fade_runs_no_action() {
        let mut harness = Harness::new();
        let confirms = Rc::new(Cell::new(0u32));
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Double click")
                .confirm("OK", {
                    let confirms = confirms.clone();
                    move |_| confirms.set(confirms.get() + 1)
                })
                .cancel("Cancel", |_| panic!("取消回调不得被双击触发"))
                .on_close(record_close(reason.clone())),
            "double-click",
        );
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        let ok = dialog
            .confirm_button()
            .expect("确认按钮")
            .node()
            .expect("节点");
        harness.click_node(ok);
        assert_eq!(confirms.get(), 1, "第一次点击跑确认回调");
        assert!(dialog.is_closing(), "第一次点击开始淡出");
        harness.click_node(ok);
        assert_eq!(
            confirms.get(),
            1,
            "淡出中的第二次点击不得再跑用户回调（D2）"
        );
        for _ in 0..24 {
            harness.app.advance(Duration::from_millis(16));
            let _ = dialog.sync(harness.app.app_mut()).expect("同步");
            let _ = harness.app.drain().expect("帧");
            if !dialog.is_open(harness.app.app()) {
                break;
            }
        }
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(
            reason.get(),
            Some(DialogClose::Confirmed),
            "关闭原因保持第一次的 Confirmed"
        );
    }

    /// D3（审查 D4）：关闭淡出中 `open` 必须收尾上一次关闭（`on_close` 一次）并重建，不得静默丢弃。
    #[test]
    fn reopening_while_closing_finishes_the_close_then_reopens() {
        let mut harness = Harness::new();
        let closes = Rc::new(Cell::new(0u32));
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Reopen while closing").on_close({
                let closes = closes.clone();
                let reason = reason.clone();
                move |closed, _app| {
                    closes.set(closes.get() + 1);
                    reason.set(Some(closed));
                }
            }),
            "reopen-closing",
        );
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        assert!(dialog.close(harness.app.app_mut()));
        assert!(dialog.is_closing(), "进入淡出");
        assert!(
            dialog
                .open(harness.app.app_mut(), harness.window)
                .expect("关闭中重开"),
            "关闭中 open 必须真的重开（不得返回 Ok(false) 静默丢弃）"
        );
        assert!(dialog.is_open(harness.app.app()));
        assert!(!dialog.is_closing());
        assert_eq!(closes.get(), 1, "上一次关闭必须先收尾（on_close 派发一次）");
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
        harness.settle(&dialog);
        let panel = dialog.panel_node().expect("重开后的面板");
        assert!(harness.tree_has(panel), "重开必须有新面板");
        assert_eq!(harness.layer.len(harness.window), 1, "旧浮层不得残留");
    }

    /// D3（审查 D4）：`fade=false` 下浮层已卸载但状态还没 sync 时重开，同样先收尾再重建。
    #[test]
    fn reopening_before_the_closed_state_is_synced_reopens() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Unsynced")
                .fade(false)
                .on_close(record_close(reason.clone())),
            "reopen-unsynced",
        );
        let close = dialog
            .close_button()
            .expect("关闭按钮")
            .node()
            .expect("关闭按钮节点");
        harness.click_node(close);
        assert!(
            dialog.is_open(harness.app.app()),
            "fade=false 已卸载：状态在帧边界更新"
        );
        assert!(dialog.is_closing());
        assert!(
            dialog
                .open(harness.app.app_mut(), harness.window)
                .expect("未收尾时重开"),
            "未收尾的关闭必须先收尾再重开"
        );
        assert!(dialog.is_open(harness.app.app()));
        assert!(!dialog.is_closing());
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
        harness.settle(&dialog);
        assert!(harness.tree_has(dialog.panel_node().expect("新面板")));
    }

    /// D8：`close_now` 走混合路径时保留已记录的原因，不得改写成 `Dismissed`。
    #[test]
    fn close_now_keeps_the_reason_the_close_started_with() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Reason")
                .confirm("OK", |_| {})
                .on_close(record_close(reason.clone())),
            "reason",
        );
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        let ok = dialog
            .confirm_button()
            .expect("确认按钮")
            .node()
            .expect("节点");
        harness.click_node(ok);
        assert!(dialog.is_closing());
        assert!(dialog.close_now(harness.app.app_mut()), "立即关闭");
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(
            reason.get(),
            Some(DialogClose::Confirmed),
            "close_now 不得把 Confirmed 改写成 Dismissed（D8）"
        );
    }

    /// D9：`release` 必须摘掉按钮的 Action handler（变异「release 不释放」→ 本测试红）。
    #[test]
    fn release_removes_the_dialog_button_action_handlers() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Release")
                .fade(false)
                .confirm("OK", |_| {})
                .cancel("Cancel", |_| {}),
            "release",
        );
        let actions: Vec<Action> = [
            dialog.confirm_button(),
            dialog.cancel_button(),
            dialog.close_button(),
        ]
        .into_iter()
        .flatten()
        .map(|button| button.action_identity().expect("按钮动作身份"))
        .collect();
        assert_eq!(actions.len(), 3, "确认 / 取消 / 关闭三个按钮");
        for action in &actions {
            assert!(
                harness.app.app().has_action_handler(action),
                "{action:?} 必须在册"
            );
        }
        dialog.release(harness.app.app_mut());
        for action in &actions {
            assert!(
                !harness.app.app().has_action_handler(action),
                "{action:?} 必须在 release 后摘除（D9）"
            );
        }
        dialog.release(harness.app.app_mut());
        assert!(dialog.close_button().is_none(), "release 后按钮句柄已释放");
    }

    /// D9：alert 内容面板的按钮 Action handler 同样由 `Dialog::release` 摘除。
    #[test]
    fn release_removes_the_alert_content_action_handlers() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::alert("Release alert", |_dialog, _theme| {
                AlertBuilder::new()
                    .ok_cancel("Release alert", "Content")
                    .ok_action(|_, _| {})
                    .cancel_action(|_| {})
            })
            .fade(false),
            "release-alert",
        );
        let ok_action = dialog
            .with_alert(|alert| alert.ok_button().action_identity())
            .flatten()
            .expect("alert OK 动作身份");
        let cancel_action = dialog
            .with_alert(|alert| {
                alert
                    .cancel_button()
                    .and_then(|button| button.action_identity())
            })
            .flatten()
            .expect("alert Cancel 动作身份");
        for action in [&ok_action, &cancel_action] {
            assert!(harness.app.app().has_action_handler(action));
        }
        dialog.release(harness.app.app_mut());
        for action in [&ok_action, &cancel_action] {
            assert!(
                !harness.app.app().has_action_handler(action),
                "{action:?} 必须在 release 后摘除（alert 内容侧）"
            );
        }
    }

    /// D10：I3 在**默认** `fade = true` 下也成立（稳态零帧）。
    #[test]
    fn a_settled_dialog_with_the_default_fade_requests_no_frames() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Idle fade").content(content_box(40.0, 20.0)),
            "idle-fade",
        );
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        assert!(
            !dialog.sync(harness.app.app_mut()).expect("同步"),
            "淡入完成后的稳态 sync 必须返回 false（I3，默认 fade=true）"
        );
        let outcome = harness
            .app
            .app_mut()
            .frame(harness.window)
            .expect("窗口")
            .expect("帧");
        assert_eq!(outcome, FrameOutcome::Idle, "稳态不请求帧");
    }

    // ── P2-4a3 alert 收尾（D6 角色唯一 / D2 同类双击守卫）────────────────────────────

    /// D6：alert 形状全树只有一个对话框角色（包装层 `AlertDialog`）；内容根是通用容器。
    #[test]
    fn an_alert_shape_declares_exactly_one_dialog_role() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dialog = harness.dialog(
            DialogBuilder::alert("Delete file", |_dialog, _theme| {
                AlertBuilder::new().ok_cancel("Delete file", "This cannot be undone.")
            })
            .fade(false),
            "alert-role-unique",
        );
        let wrapper = dialog.overlay_node().expect("包装节点");
        let content = dialog
            .with_alert(|alert| alert.root_node())
            .expect("alert 内容根");
        let snapshot = full_a11y(&mut harness.app);
        let dialog_roles = snapshot
            .nodes()
            .iter()
            .filter(|node| matches!(node.role, A11yRole::Dialog | A11yRole::AlertDialog))
            .count();
        assert_eq!(
            dialog_roles, 1,
            "alert 形状全树只能有一个对话框角色（P2-4d1 审查 D6）"
        );
        assert_eq!(a11y_of(&snapshot, wrapper).role, A11yRole::AlertDialog);
        let record = a11y_of(&snapshot, content);
        assert_eq!(
            record.role,
            A11yRole::GenericContainer,
            "alert 内容根不得重复声明对话框角色"
        );
        assert_eq!(
            record.label_text(),
            None,
            "可访问名归包装层，内容根不重复标题"
        );
    }

    /// D2（alert 内容同类）：退场淡出窗口内再点 alert 的 OK / Cancel 不得再跑用户动作。
    #[test]
    fn a_second_click_on_alert_content_during_the_fade_runs_no_action() {
        let mut harness = Harness::new();
        let ok_runs = Rc::new(Cell::new(0u32));
        let cancel_runs = Rc::new(Cell::new(0u32));
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::alert("Double click", {
                let ok_runs = ok_runs.clone();
                let cancel_runs = cancel_runs.clone();
                move |dialog, _theme| {
                    let dialog = dialog.clone();
                    let ok = ok_runs.clone();
                    let cancel = cancel_runs.clone();
                    AlertBuilder::new()
                        .ok_cancel("Double click", "Delete?")
                        .ok_action(move |_, app| {
                            ok.set(ok.get() + 1);
                            dialog.close(app);
                        })
                        .cancel_action(move |_| cancel.set(cancel.get() + 1))
                }
            })
            .on_close(record_close(reason.clone())),
            "alert-double-click",
        );
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        let ok = dialog
            .with_alert(|alert| alert.ok_button().node())
            .flatten()
            .expect("alert OK 节点");
        let cancel = dialog
            .with_alert(|alert| alert.cancel_button().and_then(|button| button.node()))
            .flatten()
            .expect("alert Cancel 节点");
        harness.click_node(ok);
        assert_eq!(ok_runs.get(), 1, "第一次点击跑 alert 的 OK 动作");
        assert!(dialog.is_closing(), "OK 动作里的 close 开始淡出");
        harness.click_node(ok);
        assert_eq!(
            ok_runs.get(),
            1,
            "淡出中的第二次点击不得再跑 alert 的 OK 动作（D2）"
        );
        harness.click_node(cancel);
        assert_eq!(
            cancel_runs.get(),
            0,
            "淡出中 alert 的 Cancel 动作同样不得跑"
        );
        for _ in 0..24 {
            harness.app.advance(Duration::from_millis(16));
            let _ = dialog.sync(harness.app.app_mut()).expect("同步");
            let _ = harness.app.drain().expect("帧");
            if !dialog.is_open(harness.app.app()) {
                break;
            }
        }
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
        assert_eq!(ok_runs.get(), 1, "整段淡出只跑一次 OK 动作");
    }
}

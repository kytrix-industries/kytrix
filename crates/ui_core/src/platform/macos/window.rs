//! 原生窗口：`NSWindow` + 自绘标题栏 + 交通灯。
//!
//! 状态同步（尺寸 / 缩放 / 焦点 / 可见性 / 位置）走「每轮快照对比」而不是 delegate 或通知：
//! 泵每轮都要读一次这些值（O(1)）。delegate 回调（`windowDidMove:` 等）只**置位 + 唤醒**，
//! 平台真值仍由 `sync_state` 快照读取，避免两个真值来源。**例外是关闭**：AppKit 的关闭是
//! 一次性的、带否决语义的动作，没有「快照对比」可做（窗口没了就再也读不到），必须由平台
//! 关闭回调驱动。见 [`WindowDelegate`] 与 `platform::macos_close`。
//!
//! # 关闭语义（B5，用户裁决 D4(a)）
//!
//! 目标不变量（三端统一）：平台只**通知**关闭请求，应用在 `on_close` 里裁决（可否决），
//! 同意后平台才真正关闭；macOS 上红点 / Cmd+W / 菜单 Close **不得播放系统提示音**。
//!
//! - **手势路径（可否决）**：标准关闭按钮被重定向到 `GuiView` 的 `performClose:` 覆盖
//!   （不调 `super`，因此不触发 `windowShouldClose:` ⇒ 无系统 beep）；Cmd+W / 菜单 Close 的
//!   responder chain 也先到 first responder（`GuiView`）。两者都只置关闭桥 + 唤醒泵。
//!   泵投递 `Event::CloseRequested` → `App::request_close` → 同意后才 `NSWindow.close()`。
//! - **防御路径**：`windowShouldClose:` 保留（AppKit 若从非标准路径问过来），置位 + 唤醒 +
//!   返回 `false`。**残余风险**：这条路径不是 UI 的常规路径（关闭按钮已重定向、Cmd+W 命中
//!   first responder 的 `performClose:` 覆盖），但若有第三方/AppleScript 直接给 `NSWindow`
//!   发 `performClose:`，AppKit 会在 delegate 返回 `false` 时播放系统提示音 —— 这是保留
//!   `windowShouldClose:` 语义的代价，记录在案。
//! - **外部关闭路径（不可否决）**：`windowWillClose:` 是「窗口真的关了」的唯一权威来源。
//!   引擎自己关窗时先断 delegate 再 `close()`（不会回调），因此收到它的唯一情形是 AppKit
//!   自己关掉了窗口；此时只做 App 侧关闭记账 + 移除原生窗口，**不**再投递事件。
//! - **禁用 window tabbing**（类级 `setAllowsAutomaticWindowTabbing(false)` + 每窗
//!   `TabbingMode::Disallowed`）：窗口不并入系统 tab 组，Window 菜单因此不再出现
//!   Merge All Windows；关闭语义不会被「关一个 tab」这类非窗口级动作打乱。
//!
//! 可见性（R09-9）三个来源：最小化（`isMiniaturized`）、应用隐藏（`NSApp.hide`，Cmd+H）、
//! **被完全遮挡/在别的 Space**（`occlusionState` 不含 `Visible`）。三者任一为真，帧就不再产出、
//! display link 暂停（见 `event_loop::pump`）——交易终端最小化几小时是常态，120 Hz 唤醒是纯浪费。
//! 可见性**只**用于帧门控：旧实现曾用它反推「窗口已关闭」，但最小化/隐藏/tab 合并都会让
//! `isVisible` 为 false，那是误判（B5 已删除该反推，关闭只信 `windowWillClose:`）。
//!
//! 摆放（B3）：`WindowOptions.position` / `min_size` / `max_size` 在 [`NativeWindow::create`]
//! 消费；坐标换算 / 工作区 clamp 的纯逻辑在 `platform::macos_placement`（Windows 可单测）。
//! 位置变化经 `windowDidMove:` 唤醒 + `sync_state` 快照比较，产出 `Event::Moved`。
//!
//! # 窗口层级 / owner（X1 原生部分）
//!
//! - **owner**：建窗后对 owner 窗口调 `addChildWindow:ordered:`（`NSWindowAbove`），保存
//!   owner 的强引用以便关闭时 `removeChildWindow:`。这是 AppKit 的 **transient parent**
//!   语义：child 恒在 parent 之上、随 parent 一起进出 z 序，parent 关闭/orderOut 时 AppKit
//!   会把 child 一并 orderOut（不销毁）。**引擎层**（`app/window_set.rs`）已负责 owner 关闭
//!   时级联关闭 owned 窗口，两者不冲突：引擎先把 App 侧窗口关掉，平台层再各自关原生窗口；
//!   AppKit 的自动 orderOut 至多让窗口提前离开屏幕（同一轮泵内就被移除）。
//! - **always_on_top**：`NSFloatingWindowLevel`。
//! - **modal**（RB-07，用户批准方案 A）：原生路径 = 模态层级（`NSModalPanelWindowLevel`）
//!   与「作为 owner 的 child window」；此外**打开期间对 owner 施加原生拦截**：快照 owner 的
//!   chrome（[`OwnerChrome`]）后设 `ignoresMouseEvents:YES`、禁用红点/最小化/缩放、关闭
//!   背景拖动，关闭时按快照恢复（含把键盘焦点还给仍可用的 owner）。模态窗口显示时
//!   `makeKeyAndOrderFront:` 接管键盘焦点，泵每轮用 `macos_owner::modal_takes_key` 把被
//!   owner 抢回的 key 拿回来 —— 否则键盘事件进 owner 会被引擎的模态输入过滤丢弃，模态窗口
//!   收不到键。引擎层（`app/window_set.rs` 的 `is_input_blocked_by_modal`）仍按 owner 关系
//!   过滤输入，作为原生拦截被绕过的兜底。
//!
//!   模态**不用** `runModalForWindow:`（嵌套 run loop 与本引擎在 `NSApplication.run` 的 run
//!   loop 上跑的泵重入）与 `beginSheet:`（会改变窗口形态、把模态窗口嵌进 owner 的标题栏；
//!   用户已裁决不用 sheet）。层级选择（模态 > 置顶）与拦截/恢复决策的纯逻辑在
//!   `platform::macos_owner`，Windows 可单测。
//! - 请求了 owner 但平台侧找不到对应原生窗口（创建批次顺序/非法 id）⇒ 降级为无 owner 并
//!   写诊断，不 panic（见 `event_loop::attach_owners`）。这种窗口没有 owner 可拦截，引擎层
//!   的模态输入过滤也不会命中它（`is_input_blocked_by_modal` 判的是直接 owner），与
//!   `WindowOptions::modal` 的降级口径一致。

use std::{cell::Cell, rc::Rc, sync::Arc};

use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSFloatingWindowLevel,
    NSModalPanelWindowLevel, NSScreen, NSWindow, NSWindowButton, NSWindowCollectionBehavior,
    NSWindowDelegate, NSWindowOcclusionState, NSWindowOrderingMode, NSWindowStyleMask,
    NSWindowTabbingMode, NSWindowTitleVisibility,
};
use objc2_foundation::{
    NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};
use objc2_quartz_core::CAMetalLayer;

use crate::{
    app::App,
    error::EngineError,
    event::{Event, ImeEvent},
    math::{Rect, Scale, Size},
    paint::style::Cursor,
    platform::{
        dispatcher::Dispatcher,
        macos::{a11y::MacA11y, dpi, menu, view::GuiView, wake},
        macos_close::{CloseAction, CloseBridge},
        macos_owner::{OwnerChrome, WindowLevel, modal_takes_key, release_modal_owner},
        macos_placement as placement,
        menu::MenuCommand,
    },
    window::{WindowId, WindowOptions},
};

/// `NSWindowDelegate` 的 ivar：只记「AppKit 关闭窗口的事实」并唤醒泵。
///
/// 回调里**不碰 `App`**（与 `GuiView` 同一纪律）：置位关闭桥 / 唤醒，由泵在主线程裁决
/// `App::request_close`。两种关闭来源在 `CloseBridge` 里严格区分（B5，见
/// `platform::macos_close`）：
///
/// - `windowShouldClose:`（防御路径）：用户手势 · 可否决 ⇒ 置 `Requested` 并返回 `false`
///   （AppKit 因此不关窗；引擎自己在裁决通过后 `close()`，`close` 不会再问 delegate，
///   不会递归）。返回 `false` 的代价是非标准调用方会听到系统提示音，见模块文档。
/// - `windowWillClose:`（外部关闭）：AppKit 真的关了窗口 ⇒ 置终态 `WillClose`，泵只记账。
///
/// `moved`（B3）同理：`windowDidMove:` 只置位 + `dispatcher.wake()`，位置由泵的
/// `sync_state` 从 `NSView` 快照算出（回调里拿不到 `&mut App`，也不该在回调里换算）。
struct WindowIvars {
    close: Rc<CloseBridge>,
    moved: Cell<bool>,
    dispatcher: Arc<Dispatcher>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KytrixWindowDelegate"]
    #[ivars = WindowIvars]
    struct WindowDelegate;

    unsafe impl NSObjectProtocol for WindowDelegate {}

    unsafe impl NSWindowDelegate for WindowDelegate {
        /// AppKit 的关窗防御路径（关闭按钮已被重定向到 `GuiView::performClose:`，Cmd+W /
        /// 菜单 Close 命中 first responder；这条路径只在 AppKit 从非标准入口问起时才走）。
        ///
        /// 与 `performClose:` 共用同一个 [`CloseBridge`]：去重、只置一次位。返回 `false`
        /// 保证 AppKit 不自行关窗（真正的关闭由泵在裁决通过后执行）。**残余**：若这条路径
        /// 被触发，AppKit 会因 `false` 播放系统提示音 —— 无法两全（要否决就不能返回 YES）。
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            if self.ivars().close.request() {
                self.ivars().dispatcher.wake();
            }
            // 恒拒绝 AppKit 的同步关闭：真正关闭由泵在 `App::request_close` 同意后调 `close()`。
            false
        }

        /// `windowWillClose:`：**「窗口真的关了」的唯一权威来源**（B5）。
        ///
        /// 只置位 + 唤醒：外部关闭不可否决，泵在下一轮把它翻译成「App 侧关闭记账 + 移除原生
        /// 窗口」。不在这里碰 `App`（回调里没有 `&mut App`，也不该在 AppKit 的关闭通知里跑
        /// 应用关闭处理器 —— 顺序与可重入性都不可控）。
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            if self.ivars().close.will_close() {
                self.ivars().dispatcher.wake();
            }
        }

        // R09-9：遮挡 / 最小化 / 恢复只**唤醒泵**——可见性是泵每轮快照读的（`sync_state`），
        // 在 delegate 里回查/缓存反而会与「每轮快照」形成两个真值来源。这里选 delegate 而不是
        // `NSNotificationCenter` 观察者：窗口本来就有 delegate，多挂四个通知源是重复的注册与
        // 生命周期负担（delegate 是 weak，我们已持强引用）。
        #[unsafe(method(windowDidChangeOcclusionState:))]
        fn window_did_change_occlusion_state(&self, _notification: &NSNotification) {
            self.ivars().dispatcher.wake();
        }

        #[unsafe(method(windowDidMiniaturize:))]
        fn window_did_miniaturize(&self, _notification: &NSNotification) {
            self.ivars().dispatcher.wake();
        }

        #[unsafe(method(windowDidDeminiaturize:))]
        fn window_did_deminiaturize(&self, _notification: &NSNotification) {
            self.ivars().dispatcher.wake();
        }

        /// B3：窗口位置变化（拖动标题栏 / 系统摆放 / 跨屏 / `setFrame`）。
        ///
        /// 只置位 + 唤醒：位置换算与 `Event::Moved` 投递由泵做（回调不碰 `App`）。
        /// 唤醒保证「拖动期间至少每个 run loop 一轮」收到位置事件。
        #[unsafe(method(windowDidMove:))]
        fn window_did_move(&self, _notification: &NSNotification) {
            self.ivars().moved.set(true);
            self.ivars().dispatcher.wake();
        }
    }
);

impl WindowDelegate {
    fn new(
        mtm: MainThreadMarker,
        close: Rc<CloseBridge>,
        dispatcher: Arc<Dispatcher>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(WindowIvars {
            close,
            moved: Cell::new(false),
            dispatcher,
        });
        // SAFETY: `init` 是 NSObject 的指定初始化器；`this` 由 `alloc` 产生且未初始化。
        unsafe { msg_send![super(this), init] }
    }

    /// 取走「AppKit 报告窗口移动」标记（B3；位置真值仍由 `sync_state` 快照读取）。
    fn take_moved(&self) -> bool {
        self.ivars().moved.replace(false)
    }
}

/// 最小客户区（逻辑像素；与 Win32 / Wayland 同源）。
pub(crate) const MIN_SIZE_LOGICAL: (f32, f32) = (320.0, 200.0);

/// 全部屏幕的 frame / visibleFrame 快照（B3；`NSScreen` 是主线程对象，建窗与泵都在主线程）。
fn screen_areas(mtm: MainThreadMarker) -> Vec<placement::ScreenArea> {
    let screens = NSScreen::screens(mtm);
    let mut areas = Vec::with_capacity(screens.count());
    for screen in screens.to_vec() {
        areas.push(placement::ScreenArea::new(
            ns_area(screen.frame()),
            ns_area(screen.visibleFrame()),
        ));
    }
    areas
}

/// `NSRect` → 平台无关的 [`placement::Area`]（AppKit 全局坐标、point）。
fn ns_area(rect: NSRect) -> placement::Area {
    placement::Area::new(
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    )
}

/// 读 owner 窗口当前的 [`OwnerChrome`]（RB-07 的快照；模态打开时调用）。
///
/// 三个标准按钮在无标题栏的窗口上可能不存在（`standardWindowButton:` 返回 `nil`）：
/// 那种情况下该项按「禁用」记录，恢复时同样跳过（没有按钮可写）。
fn read_owner_chrome(window: &NSWindow) -> OwnerChrome {
    let enabled = |button: NSWindowButton| {
        window
            .standardWindowButton(button)
            .is_some_and(|button| button.isEnabled())
    };
    OwnerChrome {
        ignores_mouse_events: window.ignoresMouseEvents(),
        movable_by_window_background: window.isMovableByWindowBackground(),
        close_button_enabled: enabled(NSWindowButton::CloseButton),
        minimize_button_enabled: enabled(NSWindowButton::MiniaturizeButton),
        zoom_button_enabled: enabled(NSWindowButton::ZoomButton),
    }
}

/// 把 [`OwnerChrome`] 逐项写进 owner 窗口（模态期施加拦截 / 关闭时恢复原值）。
fn apply_owner_chrome(window: &NSWindow, chrome: OwnerChrome) {
    window.setIgnoresMouseEvents(chrome.ignores_mouse_events);
    window.setMovableByWindowBackground(chrome.movable_by_window_background);
    for (button, enabled) in [
        (NSWindowButton::CloseButton, chrome.close_button_enabled),
        (
            NSWindowButton::MiniaturizeButton,
            chrome.minimize_button_enabled,
        ),
        (NSWindowButton::ZoomButton, chrome.zoom_button_enabled),
    ] {
        if let Some(button) = window.standardWindowButton(button) {
            button.setEnabled(enabled);
        }
    }
}

/// `position = None` 时的平台默认摆放（B3）：主屏工作区左上角按 26 point 阶梯级联。
///
/// AppKit 没有「自动摆放」的默认行为（`initWithContentRect:` 严格按给定矩形创建），
/// 因此平台层自己做级联（等价 `NSWindow.cascadeTopLeftFromPoint:` 的步长，Qt / winit
/// 的默认摆放同款）—— 多窗口不再完全重叠，兑现 `WindowOptions::position = None` 文档。
fn cascade_content_origin(
    mtm: MainThreadMarker,
    screens: &[placement::ScreenArea],
    size: Size,
    index: usize,
) -> (f64, f64) {
    let Some(desktop) = placement::Desktop::from_screens(screens) else {
        // 无屏幕（headless / 无显示器）：沿用历史默认位置，保证窗口仍可创建。
        return (80.0, 80.0);
    };
    let main_visible = NSScreen::mainScreen(mtm)
        .map(|screen| ns_area(screen.visibleFrame()))
        .unwrap_or(screens[0].visible());
    let position = placement::cascade_position(desktop, main_visible, index);
    placement::content_origin_for_position(position, size, screens)
}

/// 一个原生窗口（`Application::run` 持有；`Drop` 时关闭）。
pub(crate) struct NativeWindow {
    mtm: MainThreadMarker,
    id: WindowId,
    window: Retained<NSWindow>,
    view: Retained<GuiView>,
    /// `delegate` 是 **weak** 属性：必须由门面持有强引用，否则回调会打在已释放对象上。
    delegate: Retained<WindowDelegate>,
    /// 关闭桥（B5）：view / delegate 写入、泵消费（与 `GuiView` / `WindowDelegate` 共享）。
    close: Rc<CloseBridge>,
    /// owner 窗口的原生引用（X1；`addChildWindow:ordered:` 的 parent）。
    ///
    /// 保持强引用：关闭本窗口时要 `removeChildWindow:`，即使 parent 的 `NativeWindow` 已先
    /// 被移除，NSWindow 对象也必须仍然存活（`Retained` 保证这一点）。
    owner: Option<Retained<NSWindow>>,
    /// 模态窗口对 owner 施加原生拦截前的 chrome 快照（RB-07）。
    ///
    /// `Some` = 本窗口是模态窗口且已拦截 owner（`None` = 没有施加拦截）。关闭时按快照恢复
    /// 并 `take` 掉，因此「关闭 / 外部关闭 / 级联关闭 / Drop / App 关闭」多条收尾路径都幂等、
    /// 不会把 owner 永久留在不可交互状态。
    modal_owner_chrome: Option<OwnerChrome>,
    scale: Scale,
    logical_size: Size,
    /// 最近一次同步的快照（用于检测变化）。
    seen_scale: f64,
    /// 最近一次写进 display link 的屏幕刷新率（换屏检测）。
    seen_frame_rate: f32,
    seen_size: (f64, f64),
    was_key: bool,
    /// 当前是否**可见**（最小化 / 应用隐藏 / 完全遮挡任一为真则为 false，R09-9）。
    visible: bool,
    destroyed: bool,
    /// 最近一次下发的 IME caret 区域（逻辑坐标）。
    ime_area: Option<Rect>,
    /// 已应用到 `GuiView` 的 IME 启用状态（B6；目标状态来自 `App::ime_enabled`）。
    ime_enabled: bool,
    /// `sync_state` 观察到的「窗口刚失去键盘焦点」边沿（泵在同步后消费它做 IME 清场）。
    focus_lost: bool,
    /// 最近一次观察到的内容区左上角（**AppKit 全局坐标**，point）：`Event::Moved` 的去重基准。
    ///
    /// 用 AppKit 坐标比较而不是引擎坐标：屏幕布局变化会改变两者之间的平移/翻转，用引擎
    /// 坐标比较会在「没动过的窗口」上误报 `Event::Moved`。
    seen_content: placement::Area,
}

impl NativeWindow {
    /// 建**隐藏**窗口（不 `makeKeyAndOrderFront`）并装好 view + a11y adapter。
    ///
    /// `cascade_index` 是「本窗口是本次运行创建的第几个」：`WindowOptions.position = None`
    /// 时用于平台默认的级联摆放（B3，见 `macos_placement::cascade_position`）。
    /// `a11y_consumer_epoch` 是窗口的消费端代际句柄（与引擎 `A11yBridge` 共享）：AX 重新激活
    /// 时激活回调 `bump()` 它，引擎把下一笔升成全树（E1）。
    pub(crate) fn create(
        mtm: MainThreadMarker,
        id: WindowId,
        options: &WindowOptions,
        dispatcher: &Arc<Dispatcher>,
        cascade_index: usize,
        a11y_consumer_epoch: crate::a11y::adapter::A11yConsumerEpoch,
    ) -> Result<Self, EngineError> {
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;
        let size = Size::new(options.size.width.max(1.0), options.size.height.max(1.0));
        // B3：`position` 是「虚拟桌面内容区左上角、y 向下」的逻辑点；平台层换算成 AppKit
        // 的内容区矩形（左下原点、y 向上）并 clamp 进目标屏幕的 visibleFrame。`None` 走
        // 平台默认摆放（主屏工作区左上角 26 point 阶梯级联）。
        //
        // 非有限的 `position`（NaN/∞）按 `None` 处理：NSRect 里塞 NaN 会让 AppKit 行为未定义。
        let screens = screen_areas(mtm);
        let (content_x, content_y) = match options.position.filter(|position| position.is_finite())
        {
            Some(position) => placement::content_origin_for_position(position, size, &screens),
            None => cascade_content_origin(mtm, &screens, size, cascade_index),
        };
        let rect = NSRect::new(
            NSPoint::new(content_x, content_y),
            NSSize::new(f64::from(size.width), f64::from(size.height)),
        );
        // SAFETY: `alloc` + 指定初始化器是 NSWindow 的合法构造路径；`rect` 是栈上值。
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: 关闭时不让 AppKit 释放窗口（我们持有 `Retained`）。
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(options.title.as_str()));
        window.setTitlebarAppearsTransparent(true);
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        // fullscreen 转场使用不透明窗口；透明窗口会让 AppKit 的全屏 Space 停在过渡快照上
        // （实测：view/layer 已 1680×1050，屏幕仍显示旧 960×740 快照）。引擎自己画满背景，
        // 不需要窗口透明。
        window.setOpaque(true);
        // 首帧（layer 第一次 present）之前由 AppKit 填底色；与 layer 背景色同源（sRGB）。
        let [red, green, blue, _] = crate::render::color::background_srgb(options.background);
        let background = NSColor::colorWithSRGBRed_green_blue_alpha(
            f64::from(red),
            f64::from(green),
            f64::from(blue),
            1.0,
        );
        window.setBackgroundColor(Some(&background));
        window.setAcceptsMouseMovedEvents(true);
        // B3：客户区 min/max（逻辑点）。`min > max` 的钳制在平台无关纯逻辑里做；
        // `max = None` 时不调用 `setContentMaxSize:`（保持 AppKit 的默认无限上限）。
        let (min_size, max_size) = placement::content_size_bounds(
            options.min_size,
            options.max_size,
            Size::new(MIN_SIZE_LOGICAL.0, MIN_SIZE_LOGICAL.1),
        );
        window.setContentMinSize(NSSize::new(
            f64::from(min_size.width),
            f64::from(min_size.height),
        ));
        if let Some(max_size) = max_size {
            window.setContentMaxSize(NSSize::new(
                f64::from(max_size.width),
                f64::from(max_size.height),
            ));
        }
        // B5：禁用本窗口的自动 tabbing（类级开关在 `event_loop::run` 里先设好；这里按窗口
        // 再声明一次 Disallowed —— Apple 文档要求 tabbing mode 在窗口显示前设置）。窗口因此
        // 不并入系统 tab 组，Window 菜单不再出现 Merge All Windows，关闭语义保持窗口级。
        window.setTabbingMode(NSWindowTabbingMode::Disallowed);

        // 显式声明该窗口是原生全屏的 primary 窗口；默认 collectionBehavior=0 时
        // AppKit 的 fullscreen 转场会停在过渡快照上（实测：窗口/view/layer 都已
        // 1680×1050，但屏幕仍显示旧的 960×740 快照）。
        window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);

        let close = Rc::new(CloseBridge::new());
        let view = GuiView::new(mtm, Arc::clone(dispatcher), Rc::clone(&close));
        window.setContentView(Some(&view));
        // 原生全屏转场由 AppKit 直接改 window frame；contentView 必须随宽度/高度伸缩，
        // 否则引擎看到的一直是 960×740，layer/drawable 也不会覆盖全屏窗口。
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        view.setWantsLayer(true);
        window.makeFirstResponder(Some(&view));
        // B5：标准关闭按钮默认把 `performClose:` 发给窗口；重定向到 view 的覆盖实现，
        // 使红点与 Cmd+W / 菜单 Close 走同一条「置位 + 唤醒泵」路径（不触发
        // `windowShouldClose:` ⇒ 不响系统提示音）。按钮归引擎：应用自己 setAction 会被
        // 这里的建窗顺序覆盖，见 `WindowOptions` / `GuiView::performClose:` 文档。
        if let Some(button) = window.standardWindowButton(NSWindowButton::CloseButton) {
            // SAFETY: target 是同一窗口的 contentView（生命周期覆盖窗口），且刚为它实现了
            // `performClose:`；`target` 是 weak 属性，view 由窗口持有，不会悬空。
            let target: &AnyObject = unsafe { &*(&*view as *const GuiView).cast::<AnyObject>() };
            unsafe { button.setTarget(Some(target)) };
            // SAFETY: `menu::selector(MenuCommand::Close)` 就是主菜单 Close 项用的
            // `performClose:`（同一份映射，见 `platform::macos::menu::selector`），view 已实现。
            unsafe { button.setAction(Some(menu::selector(MenuCommand::Close))) };
        }

        // 原生交通灯全部保留：关闭 / 最小化 / 缩放。标题栏透明且标题隐藏，内容铺满窗口，
        // 因此窗口控件浮在内容上方（应用用顶部安全区避让）。
        let a11y = MacA11y::new(&view, dispatcher.clone(), id, a11y_consumer_epoch)?;
        view.install_a11y(a11y);

        // 关闭拦截：关闭按钮 / Cmd-W / 菜单 Close 都先到 view 的 `performClose:`（只置位 +
        // 唤醒）；`windowShouldClose:` 是 AppKit 的防御路径（同样置位 + 返回 false）；
        // `windowWillClose:` 报告 AppKit 真的关闭了窗口。三者共享同一份关闭桥，泵裁决。
        let delegate = WindowDelegate::new(mtm, Rc::clone(&close), Arc::clone(dispatcher));
        // SAFETY: ProtocolObject::from_ref 要求该对象实现了目标协议 —— 上面刚 impl 过。
        let protocol: &ProtocolObject<dyn NSWindowDelegate> = ProtocolObject::from_ref(&*delegate);
        window.setDelegate(Some(protocol));
        let mut native = Self {
            mtm,
            id,
            window,
            view,
            delegate,
            close,
            // X1：owner 关系由泵在建窗批次**全部创建之后**接线（`apply_owner`）：
            // `App` 的窗口表是 HashMap，owner 可能排在 owned 之后。
            owner: None,
            // RB-07：模态窗口在 `apply_owner` 接线 owner 时快照并拦截；这里还没有 owner。
            modal_owner_chrome: None,
            scale: options.scale,
            logical_size: options.size,
            seen_scale: 1.0,
            seen_frame_rate: 0.0,
            seen_size: (0.0, 0.0),
            was_key: false,
            visible: true,
            destroyed: false,
            ime_area: None,
            // 与 `App::ime_enabled` 的默认值一致（默认启用）；首次 `sync_ime_enabled`
            // 因此不会产生任何平台调用。
            ime_enabled: true,
            focus_lost: false,
            seen_content: placement::Area::new(0.0, 0.0, 0.0, 0.0),
        };
        native.sync_initial(mtm)?;
        Ok(native)
    }

    /// 初次同步：读 `backingScaleFactor` / 内容尺寸 / 初始内容区位置，写回字段（**不**产生事件）。
    fn sync_initial(&mut self, _mtm: MainThreadMarker) -> Result<(), EngineError> {
        let factor = self.window.backingScaleFactor();
        let scale = dpi::scale_from(factor)?;
        self.scale = scale;
        self.seen_scale = factor;
        let size = self.view.frame().size;
        self.seen_size = (size.width, size.height);
        self.logical_size = Size::new(size.width as f32, size.height as f32);
        // B3：把实际生效的位置记成 `Event::Moved` 的去重基准 —— 创建时的请求位置可能被
        // AppKit 的 frame 约束微调过（或者被我们的 visibleFrame clamp 过），这里以真值
        // 为准，避免启动后的第一轮同步补发一条「假移动」。
        self.seen_content = self.content_rect_in_screen();
        Ok(())
    }

    /// view 的内容区在 AppKit 全局坐标下的矩形（point）。
    ///
    /// 用「view 的矩形」而不是 `window.frame()`：窗口是 `FullSizeContentView`，内容区铺满
    /// 整个 frame；`NSWindow.convertRectToScreen:` 把内容坐标系里的矩形换算成屏幕坐标，
    /// 与创建时喂给 `initWithContentRect:` 的矩形是同一个语义，读写才能互逆。
    fn content_rect_in_screen(&self) -> placement::Area {
        ns_area(self.window.convertRectToScreen(self.view.frame()))
    }

    /// 把渲染器的 `CAMetalLayer` 挂到 view 上（`wantsLayer` 已在 `create` 里打开）。
    ///
    /// 只在这里把 layer 交给 AppKit；`contentsScale` / `drawableSize` / 几何的唯一写者是**主线程**
    /// 的 `Renderer::set_layer_scale`（§5.3 / C6），GPU 线程只读 drawable 的实际尺寸。
    ///
    /// 挂上之后立刻用 **view bounds 的精确点**再写一次 layer 几何（C6）：`new_layer` 建层时用的是
    /// `backing / scale`（半像素对齐时可能差 `0.5/scale` pt），这里拿得到 view 的真值。
    /// layer-hosting 的几何责任在 App 侧（Apple `NSView.wantsLayer`），图层在 `setLayer:` 之前
    /// frame 是 zero；真机定案表明 AppKit 也会同步同样的值 ⇒ 本次写入是幂等防御（见
    /// `swapchain::set_layer_geometry` 的文档）。
    pub(crate) fn attach_layer(&self, layer: Retained<CAMetalLayer>) {
        self.view.setLayer(Some(&layer));
        let bounds = self.view.bounds();
        crate::render::macos::swapchain::set_layer_geometry(
            &layer,
            (bounds.size.width, bounds.size.height),
        );
    }

    /// 显示窗口并接管键盘焦点。
    ///
    /// **模态窗口显示时必须拿到 key**（RB-07）：模态通常从 owner 的按钮 / 菜单打开，若焦点
    /// 留在 owner，键盘事件会进 owner 并被引擎的模态输入过滤丢弃 ⇒ 用户无法向模态对话框
    /// 打字。`makeKeyAndOrderFront:` 无条件接管焦点（与 Windows 模态窗口 `SetForegroundWindow`
    /// 的观感一致）；之后 owner 若通过 AppKit 的窗口循环重新抢到 key，泵每轮对账把它拿回来
    /// （见 [`Self::sync_state`]）。
    pub(crate) fn show(&self) {
        self.window.makeKeyAndOrderFront(None);
        self.window.makeFirstResponder(Some(&self.view));
    }

    pub(crate) fn id(&self) -> WindowId {
        self.id
    }

    /// 原生窗口句柄（X1）：泵把 owner 的 `NSWindow` 交给 owned 窗口挂接 child 关系用。
    pub(crate) fn window_handle(&self) -> Retained<NSWindow> {
        self.window.clone()
    }

    /// 建窗后的 X1 接线：设置窗口层级 + 挂接 owner（`addChildWindow:ordered:`）+ 模态期
    /// 原生拦截 owner（RB-07）。
    ///
    /// 分两阶段调用（先创建全部原生窗口、再接线）的原因见 `event_loop::attach_owners`：
    /// `App` 的窗口表是 `HashMap`，创建批次的顺序不可控而 owner 必须先有原生窗口。
    ///
    /// - **层级**：选择逻辑在 `platform::macos_owner`（模态压过置顶）。`Normal` 不调用
    ///   `setLevel:` —— 保持 AppKit 默认层级（`NSNormalWindowLevel` == 0）。
    /// - **owner**：`addChildWindow:ordered:` 是 transient parent 语义（child 恒在 parent
    ///   之上、随 parent 进出 z 序，parent 关闭时被 orderOut）；引擎层的 owner 关闭级联与
    ///   它不冲突（见模块文档）。
    /// - **模态**：owner 接线成功后对 owner 施加 [`OwnerChrome::BLOCKED`] 原生拦截（RB-07，
    ///   见 [`Self::block_modal_owner`]）。`ModalPanel` 层级只会在 owner 已解析时出现
    ///   （`attach_owners` 的判据），因此这里不需要再判 owner 是否存在。
    pub(crate) fn apply_owner(&mut self, owner: Option<Retained<NSWindow>>, level: WindowLevel) {
        match level {
            WindowLevel::Normal => {}
            WindowLevel::Floating => self.window.setLevel(NSFloatingWindowLevel),
            WindowLevel::ModalPanel => self.window.setLevel(NSModalPanelWindowLevel),
        }
        if let Some(owner) = owner {
            // SAFETY: 两个 NSWindow 都是本进程主线程上创建、由 `NativeWindow` 持有的对象；
            // `addChildWindow:ordered:` 是 AppKit 的窗口关系 API（ordered 只决定 z 序，不转移
            // 所有权）。调用方已过滤自指（`platform::macos_owner::resolve_owner`）。
            unsafe { owner.addChildWindow_ordered(&self.window, NSWindowOrderingMode::Above) };
            self.owner = Some(owner);
        }
        if matches!(level, WindowLevel::ModalPanel) {
            self.block_modal_owner();
        }
    }

    /// 打开模态窗口时对 owner 施加原生拦截（RB-07，用户批准方案 A）。
    ///
    /// 快照 owner 的 chrome **再**施加 [`OwnerChrome::BLOCKED`]：owner 对鼠标透明、标准按钮
    /// （红点 / 最小化 / 缩放）禁用、不可由背景拖动 —— 与 Windows `EnableWindow(owner, FALSE)`
    /// 同语义。快照存在 `modal_owner_chrome`，关闭时由 [`Self::restore_modal_owner`] 按原值
    /// 恢复（不假设默认值：应用可能自己禁用过红点、开启过背景拖动）。
    ///
    /// 拦截只是语义对齐的一半：模态窗口的键盘焦点由 `show()`（显示时 `makeKeyAndOrderFront:`）
    /// 与 `sync_state`（每轮把被 owner 抢走的 key 拿回来）保证。
    fn block_modal_owner(&mut self) {
        let Some(owner) = self.owner.as_ref() else {
            return;
        };
        let original = read_owner_chrome(owner);
        apply_owner_chrome(owner, OwnerChrome::BLOCKED);
        self.modal_owner_chrome = Some(original);
    }

    /// 恢复被模态拦截的 owner（RB-07；幂等）。
    ///
    /// 关闭 / 外部关闭 / 级联关闭（owner 关闭导致 owned 关闭）/ Drop / App 关闭都汇到这里：
    /// `modal_owner_chrome` 被 `take`，第二次调用是 no-op。恢复目标由平台无关的
    /// [`release_modal_owner`] 决定（只在 owner 仍处拦截态时写回快照；多模态窗口下不会被
    /// 后来的窗口把拦截态写回去）。owner 仍可用时把键盘焦点还给它 —— 与 Windows
    /// `EnableWindow(owner, TRUE)` 的恢复语义对齐。
    fn restore_modal_owner(&mut self) {
        let Some(original) = self.modal_owner_chrome.take() else {
            return;
        };
        let Some(owner) = self.owner.as_ref() else {
            return;
        };
        let release = release_modal_owner(
            read_owner_chrome(owner),
            original,
            self.window.isKeyWindow(),
            owner.isVisible() && !owner.isMiniaturized(),
        );
        if let Some(chrome) = release.chrome {
            apply_owner_chrome(owner, chrome);
        }
        if release.return_key {
            owner.makeKeyWindow();
        }
    }

    /// 解除 owner 关系（幂等；引擎主动关闭与外部关闭两条收尾路径共用）。
    ///
    /// AppKit 不保证 `NSWindow.close` 会把本窗口从 parent 的 `childWindows` 里摘掉（文档只
    /// 承诺 parent 关闭时 child 一并 orderOut），因此显式 `removeChildWindow:`。parent 即使
    /// 已先被关闭，其 `NSWindow` 对象仍由这里的强引用保活，调用安全。
    fn detach_owner(&mut self) {
        if let Some(owner) = self.owner.take() {
            owner.removeChildWindow(&self.window);
        }
    }

    pub(crate) fn scale(&self) -> Scale {
        self.scale
    }

    pub(crate) fn logical_size(&self) -> Size {
        self.logical_size
    }

    /// 真实 backing 像素尺寸（`Window.surface` / `CAMetalLayer.drawableSize` 的唯一整数）。
    ///
    /// `convertSizeToBacking(view.bounds.size)` 是 AppKit 实际分配的像素数（round，不是 ceil；
    /// 至少 1 px）；`size × scale` 的 covering 只是布局估算，半像素对齐时可能差 1 px。
    pub(crate) fn backing_size(&self) -> (u32, u32) {
        let backing = self.view.convertSizeToBacking(self.view.bounds().size);
        (
            (backing.width.round() as u32).max(1),
            (backing.height.round() as u32).max(1),
        )
    }

    /// 窗口所在屏幕的最大刷新率（display link 的 `preferredFrameRateRange` 上界，§5.3）。
    ///
    /// 取不到屏幕（完全离屏 / 无显示器）时退回 60 Hz；高刷屏由系统给真值。
    pub(crate) fn maximum_frames_per_second(&self) -> f32 {
        self.window
            .screen()
            .map_or(60.0, |screen| screen.maximumFramesPerSecond() as f32)
    }

    /// 窗口当前是否可见（最小化 / 应用隐藏 / 完全遮挡任一为真 ⇒ `false`）。
    ///
    /// 泵用它门控出帧与 display link（R09-9）：不可见时**不产出帧**，渲染器的
    /// `pacing_paused()` 才能在「无待画帧 / 无在飞帧」时把 display link 真正停下来。
    pub(crate) fn is_visible(&self) -> bool {
        self.visible
    }

    /// 取走本轮平台事件。
    pub(crate) fn take_events(&self) -> Vec<Event> {
        self.view.take_events()
    }

    /// 每轮对比窗口状态，按需产生 `DpiChanged` / `Resized` / `FocusChanged` / `CloseRequested`。
    ///
    /// 返回值 = 本轮**产生了状态变化**（事件或可见性边沿）。泵据此在同一个泵周期内出帧：
    /// 不再从 `sync_state` 里调 `Dispatcher::wake()`（那会 signal 一次 run loop source，
    /// 让同一轮多跑一次空泵 —— R09-15 的第二个来源）。
    pub(crate) fn sync_state(&mut self, app: &mut App) -> Result<bool, EngineError> {
        let mut events: Vec<Event> = Vec::new();
        // 换屏（或系统刷新率变化）：display link 的偏好帧率建窗时只写一次，必须每轮比较
        // 当前屏幕的 `maximumFramesPerSecond` 并更新，否则窗口被拖到 120 Hz 屏上仍按 60
        // 请求（高刷屏上永远 60 fps），反之在 60 Hz 屏上请求 120（review mac-1）。
        let refresh = self.maximum_frames_per_second();
        if (refresh - self.seen_frame_rate).abs() > 0.5 {
            self.seen_frame_rate = refresh;
            if let Some(target) = app.window_mut(self.id) {
                target.set_max_frame_rate(refresh);
            }
        }
        // 真实 backing 每轮算一次：Dpi / Resized 都要把它写进渲染目标（与事件顺序无关）。
        let backing = self.backing_size();
        let factor = self.window.backingScaleFactor();
        if (factor - self.seen_scale).abs() > f64::EPSILON {
            let scale = dpi::scale_from(factor)?;
            self.seen_scale = factor;
            self.scale = scale;
            if let Some(window) = app.window_mut(self.id) {
                window.set_scale(scale);
                // DPI 变化但 surface 恰好相同时也要刷新 `contentsScale`（`set_surface` 内部先写图层）。
                window.set_surface(backing);
            }
            events.push(Event::DpiChanged {
                dpi: dpi::dpi_from_scale(scale),
                scale,
            });
        }
        let size = self.view.frame().size;
        if (size.width - self.seen_size.0).abs() > 0.5
            || (size.height - self.seen_size.1).abs() > 0.5
        {
            self.seen_size = (size.width, size.height);
            let logical = Size::new(size.width as f32, size.height as f32);
            self.logical_size = logical;
            if let Some(window) = app.window_mut(self.id) {
                window.set_size(logical);
                // `set_size` 不再重算平台真值；这里写入 AppKit 的真实 backing。
                window.set_surface(backing);
            }
            events.push(Event::Resized { size: logical });
        }
        // B3：位置变化（拖动标题栏 / 系统摆放 / 跨屏 / 程序化 `setFrame`）。
        //
        // `windowDidMove:` 只负责尽早唤醒泵（回调不碰 App）；这里的快照比较才是权威判据：
        // 用 AppKit 坐标的内容区左上角做去重，拖动期间每轮泵最多产出一条 `Event::Moved`
        // （不要求每像素，但 ≥ 每个 run loop 一轮）。容差 0.5 point 与尺寸同步一致。
        let content = self.content_rect_in_screen();
        let appkit_moved = self.delegate.take_moved();
        if appkit_moved
            || (content.x() - self.seen_content.x()).abs() > 0.5
            || (content.max_y() - self.seen_content.max_y()).abs() > 0.5
        {
            self.seen_content = content;
            let screens = screen_areas(self.mtm);
            let position = placement::position_from_content_rect(content, &screens);
            events.push(Event::Moved { position });
        }
        // RB-07：模态窗口必须持有键盘焦点。owner 被原生拦截后用户点不到它，但 AppKit 仍可能
        // 通过窗口循环 / 系统激活把 owner 变成 key window；此时键盘事件进 owner，被引擎的
        // 模态输入过滤丢弃 ⇒ 模态窗口收不到键（本项最严重的问题）。每轮对账一次：owner 抢到
        // key 而模态不是 key ⇒ 在读取本轮焦点快照**之前**拿回来，应用因此看不到一次假的
        // 失焦（也不会触发失焦的 IME 清场）。判定在平台无关的
        // `platform::macos_owner::modal_takes_key`（Windows 可单测）。
        let owner_is_key = self.owner.as_ref().is_some_and(|owner| owner.isKeyWindow());
        if modal_takes_key(
            self.modal_owner_chrome.is_some(),
            owner_is_key,
            self.window.isKeyWindow(),
        ) {
            self.window.makeKeyWindow();
        }
        let key = self.window.isKeyWindow();
        if key != self.was_key {
            self.was_key = key;
            self.view.focus_changed(key);
            events.push(Event::FocusChanged { focused: key });
            if !key {
                // B9(mac)：失焦边沿留给泵做 IME 清场 —— 这里只置位（`sync_state` 可能
                // 在嵌套/无关回调里被调用，清场要碰 `&mut App` 的公共入口，放到同一轮泵
                // 的状态同步之后执行，见 [`Self::handle_focus_lost`]）。
                self.focus_lost = true;
            }
        }
        // R09-9：可见性从同一个快照里派生（不可见 = 最小化 / 应用隐藏 / 被完全遮挡），
        // 边沿处强制一次全量重绘（drawable 与 persist 的内容在暂停期间都不可信）。
        //
        // B5：这里**不再**反推「窗口已关闭」。旧实现用 `!isVisible && !minimized &&
        // !appHidden` 当作关闭事件，但最小化 / 应用隐藏 / 系统 tab 合并都会让 `isVisible`
        // 为 false，于是「窗口被遮挡/切到别的 Space」也可能被误判成关闭并投递
        // `Event::CloseRequested`。关闭的唯一权威来源是 delegate 的 `windowWillClose:`
        // （见 `platform::macos_close` 与泵的关闭段）。
        let minimized = self.window.isMiniaturized();
        let app_hidden = NSApplication::sharedApplication(self.mtm).isHidden();
        let occluded = !self
            .window
            .occlusionState()
            .contains(NSWindowOcclusionState::Visible);
        let visibility_changed = self.sync_visibility(app, !minimized && !app_hidden && !occluded);
        let changed = !events.is_empty() || visibility_changed;
        for event in events {
            app.dispatch(self.id, event);
        }
        Ok(changed)
    }

    /// 可见性边沿的处理：不可见时停帧（泵据此暂停 display link），恢复时请求一次全量重绘。
    ///
    /// 返回 `true` = 发生了边沿（调用方需要出帧）。
    fn sync_visibility(&mut self, app: &mut App, visible: bool) -> bool {
        if visible == self.visible {
            return false;
        }
        self.visible = visible;
        if !visible {
            // 进入不可见：泵不会再为这个窗口产帧，渲染器随之暂停 display link（R09-9）。
            return true;
        }
        // 恢复：暂停期间 drawable 池可能已重建、persist 内容也可能过期 ⇒ 全量重绘一帧。
        let backing = self.backing_size();
        if let Some(window) = app.window_mut(self.id) {
            window.set_surface(backing);
            window.request_repaint();
        }
        true
    }

    /// 交付一笔 a11y 更新（`a11y_pending` 为 O(1) 过滤；adapter 不活跃时闭包不执行）。
    pub(crate) fn update_a11y(&mut self, app: &mut App) {
        if !app.a11y_pending(self.id) {
            return;
        }
        let id = self.id;
        self.view
            .update_a11y(|| crate::a11y::adapter::build_update(app, id));
    }

    /// 同步 IME caret 区域到 `NSTextInputClient` 的 `firstRect`（每轮泵调用，独立于帧门禁）。
    ///
    /// 同一个来源顺带派生 **Edit 菜单项的可用性快照**（R09-1 收尾 / AR3 P2-11）：
    /// `Ok(Some)` = 有焦点文本节点且有 caret ⇒ 文本编辑态。可用性**不等于**编辑态——还要求
    /// 该命令的按键在引擎侧真有接收者（`App::edit_menu_availability`），因此「启用了却没动作」
    /// 不可能出现（应用没绑定 Cmd+C 时 Copy 项置灰，而不是可点却什么都不做）。
    ///
    /// 已知口径（AR3 P2-10）：编辑态由 **IME caret hint** 派生，即「应用是否声明过文本插入点」，
    /// 而不是「焦点节点是否可编辑」。引擎没有内建文本控件（编辑语义归应用），只有应用自己知道
    /// 插入点在哪，因此当前这是唯一能跨应用工作的判据；它现在只会与「应用已绑定该命令」合取，
    /// 所以误差方向是**偏保守**（宁可置灰）。
    pub(crate) fn sync_ime_cursor_area(&mut self, app: &App) {
        let area = match app.ime_cursor_area(self.id) {
            Ok(area) => area,
            // 文本尚未 shape / 文本刚变化：保留上一次有效区域（编辑态也不变）。
            Err(_) => return,
        };
        self.view
            .set_edit_availability(app.edit_menu_availability(area.is_some()));
        if self.ime_area == area {
            return;
        }
        self.ime_area = area;
        self.view.set_ime_cursor_area(area);
    }

    /// 同步 IME 启用状态到 `GuiView.inputContext`（每轮泵调用；目标状态来自
    /// `App::ime_enabled`，B6）。
    ///
    /// 与 Windows 的 `NativeWindow::sync_ime_enabled` 同构：每轮对账「目标状态 vs 已应用
    /// 状态」，只在变化时执行平台动作。禁用 ⇒ AppKit 丢弃转换会话 + 清引擎组合态（有组合
    /// 时补 `ImeEvent::End`，经 `App::dispatch` 立刻交付）+ 清 App 侧 caret hint；启用 ⇒
    /// 下一次按键自然取回 super 的上下文，并清 `ime_area` 缓存让 hint 重发（恢复的上下文
    /// 不保证还留着旧的候选窗/组合窗表单）。
    ///
    /// 清 caret hint 走 `App::clear_ime_cursor_area`（公共入口，会 `dispatcher.wake()`）。
    /// 启停是**边沿事件**（不是每帧动作），因此这里多出的一轮空泵可忽略；不在
    /// `sync_state` 里做是为了不破坏「`sync_state` 不唤醒泵」的纪律（R09-15②）。
    pub(crate) fn sync_ime_enabled(&mut self, app: &mut App) {
        let enabled = app.ime_enabled(self.id);
        if enabled == self.ime_enabled {
            return;
        }
        self.ime_enabled = enabled;
        if !enabled {
            app.clear_ime_cursor_area(self.id);
        }
        self.ime_area = None;
        if self.view.apply_ime_enabled(enabled) {
            app.dispatch(self.id, Event::Ime(ImeEvent::End));
        }
    }

    /// 取走「窗口刚失去键盘焦点」边沿（`sync_state` 置位；泵唯一读点）。
    pub(crate) fn take_focus_lost(&mut self) -> bool {
        std::mem::take(&mut self.focus_lost)
    }

    /// 窗口失焦的 IME 清场（B9 mac 部分；泵每轮在 `sync_state` 之后调用）。
    ///
    /// 用户裁决的统一清场路径：**清 caret hint → 重置组合状态 → 补 `End`**。AppKit 在窗口
    /// resign key 时通常自己会取消转换会话（走 `unmarkText:`，那条回调已经交付过 `End`），
    /// 这里兜底「没有取消」的输入法；两种情况都做到 `End` 恰好一条。
    ///
    /// 不引入嵌套 runloop：全部在泵里同步完成（view 回调纪律不变 —— 回调只置位/唤醒）。
    pub(crate) fn handle_focus_lost(&mut self, app: &mut App) {
        app.clear_ime_cursor_area(self.id);
        self.ime_area = None;
        if self.view.reset_ime_composition() {
            app.dispatch(self.id, Event::Ime(ImeEvent::End));
        }
    }

    /// 引擎请求的光标形状（R09-11；`WindowId` 的定向投递在泵里完成）。
    pub(crate) fn set_cursor(&self, cursor: Cursor) {
        self.view.set_cursor(cursor);
    }

    /// 同步自绘标题栏拖动区到 `GuiView`（X2；泵每轮对账，值变化才写 view）。
    ///
    /// 区域是逻辑窗口坐标（客户区左上原点、y 向下），与指针事件同一坐标系；`mouseDown:`
    /// 命中区域时把拖动交给 WindowServer（`performWindowDragWithEvent:`），不进入引擎指针
    /// 分发。应用在布局/尺寸变化后重新 `App::set_drag_regions`，泵下一轮对账（Chrome
    /// `SetDraggableRegions` 同模型）。
    pub(crate) fn sync_drag_regions(&self, regions: &[Rect]) {
        self.view.set_drag_regions(regions);
    }

    /// 关闭窗口（幂等；引擎主动关闭路径）。
    ///
    /// 先断 delegate 再 `NSWindow.close`：`close` 不会询问 `windowShouldClose:`，断掉 delegate
    /// 还避免 AppKit 在关闭过程中回调进泵状态（我们仍持 delegate 强引用）。断 delegate 的
    /// 直接后果是**不会**收到本次关闭的 `windowWillClose:` —— 这是刻意的：那次通知属于
    /// 「外部关闭」路径（见 [`Self::mark_externally_closed`]），引擎主动关闭不需要再自证一次。
    pub(crate) fn close(&mut self) {
        if self.destroyed {
            return;
        }
        // R09-7：a11y adapter 必须在**窗口还活着**时卸掉并析构 ——
        // `accesskit_macos::Context::drop` 会对每个元素发 `NSAccessibilityPostNotification(
        // UIElementDestroyed)`，AppKit 同步回查 parent/frame；窗口已释放时 accesskit 内部的
        // `view.window().unwrap()` 会 panic（穿过 ObjC 帧 = abort），且 `accessibilityChildren`
        // 会 `borrow_mut` 正在 `drop_in_place` 的 ivars（UB）。
        self.view.uninstall_a11y();
        // RB-07：模态窗口必须先恢复被拦截的 owner（chrome + 键盘焦点），再解除 owner 关系 ——
        // 恢复要用 `self.owner` 的强引用；App 关闭 / 级联关闭 / Drop 最终都走到这里。
        self.restore_modal_owner();
        // X1：先解除 owner 关系（parent 可能仍存活），再关窗。
        self.detach_owner();
        // delegate 是 weak：先断掉，避免关闭期间的 AppKit 回调打进来（我们仍持强引用）。
        self.window.setDelegate(None);
        self.window.close();
        self.destroyed = true;
    }

    /// 泵每轮的关闭决议（B5；唯一读点，纯状态机在 `platform::macos_close`）。
    ///
    /// `verdict` 只在用户手势路径被调用一次，且**先**由调用方 dispatch
    /// `Event::CloseRequested`、再返回 `App::request_close` 的结果（事件先于处理器，与
    /// Windows 同序）。外部关闭路径（`windowWillClose:`）不调用它。
    pub(crate) fn decide_close(&self, verdict: impl FnOnce() -> bool) -> CloseAction {
        self.close.decide(verdict)
    }

    /// 外部关闭（`windowWillClose:`）后的收尾：窗口已由 AppKit 关闭，引擎只做记账。
    ///
    /// 与 [`Self::close`] 的区别：后者是引擎主动关窗（自己 `NSWindow.close()`）；这里窗口
    /// **已经关了**，不能再关第二次，只需卸 a11y、断 delegate、标记已销毁，让泵把原生窗口
    /// 从列表移除。外部关闭**不可否决**（AppKit 已经执行完毕），因此这里不碰 `App` 的关闭
    /// 处理器 —— 泵只调 `App::close_window` 做引擎侧记账（见 `event_loop::pump`）。
    pub(crate) fn mark_externally_closed(&mut self) {
        if self.destroyed {
            return;
        }
        self.view.uninstall_a11y();
        // RB-07：外部关闭同样要恢复被本模态窗口拦截的 owner（chrome + 键盘焦点），
        // 再摘掉 owner 关系 —— 否则 owner 永久不可交互（红点灰显 + 收不到鼠标）。
        self.restore_modal_owner();
        // X1：外部关闭同样要摘掉 owner 关系，避免 parent 的 `childWindows` 留着已关闭对象。
        self.detach_owner();
        self.window.setDelegate(None);
        self.destroyed = true;
    }

    /// 窗口是否已被 `close()` 销毁（泵据此从列表移除）。
    pub(crate) fn is_destroyed(&self) -> bool {
        self.destroyed
    }

    /// 动画定时器的开关（`pump` 每轮调用；空闲时移除定时器 ⇒ 不轮询）。
    ///
    /// 间隔按**屏幕实际刷新率**取（P1-2.12）：写死 `1/60` 在 120 Hz 屏上会让动画被系统
    /// 的定时器节流成 60 Hz，而在 50 Hz 屏上又高于刷新率、白跑一轮。
    pub(crate) fn set_animating(&self, active: bool) {
        let refresh = self.maximum_frames_per_second().max(1.0);
        wake::set_animating(active, f64::from(1.0 / refresh));
    }
}

impl Drop for NativeWindow {
    fn drop(&mut self) {
        self.close();
    }
}

//! 原生窗口：`NSWindow` + 自绘标题栏 + 交通灯。
//!
//! 状态同步（尺寸 / 缩放 / 焦点 / 可见性 / 位置）走「每轮快照对比」而不是 delegate 或通知：
//! 泵每轮都要读一次这些值（O(1)）。delegate 回调（`windowDidMove:` 等）只**置位 + 唤醒**，
//! 平台真值仍由 `sync_state` 快照读取，避免两个真值来源。**例外是关闭**：`windowShouldClose:`
//! 是 AppKit 在关窗前唯一的同步钩子，没有它就无法拦截用户点红点 / Cmd-W（窗口已被 AppKit
//! 关掉，泵事后才观察到，veto 也就失去意义）。见 [`WindowDelegate`]。
//!
//! 可见性（R09-9）三个来源：最小化（`isMiniaturized`）、应用隐藏（`NSApp.hide`，Cmd+H）、
//! **被完全遮挡/在别的 Space**（`occlusionState` 不含 `Visible`）。三者任一为真，帧就不再产出、
//! display link 暂停（见 `event_loop::pump`）——交易终端最小化几小时是常态，120 Hz 唤醒是纯浪费。
//!
//! 摆放（B3）：`WindowOptions.position` / `min_size` / `max_size` 在 [`NativeWindow::create`]
//! 消费；坐标换算 / 工作区 clamp 的纯逻辑在 `platform::macos_placement`（Windows 可单测）。
//! 位置变化经 `windowDidMove:` 唤醒 + `sync_state` 快照比较，产出 `Event::Moved`。
//!
//! 可见性（R09-9）三个来源：最小化（`isMiniaturized`）、应用隐藏（`NSApp.hide`，Cmd+H）、
//! **被完全遮挡/在别的 Space**（`occlusionState` 不含 `Visible`）。三者任一为真，帧就不再产出、
//! display link 暂停（见 `event_loop::pump`）——交易终端最小化几小时是常态，120 Hz 唤醒是纯浪费。

use std::{cell::Cell, sync::Arc};

use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
    runtime::ProtocolObject,
};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSScreen, NSWindow,
    NSWindowCollectionBehavior, NSWindowDelegate, NSWindowOcclusionState, NSWindowStyleMask,
    NSWindowTitleVisibility,
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
        macos::{a11y::MacA11y, dpi, view::GuiView, wake},
        macos_placement as placement,
    },
    window::{WindowId, WindowOptions},
};

/// `NSWindowDelegate` 的 ivar：只记「AppKit 请求关窗」这一个事实并唤醒泵。
///
/// 回调里**不碰 `App`**（与 `GuiView` 同一纪律）：置位 / 唤醒，由泵在主线程决定
/// `App::request_close` 的裁决。`windowShouldClose:` 恒返回 `false` —— 真正的关闭由泵在
/// 应用同意之后调 `NSWindow::close`（`close` 不再询问 delegate，因此不会递归）。
///
/// `moved`（B3）同理：`windowDidMove:` 只置位 + `dispatcher.wake()`，位置由泵的
/// `sync_state` 从 `NSView` 快照算出（回调里拿不到 `&mut App`，也不该在回调里换算）。
struct WindowIvars {
    requested: Cell<bool>,
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
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            self.ivars().requested.set(true);
            self.ivars().dispatcher.wake();
            // 恒拒绝 AppKit 的默认关闭：由泵在 `App::request_close` 同意后再 `close()`。
            false
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
    fn new(mtm: MainThreadMarker, dispatcher: Arc<Dispatcher>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(WindowIvars {
            requested: Cell::new(false),
            moved: Cell::new(false),
            dispatcher,
        });
        // SAFETY: `init` 是 NSObject 的指定初始化器；`this` 由 `alloc` 产生且未初始化。
        unsafe { msg_send![super(this), init] }
    }

    /// 取走「AppKit 请求关窗」标记。
    fn take_request(&self) -> bool {
        self.ivars().requested.replace(false)
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
        areas.push(placement::ScreenArea {
            frame: ns_area(screen.frame()),
            visible: ns_area(screen.visibleFrame()),
        });
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
        .unwrap_or(screens[0].visible);
    let position = placement::cascade_position(desktop, main_visible, index);
    placement::content_origin_for_position(position, size, screens)
}

/// 关窗请求的来源。
///
/// `Observed` 是「窗口已不可见」的事后观察：`sync_state` 已经 dispatch 过事件，
/// 泵不能再补发一次（复审 F3 的双投递缺陷）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseRequestSource {
    /// AppKit 的关窗手势（红点 / Cmd-W / performClose:）。
    Gesture,
    /// 「窗口已不可见」的事后观察（销毁 / 应用退出）。
    Observed,
}

/// 一个原生窗口（`Application::run` 持有；`Drop` 时关闭）。
pub(crate) struct NativeWindow {
    mtm: MainThreadMarker,
    id: WindowId,
    window: Retained<NSWindow>,
    view: Retained<GuiView>,
    /// `delegate` 是 **weak** 属性：必须由门面持有强引用，否则回调会打在已释放对象上。
    delegate: Retained<WindowDelegate>,
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
    /// 「窗口已不可见」的事后观察是否已经报过一次（同步 dispatch 一次给观察者）。
    close_requested: bool,
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
    pub(crate) fn create(
        mtm: MainThreadMarker,
        id: WindowId,
        options: &WindowOptions,
        dispatcher: &Arc<Dispatcher>,
        cascade_index: usize,
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
        // 显式声明该窗口是原生全屏的 primary 窗口；默认 collectionBehavior=0 时
        // AppKit 的 fullscreen 转场会停在过渡快照上（实测：窗口/view/layer 都已
        // 1680×1050，但屏幕仍显示旧的 960×740 快照）。
        window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);

        let view = GuiView::new(mtm);
        window.setContentView(Some(&view));
        // 原生全屏转场由 AppKit 直接改 window frame；contentView 必须随宽度/高度伸缩，
        // 否则引擎看到的一直是 960×740，layer/drawable 也不会覆盖全屏窗口。
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        view.setWantsLayer(true);
        window.makeFirstResponder(Some(&view));

        // 原生交通灯全部保留：关闭 / 最小化 / 缩放。标题栏透明且标题隐藏，内容铺满窗口，
        // 因此窗口控件浮在内容上方（应用用顶部安全区避让）。
        let a11y = MacA11y::new(&view, dispatcher.clone(), id)?;
        view.install_a11y(a11y);

        // 关闭拦截：AppKit 的关窗手势（红点 / Cmd-W / performClose:）走 delegate，
        // windowShouldClose: 恒返回 false 并置位，由泵问过 App::request_close 后才真正 close。
        let delegate = WindowDelegate::new(mtm, Arc::clone(dispatcher));
        // SAFETY: ProtocolObject::from_ref 要求该对象实现了目标协议 —— 上面刚 impl 过。
        let protocol: &ProtocolObject<dyn NSWindowDelegate> = ProtocolObject::from_ref(&*delegate);
        window.setDelegate(Some(protocol));
        let mut native = Self {
            mtm,
            id,
            window,
            view,
            delegate,
            scale: options.scale,
            logical_size: options.size,
            seen_scale: 1.0,
            seen_frame_rate: 0.0,
            seen_size: (0.0, 0.0),
            was_key: false,
            visible: true,
            close_requested: false,
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
    /// 只在这里把 layer 交给 AppKit；`contentsScale` / `drawableSize` 的唯一写者是**主线程**的
    /// `Renderer::set_layer_scale`（§5.3），GPU 线程只读 drawable 的实际尺寸。
    pub(crate) fn attach_layer(&self, layer: Retained<CAMetalLayer>) {
        self.view.setLayer(Some(&layer));
    }

    pub(crate) fn show(&self) {
        self.window.makeKeyAndOrderFront(None);
        self.window.makeFirstResponder(Some(&self.view));
    }

    pub(crate) fn id(&self) -> WindowId {
        self.id
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
            || (content.x - self.seen_content.x).abs() > 0.5
            || (content.max_y() - self.seen_content.max_y()).abs() > 0.5
        {
            self.seen_content = content;
            let screens = screen_areas(self.mtm);
            let position = placement::position_from_content_rect(content, &screens);
            events.push(Event::Moved { position });
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
        // 关闭判定只看「窗口真的不在了」：最小化（`isMiniaturized`）与 `NSApp.hide()`
        // 都会让 `isVisible` 变 false，但窗口并没有被关闭。
        let minimized = self.window.isMiniaturized();
        let app_hidden = NSApplication::sharedApplication(self.mtm).isHidden();
        let closed = !self.window.isVisible() && !minimized && !app_hidden;
        if !closed {
            self.close_requested = false;
        } else if !self.destroyed && !self.close_requested {
            self.close_requested = true;
            events.push(Event::CloseRequested);
        }
        // R09-9：可见性从同一个快照里派生（不可见 = 最小化 / 应用隐藏 / 被完全遮挡），
        // 边沿处强制一次全量重绘（drawable 与 persist 的内容在暂停期间都不可信）。
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

    /// 关闭窗口（幂等）。
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
        // delegate 是 weak：先断掉，避免关闭期间的 AppKit 回调打进来（我们仍持强引用）。
        self.window.setDelegate(None);
        self.window.close();
        self.destroyed = true;
    }

    /// 取走「关闭已请求」标志（泵唯一读点）。
    pub(crate) fn take_close_request(&mut self) -> Option<CloseRequestSource> {
        // 两个来源：AppKit 的关窗手势（delegate 置位）与「窗口已不可见」的事后观察
        // （销毁 / 应用退出等非手势路径）。调用方据此决定要不要补发 `CloseRequested`
        // —— 观察路径已经在 `sync_state` 里发过了，再发就是双投递（复审 F3）。
        if self.delegate.take_request() {
            return Some(CloseRequestSource::Gesture);
        }
        std::mem::take(&mut self.close_requested).then_some(CloseRequestSource::Observed)
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

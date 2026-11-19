//! 原生窗口：`NSWindow` + 自绘标题栏 + 交通灯。
//!
//! 状态同步（尺寸 / 缩放 / 焦点 / 可见性）走「每轮快照对比」而不是 delegate 或通知：泵每轮都要
//! 读一次这些值（O(1)）。**例外是关闭**：`windowShouldClose:` 是 AppKit 在关窗前唯一的同步
//! 钩子，没有它就无法拦截用户点红点 / Cmd-W（窗口已被 AppKit 关掉，泵事后才观察到，veto 也就
//! 失去意义）。见 [`WindowDelegate`]。
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
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSWindow,
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
    event::Event,
    math::{Rect, Scale, Size},
    paint::style::Cursor,
    platform::{
        dispatcher::Dispatcher,
        macos::{a11y::MacA11y, dpi, view::GuiView, wake},
    },
    window::{WindowId, WindowOptions},
};

/// `NSWindowDelegate` 的 ivar：只记「AppKit 请求关窗」这一个事实并唤醒泵。
///
/// 回调里**不碰 `App`**（与 `GuiView` 同一纪律）：置位 / 唤醒，由泵在主线程决定
/// `App::request_close` 的裁决。`windowShouldClose:` 恒返回 `false` —— 真正的关闭由泵在
/// 应用同意之后调 `NSWindow::close`（`close` 不再询问 delegate，因此不会递归）。
struct WindowIvars {
    requested: Cell<bool>,
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
    }
);

impl WindowDelegate {
    fn new(mtm: MainThreadMarker, dispatcher: Arc<Dispatcher>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(WindowIvars {
            requested: Cell::new(false),
            dispatcher,
        });
        // SAFETY: `init` 是 NSObject 的指定初始化器；`this` 由 `alloc` 产生且未初始化。
        unsafe { msg_send![super(this), init] }
    }

    /// 取走「AppKit 请求关窗」标记。
    fn take_request(&self) -> bool {
        self.ivars().requested.replace(false)
    }
}

/// 最小客户区（逻辑像素；与 Win32 / Wayland 同源）。
pub(crate) const MIN_SIZE_LOGICAL: (f32, f32) = (320.0, 200.0);

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
}

impl NativeWindow {
    /// 建**隐藏**窗口（不 `makeKeyAndOrderFront`）并装好 view + a11y adapter。
    pub(crate) fn create(
        mtm: MainThreadMarker,
        id: WindowId,
        options: &WindowOptions,
        dispatcher: &Arc<Dispatcher>,
    ) -> Result<Self, EngineError> {
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;
        let rect = NSRect::new(
            NSPoint::new(80.0, 80.0),
            NSSize::new(
                f64::from(options.size.width.max(1.0)),
                f64::from(options.size.height.max(1.0)),
            ),
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
        window.setContentMinSize(NSSize::new(
            f64::from(MIN_SIZE_LOGICAL.0),
            f64::from(MIN_SIZE_LOGICAL.1),
        ));
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
        };
        native.sync_initial(mtm)?;
        Ok(native)
    }

    /// 初次同步：读 `backingScaleFactor` / 内容尺寸，写回字段（**不**产生事件）。
    fn sync_initial(&mut self, _mtm: MainThreadMarker) -> Result<(), EngineError> {
        let factor = self.window.backingScaleFactor();
        let scale = dpi::scale_from(factor)?;
        self.scale = scale;
        self.seen_scale = factor;
        let size = self.view.frame().size;
        self.seen_size = (size.width, size.height);
        self.logical_size = Size::new(size.width as f32, size.height as f32);
        Ok(())
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
        let key = self.window.isKeyWindow();
        if key != self.was_key {
            self.was_key = key;
            self.view.focus_changed(key);
            events.push(Event::FocusChanged { focused: key });
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

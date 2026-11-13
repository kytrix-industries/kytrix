//! macOS 主事件循环（`NSApplication::run()` + `CFRunLoopObserver` / `CFRunLoopSource` 泵）
//! （`design/phase-12-macos-linux.md` §7.1；macOS 验证修正见 `macos-verification.md` §2.8）。
//!
//! 结构：
//! - **事件循环交给 AppKit**：`NSApplication::run()` 处理 NSEvent、原生全屏转场、live resize、
//!   菜单与 Space 切换；引擎不再手写 `nextEventMatchingMask`（旧实现无法完成 fullscreen 转场）。
//! - **泵**由三条 common-modes 路径驱动：`CFRunLoopObserver`（BeforeWaiting/AfterWaiting，处理
//!   AppKit 事件后的状态同步）、wake `CFRunLoopSource`（跨线程唤醒）、连续帧 `CFRunLoopTimer`
//!   （动画/HUD）。三者都只在主线程执行。
//! - 退出：泵发现 `exit` 后撤掉 source/timer，投一个空 NSEvent，再 `NSApplication::stop(None)`
//!   + `CFRunLoop::stop`/`wake_up`；`run()` 返回（F-165）。
//!
//! `App` 以 `Box` 存放、地址稳定；C 回调只在主线程、且主线程在 run loop 中时执行
//! （此时没有其它 `&mut App` 存活），因此用裸指针穿透 ObjC 回调是安全的。

use std::{ffi::c_void, sync::Arc};

use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventModifierFlags, NSEventType,
};
use objc2_core_foundation::{
    CFRunLoop, CFRunLoopActivity, CFRunLoopObserver, CFRunLoopObserverContext, CFRunLoopTimer,
};
use objc2_foundation::NSPoint;

use crate::{
    app::App,
    error::EngineError,
    platform::{
        dispatcher::Dispatcher,
        macos::{wake, window::NativeWindow},
    },
    render::backend::Renderer,
    window::{WindowId, WindowOptions},
};

/// 泵的共享状态（堆上、地址稳定；C 回调通过 `info` 拿到它）。
struct LoopState {
    /// `Box<App>` 的裸指针（`Drop` 时还原）。
    app: *mut App,
    windows: Vec<NativeWindow>,
    dispatcher: Arc<Dispatcher>,
    /// `NSApp.run()` 需要一个稳定的 Retained；退出时 `stop(None)`。
    ns_app: Retained<NSApplication>,
    exit: bool,
    error: Option<EngineError>,
}

impl Drop for LoopState {
    fn drop(&mut self) {
        // SAFETY: 指针由 `run` 里的 `Box::into_raw(Box::new(app))` 产生，只在这里还原一次。
        unsafe { drop(Box::from_raw(self.app)) };
    }
}

/// `Application::run` 的平台入口。
pub(crate) fn run(app: App) -> Result<(), EngineError> {
    let mtm = MainThreadMarker::new().ok_or(EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("NSApplication requires the main thread"),
    })?;
    let ns_app = NSApplication::sharedApplication(mtm);
    ns_app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    ns_app.finishLaunching();
    ns_app.activate();

    let mut app = app;
    let dispatcher = app.dispatcher();
    // 唤醒句柄由本平台的 `wake::install` 安装；`attach` 的 token 只对 Win32 有意义。
    dispatcher.attach(1);
    let requests: Vec<(WindowId, WindowOptions)> = app
        .windows()
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    let mut windows: Vec<NativeWindow> = Vec::with_capacity(requests.len());
    for (id, options) in requests {
        let native = NativeWindow::create(mtm, id, &options, &dispatcher)?;
        let scale = native.scale();
        let size = native.physical_size();
        let renderer = Renderer::new_for_layer(size, f64::from(scale.factor()))?;
        {
            let layer = renderer
                .layer_handle()
                .ok_or_else(|| EngineError::AppInit {
                    detail: smol_str::SmolStr::new_static("metal renderer has no CAMetalLayer"),
                })?;
            native.attach_layer(layer);
        }
        if let Some(window) = app.window_mut(id) {
            window.set_renderer(renderer);
            window.set_scale(scale);
            window.set_size(native.logical_size());
        }
        native.show();
        windows.push(native);
    }

    let state = Box::into_raw(Box::new(LoopState {
        app: Box::into_raw(Box::new(app)),
        windows,
        dispatcher: dispatcher.clone(),
        ns_app: ns_app.clone(),
        exit: false,
        error: None,
    }));
    if !wake::install(state as *mut c_void, pump_callback, timer_callback) {
        // SAFETY: `state` 刚由 `Box::into_raw` 产生，尚未交给任何回调。
        unsafe { drop(Box::from_raw(state)) };
        return Err(EngineError::AppInit {
            detail: smol_str::SmolStr::new_static("CFRunLoop source installation failed"),
        });
    }
    let run_loop = CFRunLoop::main().ok_or(EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("main CFRunLoop is unavailable"),
    })?;
    let observer = {
        let mut context = CFRunLoopObserverContext {
            version: 0,
            info: state as *mut c_void,
            retain: None,
            release: None,
            copyDescription: None,
        };
        // SAFETY: context 是栈上值，`CFRunLoopObserverCreate` 同步拷贝；info 在卸载前有效。
        unsafe {
            CFRunLoopObserver::new(
                None,
                (CFRunLoopActivity::BeforeWaiting | CFRunLoopActivity::AfterWaiting).0,
                true,
                0,
                Some(observer_callback),
                &mut context,
            )
        }
    };
    let Some(observer) = observer else {
        wake::uninstall();
        // SAFETY: state 尚未交给任何回调（observer 创建失败）。
        unsafe { drop(Box::from_raw(state)) };
        return Err(EngineError::AppInit {
            detail: smol_str::SmolStr::new_static("CFRunLoop observer installation failed"),
        });
    };
    run_loop.add_observer(Some(&observer), wake::common_mode());

    // 首帧：显示之后请求一帧；source 已挂 common modes，会唤醒 NSApp.run。
    dispatcher.wake();
    ns_app.run();

    run_loop.remove_observer(Some(&observer), wake::common_mode());
    // 卸载唤醒源与定时器（之后 `Dispatcher::wake` 走「尚未安装」分支，不会再进回调）。
    wake::uninstall();
    // SAFETY: `state` 由本函数独占，且回调已经不可能再被触发；`Drop` 会还原并释放 `App`。
    let mut state = unsafe { Box::from_raw(state) };
    match state.error.take() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// 泵一次；`tick` = 本次由连续帧定时器触发（而不是 source/observer）。
fn pump_and_stop(state: &mut LoopState, tick: bool) {
    if state.exit {
        return;
    }
    if let Err(error) = pump(state, tick) {
        state.error = Some(error);
        state.exit = true;
    }
    if state.exit {
        // 先撤掉定时器/唤醒源：HUD 开着时 60Hz timer 仍在唤醒 run loop，
        // `NSApp.stop` 会等不到 NSEvent 而永远不返回（实测关窗后进程不退出）。
        wake::uninstall();
        // `NSApplication::run` 睡在 `nextEventMatchingMask`；投一个 application-defined
        // 空事件，让 stop 标志在下一次取事件时生效。CFRunLoopWakeUp 只唤醒 run loop，
        // 不产生 NSEvent，单独用它无法让 `run` 返回。
        if let Some(event) = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags::empty(),
            0.0,
            0,
            None,
            0,
            0,
            0,
        ) {
            state.ns_app.postEvent_atStart(&event, true);
        }
        state.ns_app.stop(None);
        if let Some(run_loop) = CFRunLoop::current() {
            run_loop.stop();
            run_loop.wake_up();
        }
    }
}

/// 泵：事件 → App，然后提交本帧 / 回投任务 / a11y / 关闭。
///
/// `tick` 只由 60Hz 定时器置位：连续帧不能反过来自己 `dispatcher.wake()`，否则 source
/// 会在同一轮 run loop 里反复唤醒，主线程变成忙循环（render 线程异步后实测 830fps）。
///
/// # Errors
/// 帧提交失败（device lost / swapchain）会立刻终止循环。
fn pump(state: &mut LoopState, tick: bool) -> Result<(), EngineError> {
    // SAFETY: `app` 指针由 `run` 保证有效，且此刻没有其它 `&mut App`。
    let app = unsafe { &mut *state.app };
    app.sync_clock();
    // 空闲时也要收 render 线程的反馈：设备丢失/提交失败不能等到下一次输入才冒泡。
    app.poll_renderers()?;

    for native in state.windows.iter_mut() {
        let id = native.id();
        let mut delivered = false;
        for event in native.take_events() {
            app.dispatch(id, event);
            delivered = true;
        }
        native.sync_state(app, &state.dispatcher)?;
        if delivered {
            // 与 Win32 的 pump 一致：输入事件之后必须唤醒，否则应用侧状态变更要等下一个
            // NSEvent 才出帧（macOS 没有「每消息 wake」的默认行为）。
            state.dispatcher.wake();
        }
    }

    app.run_frame_hook();
    // 连续帧的节奏由 60Hz 定时器提供；`tick` 为真时直接出帧，不再写回
    // `Dispatcher::wake`。source/observer 路径只响应显式 wake（输入、任务、应用请求）。
    let wake = state.dispatcher.take_frame_wake();
    if wake || (tick && app.needs_continuous_frames()) {
        frame_all(app)?;
    }

    // 交互反馈：光标（v1 只支持系统箭头）；macOS 的指针捕获是隐式的，请求直接丢弃。
    if let Some(native) = state.windows.first()
        && app.take_cursor_change().is_some()
    {
        native.set_cursor();
    }
    let _ = app.take_capture_request();

    for native in state.windows.iter_mut() {
        native.update_a11y(app);
        native.sync_ime_cursor_area(app);
    }
    for task in state.dispatcher.drain_tasks() {
        task.run(app);
    }

    // 连续帧定时器：有动画或 HUD 可见才挂（其余时间空闲零唤醒）。
    if let Some(native) = state.windows.first() {
        native.set_animating(app.needs_continuous_frames());
    }

    // 关闭：请求关闭的窗口真正关闭；一个不剩就退出。
    let mut closed = false;
    for native in state.windows.iter_mut() {
        if native.take_close_request() {
            app.close_window(native.id());
            native.close();
            closed = true;
        }
    }
    if closed {
        state.windows.retain(|native| !native.is_destroyed());
    }
    if state.windows.is_empty() || app.windows().next().is_none() {
        state.exit = true;
    }
    Ok(())
}

/// 逐窗口提交一帧；`Err` 立刻终止（device lost / swapchain 失败都不可重试）。
fn frame_all(app: &mut App) -> Result<(), EngineError> {
    let ids: Vec<WindowId> = app.windows().map(|(id, _)| id).collect();
    for id in ids {
        match app.frame(id) {
            None | Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error),
        }
    }
    Ok(())
}

/// 唤醒源的 `perform` 回调：主线程、run loop 上执行泵。
///
/// # Safety
///
/// 只由 CoreFoundation 调用：`info` 必须是 `run` 安装时传入的 `LoopState` 指针，且调用发生在
/// 主线程、主线程正阻塞在 run loop 里（此时没有其它 `&mut App` 存活）。
unsafe extern "C-unwind" fn pump_callback(info: *mut c_void) {
    if info.is_null() {
        return;
    }
    // SAFETY: `info` 是 `run` 里的 `LoopState` 指针；回调只在主线程执行（见模块注释）。
    let state = unsafe { &mut *(info as *mut LoopState) };
    pump_and_stop(state, false);
}

/// run loop 观察者：AppKit 处理完一轮事件后同步窗口状态/提交帧（含全屏/live-resize 转场）。
///
/// # Safety
///
/// 同 [`pump_callback`]：`info` 是 `run` 安装时传入的 `LoopState` 指针，调用只在主线程发生。
unsafe extern "C-unwind" fn observer_callback(
    _observer: *mut CFRunLoopObserver,
    _activity: CFRunLoopActivity,
    info: *mut c_void,
) {
    if info.is_null() {
        return;
    }
    // SAFETY: 同 `pump_callback`。
    let state = unsafe { &mut *(info as *mut LoopState) };
    pump_and_stop(state, false);
}

/// 动画定时器回调：与 `pump_callback` 同一份泵。
///
/// # Safety
///
/// 同 [`pump_callback`]：`info` 是 `run` 安装的 `LoopState` 指针，调用只在主线程发生。
unsafe extern "C-unwind" fn timer_callback(_timer: *mut CFRunLoopTimer, info: *mut c_void) {
    if info.is_null() {
        return;
    }
    // SAFETY: 同 `pump_callback`。
    let state = unsafe { &mut *(info as *mut LoopState) };
    pump_and_stop(state, true);
}

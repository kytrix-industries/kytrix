//! 引擎自有事件循环（`NSApplication` + `CFRunLoopSource` 唤醒 + 动画定时器）
//! （`design/phase-12-macos-linux.md` §7.1）。
//!
//! 结构：
//! - 主线程：`nextEventMatchingMask:` 排空事件 → `sendEvent:`（AppKit 同步回调 `GuiView`）→ 泵；
//!   空闲时用 `distantFuture` 阻塞，**唤醒源 / 动画定时器**在 run loop 上直接执行泵（不轮询，I3）。
//! - 跨线程：`Dispatcher::wake` → `CFRunLoopSourceSignal` + `CFRunLoopWakeUp`。
//!
//! `App` 以 `Box` 存放、地址稳定；两个 C 回调只在主线程、且主线程阻塞在 run loop 时执行
//! （此时没有其它 `&mut App` 存活），因此用裸指针穿透 ObjC 回调是安全的。

use std::{ffi::c_void, sync::Arc};

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask};
use objc2_core_foundation::{CFRunLoop, CFRunLoopTimer};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode};

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

/// 泵的共享状态（堆上、地址稳定；两个 C 回调通过 `info` 拿到它）。
struct LoopState {
    /// `Box<App>` 的裸指针（`Drop` 时还原）。
    app: *mut App,
    windows: Vec<NativeWindow>,
    dispatcher: Arc<Dispatcher>,
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
        let mut native = NativeWindow::create(mtm, id, &options, &dispatcher)?;
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

    // 首帧：显示之后请求一帧。
    dispatcher.wake();

    loop {
        // ① 排空 NSEvents（非阻塞）：`sendEvent` 同步回调 `GuiView`，事件进 view 的队列。
        drain_events(&ns_app);
        // SAFETY: `state` 由本函数独占（回调只在主线程阻塞期间执行，见模块注释）。
        let result = pump(unsafe { &mut *state });
        if let Err(error) = result {
            // SAFETY: 同上；此时没有任何仍在使用的 `&mut LoopState`。
            let state = unsafe { &mut *state };
            state.error = Some(error);
            state.exit = true;
        }
        // SAFETY: 只读访问，且上一句的 `&mut` 已结束（NLL）。
        if unsafe { &*state }.exit {
            break;
        }
        // ② 阻塞等待：NSEvent 到达；唤醒源 / 动画定时器在 run loop 上执行泵。
        if let Some(event) = ns_app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&NSDate::distantFuture()),
            // SAFETY: `NSDefaultRunLoopMode` 是 Foundation 的静态常量（进程生命周期内有效）。
            unsafe { NSDefaultRunLoopMode },
            true,
        ) {
            ns_app.sendEvent(&event);
        }
    }

    // 卸载唤醒源与定时器（之后 `Dispatcher::wake` 走「尚未安装」分支，不会再进回调）。
    wake::uninstall();
    // SAFETY: `state` 由本函数独占，且回调已经不可能再被触发；`Drop` 会还原并释放 `App`。
    let mut state = unsafe { Box::from_raw(state) };
    match state.error.take() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn drain_events(ns_app: &NSApplication) {
    while let Some(event) = ns_app.nextEventMatchingMask_untilDate_inMode_dequeue(
        NSEventMask::Any,
        Some(&NSDate::distantPast()),
        // SAFETY: 同 `run` 里的静态常量。
        unsafe { NSDefaultRunLoopMode },
        true,
    ) {
        ns_app.sendEvent(&event);
    }
}

/// 泵：事件 → App，然后提交本帧 / 回投任务 / a11y / 关闭。
///
/// # Errors
/// 帧提交失败（device lost / swapchain）会立刻终止循环。
fn pump(state: &mut LoopState) -> Result<(), EngineError> {
    // SAFETY: `app` 指针由 `run` 保证有效，且此刻没有其它 `&mut App`。
    let app = unsafe { &mut *state.app };
    app.sync_clock();

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
    // 动画期间每个 tick（定时器回调 → 本函数）都要出帧：定时器只负责唤醒 run loop，
    // 帧请求仍走 `Dispatcher`（与 Win32 的 `WM_TIMER → wake` 同一语义）。
    if app.has_animations() {
        state.dispatcher.wake();
    }
    if state.dispatcher.take_frame_wake() {
        frame_all(app)?;
    }

    // 交互反馈：光标（v1 只支持系统箭头）；macOS 的指针捕获是隐式的，请求直接丢弃。
    if let Some(native) = state.windows.first() {
        if app.take_cursor_change().is_some() {
            native.set_cursor();
        }
    }
    let _ = app.take_capture_request();

    for native in state.windows.iter_mut() {
        native.update_a11y(app);
        native.sync_ime_cursor_area(app);
    }
    for task in state.dispatcher.drain_tasks() {
        task.run(app);
    }

    // 动画定时器：有动画才挂（空闲零唤醒）。
    if let Some(native) = state.windows.first() {
        native.set_animating(app.has_animations());
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
    if state.exit {
        return;
    }
    if let Err(error) = pump(state) {
        state.error = Some(error);
        state.exit = true;
    }
    // 不论成功还是失败，`exit` 都必须打断阻塞在 `nextEventMatchingMask` 的主循环
    // （关掉最后一个窗口时泵会把 `exit` 置位，但此刻回调已经返回）。
    if state.exit {
        if let Some(run_loop) = CFRunLoop::current() {
            run_loop.stop();
        }
    }
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
    if state.exit {
        return;
    }
    if let Err(error) = pump(state) {
        state.error = Some(error);
        state.exit = true;
    }
    // 动画 tick 上的退出同样要打断阻塞（与 `pump_callback` 一致）。
    if state.exit {
        if let Some(run_loop) = CFRunLoop::current() {
            run_loop.stop();
        }
    }
}

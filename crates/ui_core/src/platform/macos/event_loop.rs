//! macOS 主事件循环（`NSApplication::run()` + `CFRunLoopObserver` / `CFRunLoopSource` 泵）
//! （macOS 验证修正见 `macos-verification.md` §2.8）。
//!
//! 结构：
//! - **事件循环交给 AppKit**：`NSApplication::run()` 处理 NSEvent、原生全屏转场、live resize、
//!   菜单与 Space 切换；引擎不再手写 `nextEventMatchingMask`（旧实现无法完成 fullscreen 转场）。
//! - **应用壳层**（R09-1 / R09-16）：`run()` 在开窗前装好 `NSMenu` 主菜单与
//!   `NSApplicationDelegate`。菜单提供 Cmd+Q / Cmd+W / Cmd+H 的承载对象；Edit 菜单项
//!   （Cmd+C/V/X/A/Z）**只在应用真的绑定了对应按键时启用**（`App::edit_menu_availability`），
//!   否则置灰 —— 引擎没有内建文本控件，编辑语义由应用绑定拥有。
//!   delegate 把 `terminate:` 变成「通知引擎 → 引擎两阶段询问+关窗收尾 → 回复 AppKit」的握手。
//! - **泵**由三条 common-modes 路径驱动：`CFRunLoopObserver`（**只注册 `BeforeWaiting`**，
//!   每轮 run loop 一次）、wake `CFRunLoopSource`（跨线程唤醒）、连续帧 `CFRunLoopTimer`
//!   （只有动画；HUD 自 P-C 起是纯观察者、不再产生帧）。三者都只在主线程执行。
//! - 退出：泵发现 `exit` 后撤掉 source/timer，投一个空 NSEvent，再 `NSApplication::stop(None)`
//!   + `CFRunLoop::stop`/`wake_up`；`run()` 返回（F-165）。
//!
//! `App` 以 `Box` 存放、地址稳定；C 回调只在主线程、且主线程在 run loop 中时执行
//! （此时没有其它 `&mut App` 存活），因此用裸指针穿透 ObjC 回调是安全的。

use std::{ffi::c_void, sync::Arc, time::Duration};

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
    interaction::InputThresholds,
    platform::{
        dispatcher::Dispatcher,
        macos::{
            app_delegate::{AppDelegate, QuitHandshake},
            menu, wake,
            window::NativeWindow,
        },
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
    /// `NSApplication.delegate` 是 **weak** 属性：必须由门面持有强引用，否则回调会打在已释放
    /// 对象上（与 `NativeWindow` 的 window delegate 同一个坑）。
    _delegate: Retained<AppDelegate>,
    /// 退出握手（delegate 与泵共享）。
    quit: Arc<QuitHandshake>,
    /// 最近一次同步给引擎的系统双击间隔（R09-10：系统设置可以在运行中改变）。
    double_click: Duration,
    /// 退出握手正在跑（P1-2 的另一半防线）：关闭处理器里弹 `NSAlert` 之类会起**嵌套 run loop**，
    /// `BeforeWaiting` 观察者会在里面再泵一次；若那里再看到 `quit.pending()`，就会把同一批关闭
    /// 处理器问第二遍，并对 AppKit 回复两次 `replyToApplicationShouldTerminate:`（协议只允许一次）。
    quitting: bool,
    exit: bool,
    error: Option<EngineError>,
}

impl Drop for LoopState {
    fn drop(&mut self) {
        // SAFETY: 指针由 `run` 里的 `Box::into_raw(Box::new(app))` 产生，只在这里还原一次。
        unsafe { drop(Box::from_raw(self.app)) };
    }
}

/// 系统设置的用户双击速度（`+[NSEvent doubleClickInterval]`，缺省 0.5 s；R02-15 / R09-10）。
///
/// 这是 AppKit **真正用于判定 `NSEvent.clickCount`** 的间隔，因此引擎的连击推断必须用同一个
/// 值，否则「系统认为的双击」会被引擎判成两次单击。读不到时退回平台缺省。
fn double_click_time() -> Duration {
    let defaults = InputThresholds::platform_default();
    let interval = NSEvent::doubleClickInterval();
    if interval.is_finite() && interval > 0.0 {
        Duration::from_secs_f64(interval)
    } else {
        defaults.double_click_time
    }
}

/// 用系统设置初始化输入阈值（R02-15）。
///
/// 位置容差与拖拽阈值 AppKit 没有公开查询，保留平台缺省（4 px）。
fn install_input_thresholds(app: &mut App) {
    let defaults = InputThresholds::platform_default();
    app.set_input_thresholds(InputThresholds {
        double_click_time: double_click_time(),
        ..defaults
    });
}

/// 运行期跟随系统双击速度（R09-10）。
///
/// 用户在「系统设置 → 辅助功能 → 指针控制」里改了双击速度之后，引擎若继续用启动时读到的值，
/// 连击推断就会与 AppKit 的 `clickCount` 分叉（把双击判成两次单击 = 交易终端里的误操作）。
/// 每轮读一次 `+doubleClickInterval` 是常量级开销，只在值变化时写回引擎。
fn sync_double_click_interval(app: &mut App, seen: &mut Duration) {
    let current = double_click_time();
    if current == *seen {
        return;
    }
    *seen = current;
    let defaults = InputThresholds::platform_default();
    app.set_input_thresholds(InputThresholds {
        double_click_time: current,
        ..defaults
    });
}

/// `Application::run` 的平台入口。
pub(crate) fn run(app: App) -> Result<(), EngineError> {
    let mtm = MainThreadMarker::new().ok_or(EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("NSApplication requires the main thread"),
    })?;
    let ns_app = NSApplication::sharedApplication(mtm);
    ns_app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    ns_app.finishLaunching();
    // R09-1：主菜单必须**在任何输入之前**就位。AppKit 的键等价分发是
    // `sendEvent:` → `[mainMenu performKeyEquivalent:]` → 失败才给 key window；主菜单为 nil 时
    // Cmd+Q / Cmd+W / Cmd+C 全部没有承载对象，只能退化成普通 keyDown 并被丢弃。
    menu::install(mtm, &ns_app);

    let mut app = app;
    let dispatcher = app.dispatcher();
    // R09-16：退出握手。delegate 必须自己持强引用（`NSApplication.delegate` 是 weak）。
    let quit = QuitHandshake::new();
    let delegate = AppDelegate::new(mtm, Arc::clone(&dispatcher), Arc::clone(&quit));
    ns_app.setDelegate(Some(delegate.protocol()));
    // 唤醒句柄由本平台的 `wake::install` 安装；`attach` 的 token 只对 Win32 有意义。
    dispatcher.attach(1);
    install_input_thresholds(&mut app);
    let requests: Vec<(WindowId, WindowOptions)> = app
        .windows()
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    let mut windows: Vec<NativeWindow> = Vec::with_capacity(requests.len());
    for (id, options) in requests {
        windows.push(open_window(mtm, &mut app, id, &options, &dispatcher)?);
    }
    // `activate` 必须在窗口已经存在之后（macOS 14+ 的无参版本不会自己找窗口，
    // 在首窗之前调用可能「没真正到前台」）。
    ns_app.activate();

    let state = Box::into_raw(Box::new(LoopState {
        app: Box::into_raw(Box::new(app)),
        windows,
        dispatcher: dispatcher.clone(),
        ns_app: ns_app.clone(),
        _delegate: delegate,
        quit: Arc::clone(&quit),
        double_click: double_click_time(),
        quitting: false,
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
        // R09-15：**只注册 `BeforeWaiting`**。一个 run loop 周期是
        // `BeforeWaiting → 睡眠/处理 source → AfterWaiting → BeforeWaiting → …`；同时注册两个
        // activity 会让同一条完整泵每周期跑**两次**（120 Hz 下 = 240 次/秒，每次含每窗口 6 次
        // ObjC 调用 + `Vec` 分配 + 每窗口一次锁），而两者语义重复：
        // - AppKit 事件派发引起的状态变化会在**下一次** `BeforeWaiting` 被看到；
        // - 跨线程唤醒由 `CFRunLoopSource`（`wake::install`）覆盖；
        // - 连续帧由 `CFRunLoopTimer` 覆盖。
        // 因此 `AfterWaiting` 是纯增量开销。winit 的 macOS 事件循环同样只注册
        // `BeforeWaiting | Exit`。
        //
        // SAFETY: context 是栈上值，`CFRunLoopObserverCreate` 同步拷贝；info 在卸载前有效。
        unsafe {
            CFRunLoopObserver::new(
                None,
                CFRunLoopActivity::BeforeWaiting.0,
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
    let error = state.error.take();
    let awaiting_reply = state.quit.pending();
    let allowed = state.quit.allowed();
    // R09-16：**先**让 `state` 析构（`App` → 各 `Renderer`（join GPU 线程、invalidate display
    // link）→ 原生窗口），**再**回复 AppKit。反过来（先进 AppKit 的退出路径）会让所有析构被
    // `exit()` 跳过。
    drop(state);
    if awaiting_reply || allowed {
        // `NSTerminateLater` 必须被回复，否则 AppKit 会一直等在这个握手上。
        // 出错时回复 `false`：取消终止握手，让调用方看到 `Err`（进程按正常路径退出）。
        ns_app.replyToApplicationShouldTerminate(allowed && error.is_none());
    }
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// 建一个原生窗口 + 渲染器，并把平台真值写回 `App` 的窗口。
///
/// **顺序**（R09-5 的平台侧一半）：先 `show()` 再同步编译 GPU 对象。渲染器构造里有
/// `MTLDevice` / 两个 metallib / 4 条 PSO 的同步编译（数十毫秒），放在 `show()` 之前会表现为
/// 「点了新窗口，整个应用卡一下才出现窗口」；放在之后则是「窗口立刻出现（AppKit 用背景色
/// 铺底），内容稍后到位」。真正的根治（进程级共享 device/library/PSO）在 `render/macos`
/// 的 `GpuCore` 重构里。
fn open_window(
    mtm: MainThreadMarker,
    app: &mut App,
    id: WindowId,
    options: &WindowOptions,
    dispatcher: &Arc<Dispatcher>,
) -> Result<NativeWindow, EngineError> {
    let a11y_epoch = app
        .window(id)
        .map_or_else(crate::a11y::adapter::A11yConsumerEpoch::new, |window| {
            window.a11y_consumer_epoch()
        });
    let native = NativeWindow::create(mtm, id, options, dispatcher, a11y_epoch)?;
    // 先显示：用户立刻看到窗口（背景色由 `NSColor` 铺底）。
    native.show();
    let scale = native.scale();
    let size = native.backing_size();
    let renderer = Renderer::new_for_layer(
        size,
        f64::from(scale.factor()),
        native.maximum_frames_per_second(),
        options.background,
        Arc::clone(dispatcher),
    )?;
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
        // `set_size` 不再重算平台真值；这里写入 AppKit 的真实 backing。
        window.set_surface(size);
        // A5：帧预算的刷新率提示 = 窗口所在屏的 `maximumFramesPerSecond`（ProMotion 120 也覆盖）。
        window.set_frame_refresh_hz(native.maximum_frames_per_second());
    }
    Ok(native)
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
        // 先撤掉定时器/唤醒源：连续帧定时器（动画 / a11y 补发）仍在唤醒 run loop 时，
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
/// `tick` 只由连续帧定时器置位：连续帧不能反过来自己 `dispatcher.wake()`，否则 source
/// 会在同一轮 run loop 里反复唤醒，主线程变成忙循环（render 线程异步后实测 830fps）。
///
/// # Errors
/// 帧提交失败（device lost / swapchain）会立刻终止循环。
fn pump(state: &mut LoopState, tick: bool) -> Result<(), EngineError> {
    // SAFETY: `app` 指针由 `run` 保证有效，且此刻没有其它 `&mut App`。
    let app = unsafe { &mut *state.app };
    app.sync_clock();
    // 运行时窗口生命周期（P1-2.8）：`App` 里新增/移除的窗口要同步到原生窗口。旧实现只在
    // `run` 启动时枚举一次，运行时 `App::create_window` 的窗口永远没有原生窗口与渲染器。
    sync_native_windows(app, &mut state.windows, &state.dispatcher)?;
    // 空闲时也要收 render 线程的反馈：设备丢失/提交失败不能等到下一次输入才冒泡。
    app.poll_renderers()?;
    // R09-10：双击间隔跟随系统设置（用户可能刚在系统设置里改过双击速度）。
    sync_double_click_interval(app, &mut state.double_click);

    // 本轮的「有变化」标记：输入事件与窗口状态变化都必须在**同一轮**出帧。
    // R09-15②：这里不再调 `Dispatcher::wake()`（那会 signal run loop source，让同一轮多跑一次
    // 空泵）—— 状态变化直接送进下面的帧门禁。
    let mut changed = false;
    for native in state.windows.iter_mut() {
        let id = native.id();
        for event in native.take_events() {
            app.dispatch(id, event);
            changed = true;
        }
        changed |= native.sync_state(app)?;
    }

    app.run_frame_hook();
    // 连续帧的节奏由定时器（observer 每轮一次）提供；`tick` 为真时直接出帧，不再写回
    // `Dispatcher::wake`。source/observer 路径只响应显式 wake（输入、任务、应用请求）。
    //
    // A5：`tick` 是 display link 节拍的 tick（定时器只在需要连续帧 / 有预算推迟帧 / a11y 补发
    // 时挂着），因此 tick 驱动的连续帧走 **force 通道**（这一拍本来就该出帧，不能又被预算合并
    // 一拍）；预算推迟的帧走普通通道（tick 到期时预算门禁已经放行）。
    let continuous = app.needs_continuous_frames();
    let deferred = app.has_deferred_frames();
    let wake = state.dispatcher.take_frame_wake();
    if wake || changed || (tick && (continuous || deferred)) {
        frame_all(app, &state.windows, tick && (continuous || deferred))?;
    }

    // display link 的节奏（§5.3 + R09-9）：连续帧 / 有帧要画 / 有在飞帧时恢复，否则暂停
    // （空闲 0 唤醒）。**不可见**（最小化 / 应用隐藏 / 被完全遮挡）时无条件停帧 —— 那三种
    // 状态下窗口一个像素都不显示，却会产生帧请求把 display link 永久钉在 120 Hz。
    for native in state.windows.iter() {
        let visible = native.is_visible();
        if let Some(window) = app.window_mut(native.id()) {
            // A5：窗口可能被拖到另一块屏（刷新率不同）⇒ 每轮泵更新预算提示（同值幂等）。
            window.set_frame_refresh_hz(native.maximum_frames_per_second());
            // RA-5：不可见窗口不产帧（下面 `sync_pacing` 会停 display link），推迟标记必须清掉，
            // 否则 `has_deferred_frames()` 让连续帧定时器常驻（空闲零唤醒不变式被破坏）。
            if !visible {
                window.clear_frame_budget_deferred();
            }
            window.sync_pacing(continuous && visible);
        }
    }

    // 交互反馈：光标形状按 **WindowId 定向**投递（R09-11 / R09-25）。
    // 旧实现只把 `Cursor` 施加到第一个窗口，其余窗口的光标请求被静默丢弃。
    if let Some((target, cursor)) = app.take_cursor_change()
        && let Some(native) = state
            .windows
            .iter()
            .find(|native| native.id() == target && !native.is_destroyed())
    {
        native.set_cursor(cursor);
    }
    // 指针捕获（R02-8）：AppKit 在 `mouseDown:` 之后**隐式捕获**（dragged/up 事件按按下时命中的
    // view 路由，即使指针移出窗口），所以这里没有需要调用的系统 API —— 请求被消费掉是正确行为，
    // 引擎侧的捕获状态（事件路由 / `PointerCancel` 语义）照常生效。这一点在
    // `App::capture_pointer` 的文档里明确写出，不假装平台请求成功。
    let _ = app.take_capture_requests();

    for native in state.windows.iter_mut() {
        native.update_a11y(app);
        native.sync_ime_cursor_area(app);
    }
    for task in state.dispatcher.drain_tasks() {
        task.run(app);
    }

    // 连续帧定时器：只有动画在跑才挂（P-C 起 HUD 是纯观察者、不再是帧源；其余时间空闲零唤醒）。
    // A5：被预算门禁推迟的帧也要一个一次性唤醒（定时器到下一拍时泵会走 `tick` 分支出帧）。
    if let Some(native) = state.windows.first() {
        // a11y 到期补发也借用这个定时器（同 Windows）：到期交付后自动停表。
        native.set_animating(
            app.needs_continuous_frames() || app.has_deferred_frames() || app.a11y_retry_pending(),
        );
    }

    // R09-16：退出握手。Dock 退出 / osascript / Cmd+Q 都汇到 `applicationShouldTerminate:`，
    // 那里只置位 + 唤醒；真正的关闭流程在这里跑（与点红点/Cmd-W 同一个裁决点）。
    if state.quit.pending() && !state.quitting {
        // 处理器里可能弹模态面板（嵌套 run loop ⇒ 本函数重入）：`quitting` 保证同一时刻只有
        // 一次询问在进行，且 `replyToApplicationShouldTerminate:` 只发一次（见 `LoopState.quitting`）。
        state.quitting = true;
        request_quit(state, app);
        state.quitting = false;
    }

    // 关闭：请求关闭的窗口真正关闭；一个不剩就退出。
    let mut closed = false;
    for native in state.windows.iter_mut() {
        let Some(source) = native.take_close_request() else {
            continue;
        };
        // 只有**手势**路径需要在这里补发事件（`sync_state` 不产生它）；观察路径
        // （窗口已不可见）已经在 `sync_state` 里 dispatch 过，再发就是双投递（复审 F3）。
        if source == crate::platform::macos::window::CloseRequestSource::Gesture {
            app.dispatch(native.id(), crate::event::Event::CloseRequested);
        }
        // 先问应用：`App::on_close` 返回 `false` 就拦下这次关闭（P2-8）。
        // 观察路径下窗口已经不可见、拦不住（AppKit 已关），这里只决定是否从 App 侧移除。
        if !app.request_close(native.id()) {
            continue;
        }
        native.close();
        closed = true;
    }
    if closed {
        state.windows.retain(|native| !native.is_destroyed());
    }
    if state.windows.is_empty() || app.windows().next().is_none() {
        state.exit = true;
    }
    Ok(())
}

/// 退出握手：先询问全部窗口，再关窗（R09-16 / AR3 P1-2）。
///
/// 复用用户点窗口红点时的**同一个裁决点**（窗口的关闭处理器），因此「有未保存的布局/未确认的
/// 订单时不许退出」这类 veto 对 Cmd+Q / Dock 退出同样生效。
///
/// **两阶段**（AR3 macOS P1-2）：旧实现逐窗 `App::request_close`，而它在批准时会**立刻**关窗，
/// 于是「窗口 A 同意、窗口 B veto」的退出会留下已经被拆掉的 A（Renderer 已 drop、应用状态
/// 丢失），而用户看到的是「退出已取消」；下一轮 `sync_native_windows` 还会把 A 的原生窗口也
/// 销毁。现在询问阶段**没有任何副作用**（只跑关闭处理器），全部同意后才统一关闭；任一 veto
/// 则整个握手回滚 —— 一个窗口都不关。`App::request_close_all` 是这条策略的唯一实现（可在
/// 开发机上用无窗口逻辑层单测）。
///
/// 全部同意后关闭所有原生窗口，由泵末尾的「一个不剩 ⇒ exit」收尾；`run()` 在析构完成后回复
/// AppKit。
fn request_quit(state: &mut LoopState, app: &mut App) {
    let ids: Vec<WindowId> = app.windows().map(|(id, _)| id).collect();
    if !app.request_close_all(&ids) {
        // 应用拒绝退出：必须回复 AppKit「不终止」，否则它会一直等在这个握手上。
        // 询问阶段没有关任何窗口，因此这里不需要回滚窗口状态。
        state.quit.veto();
        state.ns_app.replyToApplicationShouldTerminate(false);
        return;
    }
    for native in state.windows.iter_mut() {
        native.close();
    }
    // 允许终止：`run()` 里的 `ns_app.run()` 返回、`App`/`Renderer` 析构完成之后再回复 AppKit。
    state.quit.allow();
}

/// 把 `App` 的窗口集合同步到原生窗口（运行时创建 / 应用主动关闭，P1-2.8）。
///
/// 与 Win32 的同名函数同构：
/// - **创建**：`App` 里存在但还没有原生窗口的 id ⇒ 建 `NativeWindow` + `Renderer`，挂
///   `CAMetalLayer`，并把尺寸 / DPI / surface 写回 `Window`（与启动路径同一套写入顺序）。
/// - **销毁**：原生窗口对应的 `App` 窗口已经不存在 ⇒ 从列表移除（`NativeWindow::drop`
///   负责关窗；`App` 丢弃 `Window` 时已经 drop 了它的 `Renderer`，GPU 线程 join 早于关窗）。
fn sync_native_windows(
    app: &mut App,
    windows: &mut Vec<NativeWindow>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<(), EngineError> {
    windows.retain(|native| app.window(native.id()).is_some());
    let existing: Vec<WindowId> = windows.iter().map(NativeWindow::id).collect();
    let pending: Vec<(WindowId, WindowOptions)> = app
        .windows()
        .filter(|(id, _)| !existing.contains(id))
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    // `pump` 只在主线程、run loop 上执行，因此这里拿得到 MainThreadMarker。
    let mtm = MainThreadMarker::new().ok_or(EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("window creation requires the main thread"),
    })?;
    for (id, options) in pending {
        windows.push(open_window(mtm, app, id, &options, dispatcher)?);
    }
    Ok(())
}

/// 逐窗口提交一帧；`Err` 立刻终止（device lost / swapchain 失败都不可重试）。
///
/// **不可见窗口不产帧**（R09-9）：不 publish ⇒ 渲染器的 `pending_frame` 保持为假 ⇒
/// `pacing_paused()` 才有机会为真，display link 才会真正停下来。恢复可见时
/// `NativeWindow::sync_state` 置全量 damage 并请求一帧。
///
/// `force` = 本帧由 display link 节拍 tick 驱动（force 通道，绕过 A5 的帧预算门禁）；
/// 普通唤醒路径传 `false`。
fn frame_all(app: &mut App, windows: &[NativeWindow], force: bool) -> Result<(), EngineError> {
    for native in windows {
        if !native.is_visible() {
            continue;
        }
        let outcome = if force {
            app.frame_forced(native.id())
        } else {
            app.frame(native.id())
        };
        match outcome {
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
/// 只注册 `BeforeWaiting`（R09-15）：每轮 run loop 恰好泵一次。
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

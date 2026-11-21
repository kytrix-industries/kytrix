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

use std::{
    cell::RefCell,
    ffi::c_void,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventModifierFlags, NSEventType,
    NSWindow,
};
use objc2_core_foundation::{
    CFRunLoop, CFRunLoopActivity, CFRunLoopObserver, CFRunLoopObserverContext, CFRunLoopTimer,
};
use objc2_foundation::NSPoint;
use smallvec::SmallVec;

use crate::{
    app::App,
    error::EngineError,
    interaction::InputThresholds,
    platform::{
        dispatcher::Dispatcher,
        macos::{
            app_delegate::{AppDelegate, QuitHandshake},
            menu, wake,
            window::{NativeWindow, OwnerWindow},
        },
        macos_close::CloseAction,
        macos_owner::{
            ModalOwnerChromes, OwnerResolution, WindowKeyInput, reclaim_modal_keys, resolve_owner,
            window_level,
        },
    },
    render::{backend::Renderer, macos::core::GpuCore},
    window::{Window, WindowId, WindowOptions},
};

/// 泵的共享状态（堆上、地址稳定；C 回调通过 `info` 拿到它）。
struct LoopState {
    /// `Box<App>` 的裸指针（`Drop` 时还原）。
    app: *mut App,
    windows: Vec<NativeWindow>,
    dispatcher: Arc<Dispatcher>,
    /// 模态 owner 拦截记账（RB-07；RB-13-mac 的派生判据）：与所有 `NativeWindow` 共享，
    /// 运行期新建的窗口也必须用同一份（见 `sync_native_windows`）。
    modal_registry: Rc<RefCell<ModalOwnerChromes>>,
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
    // B5：禁用 AppKit 的自动窗口 tabbing（类级开关，10.12+）。必须在**创建任何窗口之前**
    // 设置：打开后 AppKit 会按 tabbingIdentifier / 用户偏好把新窗口并进 tab 组，并往
    // Window 菜单追加「Merge All Windows」等项。引擎的关闭语义以 `windowWillClose:` 为
    // 唯一权威来源、以窗口为关闭单位；合 tab 会把「关一个 tab」与「关窗口」的语义分叉，
    // 且非前台 tab 的 `isVisible()` 为 false（旧实现的可见性反推会误判成关闭，B5 已删）。
    NSWindow::setAllowsAutomaticWindowTabbing(false, mtm);
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
    // RB-07：模态 owner 拦截记账（启动批与运行期建窗共用同一份；见 `LoopState`）。
    let modal_registry: Rc<RefCell<ModalOwnerChromes>> =
        Rc::new(RefCell::new(ModalOwnerChromes::default()));
    // X1：两阶段建窗（先全部创建、再接线 owner/层级、最后显示 + 建渲染器）。`App` 的窗口
    // 表是 `HashMap`，创建顺序不可控，而 owner 必须先有原生窗口，见 `attach_owners`。
    create_native_windows(
        mtm,
        &app,
        &requests,
        &dispatcher,
        &mut windows,
        &modal_registry,
    )?;
    attach_owners(&requests, &mut windows, 0);
    for (native, (_, options)) in windows.iter_mut().zip(requests.iter()) {
        show_window(&mut app, native, options, &dispatcher)?;
    }
    // `activate` 必须在窗口已经存在之后（macOS 14+ 的无参版本不会自己找窗口，
    // 在首窗之前调用可能「没真正到前台」）。
    ns_app.activate();

    let state = Box::into_raw(Box::new(LoopState {
        app: Box::into_raw(Box::new(app)),
        windows,
        dispatcher: dispatcher.clone(),
        modal_registry,
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

/// 建一批原生窗口（**不显示**；`cascade_index` 按批内顺序级联摆放）。
///
/// 与 [`show_window`] 拆开是为了 X1 的 owner 接线：`App` 的窗口表是 `HashMap`，一批窗口的
/// 创建顺序不可控，而 owner 必须先有原生窗口。因此建窗分三步：
/// 1. [`create_native_windows`] 全部创建（隐藏）；
/// 2. [`attach_owners`] 把 owner/层级接上（owner 可以在这批里、也可以是更早的批次）；
/// 3. [`show_window`] 逐个显示 + 建渲染器。
fn create_native_windows(
    mtm: MainThreadMarker,
    app: &App,
    requests: &[(WindowId, WindowOptions)],
    dispatcher: &Arc<Dispatcher>,
    windows: &mut Vec<NativeWindow>,
    modal_registry: &Rc<RefCell<ModalOwnerChromes>>,
) -> Result<(), EngineError> {
    let start = windows.len();
    windows.reserve(requests.len());
    for (offset, (id, options)) in requests.iter().enumerate() {
        // RE-01：adapter 必须持有窗口 `A11yBridge` 的**那一份**消费端代际（唯一入口
        // `App::window_a11y_epoch`）；不存在的窗口不能建原生窗口。
        let a11y_epoch = app
            .window_a11y_epoch(*id)
            .ok_or_else(|| EngineError::AppInit {
                detail: smol_str::SmolStr::new_static(
                    "window disappeared while creating native window",
                ),
            })?;
        windows.push(NativeWindow::create(
            mtm,
            *id,
            options,
            dispatcher,
            start + offset,
            a11y_epoch,
            Rc::clone(modal_registry),
        )?);
    }
    Ok(())
}

/// X1：把 owner 关系与窗口层级接到刚创建的一批原生窗口上（`start` = 本批起始下标）。
///
/// - **owner**：`addChildWindow:ordered:`（transient parent 语义；见
///   `NativeWindow::apply_owner` 与 `window.rs` 模块文档）。找不到 owner 原生窗口
///   （创建顺序之外的竞态 / 非法 id）⇒ 降级为无 owner 并写诊断，不 panic。
/// - **层级**：模态（owner 已解析）> 置顶 > 普通；纯逻辑在
///   `platform::macos_owner::window_level`（Windows 可单测）。
/// - **模态原生拦截**（RB-07；RB-13-mac）：`ModalPanel` 层级 ⇒ `apply_owner` 在共享记账
///   （`ModalOwnerChromes`）登记该模态窗口；只有该 owner 的**第一个**模态窗口才快照并施加
///   [`crate::platform::macos_owner::OwnerChrome::BLOCKED`]（`ignoresMouseEvents` + 禁用
///   标准按钮 + 关闭背景拖动），最后一个关闭的模态窗口按快照恢复（多模态窗口时 owner 保持
///   拦截，焦点交给另一个模态窗口）。owner 没解析出来的模态窗口按普通窗口处理（没有可拦截
///   的对象），引擎层的输入过滤同样不会命中它。
fn attach_owners(
    requests: &[(WindowId, WindowOptions)],
    windows: &mut [NativeWindow],
    start: usize,
) {
    for (offset, (id, options)) in requests.iter().enumerate() {
        let index = start + offset;
        let resolution = resolve_owner(*id, options.owner, |owner| {
            windows.iter().any(|native| native.id() == owner)
        });
        let owner = match resolution {
            OwnerResolution::Attach(owner) => windows
                .iter()
                .find(|native| native.id() == owner)
                .map(|native| OwnerWindow {
                    id: owner,
                    window: native.window_handle(),
                }),
            OwnerResolution::None => None,
            OwnerResolution::Missing => {
                // 引擎层（`app/window_set.rs`）保证 options.owner 合法；走到这里说明平台侧
                // 没找到对应原生窗口。窗口仍然可用，只是失去 transient parent 语义。
                eprintln!(
                    "kytrix: 窗口 {id:?} 的 owner {:?} 没有原生窗口；降级为无 owner",
                    options.owner
                );
                None
            }
        };
        // 模态窗口只能在 owner 已解析时抬层级（没有 owner 的模态由引擎层降级为普通窗口）。
        let level = window_level(options.always_on_top, options.modal && owner.is_some());
        windows[index].apply_owner(owner, level);
    }
}

/// 显示一个已建好的原生窗口 + 创建渲染器，并把平台真值写回 `App` 的窗口。
///
/// **顺序**（R09-5 的平台侧一半）：先 `show()` 再同步编译 GPU 对象。渲染器构造里有
/// `MTLDevice` / 两个 metallib / 4 条 PSO 的同步编译（数十毫秒），放在 `show()` 之前会表现为
/// 「点了新窗口，整个应用卡一下才出现窗口」；放在之后则是「窗口立刻出现（AppKit 用背景色
/// 铺底），内容稍后到位」。真正的根治（进程级共享 device/library/PSO）在 `render/macos`
/// 的 `GpuCore` 重构里。
fn show_window(
    app: &mut App,
    native: &mut NativeWindow,
    options: &WindowOptions,
    dispatcher: &Arc<Dispatcher>,
) -> Result<(), EngineError> {
    let id = native.id();
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
    Ok(())
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
    sync_native_windows(
        app,
        &mut state.windows,
        &state.dispatcher,
        &state.modal_registry,
    )?;
    // 空闲时也要收 render 线程的反馈：设备丢失/提交失败不能等到下一次输入才冒泡。
    // C4：**逐窗口** poll —— `DeviceLost` 当场整 renderer 重建（泵继续运行），其它 GPU 错误
    // 保持致命并原样冒泡（应用退出语义不变）。返回 `true` = 本轮恢复过，需要在同一轮强制出帧，
    // 否则 `force_full_republish` 只置脏位、要等下一次外部唤醒才会被消费。
    let recovered = poll_renderers_recovering(app, &state.windows, &state.dispatcher)?;
    // R09-10：双击间隔跟随系统设置（用户可能刚在系统设置里改过双击速度）。
    sync_double_click_interval(app, &mut state.double_click);

    // 本轮的「有变化」标记：输入事件与窗口状态变化都必须在**同一轮**出帧。
    // R09-15②：这里不再调 `Dispatcher::wake()`（那会 signal run loop source，让同一轮多跑一次
    // 空泵）—— 状态变化直接送进下面的帧门禁。
    let mut changed = recovered;
    // RBM-01：模态 key 对账必须是**泵级预扫描**，并且在任何窗口读取自己的焦点快照之前完成。
    // 若放进逐窗口的 `sync_state`，窗口按创建顺序同步（owner 先建、先 sync）：owner 会先看到
    // `isKeyWindow == true` 并投递假 `FocusChanged{true}`，随后模态窗口才把 key 拿回 ⇒ 下一轮
    // owner 再投 `FocusChanged{false}` 并触发多余的失焦 IME 清场。判定在平台无关的
    // `platform::macos_owner::reclaim_modal_keys`（Windows 可单测）。
    let key_inputs: SmallVec<[WindowKeyInput; 8]> =
        state.windows.iter().map(NativeWindow::key_input).collect();
    if let Some(modal) = reclaim_modal_keys(&key_inputs)
        && let Some(native) = state.windows.iter().find(|native| native.id() == modal)
    {
        native.make_key();
    }
    // E6：窗口控制请求（自绘标题栏的系统按钮 / 全屏；原生 AppKit 路径）。先应用控制、再对账
    // chrome（见下面的 `sync_chrome`）：`performZoom:` / `toggleFullScreen:` 之后同一轮就能读到
    // 新的 `isZoomed` / `styleMask` 状态并把变化写回 `App` + 出帧。
    if let Some((window, control)) = app.take_window_control()
        && let Some(native) = state
            .windows
            .iter()
            .find(|native| native.id() == window && !native.is_destroyed())
    {
        native.apply_window_control(control);
    }
    for native in state.windows.iter_mut() {
        let id = native.id();
        for event in native.take_events() {
            app.dispatch(id, event);
            changed = true;
        }
        changed |= native.sync_state(app)?;
        // E6：标题 / 交通灯位置 / `WindowState`（`isZoomed` + `styleMask` 全屏位）。变化时
        // `Window::set_state` 已给根节点标 `PAINT`，本函数返回 true ⇒ 同一轮走下面的帧门禁。
        changed |= native.sync_chrome(app);
        // B9(mac)：窗口刚失去键盘焦点 ⇒ IME 统一清场（清 caret hint / 丢弃组合 / 补 End）。
        // 放在 `sync_state` 之后：`FocusChanged { focused: false }` 已经按事件顺序投递，
        // 清场产生的 `ImeEvent::End` 排在其后；清场要 `&mut App`，因此由泵执行而不是在
        // view / delegate 回调里（回调纪律：只置位 + 唤醒）。
        if native.take_focus_lost() {
            native.handle_focus_lost(app);
        }
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
        // IME 启用状态先于 caret hint：刚禁用时不必再写表单，刚启用时要重发 hint
        //（`sync_ime_enabled` 会清掉 `ime_area` 缓存）。
        native.sync_ime_enabled(app);
        native.sync_ime_cursor_area(app);
        // X2：自绘标题栏拖动区（泵每轮对账；`NativeWindow` 内部只在值变化时写 view）。
        if let Some(window) = app.window(native.id()) {
            native.sync_drag_regions(window.drag_regions());
        }
    }
    // E5 / RE-04 / RE-05：**本轮确实交付过** a11y 且仍有未走完的批次 ⇒ 立刻唤醒下一轮泵
    // （无 AT 客户端时无交付 ⇒ 绝不 wake，I3 空闲零唤醒）；判定 + wake 的唯一入口。
    app.wake_for_a11y_backlog(state.dispatcher.as_ref());
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
        // E10：一次性定时器（`App::after`）—— 独立的一次性 run loop 定时器，等待期间不产生
        // 连续帧；必须在 ⑤ 任务之后对账，任务里注册的定时器才能在同一轮挂表。
        native.set_wake_deadline(app.next_wake_deadline());
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

    // 关闭（B5）：两个来源，语义严格区分（状态机见 `platform::macos_close`）。
    //
    // - **用户手势**（红点 / Cmd+W / 菜单 Close / `performClose:`）：先 dispatch
    //   `Event::CloseRequested`（通知，三端同序），再问 `App::request_close`（`on_close`
    //   裁决，可否决）。批准后才关原生窗口；否决则窗口原样保留。
    // - **外部关闭**（`windowWillClose:`）：AppKit 已经关闭了窗口，不可否决 —— 不再投递
    //   事件、不再询问；只做 App 侧关闭记账（`App::close_window`）并移除原生窗口。
    //
    // 事件先于处理器由 `CloseBridge::decide` 的闭包顺序保证：闭包里先 dispatch 再返回
    // `request_close` 的结果，且只在手势路径执行、只执行一次。
    let mut removed = false;
    for native in state.windows.iter_mut() {
        let id = native.id();
        let action = native.decide_close(|| {
            app.dispatch(id, crate::event::Event::CloseRequested);
            app.request_close(id)
        });
        match action {
            CloseAction::Keep => {}
            CloseAction::Close => {
                native.close();
                removed = true;
            }
            CloseAction::RemoveNative => {
                // 外部关闭：窗口已经被 AppKit 关掉，没有可裁决的对象。`App` 里可能还留着
                // 这扇窗口（应用没有参与关闭），直接做引擎侧记账；下一轮
                // `sync_native_windows` 也不会再为它建原生窗口。
                app.close_window(id);
                native.mark_externally_closed();
                removed = true;
            }
        }
    }
    if removed {
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
///   与启动路径共用 X1 的两阶段建窗（[`create_native_windows`] → [`attach_owners`] →
///   [`show_window`]）：owner 可能同批创建（顺序不可控）或已存在。
/// - **销毁**：原生窗口对应的 `App` 窗口已经不存在 ⇒ 从列表移除（`NativeWindow::drop`
///   负责关窗；`App` 丢弃 `Window` 时已经 drop 了它的 `Renderer`，GPU 线程 join 早于关窗）。
fn sync_native_windows(
    app: &mut App,
    windows: &mut Vec<NativeWindow>,
    dispatcher: &Arc<Dispatcher>,
    modal_registry: &Rc<RefCell<ModalOwnerChromes>>,
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
    let start = windows.len();
    create_native_windows(mtm, app, &pending, dispatcher, windows, modal_registry)?;
    attach_owners(&pending, windows, start);
    for (native, (_, options)) in windows[start..].iter_mut().zip(pending.iter()) {
        show_window(app, native, options, dispatcher)?;
    }
    Ok(())
}

/// 逐窗口 poll 的结论：`DeviceLost` 就地恢复，其它 GPU 错误保持致命。
///
/// 单独抽成纯函数是为了让「哪些错误触发整 renderer 重建」有可单测的唯一真值来源
/// （audit §4.4：分类粒度是恢复正确性的前提）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryDisposition {
    /// 设备级失败（`EngineError::DeviceLost`）：整 renderer 重建（C4）。
    Rebuild,
    /// 其它 GPU 错误（swapchain / persist 预算 / shader 等）：**不可重试**，保持致命。
    ///
    /// 这些错误不是「设备没了」的事实：重建一个设备也不会让 persist 预算变宽或让 swapchain
    /// 参数变合法，盲目重建只会把真实缺陷藏成一次黑屏。
    Fatal,
}

/// `DeviceLost` ⇒ 重建；其它错误 ⇒ 致命（应用退出语义不变）。
fn recovery_disposition(error: &EngineError) -> RecoveryDisposition {
    match error {
        EngineError::DeviceLost { .. } => RecoveryDisposition::Rebuild,
        _ => RecoveryDisposition::Fatal,
    }
}

/// 恢复前置条件被破坏（窗口 / 渲染器在 poll 之后消失）：编程错误，按 `AppInit` 上报。
fn recovery_invariant(detail: &'static str) -> EngineError {
    EngineError::AppInit {
        detail: smol_str::SmolStr::new_static(detail),
    }
}

/// 轮询每个窗口的 GPU 反馈；`DeviceLost` 当场恢复，其它错误原样冒泡。
///
/// 返回 `true` = 本轮发生过恢复（调用方在同一轮强制出帧，见 [`request_full_republish_after_recovery`]）。
///
/// 两层结构：
/// 1. **稳态快路径**仍走 `App::poll_renderers()`（每轮一次、按窗口短锁；空闲也要跑）；
/// 2. 它冒出 `DeviceLost` 时才**逐窗口复 poll** 定位失败窗口 —— `Renderer::poll` 的错误是粘性的，
///    失败窗口会再次返回同一条错误（成功窗口返回 `Ok(None)`）。多窗口同时设备丢失时每个窗口
///    都能被重建；其它类型的 GPU 错误在这里保持致命（应用退出语义不变）。
fn poll_renderers_recovering(
    app: &mut App,
    windows: &[NativeWindow],
    dispatcher: &Arc<Dispatcher>,
) -> Result<bool, EngineError> {
    match app.poll_renderers() {
        Ok(()) => return Ok(false),
        Err(error) if matches!(recovery_disposition(&error), RecoveryDisposition::Rebuild) => {}
        Err(error) => return Err(error),
    }
    let mut recovered = false;
    for native in windows {
        let id = native.id();
        // 逐窗口 poll 才能定位失败窗口（`App::poll_renderers` 在第一个错误处就返回了）。
        let error = match app.window_mut(id) {
            Some(window) => window.poll_renderer().err(),
            // `sync_native_windows` 保证原生窗口与 App 窗口一一对应；这里只是防御。
            None => None,
        };
        let Some(error) = error else {
            continue;
        };
        match recovery_disposition(&error) {
            RecoveryDisposition::Fatal => return Err(error),
            RecoveryDisposition::Rebuild => {
                recover_window_renderer(app, native, dispatcher, &error)?;
                recovered = true;
            }
        }
    }
    Ok(recovered)
}

/// 一个窗口的**整 renderer 重建**（C4；顺序与不变量见 `render/macos/renderer.rs` 模块文档）。
///
/// 步序（每一步的失败都意味着应用退出，走泵既有的 `state.error` 路径）：
/// 1. 捕获旧 renderer 的重建输入（图层 / 背景 / 旧 core / GPU 线程）；
/// 2. **take/drop 旧 renderer**：invalidate display link + 有界 join 旧 GPU 线程
///    （主线程上限 250 ms，见 `render/macos/renderer.rs` 的 `JOIN_TIMEOUT`）；
/// 3. `GpuCore::recover(&stale)`：换进程级设备（5 s 内最多 3 次；其它窗口已重建则复用）；
/// 4. `Renderer::new_with_layer`：复用旧 `CAMetalLayer`，重建 persist / 图集页 / 上传槽 /
///    GPU 线程 / display link；
/// 5. 覆盖合成线程的交付端点（`compositor().register`）：不覆盖的话合成线程继续把 delta 投给
///    已终止的旧 `Arc<Shared>` ⇒ 新 renderer 永远收不到帧（黑屏）；
/// 6. `force_full_republish`（含 `request_repaint`）：新 GPU 侧的实例镜像 / 图集纹理 / persist
///    都是空的，增量载荷会画成退化实例（AR4 P1-1 的同一条纪律）。
fn recover_window_renderer(
    app: &mut App,
    native: &NativeWindow,
    dispatcher: &Arc<Dispatcher>,
    error: &EngineError,
) -> Result<(), EngineError> {
    let started = Instant::now();
    let id = native.id();
    // 恢复只对设备丢失开放（`poll_renderers_recovering` 已用 `recovery_disposition` 过滤；
    // 这里是第二道守卫：非 DeviceLost 不允许走到后面的破坏性步骤）。
    let EngineError::DeviceLost { reason, .. } = error else {
        return Err(recovery_invariant(
            "device recovery: unexpected non-device-lost error",
        ));
    };
    // 平台真值（AppKit backing / DPI / 屏幕刷新率）：新 renderer 必须原样继承。
    let size = native.backing_size();
    let scale = f64::from(native.scale().factor());
    let max_frame_rate = native.maximum_frames_per_second();

    // 1) 捕获重建输入。`background` 用 renderer 的主线程真值（运行期可能被
    //    `Window::set_background_color` 改过，不能回头读建窗时的 options）。
    let (old, background) = {
        let window = app.window_mut(id).ok_or_else(|| {
            recovery_invariant("device recovery: the window disappeared before it was rebuilt")
        })?;
        let old = window.take_renderer().ok_or_else(|| {
            recovery_invariant("device recovery: the window has no renderer to rebuild")
        })?;
        let background = old.background_color().ok_or_else(|| {
            recovery_invariant("device recovery: the renderer has no window background")
        })?;
        (old, background)
    };
    let layer = old.layer_handle().ok_or_else(|| {
        recovery_invariant("device recovery: the window renderer has no CAMetalLayer")
    })?;
    let stale = old
        .gpu_core()
        .ok_or_else(|| recovery_invariant("device recovery: the window renderer has no GpuCore"))?;
    // 2) 有界 drop：停 display link → signal shutdown → join（超时 detach，旧线程不再 present）。
    drop(old);
    let join_stall = started.elapsed();
    // 3) 设备级重建（复用/限次规则在 `render/macos/core.rs`）。
    GpuCore::recover(&stale)?;
    let generation = GpuCore::generation();
    // 4) 复用旧图层的整窗口 renderer（新设备上的 persist / 图集页 / display link / GPU 线程）。
    let renderer = Renderer::new_with_layer(
        layer,
        size,
        scale,
        max_frame_rate,
        background,
        Arc::clone(dispatcher),
    )?;
    // 5) 合成线程的交付端点必须**覆盖**：ticket 只在首个真实帧注册一次，恢复时
    //    `submit_gpu` 看到 ticket 已存在就不会再注册。
    let sink = renderer.frame_sink();
    let window = app.window_mut(id).ok_or_else(|| {
        recovery_invariant("device recovery: the window disappeared while it was rebuilt")
    })?;
    if window.has_compositor_ticket()
        && let Some(sink) = sink
    {
        crate::render::compositor_thread::compositor().register(id, sink);
    }
    window.set_renderer(renderer);
    // 图层几何 / 目标尺寸写回 AppKit 真值（新 renderer 从它自己的 `size` 起算，这里同步 UI 侧）。
    window.set_surface(size);
    // 6) 整帧重发。
    request_full_republish_after_recovery(window);

    eprintln!(
        "kytrix: metal device recovered for window {id:?} (GpuCore generation {generation}): \
         {reason}; main-thread stall {:.1} ms (old GPU thread join {:.1} ms); \
         visible gap ~1-3 frames",
        started.elapsed().as_secs_f64() * 1e3,
        join_stall.as_secs_f64() * 1e3
    );
    Ok(())
}

/// 恢复后的 UI 侧整帧重发（C4 的第 6 步）。
///
/// `Window::force_full_republish` 一次性置齐三个效果（与合成线程重启的路径同一 API）：
/// 1. `delta_builder.force_full()` ⇒ 下一帧发布**全部** lane 且 `SceneDelta::full`（整块实例缓冲）；
/// 2. `full_damage_pending` ⇒ 下一帧的 damage 是 `Plan::Full`（新 persist 覆盖不到的区域没有
///    其它 clear 路径）；
/// 3. `request_repaint()` ⇒ 确保下一次泵真的会出这一帧（不依赖外部输入）。
fn request_full_republish_after_recovery(window: &mut Window) {
    window.force_full_republish();
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use smol_str::SmolStr;

    use super::{RecoveryDisposition, recovery_disposition, request_full_republish_after_recovery};
    use crate::{error::EngineError, window::WindowOptions};

    /// 恢复判定：只有 `DeviceLost` 触发整 renderer 重建，其它 GPU 错误保持致命。
    ///
    /// 变异守卫：把 `recovery_disposition` 改成「所有错误都 Rebuild」会让后半段断言变红；
    /// 改成「所有错误都 Fatal」会让前半段变红。真实进程行为由 `acceptance window` 的注入
    /// e2e 覆盖（删掉泵内恢复分支 ⇒ 进程以错误退出）。
    #[test]
    fn only_device_lost_triggers_a_renderer_rebuild() {
        let lost = EngineError::DeviceLost {
            reason: SmolStr::new_static("MTLCommandBufferError(11) DeviceRemoved"),
            log_path: PathBuf::from("/tmp/kytrix-metal-diagnostics.txt"),
        };
        assert_eq!(
            recovery_disposition(&lost),
            RecoveryDisposition::Rebuild,
            "设备丢失必须走整 renderer 重建"
        );
        for fatal in [
            EngineError::Swapchain {
                detail: SmolStr::new_static("MTLCommandBuffer failed"),
            },
            EngineError::AppInit {
                detail: SmolStr::new_static("metal newTexture: returned nil"),
            },
            EngineError::FramePipeline {
                detail: SmolStr::new_static("compositor inbox closed"),
            },
            EngineError::UnsupportedGpu {
                os: "macos",
                required: "D8",
                actual: SmolStr::new_static("no Metal device"),
            },
        ] {
            assert_eq!(
                recovery_disposition(&fatal),
                RecoveryDisposition::Fatal,
                "{fatal:?} 不是设备丢失，不得触发整 renderer 重建（保持致命 + 应用退出）"
            );
        }
    }

    /// **恢复后整帧重发**（C4 的 UI 侧契约；headless 跑，不需要窗口系统）。
    ///
    /// 新 renderer 的 GPU 侧实例镜像 / 图集纹理 / persist 都是空的，增量载荷只会填「变化」的
    /// 段 ⇒ 必须 `Plan::Full` + 重编码全部 lane（AR4 P1-1 的同一条纪律）。这里直接调用恢复
    /// 驱动用的同一个 helper，钉住「恢复 ⇒ 下一帧整帧」而不是「下一帧仍是增量」。
    #[cfg(feature = "test-support")]
    #[test]
    fn recovery_forces_a_full_republish_of_the_next_frame() {
        use crate::{
            app::App,
            math::{Rect, Scale, Size},
            node::builder::NodeBuilder,
            paint::{color::Color, style::PaintSpec},
            window::FrameOutcome,
        };

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: SmolStr::new_static("device-recovery"),
            size: Size::new(320.0, 240.0),
            scale: Scale::ONE,
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("window").tree().root();
        let mut nodes = Vec::with_capacity(3);
        for index in 0..3 {
            let node = NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, index as f32 * 24.0, 32.0, 20.0))
                .paint(PaintSpec {
                    background: Color::WHITE,
                    ..PaintSpec::default()
                })
                .build(app.window_mut(window).expect("window").tree_mut(), root)
                .expect("box");
            nodes.push(node);
        }
        let _ = app
            .frame(window)
            .expect("window")
            .expect("first full frame");
        // 前置条件：稳态帧是增量（只改一个节点 ⇒ 只重编码 1 条 lane）。
        let _ = app
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_paint(nodes[0]);
        let _ = app
            .frame(window)
            .expect("window")
            .expect("incremental frame");
        let view = app.window(window).expect("window");
        assert_eq!(
            view.record().expect("record").stats.encoded_lanes,
            1,
            "前置条件：稳态帧必须是增量帧"
        );

        // 恢复驱动在挂上新 renderer 之后调用同一个 helper。
        request_full_republish_after_recovery(app.window_mut(window).expect("window"));
        let outcome = app
            .frame(window)
            .expect("window")
            .expect("recovery republish frame");
        assert!(
            matches!(outcome, FrameOutcome::Committed(_)),
            "恢复后的整帧重发必须真的提交"
        );
        let record = app
            .window(window)
            .expect("window")
            .record()
            .expect("record");
        assert!(
            record.damage_full,
            "恢复后第一帧必须是 Plan::Full（新 persist 是空的）"
        );
        assert_eq!(
            record.stats.encoded_lanes, 3,
            "恢复后必须重编码全部 lane（新 GPU 侧实例镜像是空的）"
        );
    }
}

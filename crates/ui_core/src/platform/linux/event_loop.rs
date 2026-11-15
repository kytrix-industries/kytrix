//! 事件循环：`calloop::EventLoop` + `WaylandSource` + `calloop::ping` 唤醒
//! （`design/phase-12-macos-linux.md` §8.1）。
//!
//! 结构（单窗口 v1，与 P09 的 `Application::run` 契约一致）：
//! 1. 建 `WaylandState`（连接、协议对象、IME / 剪贴板）→ 首个 roundtrip 拿 configure；
//! 2. 所有可失败的 setup：calloop 事件循环 / 唤醒句柄 / `WaylandSource` / a11y；
//! 3. 最后建 `Renderer`（Vulkan WSI 需要 `wl_display*` / `wl_surface*`）并挂到窗口；
//! 4. 循环：drain 事件 → 同步窗口状态 → 钩子 → 按需提交 → 回投任务 → a11y → 阻塞等待。

use std::time::{Duration, Instant};

use calloop::{EventLoop, ping::make_ping};

use crate::{
    app::App,
    error::EngineError,
    platform::linux::{
        a11y::UnixA11y,
        dpi,
        surface::{self, WaylandState},
        wake,
    },
    render::backend::Renderer,
    window::FrameOutcome,
};

/// 连续帧（动画 / P17 HUD）的定时器间隔（60 Hz）。
const FRAME_TICK: Duration = Duration::from_millis(16);

/// `Application::run` 的平台入口。
pub(crate) fn run(app: App) -> Result<(), EngineError> {
    let mut app = app;
    let dispatcher = app.dispatcher();
    // 唤醒句柄由本平台的 `wake::install` 安装；`attach` 的 token 只对 Win32 有意义。
    dispatcher.attach(1);

    let (initial_size, initial_title, initial_background) = app.windows().next().map_or(
        (
            crate::math::Size::new(800.0, 600.0),
            "Kytrix",
            crate::paint::color::Color::BLACK,
        ),
        |(_, window)| {
            (
                window.size(),
                window.options().title.as_str(),
                window.options().background,
            )
        },
    );
    let (mut state, mut queue) = WaylandState::new(initial_size, initial_title)?;
    surface::initial_roundtrip(&mut queue, &mut state)?;
    surface::create_devices(&mut state)?;
    // 首个 configure 之后才知道真实尺寸。buffer 尺寸按 fractional-scale 协议取
    // `round(逻辑 × scale)`；`wp_viewport.set_destination` 由 GPU 线程在 present 前写入。
    let scale = state.scale();
    let size = state.size();
    let physical = dpi::physical_size(size, scale);
    let id = app
        .windows()
        .next()
        .map(|(id, _)| id)
        .ok_or(EngineError::AppInit {
            detail: smol_str::SmolStr::new_static("Application::run without a window"),
        })?;

    // 下面直到 `Renderer::new_for_surface` 的 setup 全都可以失败，必须排在 renderer 挂载之前：
    // `WaylandState` 持有 `wl_surface` / `wl_display`，而 renderer 的 Vulkan WSI surface /
    // `wp_viewport` 依赖它们。若 `?` 在挂载后早退，局部变量的逆序释放会让 `state` 先于
    // `app`（Window → Renderer）释放（V-122 ⑨ / P2-3）。
    let mut event_loop: EventLoop<'static, WaylandState> =
        EventLoop::try_new().map_err(|error| EngineError::AppInit {
            detail: smol_str::format_smolstr!("calloop: {error}"),
        })?;
    let (ping, ping_source) = make_ping().map_err(|error| EngineError::AppInit {
        detail: smol_str::format_smolstr!("calloop ping: {error}"),
    })?;
    event_loop
        .handle()
        .insert_source(ping_source, |_, _, _| {})
        .map_err(|error| EngineError::AppInit {
            detail: smol_str::format_smolstr!("calloop ping insert: {error}"),
        })?;
    wake::install(ping);
    let connection = state.connection().clone();
    calloop_wayland::WaylandSource::new(connection, queue)
        .insert(event_loop.handle())
        .map_err(|error| EngineError::AppInit {
            detail: smol_str::format_smolstr!("wayland source: {error}"),
        })?;

    let mut a11y = UnixA11y::new(dispatcher.clone(), id)?;

    // viewport 的所有权交给渲染器：destination 必须与 buffer 同一次 present 生效（D1 / D2）。
    let (display, surface_ptr) = state.raw_pointers();
    let renderer = Renderer::new_for_surface(
        display,
        surface_ptr,
        physical,
        initial_background,
        state.take_viewport(),
    )?;
    if let Some(window) = app.window_mut(id) {
        window.set_renderer(renderer);
        window.set_scale(scale);
        window.set_size(size);
        // `set_size` / `set_scale` 的 covering 只是平台真值写入前的估算；真实的 Wayland buffer
        // 尺寸（`round(逻辑 × scale)`）必须在这里写入（F-186 / V-122）。
        window.set_surface(physical);
    }

    dispatcher.wake();
    let mut last_scale = scale;
    let mut last_size = size;
    let result = (|| -> Result<(), EngineError> {
        loop {
            // ⓪ 窗口全关就退出：`Application::run` 的契约是「跑到没有窗口」
            // （与 Win32 的 `should_exit` / macOS 的 `windows.is_empty()` 同构）。
            // 漏了这一步时，应用自己调 `App::close_window`（如 demo 的 Esc）之后循环会永久空转（F-149）。
            if app.windows().next().is_none() {
                return Ok(());
            }
            // ① 平台事件 → App（IME / 输入 / 尺寸 / 焦点 / DPI）。
            let events = state.take_events();
            let had_events = !events.is_empty();
            for event in events {
                app.dispatch(id, event);
            }
            if had_events {
                // 与 Win32 的 pump 一致：每条平台消息之后都要唤醒，否则应用侧的状态变更
                // 要等下一次协议事件才出帧。
                dispatcher.wake();
            }
            // ①b GPU 线程反馈：设备丢失 / 呈现失败是粘性的，在这里冒泡出去
            // （`design/async-render-pipeline.md` §7.7）。所有 Vulkan 工作都在 `kytrix-vulkan`
            // 线程上，UI 线程只读反馈（I1）。
            app.poll_renderers()?;
            // ② 窗口状态镜像：**只在值变化时**设置（`set_size` 会推进 scale epoch、
            // 置 full-damage、重排根 LAYOUT，每轮无条件调用会让 UI 永久失效）。
            let scale_now = state.scale();
            let size_now = state.size();
            if let Some(window) = app.window_mut(id) {
                if scale_now != last_scale {
                    window.set_scale(scale_now);
                }
                if size_now != last_size {
                    window.set_size(size_now);
                }
                // 每轮写入真实的 Wayland buffer 尺寸（`round(逻辑 × scale)`）；同值 no-op。
                window.set_surface(dpi::physical_size(size_now, scale_now));
            }
            // 本轮的值记为「已镜像」：下一轮据此只在变化时调用 setter（②）。
            last_scale = scale_now;
            last_size = size_now;
            a11y.focus_changed(state.is_focused());
            // 交互反馈：Linux 没有全局光标 API（由客户端自绘），请求直接消费掉。
            let _ = app.take_cursor_change();
            let _ = app.take_capture_request();

            // ③ 关闭请求（`xdg_toplevel.close`）。
            if state.take_close_request() {
                app.close_window(id);
                return Ok(());
            }

            app.sync_clock();
            app.run_frame_hook();
            // ③b 自绘标题栏的窗口控制：与触发它的输入事件同一轮执行，`serial` 仍是按下时的值。
            if let Some((window, control)) = app.take_window_control()
                && window == id
            {
                match control {
                    crate::window::WindowControl::Minimize => state.minimize(),
                    crate::window::WindowControl::ToggleMaximize => state.toggle_maximize(),
                    crate::window::WindowControl::BeginMove => state.begin_move(),
                    crate::window::WindowControl::BeginResize(edge) => state.begin_resize(edge),
                }
            }
            // ④ 提交一帧。`wl_callback` 必须在 **publish 之前**请求（§7.5 第 3 条）：
            // WSI 的 `attach/commit` 发生在 GPU 线程 present 时，而 `frame` 回调只在 commit
            // 之后生效 —— 先挂回调、再把帧放进最新帧槽，GPU 线程的 present 才能让它回来。
            //
            // 先判 `frame_ready` 再消费唤醒：上一帧还没 present 时，唤醒必须留在队列里；
            // frame callback 到达后下一轮会重新看到它。反过来先消费会把 resize 的最后一帧丢掉。
            //
            // 门禁原本会在两种「publish 了但不会 commit」的情况下永久关闭（F-183）：
            // Idle（`App::frame` 返回 `Idle`，没有 publish/commit）由 `note_frame_not_committed`
            // 立即重开；present `OUT_OF_DATE`（WSI 没有 attach/commit）由下面的 200 ms 活性
            // 兜底覆盖。兜底**不是**呈现节奏：正常帧的 callback 在合成器 vsync 内返回。
            //
            // 兜底必须**每轮无条件**检查：唤醒位只在 `take_frame_wake` 被消费，而它又被
            // `frame_ready` 门控 —— 门禁关闭期间某轮已经消费过唤醒位时，用「唤醒位是否存在」
            // 门控兜底会让它永远不执行（F-183）。
            let now = Instant::now();
            if state.expire_frame_gate(now) {
                // 兜底强制开门后补一次唤醒：唤醒位可能已被上一轮取走，否则门禁虽开却没有一帧
                // 驱动重试。本轮 ④ 随后的 `take_frame_wake` 会消费这次唤醒。
                dispatcher.wake();
            }
            if state.frame_ready() && dispatcher.take_frame_wake() {
                state.request_frame();
                match app.frame(id) {
                    None | Some(Ok(FrameOutcome::Idle)) => state.note_frame_not_committed(),
                    Some(Ok(FrameOutcome::Committed(_))) => {}
                    Some(Err(error)) => return Err(error),
                }
            }
            for task in dispatcher.drain_tasks() {
                task.run(&mut app);
            }
            state.sync_ime_cursor_area(&app, id);

            // ⑤ 剪贴板发布（需要最新输入 serial）+ a11y 增量。
            let serial = state.serial();
            state.clipboard_mut().apply_pending(serial);
            a11y.set_bounds(crate::platform::linux::a11y::window_bounds(
                state.size(),
                state.scale(),
            ));
            state.update_a11y(&mut a11y, &mut app);

            // ⑥ 连续帧（动画 / P17 HUD）：定时器语义由 `dispatch` 的 timeout 提供，
            // 醒来后主动请求一帧（否则 tick 只更新状态、不出帧）。
            if app.needs_continuous_frames() {
                dispatcher.wake();
            }
            let timeout = if app.needs_continuous_frames() {
                FRAME_TICK
            } else {
                // 门禁关闭时必须睡到兜底到期（不能睡 `IDLE_TIMEOUT`）：兜底每轮无条件检查，
                // 不再依赖唤醒位留在队列里。门禁打开时 `None` ⇒ 回到事件驱动。
                state
                    .frame_gate_remaining(Instant::now())
                    .unwrap_or(surface::IDLE_TIMEOUT)
            };
            // 应用自己关窗（Esc / 标题栏关闭按钮）后不能停在下面 1h 的空闲等待里。
            if app.windows().next().is_none() {
                return Ok(());
            }
            event_loop
                .dispatch(timeout, &mut state)
                .map_err(|error| EngineError::AppInit {
                    detail: smol_str::format_smolstr!("calloop dispatch: {error}"),
                })?;
        }
    })();
    // 循环路径：`WaylandState` 持有 `wl_surface` / `wl_display`，它必须**晚于** renderer 释放，
    // 所以先显式 drop `app`（Window → Renderer），再返回。
    //
    // 对 `?` 早退路径不需要这个兜底：所有可失败的 setup 都排在 `window.set_renderer` 之前，
    // 早退时 `app` 里还没有 renderer（之后没有任何 `?`），`state` 先释放不会遇到
    // 「renderer 晚于 `wl_surface` 释放」的逆序（V-122 ⑨ / P2-3）。
    // panic 展开仍可能让局部 `state` 先于参数 `app` 释放（既有；release 是 `panic=abort`），
    // 本兜底不覆盖该路径。
    drop(app);
    result
}

mod calloop_wayland;

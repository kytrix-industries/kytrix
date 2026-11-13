//! 事件循环：`calloop::EventLoop` + `WaylandSource` + `calloop::ping` 唤醒
//! （`design/phase-12-macos-linux.md` §8.1）。
//!
//! 结构（单窗口 v1，与 P09 的 `Application::run` 契约一致）：
//! 1. 建 `WaylandState`（连接、协议对象、IME / 剪贴板）→ 首个 roundtrip 拿 configure；
//! 2. 建 `Renderer`（Vulkan WSI 需要 `wl_display*` / `wl_surface*`）；
//! 3. 循环：drain 事件 → 同步窗口状态 → 钩子 → 按需提交 → 回投任务 → a11y → 阻塞等待。

use std::time::Duration;

use calloop::{EventLoop, ping::make_ping};

use crate::{
    app::App,
    error::EngineError,
    platform::linux::{
        a11y::UnixA11y,
        surface::{self, WaylandState},
        wake, window,
    },
    render::backend::Renderer,
};

/// 连续帧（动画 / P17 HUD）的定时器间隔（60 Hz）。
const FRAME_TICK: Duration = Duration::from_millis(16);

/// `Application::run` 的平台入口。
pub(crate) fn run(app: App) -> Result<(), EngineError> {
    let mut app = app;
    let dispatcher = app.dispatcher();
    // 唤醒句柄由本平台的 `wake::install` 安装；`attach` 的 token 只对 Win32 有意义。
    dispatcher.attach(1);

    let (mut state, mut queue) = WaylandState::new()?;
    surface::initial_roundtrip(&mut queue, &mut state)?;
    surface::create_devices(&mut state)?;
    // 首个 configure 之后才知道真实尺寸；`wp_viewport.set_destination` 让 compositor 做分数缩放。
    let scale = state.scale();
    let size = state.size();
    let (display, surface_ptr) = state.raw_pointers();
    let physical = crate::platform::linux::dpi::physical_size(size, scale);
    let renderer = Renderer::new_for_surface(display, surface_ptr, physical, scale.factor())?;

    let id = app
        .windows()
        .next()
        .map(|(id, _)| id)
        .ok_or(EngineError::AppInit {
            detail: smol_str::SmolStr::new_static("Application::run without a window"),
        })?;
    if let Some(window) = app.window_mut(id) {
        window.set_renderer(renderer);
        window.set_scale(scale);
        window.set_size(size);
    }
    window::apply_min_size(state.toplevel(), scale);

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
            }
            if scale_now != last_scale {
                last_scale = scale_now;
                window::apply_min_size(state.toplevel(), scale_now);
            }
            if size_now != last_size {
                last_size = size_now;
            }
            // 分数缩放落地：逻辑尺寸交给 `wp_viewport.set_destination`（compositor 做像素缩放）。
            state.sync_viewport();
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
            // ④ 提交一帧。`wl_callback` 必须在**提交（present 触发 commit）之前**请求，
            // 否则 `done` 永远不会到达（Wayland 的 frame 回调只在 commit 之后生效）。
            if dispatcher.take_frame_wake() && state.frame_ready() {
                state.request_frame();
                match app.frame(id) {
                    None | Some(Ok(_)) => {}
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
                surface::IDLE_TIMEOUT
            };
            event_loop
                .dispatch(timeout, &mut state)
                .map_err(|error| EngineError::AppInit {
                    detail: smol_str::format_smolstr!("calloop dispatch: {error}"),
                })?;
        }
    })();
    // `WaylandState` 持有 `wl_surface` / `wl_display`：它必须**晚于** renderer 释放，
    // 所以先显式 drop `app`（Window → Renderer），再返回。
    drop(app);
    result
}

mod calloop_wayland;

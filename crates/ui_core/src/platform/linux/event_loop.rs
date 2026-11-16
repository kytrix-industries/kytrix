//! 事件循环：`calloop::EventLoop` + `WaylandSource` + `calloop::ping` 唤醒
//! （`design/phase-12-macos-linux.md` §8.1）。
//!
//! 结构（多窗口，P1-2.8 Stage C-2b）：
//! 1. 建 `WaylandState`（连接、协议工厂、首个窗口、IME / 剪贴板）→ 首个 roundtrip 拿 configure；
//! 2. 所有可失败的 setup：calloop 事件循环 / 唤醒句柄 / `WaylandSource`；
//! 3. 每轮泵开头把 `App` 的窗口集合同步到原生窗口：新增窗口建 `wl_surface` / toplevel 与
//!    渲染器，`App` 已关闭的窗口销毁协议对象（[`sync_windows`]）；
//! 4. 循环：drain 事件 → 逐窗口镜像 / 关闭 / 帧 → 钩子 → 任务 → a11y → 阻塞等待。

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use calloop::{EventLoop, ping::make_ping};

use crate::{
    app::App,
    error::EngineError,
    math::{Scale, Size},
    platform::{
        dispatcher::Dispatcher,
        linux::{
            a11y::{UnixA11y, window_bounds},
            dpi,
            surface::{self, WaylandState},
            wake,
        },
    },
    render::backend::Renderer,
    window::{FrameOutcome, WindowControl, WindowId},
};

/// 连续帧（动画 / P17 HUD）的定时器间隔（60 Hz）。
const FRAME_TICK: Duration = Duration::from_millis(16);

/// 一个窗口的 a11y adapter。
///
/// `accesskit_unix::Adapter` 表示**单个窗口**的无障碍树（Action 回投也绑定窗口 id），所以
/// 多窗口下每窗口一份；生命周期跟随 `WaylandState::windows` 的对应窗口。
struct WindowA11y {
    id: WindowId,
    adapter: UnixA11y,
}

/// 平台真值 → `Window` 的镜像记账（每窗口一份）。
///
/// `Window::set_scale` / `set_size` 会推进 scale epoch、置全量 damage 并重排根 LAYOUT ——
/// 每轮无条件调用会让 UI 永久失效，所以只在平台值**变化时**写回。
struct WindowMirror {
    id: WindowId,
    scale: Scale,
    size: Size,
}

/// `Application::run` 的平台入口。
pub(crate) fn run(app: App) -> Result<(), EngineError> {
    let mut app = app;
    let dispatcher = app.dispatcher();
    // 唤醒句柄由本平台的 `wake::install` 安装；`attach` 的 token 只对 Win32 有意义。
    dispatcher.attach(1);

    // `WaylandState::new` 要先建**一个**原生窗口（连接、协议工厂与首个 surface 的初始化在同
    // 一条路径上）；其余窗口（含启动时就存在的）由下面的 `sync_windows` 统一建。
    let (first_id, first_size, first_title) = app
        .windows()
        .next()
        .map(|(id, window)| (id, window.size(), window.options().title.clone()))
        .ok_or(EngineError::AppInit {
            detail: smol_str::SmolStr::new_static("Application::run without a window"),
        })?;
    let (mut state, mut queue) = WaylandState::new(first_size, first_title.as_str(), first_id)?;
    surface::initial_roundtrip(&mut queue, &mut state)?;
    surface::create_devices(&mut state)?;

    // 下面直到 `WaylandSource` 的 setup 全都可以失败，必须排在**任何渲染器创建之前**：
    // `WaylandState` 持有 `wl_surface` / `wl_display`，而 renderer 的 Vulkan WSI surface /
    // `wp_viewport` 依赖它们。渲染器在 `sync_windows` 里创建，之后所有可失败步骤都在闭包内，
    // 闭包外的 `drop(app)` 保证 `Window → Renderer` 先于 `WaylandState` 释放（V-122 ⑨ / P2-3）。
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

    // 每窗口一份的 UI 侧状态：a11y adapter 与平台真值镜像。两者都与 `WaylandState::windows`
    // 一一对应，生命周期统一由 `sync_windows` 维护（新增窗口补、关闭窗口删）。
    let mut a11ys: Vec<WindowA11y> = Vec::new();
    let mut mirrors: Vec<WindowMirror> = Vec::new();
    let result = (|| -> Result<(), EngineError> {
        // 启动路径与运行期共用同一条同步：首个窗口已在 `WaylandState` 里（只差渲染器），
        // 启动时就存在的其它窗口在这里建原生对象。
        sync_windows(&mut app, &mut state, &mut a11ys, &mut mirrors, &dispatcher)?;
        // 首帧：窗口集合就绪之后请求一帧（与 Win32 / macOS 泵的 `dispatcher.wake()` 同构）。
        dispatcher.wake();
        // 因**本窗口**门禁关着而被推迟的帧（唤醒位已被别的窗口消费时，该窗口的这一帧不能丢）。
        // 窗口门禁重开（`wl_callback.done`）后不需要新的唤醒，由这里补一帧。
        let mut deferred_frames: Vec<WindowId> = Vec::new();
        loop {
            // ⓪ 窗口生命周期同步（P1-2.8 Stage C-2b）：`App` 里新增的窗口建原生对象 + 渲染器，
            // `App` 已关闭的窗口销毁协议对象。必须排在本轮任何事件 / 帧处理之前：后面的逐窗口
            // 循环只处理同步后的集合。
            sync_windows(&mut app, &mut state, &mut a11ys, &mut mirrors, &dispatcher)?;
            // 窗口全关就退出：`Application::run` 的契约是「跑到没有窗口」
            // （与 Win32 的 `should_exit` / macOS 的 `windows.is_empty()` 同构）。
            // 漏了这一步时，应用自己调 `App::close_window`（如 demo 的 Esc）之后循环会永久空转（F-149）。
            if app.windows().next().is_none() {
                return Ok(());
            }
            // ① 平台事件 → App（IME / 输入 / 尺寸 / 焦点 / DPI）。
            // 先合成到期的键盘重复：Wayland 不替客户端产生重复，缺少这一步长按
            // Backspace/方向键只生效一次（P1-2.11）。合成结果直接进 `state.events`，
            // 与真实输入事件在同一轮一起派发。
            state.emit_due_key_repeats(Instant::now());
            let events = state.take_events();
            let had_events = !events.is_empty();
            // 每个事件**自带**窗口 id（P1-2.8 Stage C-2）：多窗口下同一批里可能混着不同窗口的
            // 事件，不能再用「当前窗口」统一派发。
            for (target, event) in events {
                app.dispatch(target, event);
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
            // ② 逐窗口镜像平台真值：**只在值变化时**设置（`set_scale` 推进 scale epoch；二者置
            // full-damage、重排根 LAYOUT，每轮无条件调用会让 UI 永久失效）。没有渲染器的窗口还没
            // 有镜像记账（首个 configure 之后才建），由 `sync_windows` 负责初次写入。
            for mirrored in &mut mirrors {
                let Some(window) = app.window_mut(mirrored.id) else {
                    continue;
                };
                let Some(index) = state.window_index(mirrored.id) else {
                    continue;
                };
                let scale_now = state.scale(index);
                if scale_now != mirrored.scale {
                    window.set_scale(scale_now);
                    mirrored.scale = scale_now;
                }
                let size_now = state.size(index);
                if size_now != mirrored.size {
                    window.set_size(size_now);
                    mirrored.size = size_now;
                }
                // 整数回退的 buffer scale（fractional 模式恒 1）；同值 no-op。每轮写入保证
                // scale 变化后第一帧的 buffer_scale 与 buffer 尺寸同帧成对（V-110）。
                window.set_buffer_scale(state.buffer_scale(index));
                // 每轮写入真实的 Wayland buffer 尺寸（`round(逻辑 × scale)`）；同值 no-op。
                window.set_surface(dpi::physical_size(size_now, scale_now));
            }
            // ②b 焦点状态 → a11y（键盘焦点窗口可能不同于指针进入窗口）。
            for window_a11y in &mut a11ys {
                let Some(index) = state.window_index(window_a11y.id) else {
                    continue;
                };
                window_a11y.adapter.focus_changed(state.is_focused(index));
            }
            // 交互反馈：Linux 没有全局光标 API（由客户端自绘），请求直接消费掉。
            let _ = app.take_cursor_change();
            let _ = app.take_capture_request();

            // ③ 关闭请求（`xdg_toplevel.close`）：先关 `App` 窗口（drop 渲染器 → GPU 线程
            // join），再销毁原生窗口（drop 协议对象），顺序与窗口销毁的依赖方向一致。
            let mut closing: Vec<WindowId> = Vec::new();
            for index in 0..state.window_count() {
                if state.take_close_request(index) {
                    closing.push(state.window_id(index));
                }
            }
            for id in closing {
                app.close_window(id);
                state.remove_window(id);
                a11ys.retain(|entry| entry.id != id);
                mirrors.retain(|entry| entry.id != id);
            }
            // 应用自己关窗（Esc / 标题栏关闭按钮）后不能停在下面 1h 的空闲等待里。
            if app.windows().next().is_none() {
                return Ok(());
            }

            app.sync_clock();
            app.run_frame_hook();
            // ③b 自绘标题栏的窗口控制：与触发它的输入事件同一轮执行，`serial` 仍是按下时的值。
            if let Some((window, control)) = app.take_window_control()
                && let Some(index) = state.window_index(window)
            {
                match control {
                    WindowControl::Minimize => state.minimize(index),
                    WindowControl::ToggleMaximize => state.toggle_maximize(index),
                    WindowControl::BeginMove => state.begin_move(index),
                    WindowControl::BeginResize(edge) => state.begin_resize(index, edge),
                }
            }
            // ④ 逐窗口提交一帧。`wl_callback` 必须在 **publish 之前**请求（§7.5 第 3 条）：
            // WSI 的 `attach/commit` 发生在 GPU 线程 present 时，而 `frame` 回调只在 commit
            // 之后生效 —— 先挂回调、再把帧放进最新帧槽，GPU 线程的 present 才能让它回来。
            //
            // 只有**已挂渲染器**的窗口可以出帧：新建窗口在首个 configure 之前没有 swapchain
            // （未 configure 的 `xdg_surface` 上 attach buffer 是协议错误），`sync_windows` 会等
            // configure 到达后再建渲染器。
            //
            // 先判 `frame_ready` 再消费唤醒：上一帧还没 present 时，唤醒必须留在队列里；
            // frame callback 到达后下一轮会重新看到它。反过来先消费会把 resize 的最后一帧丢掉。
            //
            // 唤醒位是**进程级**的一个电平位：某个窗口门禁关着时，不能让它被别的窗口的
            // `take_frame_wake` 吞掉 —— 否则该窗口的输入更新要等下一次无关唤醒。所以门禁关着
            // 的窗口记进 `deferred_frames`，回调开门后**不依赖新唤醒**补一帧；没有推迟项时
            // 其它已开门的窗口不会被反复重画。
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
            if state.expire_frame_gates(now) {
                // 兜底强制开门后补一次唤醒：唤醒位可能已被上一轮取走，否则门禁虽开却没有一帧
                // 驱动重试。本轮 ④ 随后的 `take_frame_wake` 会消费这次唤醒。
                dispatcher.wake();
            }
            let frame_windows: Vec<(usize, WindowId)> = (0..state.window_count())
                .map(|index| (index, state.window_id(index)))
                .filter(|(_, id)| app.window(*id).is_some_and(|window| window.has_renderer()))
                .collect();
            // 已关闭的窗口不再补帧（推迟项跟随窗口生命周期）。
            deferred_frames.retain(|id| state.window_index(*id).is_some());
            if frame_windows
                .iter()
                .any(|(index, _)| state.frame_ready(*index))
            {
                let wake = dispatcher.take_frame_wake();
                for (index, id) in frame_windows {
                    if !state.frame_ready(index) {
                        // 本轮出不了帧：有新唤醒待画就记下，门禁重开后补（见上面的注释）。
                        if wake && !deferred_frames.contains(&id) {
                            deferred_frames.push(id);
                        }
                        continue;
                    }
                    if !wake && !deferred_frames.contains(&id) {
                        continue;
                    }
                    deferred_frames.retain(|deferred| *deferred != id);
                    state.request_frame(index);
                    match app.frame(id) {
                        None | Some(Ok(FrameOutcome::Idle)) => {
                            state.note_frame_not_committed(index);
                        }
                        Some(Ok(FrameOutcome::Committed(_))) => {}
                        Some(Err(error)) => return Err(error),
                    }
                }
            }
            for task in dispatcher.drain_tasks() {
                task.run(&mut app);
            }
            // ⑤ IME caret hint（逐窗口）：每轮泵调用，独立于帧门禁。
            for index in 0..state.window_count() {
                let id = state.window_id(index);
                state.sync_ime_cursor_area(&app, id);
            }

            // ⑥ 剪贴板发布（需要最新输入 serial）+ a11y 增量。
            let serial = state.serial();
            state.clipboard_mut().apply_pending(serial);
            for window_a11y in &mut a11ys {
                let Some(index) = state.window_index(window_a11y.id) else {
                    continue;
                };
                window_a11y
                    .adapter
                    .set_bounds(window_bounds(state.size(index), state.scale(index)));
                let id = window_a11y.id;
                if app.a11y_pending(id) {
                    window_a11y
                        .adapter
                        .update(|| crate::a11y::adapter::build_update(&mut app, id));
                }
            }

            // ⑦ 连续帧（动画 / P17 HUD）：定时器语义由 `dispatch` 的 timeout 提供，
            // 醒来后主动请求一帧（否则 tick 只更新状态、不出帧）。
            if app.needs_continuous_frames() {
                dispatcher.wake();
            }
            let timeout = if app.needs_continuous_frames() {
                FRAME_TICK
            } else {
                // 门禁关闭时必须睡到最早的兜底到期（不能睡 `IDLE_TIMEOUT`）：兜底每轮无条件
                // 检查，不再依赖唤醒位留在队列里。门禁全开时 `None` ⇒ 回到事件驱动。
                state
                    .frame_gate_remaining(Instant::now())
                    .unwrap_or(surface::IDLE_TIMEOUT)
            };
            // 有按住键时把等待压到下一次重复的到期时刻：否则 `dispatch` 会睡到空闲超时，
            // 重复键要等到下一个无关事件才被合成（P1-2.11）。
            let timeout = match state.next_key_repeat_in(Instant::now()) {
                Some(due) => timeout.min(due),
                None => timeout,
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
    // `WaylandState` 持有 `wl_surface` / `wl_display`，它必须**晚于** renderer 释放，
    // 所以先显式 drop `app`（Window → Renderer 的 GPU 线程 join），再返回。闭包内所有
    // 可失败步骤（含运行期建窗 / 出帧）都经过这条路径。
    drop(app);
    result
}

/// 把 `App` 的窗口集合同步到 Wayland 原生窗口、渲染器与 a11y adapter（P1-2.8 Stage C-2b）。
///
/// 每轮泵开头调用，三个方向：
/// - **移除**：`WaylandState` 有、`App` 已没有的窗口 ⇒ [`WaylandState::remove_window`]
///   （drop 协议对象），连同 a11y adapter 与镜像记账一起删掉；
/// - **新增**：`App` 有、`WaylandState` 没有的窗口 ⇒ [`WaylandState::add_window`]
///   （`wl_surface` → `xdg_surface` → toplevel → 装饰 / fractional / viewport），并补 a11y adapter；
/// - **渲染器**：窗口已拿到首个 configure 但 `Window` 还没有渲染器 ⇒
///   `Renderer::new_for_surface`，并把尺寸 / scale / buffer scale / 真实 buffer 尺寸写回
///   `Window`（与单窗口时代的启动路径同一套写入顺序）。
fn sync_windows(
    app: &mut App,
    state: &mut WaylandState,
    a11ys: &mut Vec<WindowA11y>,
    mirrors: &mut Vec<WindowMirror>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<(), EngineError> {
    // 先移除：之后「新增」与「渲染器」两段都可以假定 `WaylandState` 里的窗口在 `App` 中存在。
    let stale: Vec<WindowId> = (0..state.window_count())
        .map(|index| state.window_id(index))
        .filter(|id| app.window(*id).is_none())
        .collect();
    for id in stale {
        state.remove_window(id);
        a11ys.retain(|entry| entry.id != id);
        mirrors.retain(|entry| entry.id != id);
    }
    let missing: Vec<(WindowId, Size, smol_str::SmolStr)> = app
        .windows()
        .filter(|(id, _)| state.window_index(*id).is_none())
        .map(|(id, window)| (id, window.size(), window.options().title.clone()))
        .collect();
    for (id, size, title) in missing {
        state.add_window(id, size, title.as_str())?;
    }
    // a11y adapter 与协议对象同生命周期：`App` 里每个窗口在 `WaylandState` 里有且仅有一个。
    for index in 0..state.window_count() {
        let id = state.window_id(index);
        if a11ys.iter().any(|entry| entry.id == id) {
            continue;
        }
        a11ys.push(WindowA11y {
            id,
            adapter: UnixA11y::new(dispatcher.clone(), id)?,
        });
    }
    // 渲染器只能在首个 configure 之后建：configure 之前 `xdg_surface` 没有配置的尺寸，WSI
    // 也不能在未 configure 的 surface 上 present。`is_configured` 由 `xdg_toplevel.configure`
    // 置位（事件循环收到事件、下一轮同步时创建渲染器）。
    let renderable: Vec<(usize, WindowId)> = (0..state.window_count())
        .map(|index| (index, state.window_id(index)))
        .filter(|(index, id)| {
            state.is_configured(*index)
                && app.window(*id).is_some_and(|window| !window.has_renderer())
        })
        .collect();
    for (index, id) in renderable {
        let size = state.size(index);
        let scale = state.scale(index);
        // buffer 尺寸按 `round(逻辑 × scale)` 取：fractional 模式下 `wp_viewport.set_destination`
        // 由 GPU 线程在 present 前写入；整数回退下 `wl_surface.set_buffer_scale(N)` 与 buffer
        // 同一次 present 生效（V-110）。
        let physical = dpi::physical_size(size, scale);
        let buffer_scale = state.buffer_scale(index);
        let (display, surface_ptr) = state.raw_pointers(index);
        // viewport 的所有权交给渲染器：destination 必须与 buffer 同一次 present 生效（D1 / D2）。
        let viewport = state.take_viewport(index);
        let surface = state.surface_clone(index);
        let Some(background) = app.window(id).map(|window| window.options().background) else {
            continue;
        };
        let renderer = Renderer::new_for_surface(
            display,
            surface_ptr,
            physical,
            background,
            viewport,
            surface,
        )?;
        let Some(window) = app.window_mut(id) else {
            continue;
        };
        // `set_renderer` 先清空图集页记账（新渲染器 = 一套空纹理 ⇒ 两张页都要整页重传）；
        // `set_surface` 会把 `full_damage_pending` 置位（新交换链必须整帧）。
        window.set_renderer(renderer);
        window.set_scale(scale);
        window.set_size(size);
        // 整数回退的 `wl_surface` buffer scale：GPU 线程在 present 前与 buffer 同一次生效
        // （fractional 模式恒 1）。必须在 `set_surface` 之前写入，第一帧就带上正确的 scale。
        window.set_buffer_scale(buffer_scale);
        // `set_size` / `set_scale` 的 covering 只是平台真值写入前的估算；真实的 Wayland buffer
        // 尺寸（`round(逻辑 × scale)`）必须在这里写入（F-186 / V-122）。
        window.set_surface(physical);
        mirrors.push(WindowMirror { id, scale, size });
    }
    Ok(())
}

mod calloop_wayland;

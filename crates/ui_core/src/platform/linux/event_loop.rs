//! 事件循环：`calloop::EventLoop` + `WaylandSource` + `calloop::ping` 唤醒
//!
//! 结构（多窗口，P1-2.8 Stage C-2b）：
//! 1. 建 `WaylandState`（连接、协议工厂、首个窗口、IME / 剪贴板 / 光标）→ 首个 roundtrip 拿 configure；
//! 2. 所有可失败的 setup：calloop 事件循环 / 唤醒句柄 / `WaylandSource`；
//! 3. 每轮泵开头把 `App` 的窗口集合同步到原生窗口：新增窗口建 `wl_surface` / toplevel 与
//!    渲染器，`App` 已关闭的窗口销毁协议对象（[`sync_windows`]）；
//! 4. 循环：drain 事件 → 逐窗口镜像 / 关闭 / 帧 → 钩子 → 任务 → IME 策略 → a11y → 阻塞等待。
//!
//! R10 整改的四处接线：
//!
//! - **光标**（R10-3）：`app.take_cursor_change()` 不再被丢弃，而是写进 `WaylandState`
//!   （按窗口记账），指针在该窗口上时立即 `set_shape` / `set_cursor`；
//! - **可见性**（R10-7）：不可见窗口既不请求 frame callback，也不启动 200 ms 活性兜底；
//! - **剪贴板 send**（R10-17）：`PendingSend` 被注册成 calloop 的可写事件源，主线程永不阻塞；
//! - **IME 降级**（R10-2）：`enable` 之后 1.5 s 没有 IM 事件 ⇒ 文本通路切回键盘，并把等待时间
//!   压进 `dispatch` 的 timeout（否则要等下一个无关事件才降级）。
//! - **强类型错误**（R10-16）：运行期 `dispatch` 失败按 `calloop::Error::IoError` 分类
//!   （`EPIPE` ⇒ 合成器消失、`EPROTO` ⇒ 协议错误）。
//!
//! P1 补充接线：
//!
//! - **关闭请求事件**（B11）：`xdg_toplevel.close` 先经
//!   [`crate::platform::wayland::close::handle_close_requests`] 投递 `Event::CloseRequested`，
//!   再跑 `on_close` 裁决（与 Windows / macOS 同序）；
//! - **IME 策略**（B9）：每轮在 `event_loop.dispatch` **之前** `sync_ime_policy`：焦点离开可编辑
//!   文本或应用 `set_ime_enabled(false)` 时，同一轮内下发 `zwp_text_input_v3.disable()`。
//! - **configure ack 时序**（B10）：初始 configure 在 `xdg_surface` 事件里立即 ack；改变
//!   尺寸/状态的 configure 由 [`crate::platform::wayland::configure::ConfigureLatch`] 记住
//!   serial，等使用该状态渲染的帧发布（`App::frame` 返回 `Committed`）后才
//!   `ack_configure`。不可见 / 没渲染器 / `Idle` 的窗口没有帧可配对，在本轮 force ack
//!   （否则静止窗口的 configure 永远不确认）。
//! - **自绘标题栏拖动区**（X2）：泵在把平台事件交给 `App` **之前**逐条判定
//!   （[`crate::platform::wayland::drag`]）：主键按下命中 `Window::drag_regions()` ⇒
//!   `WaylandState::begin_move` 发 `xdg_toplevel.move(seat, serial)`，该次 `PointerDown`
//!   不派发（拖动区是窗口 chrome，不是内容）；区域之外完全不变。
//! - **呈现证据**（A4）：GPU 线程在 present 前请求 `wp_presentation.feedback`（帧序号写进
//!   反馈 user data），终态事件由本泵在 `dispatch` 里派发；每轮 `poll_renderers` 之后把
//!   presented/discarded 按 `FrameIndex` 回填窗口呈现历史。

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use calloop::{EventLoop, ping::make_ping};

use crate::{
    app::App,
    error::{EngineError, WaylandErrorKind},
    math::{Scale, Size},
    platform::{
        dispatcher::Dispatcher,
        linux::{
            a11y::UnixA11y,
            clipboard::PendingSend,
            dpi,
            surface::{self, WaylandState},
            wake,
        },
        wayland::{close, drag},
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
    /// 最近一次下发的标题（`App::set_title` → `xdg_toplevel.set_title`；同值 no-op，E6）。
    title: smol_str::SmolStr,
}

/// `Application::run` 的平台入口。
///
/// 输入阈值（R02-15）在 Linux 上**没有**系统查询通路，本引擎也**不引入** XSettings / GTK
/// 依赖：GTK 的 `gtk-double-click-time` 在 XSettings 里（需要 xsettings 后端），Qt 走同一后端。
/// 因此这里保留 [`crate::interaction::InputThresholds::platform_default`] 的平台缺省
/// （400 ms / 4 px），而不是静默地沿用 Windows 的 500 ms —— 这是一个**明确声明**的偏离：
/// 用户在系统设置里调过双击速度时，引擎的判定会与其它应用不一致（见
/// [`crate::interaction::InputThresholds`] 的文档表）。
pub(crate) fn run(app: App) -> Result<(), EngineError> {
    let dispatcher = app.dispatcher();
    // 唤醒句柄由本平台的 `wake::install` 安装；`attach` 的 token 只对 Win32 有意义。
    dispatcher.attach(1);

    // `WaylandState::new` 要先建**一个**原生窗口（连接、协议工厂与首个 surface 的初始化在同
    // 一条路径上）；其余窗口（含启动时就存在的）由下面的 `sync_windows` 统一建。
    //
    // 取**最小 id**（= 最早创建）而不是 `App` 哈希表迭代到的任意一条：X1 的 owner 语义要求
    // owner 先于 owned 窗口建原生对象（`xdg_toplevel.set_parent` 只能引用已存在的 toplevel）。
    // `WindowId` 单调递增（永不复用）⇒ 最早创建的窗口必是本批其它窗口可能的 owner；`sync_windows`
    // 对剩余窗口按同一顺序排序（见那里的注释）。
    let (first_id, first_options) = app
        .windows()
        .min_by_key(|(id, _)| id.to_bits())
        .map(|(id, window)| (id, window.options().clone()))
        .ok_or(EngineError::AppInit {
            detail: smol_str::SmolStr::new_static("Application::run without a window"),
        })?;
    let (mut state, mut queue) = WaylandState::new(first_id, &first_options)?;
    surface::initial_roundtrip(&mut queue, &mut state)?;
    surface::create_devices(&mut state)?;
    // **`app` 必须作为局部变量声明在 `state` 之后**：Rust 的局部变量按声明逆序析构，而函数
    // 参数最后析构。旧实现在第一行 `let mut app = app;`，于是 panic 展开时 `state`
    // （`wl_surface` / `xdg_toplevel` 等协议对象）**先于** `app`（`Window → Renderer`）释放，
    // GPU 线程随后对已销毁的 surface 调 `wp_viewport.destroy()` / present（V-122 ⑨ 的
    // 展开路径缺口）。声明顺序改对之后，展开时 `app` 先析构、渲染器 join 早于协议对象销毁。
    let mut app = app;

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
    // calloop 的 LoopHandle：剪贴板的异步写出任务（R10-17）要动态注册可写事件源。
    let loop_handle = event_loop.handle();

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
                // X2：自绘标题栏拖动区内的**主键按下**转成 `xdg_toplevel.move(seat, serial)`，
                // 交给 compositor 驱动移动，**不投递** `PointerDown` —— 拖动区是窗口 chrome，
                // 不是内容：应用收不到这次按下 ⇒ 不开始指针会话、不产生 click / drag 动作；
                // 之后同一按键的 motion / up 由 compositor 的隐式抓取接管。
                //
                // 判定放在泵里（而不是 `wl_pointer.button` 的协议回调里）：那里只有
                // `WaylandState`、拿不到 `App` 的拖动区；泵在派发前读 `Window::drag_regions()`
                // 的当前值（应用在上一轮响应里设置），与 Windows 平台层「每轮对账」同一时机。
                //
                // serial：`WaylandState::begin_move` 用最近一次指针按键的 serial（协议要求隐式
                // 抓取请求带输入事件的 serial）；同一批里多个按键的极端情况取最后一条，仍是
                // 本 seat 的有效输入 serial。
                //
                // 降级：compositor 不实现交互移动时 `move` 是无副作用的 no-op（协议没有
                // 「不支持」错误）；应用仍可用 `WindowControl::BeginMove` 的显式路径。
                if let Some(position) = drag::drag_press_position(&event)
                    && app
                        .window(target)
                        .is_some_and(|window| drag::hit(window.drag_regions(), position))
                    && let Some(index) = state.window_index(target)
                {
                    state.begin_move(index);
                    continue;
                }
                app.dispatch(target, event);
            }
            if had_events {
                // 与 Win32 的 pump 一致：每条平台消息之后都要唤醒，否则应用侧的状态变更
                // 要等下一次协议事件才出帧。
                dispatcher.wake();
            }
            // ①b GPU 线程反馈：设备丢失 / 呈现失败是粘性的，在这里冒泡出去
            // 所有 Vulkan 工作都在 `kytrix-vulkan`
            // 线程上，UI 线程只读反馈（I1）。
            app.poll_renderers()?;
            // ①c A4：`wp_presentation` 的 presented/discarded 在上一轮末尾的 `dispatch` 里进了
            // 各窗口的呈现邮箱；这里按 `FrameIndex` 回填窗口呈现历史（完成反馈已在 ①b 落账，
            // 事件晚于它到达；还没出现在历史里的条目留在邮箱里下一轮再取）。
            state.backfill_presentation(&mut app);
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
                // A5：帧预算的刷新率提示 = 窗口已 enter 的 output 的当前模式（`wl_output.mode`，
                // mHz）；compositor 没发 mode 时为 `None` ⇒ 预算门禁保持禁用（退回唤醒即出帧，
                // Linux 的逐窗口 `wl_callback` 门禁仍然承担 present 节奏）。
                if let Some(hz) = state.refresh_hz(index) {
                    window.set_frame_refresh_hz(hz);
                }
                // RA-5：不可见窗口不参与帧循环（`frame_windows` 过滤掉它），推迟标记必须清掉；
                // 否则 `next_deferred_deadline()` 一直返回已过期时刻 ⇒ calloop timeout=0 空转。
                if !state.is_visible(index) {
                    window.clear_frame_budget_deferred();
                }
                // ③ R10-4 / R10-15：compositor 侧的窗口状态（装饰模式、最大化/全屏、可用能力）
                // 镜像给应用 —— 自绘标题栏据此决定是否画、按钮是否可用。值变化时
                // `Window::set_state` 已给根节点标 `PAINT`，这里再请求一帧（§8.6.4 / I3）。
                if window.set_state(state.window_state(index)) {
                    dispatcher.wake();
                }
                // E6：运行时标题（`App::set_title`）对账到 `xdg_toplevel.set_title`；同值 no-op。
                // 标题是双缓冲 toplevel 状态，随下一次 commit（present）生效。
                let title = window.options().title.clone();
                if title != mirrored.title {
                    state.set_title(index, &title);
                    mirrored.title = title;
                }
            }
            // ②b 焦点状态 → a11y（键盘焦点窗口可能不同于指针进入窗口）。
            for window_a11y in &mut a11ys {
                let Some(index) = state.window_index(window_a11y.id) else {
                    continue;
                };
                window_a11y.adapter.focus_changed(state.is_focused(index));
            }
            // R10-3：光标不再是「丢弃请求」。Wayland 上光标图像只能由客户端提供：
            // `wp_cursor_shape_v1`（首选，无 buffer 管理）或 `wl_pointer.set_cursor` + 主题图像。
            // 这里把应用的请求按窗口记账，指针停在该窗口上时立即下发（serial 用最近一次
            // `wl_pointer.enter`）。
            if let Some((window, cursor)) = app.take_cursor_change() {
                state.set_cursor_shape(window, cursor);
            }
            // 指针捕获（R02-8）：Wayland 的 `wl_pointer` 在按下期间由 compositor 做**隐式抓取**，
            // 客户端没有可调用的捕获 API（协议不提供 `setPointerCapture` 的对应物）；引擎侧的
            // 捕获状态（事件路由 + `PointerCancel` 语义）照常生效。请求被消费是正确行为，
            // 这一点在 `App::capture_pointer` 的文档里明确写出。
            let _ = app.take_capture_requests();
            // R10-2：`enable` 之后 1.5 s 内没有任何 IM 事件 ⇒ 本 seat 没有输入法引擎，文本
            // 通路切回键盘（否则字符输入全灭）。降级本身不需要出帧，但会让下一轮 timeout 恢复。
            state.degrade_text_input_if_silent(Instant::now());
            // R10-17：剪贴板的 `wl_data_source.send` 不再在主线程 `write_all`。把待写任务交给
            // calloop 的可写事件源（管道阻塞 fd，每次最多写 PIPE_BUF ⇒ 绝不阻塞事件循环）。
            for pending in state.clipboard_mut().take_pending_sends() {
                // 注册失败（例如 fd 已被对端关闭）：源被丢弃 ⇒ fd 关闭 ⇒ 对端读到 EOF，
                // 这正是协议要求的「不提供数据」。
                let _ = register_pending_send(&loop_handle, pending);
            }

            // ③ 关闭请求（`xdg_toplevel.close`）：先投递 `Event::CloseRequested`（通知），再由
            // `App::request_close` 裁决（与 Windows / macOS 同一顺序，B11）；只有批准关闭的窗口
            // 才销毁原生对象 —— 先关 `App` 窗口（drop 渲染器 → GPU 线程 join），再 drop 协议
            // 对象，顺序与窗口销毁的依赖方向一致。
            let mut closing: Vec<WindowId> = Vec::new();
            for index in 0..state.window_count() {
                if state.take_close_request(index) {
                    closing.push(state.window_id(index));
                }
            }
            for id in close::handle_close_requests(&mut app, &closing) {
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
                    WindowControl::ToggleFullscreen => state.toggle_fullscreen(index),
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
            // 立即重开；present `OUT_OF_DATE`（WSI 没有 attach/commit）由 GPU 线程**保留待画帧
            // 重试**覆盖（`render/linux/renderer.rs` 的 `Submitted::Retry`），下一次 acquire 成功
            // 就会 commit —— 因此这里**没有**也不再需要 200 ms 活性兜底（AR3 §3.7：那条兜底的
            // 唯一作用是让「可见但被完全遮挡」的窗口退化成 5 Hz 全量重绘）。不可见窗口的
            // callback 丢失由可见性入口（`wl_surface.leave` / `configure` 的 `Suspended`）
            // 直接 `force_idle` 处理，见 `WaylandState::sync_frame_gate_visibility`。
            if state.take_redraw() {
                dispatcher.wake();
            }
            let frame_windows: Vec<(usize, WindowId)> = (0..state.window_count())
                .map(|index| (index, state.window_id(index)))
                .filter(|(index, id)| {
                    // R10-7：不可见的 surface 不请求 frame callback（协议允许服务端不回包）。
                    state.is_visible(*index)
                        && app.window(*id).is_some_and(|window| window.has_renderer())
                })
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
                            // B10：本帧没有 publish（Idle / 窗口已不在），没有「使用新状态的帧」
                            // 可以配对 ⇒ force ack。**例外**：被帧预算门禁推迟的帧（A5）状态仍
                            // 保留、下一拍就会真的出帧（`deferred_frames` 保证），此时不能提前
                            // ack —— 否则 maximize 恰好撞上预算合并时又会回到「先 ack 后画」。
                            let budget_deferred = app
                                .window(id)
                                .is_some_and(crate::window::Window::frame_budget_deferred);
                            if !budget_deferred {
                                state.ack_pending_configure(index);
                            }
                            // A5：本帧被帧预算门禁推迟（不是空闲：脏位保留）⇒ 记进
                            // `deferred_frames`，到期后**不依赖新唤醒**补一帧（下面的 timeout 会
                            // 压到 `next_deferred_deadline`）。
                            if budget_deferred && !deferred_frames.contains(&id) {
                                deferred_frames.push(id);
                            }
                        }
                        Some(Ok(FrameOutcome::Committed(_))) => {
                            // B10：使用 configure 状态渲染的帧已发布给 GPU 线程（present 时
                            // `wl_surface.commit`）⇒ 现在才 ack 该 configure（Qt 式 latch）。
                            state.ack_pending_configure(index);
                        }
                        Some(Err(error)) => return Err(error),
                    }
                }
            }
            // B10：不参与帧循环的窗口（不可见 / 还没建渲染器）永远等不到「使用新状态的帧已
            // 发布」——在这里 force ack，否则静止窗口的 configure 会一直悬挂（compositor 可能
            // 不完成映射 / 状态切换）。可见且已挂渲染器的窗口在帧提交或 Idle 分支 ack，不能在
            // 这里提前发（那会回到「先 ack 后画」的旧时序）。
            for index in 0..state.window_count() {
                let id = state.window_id(index);
                let renders = state.is_visible(index)
                    && app.window(id).is_some_and(|window| window.has_renderer());
                if !renders {
                    state.ack_pending_configure(index);
                }
            }
            for task in dispatcher.drain_tasks() {
                task.run(&mut app);
            }
            // ⑤ IME：把应用目标状态（`App::ime_enabled` × `App::ime_cursor_area`）写成每窗口
            // 策略，收敛 text-input 的 enable/disable，再下发本轮 caret / content_type。
            // **必须在下面 `event_loop.dispatch` 派发协议事件之前**（本轮应用事件与布局都已
            // 处理完）：键盘 / text-input 的 enter/leave 用的是**本轮最新**策略，不会出现
            // 「按旧策略先 enable、同一轮再 disable」的抖动（B9）。
            state.sync_ime_policy(&app);

            // ⑥ 剪贴板发布（需要最新输入 serial）+ a11y 增量。
            let serial = state.serial();
            state.clipboard_mut().apply_pending(serial);
            for window_a11y in &mut a11ys {
                let Some(index) = state.window_index(window_a11y.id) else {
                    continue;
                };
                window_a11y
                    .adapter
                    // AT-SPI 的 extent = inner.origin + 节点 bounds；Wayland 拿不到窗口的
                    // 全局位置（`xdg_toplevel` 不暴露），按 GTK/GNOME 的同一约定退化为
                    // 窗口客户区相对。尺寸是物理像素（含 scale），与 a11y 节点 bounds 同空间。
                    .set_bounds(
                        crate::math::Point::ZERO,
                        state.size(index),
                        state.scale(index),
                    );
                let id = window_a11y.id;
                if app.a11y_pending(id) {
                    window_a11y
                        .adapter
                        .update(|| crate::a11y::adapter::build_update(&mut app, id));
                }
            }
            // E5 / RE-04 / RE-05：**本轮确实交付过** a11y 且仍有未走完的批次 ⇒ 立刻唤醒下一轮
            // 泵（无 AT 客户端时无交付 ⇒ 绝不 wake，I3 空闲零唤醒）；判定 + wake 的唯一入口。
            app.wake_for_a11y_backlog(dispatcher.as_ref());

            // ⑦ 连续帧（动画 / P17 HUD）：定时器语义由 `dispatch` 的 timeout 提供，
            // 醒来后主动请求一帧（否则 tick 只更新状态、不出帧）。
            if app.needs_continuous_frames() {
                dispatcher.wake();
            }
            let timeout = if app.needs_continuous_frames() {
                FRAME_TICK
            } else {
                // 门禁关闭时不再按「兜底到期」醒来（AR3 §3.7：那条 200 ms 兜底已删除）：
                // 帧回调到达、输入、协议事件都会唤醒 dispatch，其余时间真正睡到空闲超时。
                surface::IDLE_TIMEOUT
            };
            // a11y 到期补发：被 `A11Y_THROTTLE` 挡下的最后一次变化必须有人在窗口结束时叫醒
            // 泵，否则它会一直停在旧值（复审 P1-4）。醒来后 `a11y_pending` 变真即交付。
            let timeout = if app.a11y_retry_pending() {
                timeout.min(FRAME_TICK)
            } else {
                timeout
            };
            // 有按住键时把等待压到下一次重复的到期时刻：否则 `dispatch` 会睡到空闲超时，
            // 重复键要等到下一个无关事件才被合成（P1-2.11）。
            let timeout = match state.next_key_repeat_in(Instant::now()) {
                Some(due) => timeout.min(due),
                None => timeout,
            };
            // R10-2：无输入法守护进程的降级判定要看时间 —— 把等待压到判定时刻，否则
            // 「enable 之后一直没人说话」这个状态要等下一个无关事件才被发现（用户在那之前
            // 一个字符都打不出来）。
            let timeout = match state.next_ime_degrade_in(Instant::now()) {
                Some(due) => timeout.min(due),
                None => timeout,
            };
            // A5：被帧预算门禁推迟的帧到期后必须自己醒来（不依赖下一次无关事件；与 Windows
            // 的 FramePacer / macOS 连续帧定时器同义）。RA-5：不可见窗口的推迟标记已在上面
            // 清掉；这里再夹一个 1 ms 下限，避免已过期 deadline 让 calloop timeout=0 空转。
            let timeout = match app.next_deferred_deadline() {
                Some(deadline) => timeout
                    .min(deadline.saturating_duration_since(Instant::now()))
                    .max(Duration::from_millis(1)),
                None => timeout,
            };
            // 应用自己关窗（Esc / 标题栏关闭按钮）后不能停在下面 1h 的空闲等待里。
            if app.windows().next().is_none() {
                return Ok(());
            }
            // R10-16：运行期 `calloop` 失败按 `IoError` 分类 —— `EPIPE` 是合成器消失、
            // `EPROTO`（`calloop-wayland-source` 把协议错误/坏消息都转成这个 errno）是客户端
            // bug。两者对用户的提示与对 CI 的意义完全不同，不能再折叠成 `AppInit`。
            event_loop
                .dispatch(timeout, &mut state)
                .map_err(|error| EngineError::Wayland {
                    kind: match &error {
                        calloop::Error::IoError(io) => WaylandErrorKind::from_io_error(io),
                        _ => WaylandErrorKind::Protocol,
                    },
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

/// 把一个剪贴板写出任务注册成 calloop 的可写事件源（R10-17）。
///
/// 为什么不是「写线程」或「阻塞写」：
/// - 阻塞写会在 UI 线程上停在 `write_all`：对端（刚启动的 Electron 应用等）不读时，整条事件
///   循环停摆（没有帧、没有输入、没有协议事件）；
/// - 写线程要多一个线程 + 一次所有权转移，且退出路径要 join；
/// - calloop 的可写事件是**零线程**方案：epoll 说可写就写一片（≤ `PIPE_BUF`，管道上绝不
///   阻塞），写完 `PostAction::Remove` 让事件源析构 ⇒ fd 关闭 ⇒ 对端读到 EOF。
///
/// 回调只能拿到 `&mut NoIoDrop<PendingSend>`（`NoIoDrop` 没有 `DerefMut`，
/// `calloop-0.14.5/src/sources/generic.rs:114-145`），所以 `write_more` 取 `&self`、写入进度用
/// `Cell` 记账 —— 这样既不需要 `unsafe get_mut()`，也不用把待发队列搬进 `WaylandState`
/// （那会让回调还得按 fd 反查状态，反而更绕）。
///
/// 返回 `Err` 表示注册失败（源已被丢弃 ⇒ fd 关闭，对端读到 EOF，语义仍是协议允许的
/// 「没有数据」）。
fn register_pending_send(
    handle: &calloop::LoopHandle<'static, WaylandState>,
    pending: PendingSend,
) -> Result<(), calloop::Error> {
    let source =
        calloop::generic::Generic::new(pending, calloop::Interest::WRITE, calloop::Mode::Level);
    let inserted = handle.insert_source(source, |_, pending, _| {
        match pending.write_more() {
            // 还没写完：等下一次可写事件（Level 触发会让 epoll 继续报告）。
            Ok(false) => Ok(calloop::PostAction::Continue),
            // 写完 / 对端已关闭 / IO 错误：摘掉事件源，fd 随 `PendingSend` 的 drop 关闭。
            Ok(true) | Err(_) => Ok(calloop::PostAction::Remove),
        }
    });
    match inserted {
        Ok(_token) => Ok(()),
        Err(error) => {
            // `InsertError` 把源还回来；丢掉它即可（`Drop` 关闭 fd）。
            drop(error.inserted);
            Err(error.error)
        }
    }
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
    let mut missing: Vec<(WindowId, crate::window::WindowOptions)> = app
        .windows()
        .filter(|(id, _)| state.window_index(*id).is_none())
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    // X1：按**创建顺序**建原生窗口（`WindowId` 单调递增 ⇒ 升序 = 创建顺序）。owner 必须先于
    // owned 窗口存在，否则 `set_parent` 只能降级（诊断 + 不设）；`App` 的窗口表是 `HashMap`，
    // 迭代顺序不确定，这里显式排序钉住（启动路径与运行期新增共用本函数）。
    missing.sort_by_key(|(id, _)| id.to_bits());
    for (id, options) in missing {
        state.add_window(id, &options)?;
    }
    // a11y adapter 与协议对象同生命周期：`App` 里每个窗口在 `WaylandState` 里有且仅有一个。
    for index in 0..state.window_count() {
        let id = state.window_id(index);
        if a11ys.iter().any(|entry| entry.id == id) {
            continue;
        }
        // RE-01：adapter 必须持有窗口 `A11yBridge` 的**那一份**消费端代际（唯一入口
        // `App::window_a11y_epoch`）；窗口刚被关闭时本轮跳过，下一轮 remove_window 清理协议对象。
        let Some(epoch) = app.window_a11y_epoch(id) else {
            continue;
        };
        a11ys.push(WindowA11y {
            id,
            adapter: UnixA11y::new(dispatcher.clone(), id, epoch)?,
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
        // A4：呈现反馈请求句柄交给 GPU 线程（在 present 前创建 `wp_presentation.feedback`）。
        // 协议不可用时是 `None`（平台层已记豁免），该窗口 `presented_at` 如实恒为 `None`。
        let presentation = state.presentation_handle(index);
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
            presentation,
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
        // R10-4 / R10-15：首次写入 compositor 侧状态（装饰模式 / 最大化 / 全屏 / 可用能力）。
        window.set_state(state.window_state(index));
        mirrors.push(WindowMirror {
            id,
            scale,
            size,
            title: window.options().title.clone(),
        });
    }
    Ok(())
}

mod calloop_wayland;

//! `NativeWindow` 生命周期同步：运行时建窗 / 销毁、客户区尺寸 / DPI / surface 写入、
//! 平台事件 → `App` 事件。自由函数由 `pump` 的每轮步骤调用（零行为变更的结构拆分）。

use std::sync::Arc;

use crate::{
    app::App,
    error::EngineError,
    event::{Event, InputEvent, ScrollPhase, ScrollUnit, timestamp_from_ticks_32},
    platform::{
        dispatcher::Dispatcher,
        event::{PlatformEvent, PointerKind, StampedEvent},
        windows::{composition, dpi, window::NativeWindow},
    },
    render::backend::Renderer,
    window::{WindowId, WindowOptions},
};

/// 消费者：`pump_loop` 的 ②a 步骤。
/// 把 `App` 的窗口集合同步到原生窗口（运行时创建 / 应用主动关闭，P1-2.8）。
///
/// 返回 `true` 表示原生窗口集合变了（调用方需要重新指向唤醒目标与剪贴板属主）。
///
/// 两件事：
/// - **创建**：`App` 里存在但还没有原生窗口的 id ⇒ 建 `NativeWindow` + `Renderer`，并把
///   尺寸 / DPI / surface 写回 `Window`（与启动路径同一套写入顺序）。
/// - **销毁**：原生窗口对应的 `App` 窗口已经不存在 ⇒ `destroy()` + 从列表移除。
///   （用户点 X 的路径在 `close_requested_windows` 里，已经先 `app.close_window` 再 `destroy`，
///   这里只兜「应用自己关掉」的那一半。）
pub(super) fn sync_native_windows(
    app: &mut App,
    windows: &mut Vec<NativeWindow>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<bool, EngineError> {
    let mut changed = false;
    // 先销毁：避免同一个 id 在窗口关闭又被复用时留下两份原生窗口。
    windows.retain(|native| {
        if app.window(native.id()).is_some() {
            true
        } else {
            changed = true;
            false
        }
    });
    // `NativeWindow::drop` 负责 `DestroyWindow`（GPU 线程的 join 在 `Renderer::drop` 里，
    // `App::close_window` 已经丢掉了那个 `Window`，因此顺序天然正确）。
    let existing: Vec<WindowId> = windows.iter().map(NativeWindow::id).collect();
    let pending: Vec<(WindowId, WindowOptions)> = app
        .windows()
        .filter(|(id, _)| !existing.contains(id))
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    for (id, options) in pending {
        let mut native = NativeWindow::create(id, &options, dispatcher.clone())?;
        let client = native.client_size();
        let renderer = Renderer::new_for_hwnd(native.composition(), client, options.background)?;
        if let Some(window) = app.window_mut(id) {
            // `set_renderer` 先清空图集页记账（新渲染器 = 一套空纹理 ⇒ 两张页都要整页重传），
            // `set_surface` 会把 `full_damage_pending` 置位（新交换链/持久目标必须整帧）。
            window.set_renderer(renderer);
            window.set_scale(native.scale());
            window.set_size(native.logical_size());
            window.set_surface(client);
        }
        {
            let target = native.composition();
            let mut target = composition::lock(&target);
            target.set_geometry(client, client)?;
        }
        native.note_composition_geometry((client, client));
        native.show();
        windows.push(native);
        changed = true;
    }
    Ok(changed)
}

/// 消费者：`pump_loop` 的 ② 步骤。
/// 把 `WndProc` 收集的事件应用到 `App`（几何/DPI 先落地，再投递事件）。
///
/// 时间戳换算的基准（`Instant::now()` + `GetTickCount64() as u32`）每批取**一次**：R02-14
/// 要的是「事件之间的相对时刻」，同一批共用一个基准既省两次系统调用，也避免同批事件的
/// 时间戳互相跳变。两者与 `MSG.time` **同源同宽**（32 位回绕域，见
/// [`timestamp_from_ticks_32`]）—— 旧实现用 QPC 毫秒当「现在」，原点/宽度都与 `MSG.time`
/// 不同，所有公开时间戳被系统性平移。
pub(super) fn collect_events(
    app: &mut App,
    windows: &mut [NativeWindow],
    dispatcher: &Arc<Dispatcher>,
) -> Result<(), EngineError> {
    let now = std::time::Instant::now();
    let now_ticks = super::pump::tick_now();
    for native in windows.iter_mut() {
        let id = native.id();
        let events = native.state_mut().take_events();
        for stamped in events {
            apply_event(app, native, dispatcher, id, stamped, now, now_ticks)?;
        }
    }
    Ok(())
}

/// 消费者：`collect_events`。
pub(super) fn apply_event(
    app: &mut App,
    native: &mut NativeWindow,
    dispatcher: &Arc<Dispatcher>,
    id: WindowId,
    stamped: StampedEvent,
    now: std::time::Instant,
    now_ticks: u32,
) -> Result<(), EngineError> {
    let scale = native.scale();
    let to_logical = |x: i32, y: i32| {
        crate::math::Point::new(scale.to_logical(x as f32), scale.to_logical(y as f32))
    };
    let modifiers = native.state_mut().modifiers();
    // R02-14：事件时刻 = 消息时刻（`MSG.time`），而不是分发时刻。
    let timestamp = timestamp_from_ticks_32(now, now_ticks, stamped.ticks_ms);
    let app_event = match stamped.event {
        PlatformEvent::Pointer { x, y, kind } => {
            let position = to_logical(x, y);
            let input = match kind {
                PointerKind::Move => InputEvent::PointerMove {
                    position,
                    modifiers,
                    timestamp,
                },
                PointerKind::Down(button) => InputEvent::PointerDown {
                    position,
                    button,
                    modifiers,
                    timestamp,
                },
                PointerKind::Up(button) => InputEvent::PointerUp {
                    position,
                    button,
                    modifiers,
                    timestamp,
                },
            };
            Event::Input(input)
        }
        PlatformEvent::PointerEnter => {
            let (x, y) = native.state_mut().client_pos();
            Event::Input(InputEvent::PointerEnter {
                position: to_logical(x, y),
                modifiers,
                timestamp,
            })
        }
        PlatformEvent::Scroll {
            x,
            y,
            dx_lines,
            dy_lines,
        } => Event::Input(InputEvent::Scroll {
            position: to_logical(x, y),
            delta: crate::math::Point::new(dx_lines, dy_lines),
            // Windows 的 `WM_MOUSEWHEEL` 只有离散格，没有阶段/精确模式（R02-10）。
            unit: ScrollUnit::Line,
            phase: ScrollPhase::Discrete,
            modifiers,
            timestamp,
        }),
        PlatformEvent::PointerLeave => {
            let (x, y) = native.state_mut().client_pos();
            Event::Input(InputEvent::PointerLeave {
                position: to_logical(x, y),
                modifiers,
                timestamp,
            })
        }
        PlatformEvent::PointerCancel => Event::Input(InputEvent::PointerCancel { timestamp }),
        PlatformEvent::Key {
            code,
            state,
            repeat,
        } => Event::Input(InputEvent::Key {
            code,
            state,
            repeat,
            modifiers: native.state_mut().modifiers(),
            timestamp,
        }),
        PlatformEvent::Text(text) => Event::Input(InputEvent::Text { text, timestamp }),
        PlatformEvent::Ime(ime) => Event::Ime(ime),
        PlatformEvent::Resize { width, height } => {
            // 最小化时 `WM_SIZE` 是 0×0：**保留上一次真实逻辑尺寸**，不写 `set_size` /
            // `set_surface`（否则布局被压成 0、恢复时先渲染一整面单色，P2-3）；恢复窗口的
            // `WM_SIZE` 会给出真实客户区。几何由 ②b′ 统一应用（这里不逐条 `Commit`，P1-2a）。
            let size = if native.is_minimized() {
                native.logical_size()
            } else {
                // 只记录尺寸并唤醒（`design/async-render-pipeline.md` §6.7）：内容重画发生在 GPU
                // 线程按下一帧的 `FrameRequest.surface` 做，UI 线程不等待。
                native.note_client_resize((width, height));
                let size = native.logical_size();
                if let Some(window) = app.window_mut(id) {
                    window.set_size(size);
                    // 目标尺寸 = 真实客户区（`WM_SIZE` 的 lParam 就是新的客户区宽高）。
                    window.set_surface((width, height));
                }
                size
            };
            dispatcher.wake();
            Event::Resized { size }
        }
        PlatformEvent::Dpi { dpi, suggested } => {
            // DPI 同理：只更新窗口尺寸/比例并唤醒；内容重画由 GPU 线程消费新的 surface，
            // 几何由 ②b′ 统一应用（P1-2a）。
            let scale = dpi::scale_from_dpi(dpi).ok_or(EngineError::InvalidDpiScale {
                factor: dpi as f32 / dpi::BASE_DPI as f32,
            })?;
            // 有系统建议矩形时**不要**先按旧尺寸重排（P2-2）：那会先跳一次再套建议矩形，
            // 可见抖动 + 一次多余布局。只记账新 DPI，位置与尺寸完全交给建议矩形。
            let resizable = native.note_scale(scale, dpi);
            match suggested {
                Some(rect) if resizable && !native.is_minimized() => native.place(rect)?,
                // 没有可用建议矩形（系统没给 / 窗口最小化）：保持逻辑尺寸，按新 DPI 重排一次。
                _ => native.apply_scale(scale, dpi)?,
            }
            if let Some(window) = app.window_mut(id) {
                window.set_scale(scale);
                // `apply_scale` / `place` 已按新 DPI 重排非客户区：回读真实客户区。
                // 最小化时 `GetClientRect` 是 0×0（`client_size()` 会钳成 1×1）：跳过 `set_surface`，
                // 目标保持上一次平台写入的真实客户区；恢复窗口的 `WM_SIZE` 会用新客户区覆盖它。
                //
                // **逻辑尺寸必须在这里同步写回**（复审 P2-9）：`place` 只是 `SetWindowPos`，
                // 新客户区要等下一轮的 `WM_SIZE` 才进 `Window`。本泵轮次紧接着就会出帧，
                // 那时布局仍按旧逻辑尺寸跑、而渲染目标已是新物理尺寸 + 新 scale ⇒ 跨屏拖动
                // 会有一帧内容被放大/裁切。先按新 DPI 换算并写回，下一轮 `WM_SIZE` 以同值覆盖
                // （`note_client_resize` 对同值幂等）。
                if !native.is_minimized() {
                    let client = native.client_size();
                    native.note_client_resize(client);
                    window.set_size(crate::math::Size::new(
                        scale.to_logical(client.0 as f32),
                        scale.to_logical(client.1 as f32),
                    ));
                    window.set_surface(client);
                }
            }
            dispatcher.wake();
            Event::DpiChanged { dpi, scale }
        }
        PlatformEvent::Focus(focused) => Event::FocusChanged { focused },
        PlatformEvent::Appearance(dark) => Event::AppearanceChanged { dark },
        PlatformEvent::Close => Event::CloseRequested,
    };
    app.dispatch(id, app_event);
    Ok(())
}

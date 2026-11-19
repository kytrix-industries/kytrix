//! `NativeWindow` 生命周期同步：运行时建窗 / 销毁、客户区尺寸 / DPI / surface 写入、
//! 平台事件 → `App` 事件。自由函数由 `pump` 的每轮步骤调用（零行为变更的结构拆分）。

use std::{cell::RefCell, rc::Rc, sync::Arc};

use windows::Win32::Foundation::HWND;

use super::recovery;
use crate::{
    app::App,
    error::EngineError,
    event::{Event, InputEvent, ScrollPhase, ScrollUnit, timestamp_from_ticks_32},
    platform::{
        dispatcher::Dispatcher,
        event::{PlatformEvent, PointerKind, StampedEvent},
        windows::{composition, window::NativeWindow},
    },
    render::backend::Renderer,
    window::{WindowId, WindowOptions},
};

/// 消费者：`pump::run`（启动建窗）与 [`sync_native_windows`]（运行时建窗）。
///
/// 解析 `WindowOptions::owner` → 原生 owner 的 HWND（X1）。
///
/// 只在**已创建的原生窗口集合**里查：owner 必须比 owned 先创建（同一 `App` 内先创建的窗口，
/// 由 App 层保证顺序）。找不到（owner 已关闭 / 尚未创建）⇒ 返回 `None` 降级为普通顶层窗口，
/// 并写诊断 —— 与公共契约「按 None 处理并写诊断（不 panic）」一致。
///
/// 「窗口句柄永不复用」由 `WindowId` 单调分配保证：这里只按 `WindowId` 匹配，因此陈旧 id
/// 不会误绑到后来复用同一 HWND 的新窗口。
pub(super) fn resolve_owner(options: &WindowOptions, windows: &[NativeWindow]) -> Option<HWND> {
    let owner_id = options.owner?;
    match windows.iter().find(|native| native.id() == owner_id) {
        Some(owner) => Some(owner.hwnd()),
        None => {
            eprintln!(
                "kytrix: 窗口 owner={owner_id:?} 不存在（已关闭或晚于 owned 创建），按无 owner 降级"
            );
            None
        }
    }
}

/// 消费者：`pump_loop` 的 ②a 步骤。
/// 把 `App` 的窗口集合同步到原生窗口（运行时创建 / 应用主动关闭，P1-2.8）。
///
/// 返回 `true` 表示原生窗口集合变了（调用方需要重新指向唤醒目标与剪贴板属主）。
///
/// 两件事：
/// - **创建**：`App` 里存在但还没有原生窗口的 id ⇒ 建 `NativeWindow` + `Renderer`，并把
///   尺寸 / DPI / surface 写回 `Window`（与启动路径同一套写入顺序）。新窗口带回
///   `app_cell` 的弱引用（系统 size 模态循环里的成帧驱动）。
/// - **销毁**：原生窗口对应的 `App` 窗口已经不存在 ⇒ `destroy()` + 从列表移除。
///   （用户点 X 的路径在 `close_requested_windows` 里，已经先 `app.close_window` 再 `destroy`，
///   这里只兜「应用自己关掉」的那一半。）
pub(super) fn sync_native_windows(
    app: &mut App,
    app_cell: &Rc<RefCell<App>>,
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
    let mut pending: Vec<(WindowId, WindowOptions)> = app
        .windows()
        .filter(|(id, _)| !existing.contains(id))
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    // X1：owner 必须比 owned 先创建。`App` 的窗口表是哈希表（迭代顺序任意），平台层按
    // `WindowId`（单调递增 = 创建顺序）排序后再建窗，owner 解析才稳定命中。
    pending.sort_by_key(|(id, _)| id.to_bits());
    for (id, options) in pending {
        // X1：owner 在**已创建**的原生窗口里解析（App 层保证 owner 先于 owned 创建）。
        let owner = resolve_owner(&options, windows);
        let mut native =
            NativeWindow::create(id, &options, owner, dispatcher.clone(), Some(app_cell))?;
        let client = native.client_size();
        let renderer = Renderer::new_for_hwnd(native.composition(), client, options.background)?;
        // A5：帧预算的刷新率提示（窗口所在 output；查询失败 ⇒ 门禁保持禁用）。
        let refresh_hz = native.refresh_hz();
        if let Some(window) = app.window_mut(id) {
            // `set_renderer` 先清空图集页记账（新渲染器 = 一套空纹理 ⇒ 两张页都要整页重传），
            // `set_surface` 会把 `full_damage_pending` 置位（新交换链/持久目标必须整帧）。
            window.set_renderer(renderer);
            window.set_scale(native.scale());
            window.set_size(native.logical_size());
            window.set_surface(client);
            if let Some(hz) = refresh_hz {
                window.set_frame_refresh_hz(hz);
            }
        }
        {
            let target = native.composition();
            let mut target = composition::lock(&target);
            target.set_geometry(client, client)?;
        }
        // X2：运行时新建的窗口同样必须在**显示之前**拿到当前拖动区（`App::set_drag_regions`
        // 可能先于原生窗口创建调用；之后的变化由泵的 ④′ 每轮对账）。
        native.sync_drag_regions(app);
        native.note_composition_geometry((client, client));
        native.show();
        windows.push(native);
        changed = true;
    }
    Ok(changed)
}

/// 消费者：`pump_loop` 的 ②a′ 步骤（R07-14）。
///
/// 执行窗口侧登记的显示 / 电源恢复：按 [`recovery::RECOVERY_ACTIONS`] 的顺序
/// 「重新查询显示器 → 请求 GPU 重建交换链 → 请求一帧全量重绘」。
///
/// 判定（消息 → 动作序列）在 `WndProc` 里由纯函数 [`recovery::recovery_plan`] 完成，本函数
/// 只执行副作用：
/// - 重建请求是**进程级**的（`render::windows::display` 的代际），每条 GPU 线程各消费一次，
///   重建后 `FrameSink::reset_epoch` +1 ⇒ UI 整帧重发并清掉本窗口的遮挡退避；
/// - 最后 `dispatcher.wake()` 让本轮的 ④ 帧门禁放行 —— 恢复必须**立刻**出一帧，否则「重建后
///   的空目标」会一直挂到下一笔输入。
///
/// 返回 `true` = 至少一个窗口执行了恢复（诊断 / 测试用）。
pub(super) fn apply_display_recovery(
    app: &mut App,
    windows: &mut [NativeWindow],
    dispatcher: &Arc<Dispatcher>,
) -> bool {
    let mut requested = false;
    let mut rebuild_targets = false;
    for native in windows.iter_mut() {
        if !native.state_mut().take_display_recovery() {
            continue;
        }
        requested = true;
        for action in recovery::RECOVERY_ACTIONS {
            match action {
                recovery::RecoveryAction::RequeryDisplays => native.requery_display(app),
                recovery::RecoveryAction::RebuildTargets => rebuild_targets = true,
                recovery::RecoveryAction::RequestFullRepaint => {
                    if let Some(window) = app.window_mut(native.id()) {
                        window.request_repaint();
                    }
                }
            }
        }
    }
    if rebuild_targets {
        crate::render::windows::display::note_display_change();
    }
    if requested {
        dispatcher.wake();
    }
    requested
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
///
/// **所有坐标换算都用 `stamped.scale`**（该消息产生时的 DPI 比例，R07-12）：`WM_DPICHANGED`
/// 在 `WndProc` 里同步生效，因此同一条批次里的后续输入已经带上新 scale，不会再出现
/// 「跨屏拖动时指针坐标跳一下」。`NativeWindow::scale` 只用于渲染目标 / composition 与 IME
/// 表单换算，不参与事件换算。
pub(super) fn apply_event(
    app: &mut App,
    native: &mut NativeWindow,
    dispatcher: &Arc<Dispatcher>,
    id: WindowId,
    stamped: StampedEvent,
    now: std::time::Instant,
    now_ticks: u32,
) -> Result<(), EngineError> {
    let scale = stamped.scale;
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
                // 只记录尺寸并唤醒：内容重画发生在 GPU
                // 线程按下一帧的 `FrameRequest.surface` 做，UI 线程不等待。
                //
                // 逻辑尺寸用**该消息的 scale** 换算（R07-12）：`WM_SIZE` 与 `WM_DPICHANGED` 可能
                // 同批到达，用窗口侧的旧 scale 换算会得到错误逻辑尺寸（跨屏拖动的第一帧被
                // 放大/裁切）。
                native.note_client_resize(scale, (width, height));
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
        PlatformEvent::Moved { x, y } => {
            // B3：`WM_MOVE` 给的是**屏幕物理坐标**；按该消息的 scale 换算成公共 API 的逻辑
            // 坐标语义（虚拟桌面内容区左上角为原点，`Event::Moved` 的契约）。窗口移动本身
            // 不需要重画（DComp 内容随窗口移动），因此不在这里 `wake` 请求新帧。
            Event::Moved {
                position: to_logical(x, y),
            }
        }
        PlatformEvent::Dpi { dpi } => {
            // DPI 的**几何已经落地**：`WndProc` 在 `WM_DPICHANGED` 里同步 `SetWindowPos` 了系统
            // 建议矩形（R07-12），这里只做记账与渲染目标更新（P1-2a：内容重画由 GPU 线程按
            // 新 surface 做，UI 线程不等待）。
            native.note_scale(scale, dpi);
            // A5：窗口可能刚被系统搬到另一块显示器，刷新率提示跟着重查。
            let refresh_hz = native.refresh_hz();
            if let Some(window) = app.window_mut(id) {
                window.set_scale(scale);
                if let Some(hz) = refresh_hz {
                    window.set_frame_refresh_hz(hz);
                }
                // 最小化时 `GetClientRect` 是 0×0（`client_size()` 会钳成 1×1）：跳过 `set_surface`，
                // 目标保持上一次平台写入的真实客户区；恢复窗口的 `WM_SIZE` 会用新客户区覆盖它。
                //
                // **逻辑尺寸必须在这里同步写回**：`SetWindowPos` 只是改窗口，新客户区要等下一轮
                // `WM_SIZE` 才进 `Window`。本泵轮次紧接着就会出帧，那时布局仍按旧逻辑尺寸跑、
                // 而渲染目标已是新物理尺寸 + 新 scale ⇒ 跨屏拖动会有一帧内容被放大/裁切。
                // 先按新 DPI 换算并写回，下一轮 `WM_SIZE` 以同值覆盖（`note_client_resize` 同值幂等）。
                if !native.is_minimized() {
                    let client = native.client_size();
                    native.note_client_resize(scale, client);
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{apply_display_recovery, resolve_owner};
    use crate::{
        app::App,
        platform::{dispatcher::Dispatcher, windows::window::NativeWindow},
        render::windows::display,
        window::{FrameOutcome, WindowOptions},
    };

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// R07-14：泵的 ②a′ 恢复步骤必须（a）推进显示代际让 GPU 重建交换链、（b）请求一帧，且
    /// 这一帧是**全量重绘**（headless 窗口直接结算成 `Committed`；`Idle` 说明全量 damage
    /// 没被置位），（c）重新查询显示器把平台真值写回 `Window`。
    #[test]
    fn a_display_recovery_requeries_the_display_and_requests_a_full_frame() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let options = WindowOptions::default();
        let id = app.create_window(options.clone());
        let dispatcher = Dispatcher::new();
        let mut native = NativeWindow::create(id, &options, None, Arc::clone(&dispatcher), None)
            .expect("NativeWindow");
        let _ = dispatcher.take_frame_wake();

        native.state_mut().note_display_recovery();
        let before_epoch = display::epoch();
        let mut windows = vec![native];
        assert!(
            apply_display_recovery(&mut app, &mut windows, &dispatcher),
            "有恢复请求时必须执行动作"
        );

        assert!(
            display::epoch() > before_epoch,
            "恢复必须请求 GPU 重建交换链（推进显示代际）"
        );
        assert!(dispatcher.take_frame_wake(), "恢复必须请求一帧");

        let outcome = app.frame(id).expect("window").expect("frame");
        assert!(
            matches!(outcome, FrameOutcome::Committed(_)),
            "恢复后的第一帧必须是全量重绘，实际 {outcome:?}"
        );
        let window = app.window(id).expect("window");
        assert_eq!(window.scale(), windows[0].scale(), "必须重新查询显示器 DPI");
        assert_eq!(
            window.size(),
            windows[0].logical_size(),
            "必须重新查询客户区尺寸"
        );
    }

    /// 没有恢复请求时步骤必须是 no-op（否则每个泵轮次都会重建一次交换链）。
    #[test]
    fn a_pump_round_without_a_display_recovery_does_nothing() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let options = WindowOptions::default();
        let id = app.create_window(options.clone());
        let dispatcher = Dispatcher::new();
        let native = NativeWindow::create(id, &options, None, Arc::clone(&dispatcher), None)
            .expect("window");
        let _ = dispatcher.take_frame_wake();

        let mut windows = vec![native];
        assert!(!apply_display_recovery(&mut app, &mut windows, &dispatcher));
        assert!(!dispatcher.take_frame_wake(), "没有请求时不得唤醒一帧");
    }

    /// X1：`WindowOptions::owner` 按 `WindowId` 解析到**已创建**原生窗口的 HWND；
    /// 查不到（owner 已关闭 / 晚于 owned 创建）⇒ 降级为无 owner（函数内写诊断，不 panic）。
    ///
    /// 变异守卫：删掉建窗路径对 [`resolve_owner`] 的使用、或让它恒返回 `None`
    /// ⇒ `window.rs` 的 `an_owned_window_reports_its_owner...` 与真机 X1 证据红。
    #[test]
    fn owner_resolution_maps_window_ids_to_live_native_windows() {
        let options = WindowOptions::default();
        let owner = NativeWindow::create(
            crate::window::WindowId::for_test(1),
            &options,
            None,
            Dispatcher::new(),
            None,
        )
        .expect("owner window");
        let windows = vec![owner];

        let mut options = WindowOptions {
            owner: Some(crate::window::WindowId::for_test(1)),
            ..WindowOptions::default()
        };
        assert_eq!(
            resolve_owner(&options, &windows),
            Some(windows[0].hwnd()),
            "owner id 必须解析到已创建原生窗口的 HWND"
        );

        options.owner = Some(crate::window::WindowId::for_test(99));
        assert_eq!(
            resolve_owner(&options, &windows),
            None,
            "不存在的 owner 必须降级为无 owner（不 panic）"
        );

        options.owner = None;
        assert_eq!(
            resolve_owner(&options, &[]),
            None,
            "没有 owner 时直接返回 None"
        );
    }
}

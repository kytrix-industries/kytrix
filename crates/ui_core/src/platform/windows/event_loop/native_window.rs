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

pub(super) fn sync_native_windows(
    app: &mut App,
    app_cell: &Rc<RefCell<App>>,
    windows: &mut Vec<NativeWindow>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<bool, EngineError> {
    let mut changed = false;
    windows.retain(|native| {
        if app.window(native.id()).is_some() {
            true
        } else {
            changed = true;
            false
        }
    });
    let existing: Vec<WindowId> = windows.iter().map(NativeWindow::id).collect();
    let mut pending: Vec<(WindowId, WindowOptions)> = app
        .windows()
        .filter(|(id, _)| !existing.contains(id))
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    pending.sort_by_key(|(id, _)| id.to_bits());
    for (id, options) in pending {
        let owner = resolve_owner(&options, windows);
        let Some(a11y_epoch) = app.window_a11y_epoch(id) else {
            continue;
        };
        let mut native = NativeWindow::create(
            id,
            &options,
            owner,
            dispatcher.clone(),
            Some(app_cell),
            a11y_epoch,
        )?;
        let client = native.client_size();
        let renderer = Renderer::new_for_hwnd(native.composition(), client, options.background)?;
        let refresh_hz = native.refresh_hz();
        if let Some(window) = app.window_mut(id) {
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
        native.sync_drag_regions(app);
        native.note_composition_geometry((client, client));
        native.show();
        windows.push(native);
        changed = true;
    }
    Ok(changed)
}

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
        scale.to_logical_point(crate::math::DevicePoint::new(
            crate::math::dp(x),
            crate::math::dp(y),
        ))
    };
    let modifiers = native.state_mut().modifiers();
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
            delta: crate::math::LogicalPoint::new(
                crate::math::px(dx_lines),
                crate::math::px(dy_lines),
            ),
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
            let size = if native.is_minimized() {
                native.logical_size()
            } else {
                native.note_client_resize(scale, (width, height));
                let size = native.logical_size();
                if let Some(window) = app.window_mut(id) {
                    window.set_size(size);
                    window.set_surface((width, height));
                }
                size
            };
            dispatcher.wake();
            Event::Resized { size }
        }
        PlatformEvent::Moved { x, y } => Event::Moved {
            position: to_logical(x, y),
        },
        PlatformEvent::Dpi { dpi } => {
            native.note_scale(scale, dpi);
            let refresh_hz = native.refresh_hz();
            if let Some(window) = app.window_mut(id) {
                window.set_scale(scale);
                if let Some(hz) = refresh_hz {
                    window.set_frame_refresh_hz(hz);
                }
                if !native.is_minimized() {
                    let client = native.client_size();
                    native.note_client_resize(scale, client);
                    window.set_size(native.logical_size());
                    window.set_surface(client);
                }
            }
            dispatcher.wake();
            Event::DpiChanged { dpi, scale }
        }
        PlatformEvent::Focus(focused) => {
            if !focused {
                app.clear_ime_cursor_area(id);
            }
            Event::FocusChanged { focused }
        }
        PlatformEvent::Appearance(dark) => Event::AppearanceChanged { dark },
        PlatformEvent::Close => Event::CloseRequested,
    };
    app.dispatch(id, app_event);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{apply_display_recovery, apply_event, resolve_owner};
    use crate::{
        app::App,
        event::Event,
        math::{DevicePixelRatio, px},
        node::builder::NodeBuilder,
        platform::{
            dispatcher::Dispatcher,
            event::{PlatformEvent, StampedEvent},
            windows::window::NativeWindow,
        },
        render::windows::display,
        text::TextStyle,
        window::{FrameOutcome, WindowOptions},
    };

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn a_display_recovery_requeries_the_display_and_requests_a_full_frame() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let options = WindowOptions::default();
        let id = app.create_window(options.clone());
        let dispatcher = Dispatcher::new();
        let mut native = NativeWindow::create(
            id,
            &options,
            None,
            Arc::clone(&dispatcher),
            None,
            app.window_a11y_epoch(id).expect("window exists"),
        )
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

    #[test]
    fn a_pump_round_without_a_display_recovery_does_nothing() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let options = WindowOptions::default();
        let id = app.create_window(options.clone());
        let dispatcher = Dispatcher::new();
        let native = NativeWindow::create(
            id,
            &options,
            None,
            Arc::clone(&dispatcher),
            None,
            app.window_a11y_epoch(id).expect("window exists"),
        )
        .expect("window");
        let _ = dispatcher.take_frame_wake();

        let mut windows = vec![native];
        assert!(!apply_display_recovery(&mut app, &mut windows, &dispatcher));
        assert!(!dispatcher.take_frame_wake(), "没有请求时不得唤醒一帧");
    }

    #[test]
    fn owner_resolution_maps_window_ids_to_live_native_windows() {
        let options = WindowOptions::default();
        let owner = NativeWindow::create(
            crate::window::WindowId::for_test(1),
            &options,
            None,
            Dispatcher::new(),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
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

    #[test]
    fn losing_window_focus_clears_the_app_ime_caret_hint() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let options = WindowOptions::default();
        let id = app.create_window(options.clone());
        let node = {
            let window = app.window_mut(id).expect("窗口存在");
            let root = window.tree().root();
            NodeBuilder::text("abc", TextStyle::new(px(16.0)))
                .build(window.tree_mut(), root)
                .expect("文本节点")
        };
        let _ = app.refresh_text(id).expect("窗口存在").expect("文本 pass");
        let _ = app
            .window_mut(id)
            .expect("窗口存在")
            .compute_layout()
            .expect("布局");
        let _ = app.layout_text(id).expect("窗口存在").expect("文本重排");
        app.set_ime_cursor_area(id, node, 1).expect("caret hint");
        assert!(
            app.ime_cursor_area(id).expect("窗口存在").is_some(),
            "前置：文本 shape 后必须有 caret hint"
        );

        let dispatcher = Dispatcher::new();
        let mut native = NativeWindow::create(
            id,
            &options,
            None,
            Arc::clone(&dispatcher),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
        )
        .expect("window");
        let _ = dispatcher.take_frame_wake();
        apply_event(
            &mut app,
            &mut native,
            &dispatcher,
            id,
            StampedEvent {
                event: PlatformEvent::Focus(false),
                ticks_ms: 0,
                scale: DevicePixelRatio::ONE,
            },
            std::time::Instant::now(),
            0,
        )
        .expect("apply_event");

        assert_eq!(
            app.ime_cursor_area(id).expect("窗口存在"),
            None,
            "窗口失焦必须清应用侧 IME caret hint"
        );
        assert!(
            app.drain_events().iter().any(|(window, event)| {
                *window == id && matches!(event, Event::FocusChanged { focused: false })
            }),
            "失焦事件本身仍必须派发"
        );
    }
}

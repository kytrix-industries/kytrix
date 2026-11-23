use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use smol_str::SmolStr;
use ui_core::{
    app::{App, Application},
    math::{Rect, Size},
    node::builder::NodeBuilder,
    paint::{color::Color, style::PaintSpec, style_builder::StyleBuilder},
    window::{Decorations, WindowControl, WindowId, WindowOptions},
};

const DEFAULT_ACTION_BUDGET: f32 = 4.5;

const FIRST_FRAME_DEADLINE: f32 = 60.0;

const IDLE_SAMPLE_OFFSETS: (f32, f32) = (0.4, 1.0);
const IDLE_CHECK_MIN_BUDGET: f32 = 3.6;

#[derive(Default)]
struct SmokeReport {
    frames: u32,
    first_presented_at: Option<Instant>,
    initial_size: Option<Size>,
    expected_maximized: Option<Size>,
    maximized_size: Option<Size>,
    checks: u32,
    saw_maximized: bool,
    saw_restored: bool,
    restore_settled_at: Option<f32>,
    idle_first_sample: Option<(f32, u32)>,
    idle_checked: bool,
    failures: Vec<String>,
}

#[cfg(windows)]
fn expected_maximized_size(scale: f32) -> Option<Size> {
    use windows::Win32::{
        Foundation::POINT,
        Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint},
    };

    let monitor = unsafe { MonitorFromPoint(POINT { x: 100, y: 100 }, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).ok()?,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return None;
    }
    Some(Size::new(
        (info.rcWork.right - info.rcWork.left) as f32 / scale,
        (info.rcWork.bottom - info.rcWork.top) as f32 / scale,
    ))
}

#[cfg(not(windows))]
fn expected_maximized_size(_scale: f32) -> Option<Size> {
    None
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let budget = std::env::args()
        .nth(1)
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| *value > 1.0)
        .unwrap_or(DEFAULT_ACTION_BUDGET);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions {
        title: SmolStr::new_static("Kytrix · E6 frameless smoke"),
        size: Size::new(960.0, 640.0),
        decorations: Decorations::None,
        icon: Some(bytes::Bytes::from_static(include_bytes!(
            "frameless_smoke_icon.png"
        ))),
        background: Color::from_rgba8([0x14, 0x18, 0x1F, 0xFF]),
        ..WindowOptions::default()
    });
    build_scene(&mut app, window);

    let ticker = app.handle();
    ticker.clone().spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            ticker.wake();
        }
    });

    let started = Instant::now();
    let report = Rc::new(RefCell::new(SmokeReport::default()));
    let report_hook = Rc::clone(&report);
    let mut maximize_requested = false;
    let mut restore_requested = false;
    let mut last_maximized: Option<bool> = None;
    let mut last_logical_size: Option<Size> = None;
    app.on_frame(move |app| {
        let wall = started.elapsed().as_secs_f32();
        let mut report = report_hook.borrow_mut();
        report.frames = report.frames.saturating_add(1);
        let frames = report.frames;
        if let Some(target) = app.window(window) {
            if report.first_presented_at.is_none() && target.last_frame_presentation().is_some() {
                report.first_presented_at = Some(Instant::now());
                report.initial_size = Some(target.size());
                report.expected_maximized = expected_maximized_size(target.scale().factor());
                println!(
                    "smoke: first frame presented at {wall:.2}s ({frames} pump rounds, \
                     client={:.0}x{:.0}, work-area={})",
                    target.size().width,
                    target.size().height,
                    report.expected_maximized.map_or_else(
                        || "unknown (non-Windows)".to_owned(),
                        |size| format!("{:.0}x{:.0}", size.width, size.height),
                    ),
                );
            }
            let state = target.state();
            let size = target.size();
            if last_maximized != Some(state.maximized) || last_logical_size != Some(size) {
                println!(
                    "smoke: t={wall:.2}s maximized={} client={:.0}x{:.0} decorations={:?}",
                    state.maximized,
                    size.width,
                    size.height,
                    target.options().decorations
                );
                if last_maximized == Some(false) && state.maximized {
                    report.saw_maximized = true;
                }
                if last_maximized == Some(true) && !state.maximized {
                    report.saw_restored = true;
                }
                last_maximized = Some(state.maximized);
                last_logical_size = Some(size);
            }

            if maximize_requested
                && let (Some(initial), Some(expected)) =
                    (report.initial_size, report.expected_maximized)
            {
                let expected = if state.maximized { expected } else { initial };
                if (size.width - expected.width).abs() > 1.0
                    || (size.height - expected.height).abs() > 1.0
                {
                    report.failures.push(format!(
                        "t={wall:.2}s frame#{frames} maximized={} client={:.0}x{:.0} 期望 \
                         {:.0}x{:.0}（状态与客户区不同步 ⇒ 旧尺寸帧）",
                        state.maximized, size.width, size.height, expected.width, expected.height,
                    ));
                }
                report.checks = report.checks.saturating_add(1);
            } else if state.maximized {
                match report.maximized_size {
                    None => report.maximized_size = Some(size),
                    Some(previous)
                        if (previous.width - size.width).abs() > 1.0
                            || (previous.height - size.height).abs() > 1.0 =>
                    {
                        report.failures.push(format!(
                            "t={wall:.2}s frame#{frames} 最大化阶段出现两个客户区尺寸 \
                             {:.0}x{:.0} → {:.0}x{:.0}（旧尺寸帧）",
                            previous.width, previous.height, size.width, size.height,
                        ));
                    }
                    Some(_) => {}
                }
            }
        }

        let Some(presented) = report.first_presented_at else {
            if wall >= FIRST_FRAME_DEADLINE {
                println!(
                    "smoke: FAIL first frame was never presented within {FIRST_FRAME_DEADLINE:.0}s \
                     ({frames} pump rounds)"
                );
                app.close_window(window);
                std::process::exit(2);
            }
            return;
        };
        let action_time = presented.elapsed().as_secs_f32();

        if !maximize_requested && action_time >= 0.5 {
            maximize_requested = true;
            match app.request_window_control(window, WindowControl::ToggleMaximize) {
                Ok(()) => println!("smoke: maximize requested (first frame +{action_time:.2}s)"),
                Err(error) => println!("smoke: maximize request failed: {error:?}"),
            }
            let _ = app.request_repaint(window);
        }
        if maximize_requested && !restore_requested && action_time >= 1.5 {
            restore_requested = true;
            match app.request_window_control(window, WindowControl::ToggleMaximize) {
                Ok(()) => println!("smoke: restore requested (first frame +{action_time:.2}s)"),
                Err(error) => println!("smoke: restore request failed: {error:?}"),
            }
            let _ = app.request_repaint(window);
        }

        if budget >= IDLE_CHECK_MIN_BUDGET && report.saw_restored {
            let settled_at = *report.restore_settled_at.get_or_insert(action_time);
            let first_at = settled_at + IDLE_SAMPLE_OFFSETS.0;
            let second_at = settled_at + IDLE_SAMPLE_OFFSETS.1;
            if !report.idle_checked && action_time >= second_at {
                let index = last_frame_index(app, window);
                match (report.idle_first_sample, index) {
                    (Some((first_sample_at, first)), Some(second)) if first == second => {
                        report.idle_checked = true;
                        println!(
                            "smoke: CHECK idle frame_index stable at {second} (samples \
                             +{first_sample_at:.2}s/+{action_time:.2}s) PASS"
                        );
                    }
                    (Some((first_sample_at, first)), Some(second)) => {
                        report.idle_checked = true;
                        report.failures.push(format!(
                            "I3 空闲自检失败：frame_index 在空闲期从 {first} 变成 {second}（采样 \
                             +{first_sample_at:.2}s/+{action_time:.2}s）"
                        ));
                    }
                    _ => {
                        report.idle_checked = true;
                        println!("smoke: SKIP idle check (no frame presentation yet)");
                    }
                }
            } else if report.idle_first_sample.is_none()
                && action_time >= first_at
                && let Some(index) = last_frame_index(app, window)
            {
                report.idle_first_sample = Some((action_time, index));
                println!("smoke: idle sample #1 at +{action_time:.2}s frame_index={index}");
            }
        }

        if action_time >= budget {
            let title = match last_maximized {
                Some(true) => SmolStr::new_static("Kytrix · E6 frameless smoke · maximized"),
                _ => SmolStr::new_static("Kytrix · E6 frameless smoke · done"),
            };
            println!(
                "smoke: closing window (first frame +{action_time:.2}s, {frames} pump rounds, \
                 title={title})"
            );
            if let Err(error) = app.set_title(window, title) {
                println!("smoke: set_title failed: {error:?}");
            }
            let _ = app.request_repaint(window);
            app.close_window(window);
        }
    });

    println!(
        "smoke: entering event loop (frameless, first-frame budget {budget:.1}s, \
         deadline {FIRST_FRAME_DEADLINE:.0}s)"
    );
    Application::new().run(app)?;
    let report = report.borrow();
    let Some(first) = report.first_presented_at else {
        println!("smoke: FAIL no frame presented before the event loop exited");
        std::process::exit(2);
    };
    println!(
        "smoke: event loop exited cleanly after {:.2}s wall (first frame at {:.2}s, {} pump \
         rounds, no panic)",
        started.elapsed().as_secs_f32(),
        first.duration_since(started).as_secs_f32(),
        report.frames
    );

    let mut failed = false;
    for failure in &report.failures {
        println!("smoke: FAIL {failure}");
        failed = true;
    }
    if !report.saw_maximized || !report.saw_restored {
        println!(
            "smoke: FAIL max/restore edge missing (maximized={} restored={}) —— 冒烟没有覆盖到 \
             控制路径",
            report.saw_maximized, report.saw_restored
        );
        failed = true;
    }
    if report.checks == 0 {
        println!("smoke: FAIL no same-frame state/client self-check ran");
        failed = true;
    }
    if budget >= IDLE_CHECK_MIN_BUDGET && !report.idle_checked {
        println!("smoke: FAIL idle frame_index self-check did not complete");
        failed = true;
    }
    if failed {
        println!(
            "smoke: FAIL self-checks ({:?}; {} checks, {} pump rounds)",
            report.failures.len(),
            report.checks,
            report.frames
        );
        std::process::exit(3);
    }
    println!(
        "smoke: PASS all self-checks ({} same-frame checks, idle frame_index check={}, {} pump \
         rounds)",
        report.checks,
        if report.idle_checked { "PASS" } else { "SKIP" },
        report.frames
    );
    Ok(())
}

fn last_frame_index(app: &App, window: WindowId) -> Option<u32> {
    app.window(window).map(|target| target.frame_index().get())
}

fn build_scene(app: &mut App, window: WindowId) {
    let tree = app.window_mut(window).expect("窗口存在").tree_mut();
    let root = tree.root();
    let root_style = StyleBuilder::new().size_full().build();
    tree.set_style(root, root_style).expect("根样式");
    let _ = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 960.0, 640.0))
        .paint(PaintSpec {
            background: Color::from_rgba8([0x1E, 0x24, 0x2E, 0xFF]),
            ..PaintSpec::default()
        })
        .build(tree, root)
        .expect("背景");
    let _ = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 960.0, 40.0))
        .paint(PaintSpec {
            background: Color::from_rgba8([0x2A, 0x32, 0x40, 0xFF]),
            ..PaintSpec::default()
        })
        .build(tree, root)
        .expect("自绘 header");

    app.set_drag_regions(window, &[Rect::new(0.0, 0.0, 960.0, 40.0)]);
}

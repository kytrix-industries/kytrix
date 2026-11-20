//! E6（P1-E6）**frameless 实机冒烟**示例：`Decorations::None` + 窗口图标 + 运行时标题 +
//! 最大化 / 还原状态回报。
//!
//! ```text
//! cargo run -p ui_core --example frameless_smoke            # 首帧后 ~3 秒自己关窗退出
//! cargo run -p ui_core --example frameless_smoke -- 5       # 指定首帧后的秒数预算
//! ```
//!
//! 冒烟脚本（`.review` 日志）验证的事実：
//! 1. frameless 窗口能显示（客户区铺满、系统标题栏消失）且进程存活到指定秒数；
//! 2. 自绘 header 声明了拖动区（`WM_NCHITTEST` 命中 `HTCAPTION`；真机可人工拖动）；
//! 3. `App::request_window_control(ToggleMaximize)` 让 `WindowState.maximized` 变真，且最大化
//!    后客户区尺寸 = 工作区（`WM_NCCALCSIZE` 的边框补偿正确，没有内容被裁 1–8 px）；
//! 4. `App::set_title` 在运行时改标题（任务栏 / Alt+Tab 可见）；
//! 5. `App::close_window` 后事件循环干净退出（退出码 0，无 panic）。
//!
//! 时序以**首帧完成反馈**（`Window::last_frame_presentation`）为锚点：D3D12 首次管线创建在
//! 本机可能花十几秒，用墙钟会在首帧前就把窗口关掉（那样什么也证明不了）。泵空闲时不产帧，
//! 因此本示例用 `AppHandle::wake` 的 250 ms 节拍驱动采样（只属于冒烟示例，不是引擎行为）。
//!
//! 本文件只做启动与顶层组装（AGENTS §1.2）；场景是两块纯色区域，足以让「窗口真的在画」可辨。

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
    paint::{
        color::Color,
        style::{PaintSpec, Style},
    },
    window::{Decorations, WindowControl, WindowId, WindowOptions},
};

/// 首帧完成后的动作预算（秒）：`maximize +0.5` / `restore +1.5` / `close +budget`。
const DEFAULT_ACTION_BUDGET: f32 = 3.0;

/// 首帧的硬等待上限（秒）：超过即认为冒烟失败（关窗并以非零退出码报告）。
const FIRST_FRAME_DEADLINE: f32 = 60.0;

/// 冒烟观测状态（闭包与 `main` 共享：`Application::run` 会消费 `App` 及其回调）。
#[derive(Default)]
struct SmokeReport {
    /// 泵轮数（每轮 `on_frame` 调用一次）。
    frames: u32,
    /// 首帧完成反馈被观察到的时刻（`Window::last_frame_presentation`）。
    first_presented_at: Option<Instant>,
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
        // 32×32 PNG（构建期 `include_bytes!`；`WindowOptions::icon` 只在 Windows 解码）。
        icon: Some(bytes::Bytes::from_static(include_bytes!(
            "frameless_smoke_icon.png"
        ))),
        background: Color::from_rgba8([0x14, 0x18, 0x1F, 0xFF]),
        ..WindowOptions::default()
    });
    build_scene(&mut app, window);

    // 冒烟专用节拍：泵空闲时没有帧/消息，采样与校准需要外部唤醒（仅示例，不是引擎行为）。
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
    let mut last_logical_size = None;
    app.on_frame(move |app| {
        let wall = started.elapsed().as_secs_f32();
        let mut report = report_hook.borrow_mut();
        report.frames = report.frames.saturating_add(1);
        let frames = report.frames;
        if let Some(target) = app.window(window) {
            // 首帧完成反馈 = 窗口真的上屏过（不是「钩子跑过一次」）。
            if report.first_presented_at.is_none() && target.last_frame_presentation().is_some() {
                report.first_presented_at = Some(Instant::now());
                println!(
                    "smoke: first frame presented at {wall:.2}s ({frames} pump rounds, \
                     client={:.0}x{:.0})",
                    target.size().width,
                    target.size().height
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
                last_maximized = Some(state.maximized);
                last_logical_size = Some(size);
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

        // +0.5s：请求最大化（平台泵 `ShowWindow(SW_MAXIMIZE)`，状态经 `IsZoomed` 回报）。
        if !maximize_requested && action_time >= 0.5 {
            maximize_requested = true;
            match app.request_window_control(window, WindowControl::ToggleMaximize) {
                Ok(()) => println!("smoke: maximize requested (first frame +{action_time:.2}s)"),
                Err(error) => println!("smoke: maximize request failed: {error:?}"),
            }
            let _ = app.request_repaint(window);
        }
        // +1.5s：请求还原（同一条控制路径的另一半）。
        if maximize_requested && !restore_requested && action_time >= 1.5 {
            restore_requested = true;
            match app.request_window_control(window, WindowControl::ToggleMaximize) {
                Ok(()) => println!("smoke: restore requested (first frame +{action_time:.2}s)"),
                Err(error) => println!("smoke: restore request failed: {error:?}"),
            }
            let _ = app.request_repaint(window);
        }
        // +budget：运行时改标题（任务栏可见）+ 关窗；`Application::run` 在无窗口时返回。
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
            // 先请求一帧再关窗：`close_window` 本身不唤醒泵，没有这条唤醒时泵会停在
            // `GetMessageW` 上（窗口已从 App 里删掉、没人再唤醒它）。
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
    Ok(())
}

/// 最小场景：整窗背景 + 顶部 40 px 自绘 header（frameless 下内容从窗口 y=0 开始）。
fn build_scene(app: &mut App, window: WindowId) {
    let tree = app.window_mut(window).expect("窗口存在").tree_mut();
    let root = tree.root();
    let mut root_style = Style::default();
    root_style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::percent(1.0),
        height: taffy::style::Dimension::percent(1.0),
    };
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

    // 自绘 header 的可拖动区（逻辑客户区坐标）：`WM_NCHITTEST` 返回 `HTCAPTION` ⇒ 走系统的
    // move 模态循环（拖动 / 双击最大化 / Aero Snap 全免费）。冒烟不强制人工拖动，但这条声明
    // 让 `WM_NCHITTEST` 的 frameless 分支被真实消息覆盖。
    app.set_drag_regions(window, &[Rect::new(0.0, 0.0, 960.0, 40.0)]);
}

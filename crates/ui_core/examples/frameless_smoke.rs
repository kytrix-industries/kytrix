//! E6（P1-E6 / E6b）**frameless 实机冒烟 + 自检**示例：`Decorations::None` + 窗口图标 + 运行时
//! 标题 + 最大化 / 还原的**同帧**状态与客户区一致性。
//!
//! ```text
//! cargo run -p ui_core --example frameless_smoke            # 首帧后 ~3 秒自己关窗退出
//! cargo run -p ui_core --example frameless_smoke -- 5       # 指定首帧后的秒数预算
//! ```
//!
//! 冒烟脚本（`.review` 日志）验证的事实：
//! 1. frameless 窗口能显示（客户区铺满、系统标题栏消失）且进程存活到指定秒数；
//! 2. 自绘 header 声明了拖动区（`WM_NCHITTEST` 命中 `HTCAPTION`；真机可人工拖动）；
//! 3. `App::request_window_control(ToggleMaximize)` 让 `WindowState.maximized` 变真，且**同一帧**
//!    的客户区尺寸 = 工作区（P2-2：`WM_NCCALCSIZE` 的最大化边框补偿；P2-1：引擎控制路径先消费
//!    `WM_SIZE` 再对账 chrome，不得出现「新状态 + 旧客户区」的 ≈1 帧旧尺寸画面）；
//! 4. 还原方向同样自检：`maximized=false` 的当帧客户区必须回到初始尺寸；
//! 5. `App::set_title` 在运行时改标题（任务栏 / Alt+Tab 可见）；
//! 6. 还原稳定后两次采样 `Window::frame_index()` 不变（I3：空闲不提交帧；250 ms 唤醒节拍只唤醒
//!    泵、不推进提交序号）；
//! 7. `App::close_window` 后事件循环干净退出（退出码 0，无 panic）。
//!
//! 退出码：`0` = 全部自检通过；`2` = 首帧超时/未呈现；`3` = 自检失败（维度/状态不同步）。
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
const DEFAULT_ACTION_BUDGET: f32 = 4.5;

/// 首帧的硬等待上限（秒）：超过即认为冒烟失败（关窗并以非零退出码报告）。
const FIRST_FRAME_DEADLINE: f32 = 60.0;

/// I3 空闲自检：从**观察到还原边沿**起算的两次采样偏移（秒）与预算下限。
///
/// 必须等还原控制真正落地（`saw_restored`）之后再采样：还原请求到控制执行之间隔着一个泵轮
/// 唤醒（节拍线程最多 250 ms），期间还可能提交还原那一帧 —— 在请求时刻就采样会把那一帧误判成
/// 「空闲出帧」。预算低于 [`IDLE_CHECK_MIN_BUDGET`] 时跳过该检查（打印 SKIP，不算失败）。
const IDLE_SAMPLE_OFFSETS: (f32, f32) = (0.4, 1.0);
const IDLE_CHECK_MIN_BUDGET: f32 = 3.6;

/// 冒烟观测状态（闭包与 `main` 共享：`Application::run` 会消费 `App` 及其回调）。
#[derive(Default)]
struct SmokeReport {
    /// 泵轮数（每轮 `on_frame` 调用一次）。
    frames: u32,
    /// 首帧完成反馈被观察到的时刻（`Window::last_frame_presentation`）。
    first_presented_at: Option<Instant>,
    /// 首帧时的逻辑客户区尺寸（还原方向的期望值）。
    initial_size: Option<Size>,
    /// 最大化时的期望逻辑客户区尺寸（工作区物理 ÷ scale；取不到时 `None`）。
    expected_maximized: Option<Size>,
    /// 非 Windows（或工作区查询失败）时观察到的最大化客户区尺寸：用于「最大化阶段出现两个
    /// 不同尺寸 ⇒ 旧尺寸帧」的兜底判据。
    maximized_size: Option<Size>,
    /// 自检执行次数（0 = 窗口从未最大化/还原 ⇒ 冒烟没有验证到任何东西）。
    checks: u32,
    /// 观察到的最大化 / 还原边沿（PASS 要求两者都出现）。
    saw_maximized: bool,
    saw_restored: bool,
    /// 观察到还原边沿的时刻（相对首帧，秒）：I3 空闲采样的起点。
    restore_settled_at: Option<f32>,
    /// I3 空闲自检：第一次采样的 `(相对时刻, FrameIndex.get())` 与是否已完成检查。
    idle_first_sample: Option<(f32, u32)>,
    idle_checked: bool,
    /// 自检失败明细（非空 ⇒ 退出码 3）。
    failures: Vec<String>,
}

/// 最大化时客户区的期望**逻辑**尺寸：窗口所在显示器工作区的物理尺寸 ÷ 窗口 scale。
///
/// Windows 专用（其它平台返回 `None`：冒烟仍跑，只是不做「客户区 == 工作区」自检；跨端几何由
/// 各自平台的验收覆盖）。窗口默认落点是 `(100,100)`（引擎的 `DEFAULT_WINDOW_ORIGIN`），
/// `MonitorFromPoint` 因此稳定命中窗口所在显示器。
#[cfg(windows)]
fn expected_maximized_size(scale: f32) -> Option<Size> {
    use windows::Win32::{
        Foundation::POINT,
        Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint},
    };

    // SAFETY: 两个都是无副作用、无指针参数的系统查询；`info` 是栈上输出结构且 `cbSize` 已填。
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
    let mut last_logical_size: Option<Size> = None;
    app.on_frame(move |app| {
        let wall = started.elapsed().as_secs_f32();
        let mut report = report_hook.borrow_mut();
        report.frames = report.frames.saturating_add(1);
        let frames = report.frames;
        if let Some(target) = app.window(window) {
            // 首帧完成反馈 = 窗口真的上屏过（不是「钩子跑过一次」）。
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
            // 状态 / 尺寸边沿日志：旧尺寸帧会在这里表现为「状态已变、尺寸还是旧的」两行。
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

            // P2-1 / P2-2 自检：**同一帧**的状态与客户区必须一致。最大化帧的客户区 = 工作区；
            // 其余（请求前 / 还原后）帧的客户区 = 初始尺寸。旧实现先回状态、下一轮才消费
            // `WM_SIZE` ⇒ 这里会出现一格「maximized=true 但 client=初始尺寸」的 FAIL。
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
                // 非 Windows 兜底：最大化阶段若出现两个不同客户区尺寸，说明有一帧是旧尺寸。
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

        // I3 空闲自检：**还原边沿之后**的稳定期两次采样 `FrameIndex` 必须不变 —— 250 ms 节拍
        // 只唤醒泵（`Dispatcher::wake`），空闲轮次 `FrameOutcome::Idle` 不推进提交序号。
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
                        // 还没有可信的呈现快照：不算失败（首帧未反馈的窗口本来就不该采）。
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

/// 当前窗口的提交序号（拿不到时 `None`）。
fn last_frame_index(app: &App, window: WindowId) -> Option<u32> {
    app.window(window).map(|target| target.frame_index().get())
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

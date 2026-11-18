#![cfg(feature = "test-support")]
//! GUI 端到端冒烟（集成测试入口；AR5 交付缺口的那一项）。
//!
//! 场景与断言清单见 [`ui_core::test_support::gui_smoke`] 的模块文档；本文件只做三件事：
//! 跑套件、把实测证据打印出来、确认六个场景都真的跑了断言。
//!
//! # 运行（默认 `cargo test` **不跑**本文件）
//!
//! ```text
//! # 本机（三平台同一条命令；不需要显示服务器 / 合成器）
//! cargo test -p ui_core --features test-support --test gui_smoke -- --ignored --nocapture
//!
//! # CI：同上；Linux 上只需要一个可用的 Vulkan 设备（lavapipe 即可，与 tests/snapshot.rs 同口径）
//! ```
//!
//! 显式 `#[ignore]` 而不是 feature 开关：默认测试面零 GPU 依赖，CI 无显示环境不会被拖红；
//! `--ignored` 是**显式**开启（比环境变量更容易在 CI 日志里审计）。
//!
//! 套件用 `TestApp`（headless）+ 真实 GPU 离屏读回（`Window::enable_offscreen_renderer`
//! 等价链路：`GpuRenderer::submit_snapshot` + `readback`，Windows 走 WARP、macOS 走 Metal、
//! Linux 走 Vulkan），输入注入走与平台泵相同的 `App::dispatch_input` / `App::dispatch`。
//! Linux 无 `WAYLAND_DISPLAY` 时不需要任何窗口系统 —— 这正是本套件在 CI 里的运行形态。

use ui_core::test_support::gui_smoke;

/// 六个场景必须全部通过（任何断言失败都会在这里 panic，消息带场景名与实测值）。
#[test]
#[ignore = "GUI 端到端冒烟：真实 GPU 读回 + 交互场景，用 -- --ignored 显式开启"]
fn gui_end_to_end_smoke() {
    let report = gui_smoke::run_all();
    eprintln!("GUI 冒烟：backend={}", report.backend);
    for scenario in &report.scenarios {
        eprintln!("== {} ==", scenario.name);
        for line in &scenario.evidence {
            eprintln!("   {line}");
        }
    }
    assert_eq!(
        report.scenarios.len(),
        6,
        "六个场景必须全部执行（实际 {}）",
        report.scenarios.len()
    );
    assert!(
        report.evidence_count() >= 12,
        "每个场景都必须留下实测证据（实际 {} 行）",
        report.evidence_count()
    );
    for scenario in &report.scenarios {
        assert!(
            !scenario.name.is_empty() && !scenario.evidence.is_empty(),
            "场景必须有名字与证据行"
        );
    }
    eprintln!("RESULT: PASS（backend={}）", report.backend);
}

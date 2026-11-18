//! `interaction` 模式：GUI 端到端冒烟（AR5 交付缺口的那一项）。
//!
//! 场景清单与断言设计见 [`ui_core::test_support::gui_smoke`] 的模块文档；本文件只做
//! 命令行包装：跑套件 → 打印实测证据 → PASS/FAIL。
//!
//! # 与 `window` / `idle` 模式的口径差异
//!
//! - **必须** `--features test-support` 构建（套件本身在测试基建里，默认特性下不进发布物）；
//! - **不建真实平台窗口**：输入走 `App::dispatch_input` / `App::dispatch`（与平台泵**同一条**
//!   分发路径），「呈现」走离屏 GPU 渲染 + 像素读回。没有合成器的 Linux（无 `WAYLAND_DISPLAY`）
//!   因此也能跑 —— 这正是 CI 里的运行形态；真实窗口的帧率/CPU 仍由 `window` / `idle` 模式覆盖。
//! - 断言失败会 panic 到 `catch_unwind`：这里转成 `RESULT: FAIL` + 非零退出码（CI 友好）。

use crate::Config;

/// 跑 GUI 端到端冒烟；返回 `true` = 六个场景全部通过。
///
/// # Errors
/// 本二进制没有以 `--features test-support` 构建（套件不存在，无法降级运行）。
#[cfg(not(feature = "test-support"))]
pub fn run(_config: &Config) -> Result<bool, Box<dyn std::error::Error>> {
    Err(Box::<dyn std::error::Error>::from(
        "interaction 模式需要测试基建：请用 \
         `cargo run --release -p ui_core --features test-support --example acceptance -- interaction` \
         构建（test-support 不进发布物，默认特性下没有 GUI 冒烟套件）。",
    ))
}

/// 跑 GUI 端到端冒烟；返回 `true` = 六个场景全部通过。
#[cfg(feature = "test-support")]
pub fn run(_config: &Config) -> Result<bool, Box<dyn std::error::Error>> {
    let outcome = std::panic::catch_unwind(ui_core::test_support::gui_smoke::run_all);
    match outcome {
        Ok(report) => {
            println!("== GUI 端到端冒烟 · interaction 模式（headless + 离屏读回） ==");
            println!("像素后端：{}", report.backend);
            println!("场景数：{}", report.scenarios.len());
            for scenario in &report.scenarios {
                println!();
                println!("-- {} --", scenario.name);
                for line in &scenario.evidence {
                    println!("   {line}");
                }
            }
            println!();
            println!(
                "RESULT: PASS（6 个场景，{} 条实测证据）",
                report.evidence_count()
            );
            Ok(true)
        }
        Err(_) => {
            println!();
            println!("RESULT: FAIL（断言失败，诊断见上方 panic 信息）");
            Ok(false)
        }
    }
}

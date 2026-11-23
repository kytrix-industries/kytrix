use crate::Config;

#[cfg(not(feature = "test-support"))]
pub fn run(_config: &Config) -> Result<bool, Box<dyn std::error::Error>> {
    Err(Box::<dyn std::error::Error>::from(
        "interaction 模式需要测试基建：请用 \
         `cargo run --release -p ui_core --features test-support --example acceptance -- interaction` \
         构建（test-support 不进发布物，默认特性下没有 GUI 冒烟套件）。",
    ))
}

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

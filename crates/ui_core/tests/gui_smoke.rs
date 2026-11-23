#![cfg(feature = "test-support")]

use ui_core::test_support::gui_smoke;

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

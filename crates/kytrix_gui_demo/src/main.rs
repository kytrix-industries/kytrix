//! Kytrix GUI 的**最小示例应用**（`design/phase-09-win32-demo.md` §7）。
//!
//! ```text
//! cargo run -p kytrix_gui_demo                          # 真实窗口：彩色 quad + 文本 + 命中/退出
//! cargo run -p kytrix_gui_demo --features snapshot -- --snapshot out\demo.png
//! ```
//!
//! 两个入口共用同一份建树代码（[`scene`]）：窗口路径走 `Application::run`（真 present），
//! 快照路径走 `TestApp`（WARP 离屏 + readback，CI 无桌面也能验证像素）。

mod scene;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--snapshot") => snapshot(args.get(1).map(String::as_str)),
        _ => run_window(),
    }
}

/// 真实窗口：tokio runtime 由 demo 拥有，引擎只拿 `AppHandle` 需要的句柄。
fn run_window() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = kytrix_gui::app::App::new(runtime.handle().clone());
    let palette = scene::open(&mut app);
    scene::install(&mut app, palette);
    kytrix_gui::app::Application::new().run(app)?;
    Ok(())
}

/// 离屏像素：`--snapshot <path>`（需要 `--features snapshot`）。
#[cfg(feature = "snapshot")]
fn snapshot(path: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    use kytrix_gui::{
        math::Scale,
        test_support::{app::TestApp, snapshot::SnapshotCase},
        window::WindowOptions,
    };

    let path = path.unwrap_or("kytrix-demo.png");
    let mut test = TestApp::with_options(WindowOptions {
        title: "Kytrix GUI · Phase 09".into(),
        size: kytrix_gui::math::Size::new(scene::LOGICAL_WIDTH, scene::LOGICAL_HEIGHT),
        scale: Scale::ONE,
    });
    let window = test.window_id();
    scene::build_into(test.app_mut(), window);
    // 先跑一帧：`snapshot` 只读最近的实例缓冲（styles/text/layout/paint 都在 `frame` 里）。
    let _ = test.drain()?;
    let image = test
        .snapshot(SnapshotCase {
            // 场景由 demo 自己 `build_into` 挂载；这两个字段只用于 golden 命名（此处是元数据）。
            scene: kytrix_gui::test_support::snapshot::SnapshotScene::Shapes,
            theme: kytrix_gui::test_support::snapshot::SnapshotTheme::Light,
            size: kytrix_gui::math::Size::new(scene::LOGICAL_WIDTH, scene::LOGICAL_HEIGHT),
            scale: Scale::ONE,
        })
        .map_err(|error| format!("snapshot failed: {error:?}"))?;
    let buffer = image::RgbaImage::from_raw(image.width, image.height, image.rgba8)
        .ok_or("snapshot buffer size does not match its dimensions")?;
    buffer.save(path)?;
    println!("snapshot {}x{} -> {path}", image.width, image.height);
    Ok(())
}

/// 没有 `snapshot` feature 时的可读提示（不静默成功）。
#[cfg(not(feature = "snapshot"))]
fn snapshot(_path: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    Err("--snapshot needs `--features snapshot` (offscreen WARP render + PNG encoder)".into())
}

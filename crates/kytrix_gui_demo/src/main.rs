//! Kytrix GUI 的**最小示例应用**（`design/phase-09-win32-demo.md` §7、
//! `design/phase-15-demo-showcase.md`）。
//!
//! ```text
//! cargo run -p kytrix_gui_demo                          # 真实窗口：焦点 / CJK / 输入法 / 无障碍 / 滚动
//! cargo run -p kytrix_gui_demo --features snapshot -- --snapshot out\demo-showcase.png
//! ```
//!
//! 两个入口共用同一份建树代码（[`kytrix_gui_demo::scene`]）：窗口路径走 `Application::run`（真 present），
//! 快照路径走 `TestApp`（WARP 离屏 + readback，CI 无桌面也能验证像素）。
//!
//! 本文件只做启动、参数与顶层组装（AGENTS §5.4）；场景实现在库的 `scene` 模块。

use kytrix_gui::app::{App, Application};
use kytrix_gui_demo::scene::Scene;

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
    let mut app = App::new(runtime.handle().clone());
    let scene = Scene::open(&mut app);
    Scene::install(&mut app, scene);
    Application::new().run(app)?;
    Ok(())
}

/// 离屏像素：`--snapshot <path>`（需要 `--features snapshot`）。
#[cfg(feature = "snapshot")]
fn snapshot(path: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    use kytrix_gui::{
        math::Scale,
        test_support::{
            app::TestApp,
            snapshot::{SnapshotCase, SnapshotScene, SnapshotTheme},
        },
        window::WindowOptions,
    };

    let path = path.unwrap_or("kytrix-demo.png");
    let size = kytrix_gui::math::Size::new(
        kytrix_gui_demo::scene::LOGICAL_WIDTH,
        kytrix_gui_demo::scene::LOGICAL_HEIGHT,
    );
    let mut test = TestApp::with_options(WindowOptions {
        title: kytrix_gui_demo::scene::TITLE.into(),
        size,
        scale: Scale::ONE,
    });
    let window = test.window_id();
    let scene = Scene::build_into(test.app_mut(), window);
    // 先跑一帧让 `bind_style` 落地（输入框靠 `interactive: true` 可聚焦，`set_focus` 会拒绝未求值的节点）。
    let _ = test.drain()?;
    // 离屏截图要一次看全四类能力：选中色块、单选 B、滑块 70%、输入框有组合串与已提交文本、
    // 焦点落在输入框（光标可见）。
    scene.preview(test.app_mut());
    // 再跑一帧：`snapshot` 只读最近的实例缓冲（styles/text/layout/paint 都在 `frame` 里）。
    let _ = test.drain()?;
    let image = test
        .snapshot(SnapshotCase {
            // 场景由 demo 自己 `build_into` 挂载；这两个字段只用于 golden 命名（此处是元数据）。
            scene: SnapshotScene::Colors,
            theme: SnapshotTheme::Dark,
            size,
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

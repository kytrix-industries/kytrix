//! Kytrix GUI 的**离屏快照示例**，与 `demo` 共用 `scene` 模块
//! （`design/phase-15-demo-showcase.md`、`design/phase-18-demo-example.md`）。
//!
//! ```text
//! cargo run -p ui_core --example snapshot_demo --features test-support -- --snapshot out\demo-showcase.png
//! ```
//!
//! `test-support` 提供 `TestApp`（WARP 离屏 + readback）；`--snapshot [path]` 输出 PNG。
//! 本目标不安装 HUD，保证展示图稳定。

pub mod scene;

use scene::{Scene, TITLE, window_size};
use ui_core::{
    math::Scale,
    test_support::{
        app::TestApp,
        snapshot::{SnapshotCase, SnapshotScene, SnapshotTheme},
    },
    window::WindowOptions,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--snapshot") {
        return Err("usage: snapshot_demo --snapshot [path]".into());
    }
    let path = args.next().unwrap_or_else(|| "kytrix-demo.png".to_owned());
    if args.next().is_some() {
        return Err("usage: snapshot_demo --snapshot [path]".into());
    }
    capture(&path)
}

fn capture(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let size = window_size();
    let mut test = TestApp::with_options(WindowOptions {
        title: TITLE.into(),
        size,
        scale: Scale::ONE,
    });
    let window = test.window_id();
    let scene = Scene::build_into(test.app_mut(), window);
    // 先跑一帧让 `bind_style` 落地：输入框靠 `interactive: true` 可聚焦，`set_focus` 会拒绝未求值的节点。
    let _ = test.drain()?;
    // 离屏截图要一次看全四类能力：选中色块、单选 B、滑块 70%、输入框有组合串与已提交文本、
    // 焦点落在输入框（光标可见）。
    scene.preview(test.app_mut());
    // 再跑一帧：`snapshot` 只读最近的实例缓冲（styles/text/layout/paint 都在 `frame` 里）。
    let _ = test.drain()?;
    let image = test
        .snapshot(SnapshotCase {
            // 场景由 example 自己 `build_into` 挂载；这两个字段只用于 golden 命名（此处是元数据）。
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

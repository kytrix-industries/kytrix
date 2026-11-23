pub mod scene;

use scene::{Scene, TITLE, window_size};
use ui_core::{
    math::DevicePixelRatio,
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
        scale: DevicePixelRatio::ONE,
        ..WindowOptions::default()
    });
    let window = test.window_id();
    let scene = Scene::build_into(test.app_mut(), window);
    let _ = test.drain()?;
    scene.preview(test.app_mut());
    let _ = test.drain()?;
    let image = test
        .snapshot(SnapshotCase {
            scene: SnapshotScene::Colors,
            theme: SnapshotTheme::Dark,
            size,
            scale: DevicePixelRatio::ONE,
        })
        .map_err(|error| format!("snapshot failed: {error:?}"))?;
    let buffer = image::RgbaImage::from_raw(image.width, image.height, image.rgba8)
        .ok_or("snapshot buffer size does not match its dimensions")?;
    buffer.save(path)?;
    println!("snapshot {}x{} -> {path}", image.width, image.height);
    Ok(())
}

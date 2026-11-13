//! Kytrix GUI 的**真实窗口示例**（`design/phase-15-demo-showcase.md`、
//! `design/phase-18-demo-example.md`）。
//!
//! ```text
//! cargo run -p kytrix_gui --example demo
//! cargo run -p kytrix_gui --example demo --features hud   # 右上角 Debug HUD，F12 切换
//! ```
//!
//! 离屏截图是另一个 example：
//! `cargo run -p kytrix_gui --example snapshot_demo --features test-support -- --snapshot out\demo-showcase.png`。
//!
//! 本文件只做启动与顶层组装（AGENTS §5.4）；场景实现在 [`scene`]，与 `snapshot_demo` 共用。

pub mod scene;

use kytrix_gui::app::{App, Application};
use scene::Scene;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 平台事件循环由引擎拥有；应用只把 tokio runtime 的句柄交给 `App`。
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let scene = Scene::open(&mut app);
    Scene::install(&mut app, scene);
    Application::new().run(app)?;
    Ok(())
}

pub mod scene;

use scene::Scene;
use ui_core::app::{App, Application};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let scene = Scene::open(&mut app);
    Scene::install(&mut app, scene);
    Application::new().run(app)?;
    Ok(())
}

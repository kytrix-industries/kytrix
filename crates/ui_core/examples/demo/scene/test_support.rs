use ui_core::{
    event::{InputEvent, KeyCode, Modifiers},
    math::Scale,
    test_support::app::TestApp,
    window::{WindowId, WindowOptions},
};

use super::{Scene, TITLE, window_size};

pub(super) fn setup() -> (TestApp, WindowId, Scene) {
    let mut test = TestApp::with_options(WindowOptions {
        title: TITLE.into(),
        size: window_size(),
        scale: Scale::ONE,
        ..WindowOptions::default()
    });
    let window = test.window_id();
    let scene = Scene::build_into(test.app_mut(), window);
    (test, window, scene)
}

pub(super) fn setup_installed() -> (TestApp, WindowId, Scene) {
    let (mut test, window, scene) = setup();
    Scene::install(test.app_mut(), scene.clone());
    (test, window, scene)
}

pub(super) fn frame(test: &mut TestApp) {
    let _ = test.drain().expect("帧必须提交");
}

pub(super) fn press(code: KeyCode) -> InputEvent {
    InputEvent::key_down(code, Modifiers::empty())
}

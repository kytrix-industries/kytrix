//! 场景测试共用的无 GPU 搭建（`TestApp` / 窗口 / 一帧驱动）。
//!
//! 只在 `cfg(all(test, feature = "test-support"))` 下编译；生产 example 不包含。

use kytrix_gui::{
    event::{InputEvent, Key, KeyState, Modifiers},
    math::{Scale, Size},
    test_support::app::TestApp,
    window::{WindowId, WindowOptions},
};

use super::{LOGICAL_HEIGHT, LOGICAL_WIDTH, Scene, TITLE};

/// 建一个与真实窗口同尺寸的 headless 场景（未安装交互）。
pub(super) fn setup() -> (TestApp, WindowId, Scene) {
    let mut test = TestApp::with_options(WindowOptions {
        title: TITLE.into(),
        size: Size::new(LOGICAL_WIDTH, LOGICAL_HEIGHT),
        scale: Scale::ONE,
    });
    let window = test.window_id();
    let scene = Scene::build_into(test.app_mut(), window);
    (test, window, scene)
}

/// 建场景 + 安装交互（动作处理器 / 初始焦点 / 帧钩子）。
pub(super) fn setup_installed() -> (TestApp, WindowId, Scene) {
    let (mut test, window, scene) = setup();
    Scene::install(test.app_mut(), scene.clone());
    (test, window, scene)
}

/// 一帧：跑 styles/text/layout/paint 并提交。
pub(super) fn frame(test: &mut TestApp) {
    let _ = test.drain().expect("帧必须提交");
}

/// 无修饰键的按下事件。
pub(super) fn press(key: Key) -> InputEvent {
    InputEvent::Key {
        key,
        state: KeyState::Down,
        modifiers: Modifiers::empty(),
    }
}

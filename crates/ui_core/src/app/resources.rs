//! 应用外壳：资源与几何表访问面、剪贴板、事件缓冲、帧钩子、跨线程句柄、渲染器轮询。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use std::sync::Arc;

use super::{App, AppHandle};
use crate::{
    error::EngineError, event::Event, platform::dispatcher::Dispatcher, resources::ResourceCache,
    window::WindowId,
};

/// 注册每轮事件循环的钩子：在平台事件已落地、提交本帧之前调用一次。
///
/// 形态是 `&mut App`（可以建信号 / 改树 / 关窗），因此钩子内部**不能**再调用
/// `Application::run`。重复注册会替换上一个钩子。
/// 消费者：`App::on_frame`。
pub(crate) fn on_frame(app: &mut App, hook: impl FnMut(&mut App) + 'static) {
    app.frame_hook = Some(Box::new(hook));
}

/// 调用一次帧钩子（crate 私有；临时 take 以避免 `&mut self` 与闭包参数重叠）。
/// 消费者：`App::run_frame_hook`。
pub(crate) fn run_frame_hook(app: &mut App) {
    if let Some(mut hook) = app.frame_hook.take() {
        hook(app);
        app.frame_hook = Some(hook);
    }
}

/// 跨线程句柄（`AppHandle`）。
/// 消费者：`App::handle`。
#[must_use]
pub(crate) fn handle(app: &App) -> AppHandle {
    AppHandle {
        dispatcher: app.dispatcher.clone(),
        runtime: app.runtime.clone(),
    }
}

/// 平台层共享的唤醒队列（crate 私有）。
/// 消费者：`App::dispatcher`。
pub(crate) fn dispatcher(app: &App) -> Arc<Dispatcher> {
    app.dispatcher.clone()
}

/// 取走本帧的平台事件（`Application::run` 每帧填充；顺序即产生顺序）。
///
/// 队列有界（[`super::event_queue::EVENT_QUEUE_CAPACITY`]），取走不影响累计的丢弃计数。
/// 消费者：`App::drain_events`。
#[must_use]
pub(crate) fn drain_events(app: &mut App) -> Vec<(WindowId, Event)> {
    app.events.drain()
}

/// 平台层投递一个事件（`crate::platform::native` 唯一写入点）。
///
/// `Event::Input` 除了进观测缓冲，还会**立刻**走一次交互分发（P10 §5.3）；观测缓冲只用于
/// `drain_events`（应用读事件），不承担分发。缓冲是**有界**环形队列：超限丢最旧并计数
/// （R02-5），因此应用忘记 drain 也不会无界增长。
/// 消费者：`App::dispatch`。
pub(crate) fn dispatch(app: &mut App, window: WindowId, event: Event) {
    match &event {
        Event::Input(input) => {
            app.sync_clock();
            app.dispatch_input(window, input.clone());
        }
        Event::FocusChanged { focused: false } => app.on_window_focus_lost(window),
        _ => {}
    }
    app.events.push(window, event);
}

/// 写剪贴板文本（Win32 `CF_UNICODETEXT` / macOS `NSPasteboard` / Wayland `wl_data_source`）。
///
/// # Errors
/// `EngineError::Clipboard`：平台拒绝（剪贴板被占用、分配失败、无 owner 窗口）。
/// 消费者：`App::set_clipboard_text`。
pub(crate) fn set_clipboard_text(_app: &App, text: &str) -> Result<(), EngineError> {
    crate::platform::native::clipboard::set_text(text)
}

/// 读剪贴板文本（无文本内容时是空串）。
///
/// # Errors
/// `EngineError::Clipboard`：平台拒绝；Wayland 下「尚未收到过选区内容」也走这里
/// （剪贴板是异步拉模型，`design/phase-12-macos-linux.md` F-130）。
/// 消费者：`App::clipboard_text`。
pub(crate) fn clipboard_text(_app: &App) -> Result<smol_str::SmolStr, EngineError> {
    crate::platform::native::clipboard::get_text()
}

/// 图片资源缓存（只读）。应用层在**建树时**用 [`App::resources_mut`] 换 `AtlasTile`。
/// 消费者：`App::resources`。
#[must_use]
pub(crate) fn resources(app: &App) -> &ResourceCache {
    &app.resources
}

/// 图片资源缓存（可变）：`insert` / `unload` 的唯一入口。
/// 消费者：`App::resources_mut`。
pub(crate) fn resources_mut(app: &mut App) -> &mut ResourceCache {
    &mut app.resources
}

/// 静态几何注册表（只读）。
/// 消费者：`App::geometry`。
#[must_use]
pub(crate) fn geometry(app: &App) -> &crate::paint::geometry_store::GeometryStore {
    &app.geometry
}

/// 静态几何注册表（可变）：`insert` / `release` 的唯一入口。
/// 消费者：`App::geometry_mut`。
pub(crate) fn geometry_mut(app: &mut App) -> &mut crate::paint::geometry_store::GeometryStore {
    &mut app.geometry
}

/// 本 App 绑定的 tokio 句柄（**借用**，不 clone）。
///
/// 命名：不占用 `handle()` —— P09 会加 `App::handle() -> AppHandle`（那时是纯新增，零破坏）。
/// 消费者：`App::runtime_handle`。
#[must_use]
pub(crate) fn runtime_handle(app: &App) -> &tokio::runtime::Handle {
    &app.runtime
}

/// 轮询每个窗口的 GPU 线程反馈（三平台窗口路径；设备丢失 / 呈现失败在这里冒泡）。
///
/// 平台泵**每轮**调用（空闲时也要跑）：错误是粘性的，早一轮读到就能早一轮退出。
/// 消费者：`App::poll_renderers`。
pub(crate) fn poll_renderers(app: &mut App) -> Result<(), EngineError> {
    for window in app.windows.values_mut() {
        window.poll_renderer()?;
    }
    Ok(())
}

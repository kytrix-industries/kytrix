//! 应用外壳：资源与几何表访问面、剪贴板、事件缓冲、帧钩子、跨线程句柄、渲染器轮询。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use std::sync::Arc;

use super::{App, AppHandle};
use crate::{
    error::EngineError,
    event::Event,
    platform::dispatcher::Dispatcher,
    resources::{DecodeState, PendingResource, ResourceCache, ResourceImage},
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
///
/// D-6：钩子之前先轮询一次异步解码结果 —— 应用在钩子里即可取到 `Ready`/`Failed`。
/// 消费者：`App::run_frame_hook`。
pub(crate) fn run_frame_hook(app: &mut App) {
    poll_resources(app);
    if let Some(mut hook) = app.frame_hook.take() {
        hook(app);
        app.frame_hook = Some(hook);
    }
}

/// 轮询异步解码结果（D-6）：有交付就入缓冲并请求一帧。
///
/// 平台泵**每轮**调用（`App::run_frame_hook`），`App::frame` 入口也兜底一次；空闲且没有
/// pending 时 `poll` 是 O(0)，不产生帧（I3 不变式）。
///
/// **E7**：交付的 `Ready` 结果先写回引用对应 `PendingResource` 的图片节点（内在尺寸 +
/// `LAYOUT | PAINT`），再进应用可见的缓冲 —— 应用即使完全忽略 `take_resource_results`，
/// 图片也会自动上屏。
/// 消费者：`App::poll_resources`。
pub(crate) fn poll_resources(app: &mut App) {
    let results = app.resources.poll();
    if results.is_empty() {
        return;
    }
    apply_resource_results(app, &results);
    app.pending_resource_results.extend(results);
    app.request_frame();
}

/// 把交付结果里的 `Ready` 项写回图片节点（E7）。返回改写的节点数。
///
/// `Failed` 项不在这里处理：节点保持 `Pending`（占位），错误分类由应用从
/// `take_resource_results` 里读取后决定重试 / 换图。
pub(crate) fn apply_resource_results(
    app: &mut App,
    results: &[(PendingResource, DecodeState)],
) -> usize {
    let mut resolved: ahash::HashMap<PendingResource, ResourceImage> = ahash::HashMap::default();
    for (pending, state) in results {
        if let DecodeState::Ready(image) = state {
            resolved.insert(*pending, *image);
        }
    }
    if resolved.is_empty() {
        return 0;
    }
    let App { windows, .. } = app;
    let mut count = 0;
    for window in windows.values_mut() {
        let scale = window.scale().factor();
        count += window.tree_mut().resolve_pending_images(&resolved, scale);
    }
    count
}

/// E7：在布局之前解析图片节点的内在尺寸（首帧 / DPI 换档 / 资源句柄替换）。
///
/// **稳态** O(1) 早退条件：树没有「需要解析」标记且窗口 `scale_epoch` 没有变化。
/// 全树 DFS 只发生在三类低频帧（与 `refresh_text` 的「有脏就全扫」同型）：首帧
/// （`image_scale_epochs` 无记录）、挂载图片节点 / `set_image_resource` 换句柄、DPI 变化。
/// 返回尺寸变了的节点数。
pub(crate) fn refresh_images(app: &mut App, window: WindowId) -> usize {
    let App {
        windows,
        resources,
        image_scale_epochs,
        ..
    } = app;
    let Some(target) = windows.get_mut(&window) else {
        return 0;
    };
    let scale_epoch = target.scale_epoch();
    let scale_changed = image_scale_epochs.get(&window).copied() != Some(scale_epoch);
    // 先取标记：即使没有标记，DPI 变化也必须重解析（内在尺寸 = tile / scale）。
    let marked = target.tree_mut().take_images_need_refresh();
    if !marked && !scale_changed {
        return 0;
    }
    image_scale_epochs.insert(window, scale_epoch);
    let scale = target.scale().factor();
    target.tree_mut().refresh_image_intrinsics(resources, scale)
}

/// 释放图片资源并让所有窗口里引用它的图片节点失效重绘（E7）。
///
/// 返回 `false` = 资源不存在。**不动布局**：内在尺寸保留（卸载不引起布局跳动），
/// paint 跳过图元后节点背景 / 透明占位。
pub(crate) fn unload_resource(app: &mut App, id: crate::resources::ResourceId) -> bool {
    if !app.resources.unload(id) {
        return false;
    }
    let App { windows, .. } = app;
    let mut invalidated = 0;
    for window in windows.values_mut() {
        invalidated += window.tree_mut().invalidate_images_with_resource(id);
    }
    if invalidated > 0 {
        app.request_frame();
    }
    true
}

/// 关窗释放资源后：让**其它窗口**里引用已释放资源的图片节点失效（E7 悬垂防护）。
///
/// 窗口自己的树在关窗时整体丢弃（无悬垂）；这里只处理跨窗口共享句柄的情况。
/// 返回标脏的节点数（>0 时调用方应请求一帧）。
pub(crate) fn invalidate_dead_images(app: &mut App) -> usize {
    let App {
        windows, resources, ..
    } = app;
    let mut invalidated = 0;
    for window in windows.values_mut() {
        invalidated += window.tree_mut().invalidate_dead_images(resources);
    }
    invalidated
}

/// 取走已交付的异步解码结果（每项只交付一次；缓冲换空、容量归还）。
/// 消费者：`App::take_resource_results`。
pub(crate) fn take_resource_results(app: &mut App) -> Vec<(PendingResource, DecodeState)> {
    core::mem::take(&mut app.pending_resource_results)
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

/// 只取走 `keep` 认可的事件，其余**留在队列里**（原顺序）。
///
/// 消费者：`App::drain_events_where`（只拥有「自己窗口的文本 / 按键 / IME」的消费者 ——
/// 见 `base_ui::kit::input::InputNode::dispatch` 的「外壳 drain 之后逐条转发」契约）。
#[must_use]
pub(crate) fn drain_events_where(
    app: &mut App,
    keep: impl FnMut(WindowId, &Event) -> bool,
) -> Vec<(WindowId, Event)> {
    app.events.drain_where(keep)
}

/// 平台层投递一个事件（`crate::platform::native` 唯一写入点）。
///
/// `Event::Input` 除了进观测缓冲，还会**立刻**走一次交互分发（P10 §5.3）；观测缓冲只用于
/// `drain_events`（应用读事件），不承担分发。缓冲是**有界**环形队列：超限丢最旧并计数
/// （R02-5），因此应用忘记 drain 也不会无界增长。
///
/// **X1 引擎层模态兜底**：目标窗口是某个打开中的模态窗口的 owner 时，`Event::Input` /
/// `Event::Ime` 在入口处被丢弃（观测队列也不投递）—— 与 Windows 原生 `EnableWindow(owner,
/// FALSE)` 的效果一致：被模态挡住的窗口**完全看不到**输入，而不是「看到但引擎不响应」。
/// 系统通知类事件（`Resized`/`Moved`/`DpiChanged`/`AppearanceChanged`/`FocusChanged`/
/// `CloseRequested`）不受影响。Windows 与 macOS 都另有原生拦截（`EnableWindow` /
/// `ignoresMouseEvents` + 禁用标准按钮，RB-07）；这条引擎层路径是**兜底**（原生拦截被绕过
/// 或平台降级时语义仍一致），Linux/Wayland 没有原生模态协议，靠它实现模态输入语义。
/// 消费者：`App::dispatch`。
pub(crate) fn dispatch(app: &mut App, window: WindowId, event: Event) {
    if matches!(event, Event::Input(_) | Event::Ime(_))
        && super::window_set::is_input_blocked_by_modal(app, window)
    {
        return;
    }
    match &event {
        Event::Input(input) => {
            app.sync_clock();
            app.dispatch_input(window, input.clone());
        }
        Event::FocusChanged { focused } => {
            if !focused {
                app.on_window_focus_lost(window);
            }
            // v2.1 E1b：canvas 文本的 App 级默认 scale 跟随焦点窗口的 DPI。
            app.note_window_focus(window, *focused);
        }
        // `DpiChanged` 自带本次变化的权威 scale：Wayland 在派发之后才镜像 `Window::scale`
        // （读窗口会落后一次 DPI），Windows / macOS 虽然先镜像、用事件值同样正确。
        Event::DpiChanged { scale, .. } => app.note_window_dpi(window, *scale),
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
/// （剪贴板是异步拉模型）。
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

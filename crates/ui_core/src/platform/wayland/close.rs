//! 平台关闭请求的**裁决顺序**（Linux Wayland 泵的消费者）。
//!
//! # 为什么单独抽出来
//!
//! Windows（`windows/event_loop/native_window.rs`：`PlatformEvent::Close → Event::CloseRequested`
//! 经 `app.dispatch` 投递，泵再裁决）与 macOS（`macos/event_loop.rs`：手势路径先
//! `app.dispatch(id, Event::CloseRequested)` 再 `app.request_close`）都是
//! **先通知、后裁决**。旧 Linux 泵（`platform/linux/event_loop.rs`）只调 `app.request_close(id)`：
//! `on_close` 处理器照常执行（否决在 Linux 上**有效**），但应用在 `drain_events()` 里永远看不到
//! `Event::CloseRequested` —— 埋点、保存状态、关闭动画、自绘确认框这些基于事件的通路在 Linux
//! 上全部失效（审计 §11：报告「on_close 否决失效」不成立，缺的是**事件通知**）。
//!
//! 顺序如果留在 `platform/linux/event_loop.rs` 里就没法在开发机（Windows）上单测（该文件只在
//! Linux 目标编译）。因此抽到 `platform/wayland/`：纯 `App` 操作 + 返回「App 侧已关闭的窗口
//! id」，平台层据此销毁原生窗口与 a11y/镜像记账。整块 `platform::wayland` 在
//! `cfg(any(target_os = "linux", test))` 下编译，Windows 的 `cargo test` 能覆盖这条顺序。
//!
//! # 为什么不能「先裁决、再通知」
//!
//! `on_close` 里应用可能再观察事件（模态确认框、埋点）；Windows/macOS 的现状就是事件先到。
//! 反过来若处理器否决，事件仍必须**已经**投递：应用需要知道「用户尝试过关闭」，而不是只能从
//! 处理器自身被调用来反推。

use crate::{app::App, event::Event, window::WindowId};

/// 处理一批平台关闭请求：**先**投递 `Event::CloseRequested`（通知），**再**跑
/// [`App::request_close`] 裁决（`on_close` 返回 `false` 则保留窗口）。
///
/// 返回 `Vec<WindowId>`：`App` 侧确实关闭的窗口（`request_close` 返回 `true`），调用方据此
/// 销毁原生窗口与附属状态；被否决的窗口不在结果里（compositor 下一次 `xdg_toplevel.close`
/// 会再次请求）。处理器自己关掉窗口（`CloseVerdict::Gone`）时 `request_close` 返回 `false`，
/// 这里不返回它 —— 与旧 Linux 路径一致，由下一轮窗口同步兜底移除原生对象。
pub(crate) fn handle_close_requests(app: &mut App, closing: &[WindowId]) -> Vec<WindowId> {
    let mut closed = Vec::with_capacity(closing.len());
    for &id in closing {
        // 通知：`Event::CloseRequested` 的文档口径是「平台已处理关闭请求，事件仅通知」，
        // 应用据此做保存/埋点/动画，**不能**指望靠它拦下关闭（拦下走 `on_close`）。
        app.dispatch(id, Event::CloseRequested);
        // 裁决：`on_close` 返回 false ⇒ 保留窗口；不吞事件、不重试（与 Windows/macOS 同序）。
        if !app.request_close(id) {
            continue;
        }
        closed.push(id);
    }
    closed
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::handle_close_requests;
    use crate::{
        app::App,
        event::Event,
        math::{Scale, Size},
        window::{WindowId, WindowOptions},
    };

    fn setup() -> (tokio::runtime::Runtime, App, WindowId) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: "close".into(),
            size: Size::new(320.0, 240.0),
            scale: Scale::ONE,
            ..WindowOptions::default()
        });
        (runtime, app, window)
    }

    /// B11 核心回归：平台关闭请求必须**先**投递 `Event::CloseRequested`（应用可观测），
    /// **再**进入 `on_close` 裁决；否决时事件照常交付、窗口保留。
    ///
    /// 变异检查：删掉 [`handle_close_requests`] 里的 `app.dispatch(id, Event::CloseRequested)`
    /// 这一行 ⇒ `on_close` 处理器里 `drain_events()` 看不到事件 ⇒ 本测试红。
    #[test]
    fn a_close_request_notifies_before_it_is_decided() {
        let (_runtime, mut app, window) = setup();
        let notified_before_deciding = Rc::new(Cell::new(false));
        let flag = Rc::clone(&notified_before_deciding);
        app.on_close(window, move |app: &mut App| {
            // 处理器执行时事件必须已经在观测队列里（与 Windows / macOS 的同一顺序）。
            flag.set(observed_close_request(app, window));
            false // 否决
        });

        let closed = handle_close_requests(&mut app, &[window]);

        assert!(
            closed.is_empty(),
            "on_close 否决 ⇒ 不返回「已关闭」窗口（compositor 下次关闭会再请求）"
        );
        assert!(
            notified_before_deciding.get(),
            "on_close 处理器执行之前必须已经投递 Event::CloseRequested"
        );
        assert!(
            app.window(window).is_some(),
            "否决之后窗口必须保留（旧 Linux 路径的否决语义不变）"
        );
    }

    /// 允许关闭：事件先到、窗口随后从 `App` 移除，并作为「需销毁原生对象」的 id 返回。
    #[test]
    fn an_allowed_close_is_reported_after_the_event() {
        let (_runtime, mut app, window) = setup();
        let saw_event = Rc::new(Cell::new(false));
        let flag = Rc::clone(&saw_event);
        app.on_close(window, move |app: &mut App| {
            flag.set(observed_close_request(app, window));
            true
        });

        let closed = handle_close_requests(&mut app, &[window]);

        assert_eq!(closed, vec![window], "批准关闭的窗口必须交给平台层销毁");
        assert!(
            saw_event.get(),
            "on_close 处理器之前事件必须已投递（否则应用无法做关闭动画/埋点）"
        );
        assert!(
            app.window(window).is_none(),
            "批准之后 App 侧窗口必须已关闭"
        );
    }

    /// 没有注册 `on_close` 的窗口：直接关闭，但事件仍必须先投递（纯观察式应用靠它做持久化）。
    #[test]
    fn a_close_without_a_handler_still_notifies() {
        let (_runtime, mut app, window) = setup();

        let closed = handle_close_requests(&mut app, &[window]);

        assert_eq!(closed, vec![window]);
        assert!(app.window(window).is_none());
        assert!(
            observed_close_request(&mut app, window),
            "没有关闭处理器也必须在关闭前投递 CloseRequested"
        );
    }

    /// 从观测队列里找该窗口的 `Event::CloseRequested`（会一并取走其它事件，测试里不关心）。
    fn observed_close_request(app: &mut App, window: WindowId) -> bool {
        app.drain_events()
            .into_iter()
            .any(|(id, event)| id == window && matches!(event, Event::CloseRequested))
    }
}

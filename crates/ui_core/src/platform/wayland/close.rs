use crate::{app::App, event::Event, window::WindowId};

pub(crate) fn handle_close_requests(app: &mut App, closing: &[WindowId]) -> Vec<WindowId> {
    let mut closed = Vec::with_capacity(closing.len());
    for &id in closing {
        app.dispatch(id, Event::CloseRequested);
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
        math::{DevicePixelRatio, LogicalSize, px},
        window::{WindowId, WindowOptions},
    };

    fn setup() -> (tokio::runtime::Runtime, App, WindowId) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: "close".into(),
            size: LogicalSize::new(px(320.0), px(240.0)),
            scale: DevicePixelRatio::ONE,
            ..WindowOptions::default()
        });
        (runtime, app, window)
    }

    #[test]
    fn a_close_request_notifies_before_it_is_decided() {
        let (_runtime, mut app, window) = setup();
        let notified_before_deciding = Rc::new(Cell::new(false));
        let flag = Rc::clone(&notified_before_deciding);
        app.on_close(window, move |app: &mut App| {
            flag.set(observed_close_request(app, window));
            false
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

    fn observed_close_request(app: &mut App, window: WindowId) -> bool {
        app.drain_events()
            .into_iter()
            .any(|(id, event)| id == window && matches!(event, Event::CloseRequested))
    }
}

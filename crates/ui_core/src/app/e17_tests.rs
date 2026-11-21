//! E17：公开指针位置（`Window::pointer_position` / `App::pointer_position`）的引擎级用例。
//!
//! 断言面走 `test_support`（`TestApp` 的指针注入 + 帧记录），因此只在
//! `--features test-support` 的 lib 测试里编译。注入走与三端平台泵**同一条**
//! `App::dispatch_input`（`TestApp::inject`），不是另起一套捷径。
//!
//! 语义基线（spec-delta E17）：
//! - 位置是**逻辑客户区坐标**，平台层已按事件当时的 DPI 换算；引擎不再二次换算。
//! - 首次指针事件之前是 `None`，之后**保留最后值**（离开 / 失焦 / 缩放 / DPI 都不清除）。
//! - 纯记账：更新与读取都**不请求帧**（I3）。

use crate::{
    event::{Action, Event, InputEvent, Modifiers, PointerButton},
    math::{Point, Rect, Scale, Size},
    node::builder::NodeBuilder,
    test_support::app::TestApp,
    window::WindowOptions,
};

/// 悬停 / 光标都不变化的「等价移动」：只刷新位置，不产生任何脏标记 / 帧请求。
const P1: Point = Point::new(64.0, 32.0);
const P2: Point = Point::new(80.0, 48.0);

#[test]
fn nothing_is_reported_before_the_first_pointer_event() {
    let mut app = TestApp::new();
    assert_eq!(app.pointer_position(), None, "建窗后尚未收到指针事件");
    assert_eq!(app.app().pointer_position(app.window_id()), None);

    // 未知窗口（从没建过 / 已关闭）读作 None；不 panic、不建档。
    let extra = app.app_mut().create_window(WindowOptions::default());
    assert_eq!(app.app().pointer_position(extra), None);
    assert!(app.app_mut().close_window(extra), "关窗必须成功");
    assert_eq!(
        app.app().pointer_position(extra),
        None,
        "陈旧窗口句柄读作 None"
    );
}

#[test]
fn every_pointer_event_carrying_a_position_refreshes_it() {
    let mut app = TestApp::new();
    let enter = Point::new(10.0, 12.0);
    app.enter_pointer(enter);
    assert_eq!(app.pointer_position(), Some(enter), "PointerEnter 带进入点");

    app.move_pointer(P1);
    assert_eq!(app.pointer_position(), Some(P1), "PointerMove 刷新位置");

    // 第一次事件可能就是按下（窗口在指针下创建，没有 enter/move）：按下 / 抬起同样刷新。
    let down = Point::new(20.0, 24.0);
    app.inject(InputEvent::pointer_down(
        down,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    assert_eq!(app.pointer_position(), Some(down), "PointerDown 刷新位置");

    let up = Point::new(22.0, 26.0);
    app.inject(InputEvent::pointer_up(
        up,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    assert_eq!(app.pointer_position(), Some(up), "PointerUp 刷新位置");

    // 滚轮事件的位置同样是指针位置（平台只对指针所在窗口投递滚轮）。
    let scrolled = Point::new(30.0, 36.0);
    app.inject(InputEvent::scroll_lines(
        scrolled,
        Point::new(0.0, 1.0),
        Modifiers::empty(),
    ));
    assert_eq!(app.pointer_position(), Some(scrolled), "Scroll 刷新位置");
}

#[test]
fn leaving_keeps_the_last_known_position() {
    let mut app = TestApp::new();
    app.move_pointer(P1);
    let leave = Point::new(795.0, 300.0);
    app.leave_pointer(leave);
    assert_eq!(
        app.pointer_position(),
        Some(leave),
        "PointerLeave 自带离开点：保留最后值，不清成 None"
    );
    assert_eq!(app.pointer_position(), Some(leave), "重复读取稳定");
    assert_eq!(
        app.app().hovered(app.window_id()),
        None,
        "悬停已结束（保留坐标 ≠ 还在窗口内）"
    );

    // 窗口失焦同样不清除（应用可据 PointerEnter/Leave 或 hovered 判断在窗内与否）。
    app.dispatch_event(Event::FocusChanged { focused: false });
    assert_eq!(
        app.pointer_position(),
        Some(leave),
        "失焦不改变「最后已知位置」语义"
    );
}

#[test]
fn reentering_the_window_refreshes_the_position() {
    let mut app = TestApp::new();
    app.move_pointer(P1);
    app.leave_pointer(Point::new(-3.0, 5.0));
    let reenter = Point::new(400.0, 200.0);
    app.enter_pointer(reenter);
    assert_eq!(app.pointer_position(), Some(reenter));
}

#[test]
fn resizing_or_changing_dpi_does_not_remap_the_logical_position() {
    let mut app = TestApp::new();
    let window = app.window_id();
    app.move_pointer(P1);

    // 窗口缩放：位置是事件时刻的逻辑坐标快照，不随客户区尺寸变化重映射。
    app.app_mut()
        .window_mut(window)
        .expect("窗口")
        .set_size(Size::new(1600.0, 1200.0));
    app.dispatch_event(Event::Resized {
        size: Size::new(1600.0, 1200.0),
    });
    assert_eq!(app.pointer_position(), Some(P1), "缩放不清除 / 不重映射");

    // DPI：平台层在投递前已把物理坐标换算成逻辑坐标（Windows 用该消息的 scale，
    // macOS / Wayland 的坐标本来就是逻辑）。引擎只按原值记账，**不得**再乘 / 除 scale。
    let scale = Scale::new(2.0).expect("合法 scale");
    app.app_mut()
        .window_mut(window)
        .expect("窗口")
        .set_scale(scale);
    app.dispatch_event(Event::DpiChanged { dpi: 192, scale });
    assert_eq!(app.pointer_position(), Some(P1), "DPI 变化不重算旧位置");
    let logical = Point::new(120.0, 90.0);
    app.move_pointer(logical);
    assert_eq!(
        app.pointer_position(),
        Some(logical),
        "DPI=2 时逻辑坐标原样入库（引擎不做二次换算）"
    );
}

#[test]
fn a_capture_drag_keeps_updating_the_position_outside_the_window() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口").tree().root();
    let _grip = NodeBuilder::boxed()
        .layout_override(Rect::new(100.0, 0.0, 6.0, 100.0))
        .on_drag(Action::custom("split"))
        .build(
            app.app_mut().window_mut(window).expect("窗口").tree_mut(),
            root,
        )
        .expect("挂载拖动区");
    let _ = app.drain().expect("首帧");

    // 主键按下 + 越过阈值 ⇒ 引擎取得指针捕获（Windows SetCapture / AppKit 隐式 / Wayland grab）。
    app.inject(InputEvent::pointer_down(
        Point::new(103.0, 20.0),
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    app.move_pointer(Point::new(150.0, 20.0));
    assert!(
        app.app().pointer_captured(window).is_some(),
        "越过拖拽阈值必须取得指针捕获"
    );

    // 捕获期间平台继续投递窗口外坐标（客户区坐标：负数 / 超出 size 都是合法的）。
    let outside = Point::new(-40.5, -12.25);
    app.move_pointer(outside);
    assert_eq!(
        app.pointer_position(),
        Some(outside),
        "捕获拖拽期间的窗口外移动仍必须更新位置"
    );
    assert_eq!(app.app().pointer_position(window), Some(outside));

    // 窗口外松手：会话结束（捕获释放），位置保留最后值。
    let released = Point::new(-52.0, -18.0);
    app.inject(InputEvent::pointer_up(
        released,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    assert_eq!(app.pointer_position(), Some(released));
    assert_eq!(app.app().pointer_captured(window), None, "抬起必须释放捕获");
}

#[test]
fn reading_or_recording_the_position_never_requests_a_frame() {
    let mut app = TestApp::new();
    let _ = app.drain().expect("首帧");
    // 第一次移动会派生 HoverChanged 并请求一帧（既有悬停语义）：先让它落地。
    app.move_pointer(P1);
    let _ = app.drain().expect("悬停帧");

    let requests = app.app().frame_requests();
    let index = app.frame_record().frame_index;

    // ① 纯读取：两种读点都不得请求帧。
    assert_eq!(app.pointer_position(), Some(P1));
    assert_eq!(app.app().pointer_position(app.window_id()), Some(P1));
    assert_eq!(app.app().frame_requests(), requests, "读位置不得请求帧");

    // ② 悬停 / 光标不变的移动：只更新位置，不标脏、不请求帧（I3）。
    app.move_pointer(P2);
    assert_eq!(app.pointer_position(), Some(P2));
    assert_eq!(
        app.app().frame_requests(),
        requests,
        "位置记账本身不得请求帧（I3）"
    );
    let record = app.drain().expect("帧");
    assert!(!record.rendered, "纯位置记账不得产生像素帧");
    assert_eq!(record.frame_index, index, "FrameIndex 不得推进");

    // ③ 同一位置的重复移动同样不请求帧。
    app.move_pointer(P2);
    assert_eq!(app.app().frame_requests(), requests);
}

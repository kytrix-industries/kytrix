use std::time::Duration;

use ui_core::{
    event::{Action, Event, ImeEvent, InputEvent, KeyCode, Modifiers, PointerButton},
    math::{DevicePixelRatio, LogicalPoint, LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        atlas::{AtlasKind, AtlasTile},
        canvas::Canvas,
        color::Color,
        primitive::{Image as PaintImage, Quad, QuadPaint},
        style::{Clip, PaintSpec, ShadowSpec, Shadows},
        style_builder::StyleBuilder,
    },
    popup::{
        Alignment, Anchor, DismissOutcome, DismissPolicy, DismissReason, EscapePolicy, FocusPolicy,
        InitialFocus, Placement, PlacementSpec, PopupA11y, PopupAnimation, PopupId, PopupSpec,
        PopupState, PopupTier, RestoreFocus, ScrimSpec, Side,
    },
    test_support::{app::TestApp, record::PrimitiveKind},
    window::WindowId,
};

fn rect(x: f32, y: f32, width: f32, height: f32) -> LogicalRect {
    LogicalRect::new(px(x), px(y), px(width), px(height))
}

fn point(x: f32, y: f32) -> LogicalPoint {
    LogicalPoint::new(px(x), px(y))
}

fn win(app: &TestApp) -> WindowId {
    app.window_id()
}

fn build_box(app: &mut TestApp, parent: NodeId, size: (f32, f32), background: Color) -> NodeId {
    let window = win(app);
    NodeBuilder::boxed()
        .style(StyleBuilder::new().size(px(size.0), px(size.1)).build())
        .paint(PaintSpec {
            background,
            ..PaintSpec::default()
        })
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            parent,
        )
        .expect("box")
}

fn build_button(
    app: &mut TestApp,
    parent: NodeId,
    size: (f32, f32),
    action: &'static str,
) -> NodeId {
    let window = win(app);
    NodeBuilder::boxed()
        .style(StyleBuilder::new().size(px(size.0), px(size.1)).build())
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .on_click(Action::custom(action))
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            parent,
        )
        .expect("button")
}

fn root_node(app: &TestApp) -> NodeId {
    let window = win(app);
    app.app().window(window).expect("window").tree().root()
}

fn root_box(app: &mut TestApp, size: (f32, f32), background: Color) -> NodeId {
    let parent = root_node(app);
    build_box(app, parent, size, background)
}

fn root_button(app: &mut TestApp, size: (f32, f32), action: &'static str) -> NodeId {
    let parent = root_node(app);
    build_button(app, parent, size, action)
}

fn base_spec(anchor: Anchor) -> PopupSpec {
    let mut spec = PopupSpec::new(anchor, PlacementSpec::default());
    spec.animation = PopupAnimation::None;
    spec.focus = FocusPolicy::None;
    spec
}

fn menu_placement() -> PlacementSpec {
    PlacementSpec::submenu()
}

fn open_sized(
    app: &mut TestApp,
    spec: PopupSpec,
    size: (f32, f32),
    on_dismiss: Option<
        Box<dyn Fn(&mut ui_core::app::App, ui_core::popup::PopupDismiss) -> DismissOutcome>,
    >,
) -> PopupId {
    let window = win(app);
    app.app_mut()
        .open_popup(
            window,
            spec,
            move |app, window, node| {
                let target = app.window_mut(window).expect("window");
                NodeBuilder::boxed()
                    .style(StyleBuilder::new().size(px(size.0), px(size.1)).build())
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .build(target.tree_mut(), node)?;
                Ok(())
            },
            on_dismiss,
        )
        .expect("open popup")
}

fn open_child(app: &mut TestApp, parent: PopupId, anchor: Anchor, size: (f32, f32)) -> PopupId {
    let mut spec = base_spec(anchor);
    spec.parent = Some(parent);
    spec.placement = menu_placement();
    open_sized(app, spec, size, None)
}

fn popup_node(app: &TestApp, id: PopupId) -> NodeId {
    app.app()
        .popup_stack(win(app))
        .expect("stack")
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .map_or_else(|| panic!("popup {id:?} missing"), |entry| entry.node)
}

fn frame(app: &mut TestApp) {
    app.drain().expect("frame");
}

fn stable_frame(app: &mut TestApp) {
    let id = app
        .app()
        .popup_stack(win(app))
        .expect("stack")
        .entries
        .first()
        .map(|entry| entry.id);
    if let Some(id) = id {
        let node = popup_node(app, id);
        let w = win(app);
        app.app_mut()
            .request_node_repaint(w, node)
            .expect("repaint");
    }
    frame(app);
}

fn press(app: &mut TestApp, position: LogicalPoint) -> (ui_core::event::InputOutcome, bool) {
    let window = win(app);
    app.app_mut().dispatch_input_tracked_for_test(
        window,
        InputEvent::pointer_down(position, PointerButton::Primary, Modifiers::empty()),
    )
}

fn release(app: &mut TestApp, position: LogicalPoint) -> (ui_core::event::InputOutcome, bool) {
    let window = win(app);
    app.app_mut().dispatch_input_tracked_for_test(
        window,
        InputEvent::pointer_up(position, PointerButton::Primary, Modifiers::empty()),
    )
}

fn escape(app: &mut TestApp) -> (ui_core::event::InputOutcome, bool) {
    let window = win(app);
    app.app_mut().dispatch_input_tracked_for_test(
        window,
        InputEvent::key_down(KeyCode::Escape, Modifiers::empty()),
    )
}

#[test]
fn opening_a_popup_settles_geometry_in_the_first_frame() {
    let mut app = TestApp::new();
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0))),
        (120.0, 80.0),
        None,
    );
    frame(&mut app);
    let first = app.app().popup_rect(id).expect("rect");
    assert_eq!(first.hit, rect(100.0, 124.0, 120.0, 80.0));
    assert_eq!(first.a11y, first.hit);
    assert!(!first.paint.is_empty());
    assert_eq!(
        app.frame_record().stats.popup_settle_rounds,
        2,
        "首帧必须完成尺寸轮与定位轮"
    );
    assert!(!app.frame_record().stats.popup_unsettled);
    stable_frame(&mut app);
    assert_eq!(
        app.app().popup_rect(id).expect("rect").hit,
        first.hit,
        "首可见帧 rect 必须等于稳定帧"
    );
    assert_eq!(app.frame_record().stats.popup_settle_rounds, 0);
}

#[test]
fn content_growth_settles_in_one_frame_without_a_second_displacement() {
    let mut app = TestApp::new();
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0))),
        (80.0, 40.0),
        None,
    );
    frame(&mut app);
    let node = popup_node(&app, id);
    let window = win(&app);
    NodeBuilder::boxed()
        .style(StyleBuilder::new().size(px(200.0), px(40.0)).build())
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            node,
        )
        .expect("child");
    frame(&mut app);
    let grown = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(grown.size, LogicalSize::new(px(280.0), px(40.0)));
    assert!(!app.frame_record().stats.popup_unsettled);
    stable_frame(&mut app);
    assert_eq!(app.app().popup_rect(id).expect("rect").hit, grown);
}

#[test]
fn a_scrolled_anchor_carries_the_popup_in_the_same_frame() {
    let mut app = TestApp::new();
    let window = win(&app);
    let container = {
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let mut style = StyleBuilder::new().size(px(200.0), px(100.0)).build();
        style.clip = Clip::Bounds;
        let container = NodeBuilder::boxed()
            .style(style)
            .build(tree, tree.root())
            .expect("container");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(200.0), px(600.0)).build())
            .build(tree, container)
            .expect("content");
        container
    };
    app.app_mut()
        .make_scroll(
            window,
            container,
            ui_core::interaction::ScrollConfig::default(),
        )
        .expect("scroll");
    let handle = app
        .app()
        .scroll_handle(window, container)
        .expect("scroll handle");
    app.app_mut()
        .set_scroll_content(&handle, LogicalSize::new(px(200.0), px(600.0)))
        .expect("content");
    frame(&mut app);
    let content_node = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(container)
        .expect("container")
        .children()[0];
    let item = {
        let window = win(&app);
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .size(px(200.0), px(40.0))
                    .absolute()
                    .left(px(0.0))
                    .top(px(200.0))
                    .build(),
            )
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                content_node,
            )
            .expect("item")
    };
    frame(&mut app);
    let id = open_sized(
        &mut app,
        base_spec(Anchor::Node { node: item }),
        (100.0, 40.0),
        None,
    );
    frame(&mut app);
    let before = app.app().popup_rect(id).expect("rect").hit;
    let handle = app
        .app()
        .scroll_handle(window, container)
        .expect("scroll handle");
    app.app_mut()
        .scroll_to(&handle, point(0.0, 120.0))
        .expect("scroll");
    frame(&mut app);
    let after = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(after.origin.y, before.origin.y - 120.0);
    assert_eq!(after.origin.x, before.origin.x);
    stable_frame(&mut app);
    assert_eq!(app.app().popup_rect(id).expect("rect").hit, after);
}

#[test]
fn a_popup_flips_above_when_the_bottom_does_not_fit() {
    let mut app = TestApp::new();
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 540.0, 60.0, 20.0))),
        (120.0, 120.0),
        None,
    );
    frame(&mut app);
    let hit = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(
        hit,
        rect(100.0, 416.0, 120.0, 120.0),
        "flip 到锚点上方并保留 gap"
    );
}

#[test]
fn a_window_resize_reclamps_the_popup() {
    let mut app = TestApp::new();
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(400.0, 100.0, 60.0, 20.0))),
        (200.0, 80.0),
        None,
    );
    frame(&mut app);
    let before = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(before, rect(400.0, 124.0, 200.0, 80.0));
    let window = win(&app);
    app.app_mut()
        .window_mut(window)
        .expect("window")
        .set_size(LogicalSize::new(px(500.0), px(400.0)));
    frame(&mut app);
    let after = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(after.max().x, px(496.0), "必须按 viewport margin 夹取");
    assert!(after.origin.x < before.origin.x);
}

#[test]
fn three_menu_levels_follow_the_parent_without_a_frame_lag() {
    let mut app = TestApp::new();
    let first = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0))),
        (120.0, 160.0),
        None,
    );
    frame(&mut app);
    let first_node = popup_node(&app, first);
    let item = {
        let window = win(&app);
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .size(px(120.0), px(24.0))
                    .absolute()
                    .left(px(0.0))
                    .top(px(32.0))
                    .build(),
            )
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                first_node,
            )
            .expect("item")
    };
    frame(&mut app);
    let second = open_child(&mut app, first, Anchor::Node { node: item }, (80.0, 60.0));
    frame(&mut app);
    let second_node = popup_node(&app, second);
    let sub_item = {
        let window = win(&app);
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .size(px(80.0), px(24.0))
                    .absolute()
                    .left(px(0.0))
                    .top(px(8.0))
                    .build(),
            )
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                second_node,
            )
            .expect("sub item")
    };
    frame(&mut app);
    let third = open_child(
        &mut app,
        second,
        Anchor::Node { node: sub_item },
        (70.0, 40.0),
    );
    frame(&mut app);
    let origin_first = app.app().popup_rect(first).expect("rect").hit;
    let origin_second = app.app().popup_rect(second).expect("rect").hit;
    let origin_third = app.app().popup_rect(third).expect("rect").hit;
    assert_eq!(origin_second.origin.y, origin_first.origin.y + 32.0);
    assert_eq!(origin_second.origin.x, origin_first.origin.x + 120.0 + 4.0);
    assert_eq!(origin_third.origin.y, origin_second.origin.y + 8.0);
    assert_eq!(origin_third.origin.x, origin_second.origin.x + 80.0 + 4.0);
    app.app_mut()
        .set_popup_anchor(first, Anchor::WindowRect(rect(300.0, 260.0, 60.0, 20.0)))
        .expect("re-anchor");
    frame(&mut app);
    let moved_first = app.app().popup_rect(first).expect("rect").hit;
    let moved_second = app.app().popup_rect(second).expect("rect").hit;
    let moved_third = app.app().popup_rect(third).expect("rect").hit;
    let delta = LogicalPoint::new(
        moved_first.origin.x - origin_first.origin.x,
        moved_first.origin.y - origin_first.origin.y,
    );
    assert_ne!(delta, LogicalPoint::ZERO);
    assert_eq!(
        moved_second.origin,
        LogicalPoint::new(
            origin_second.origin.x + delta.x,
            origin_second.origin.y + delta.y
        ),
        "第二级必须同帧跟随父级位移"
    );
    assert_eq!(
        moved_third.origin,
        LogicalPoint::new(
            origin_third.origin.x + delta.x,
            origin_third.origin.y + delta.y
        ),
        "第三级必须同帧跟随父级位移（不得逐层滞后）"
    );
    stable_frame(&mut app);
    assert_eq!(app.app().popup_rect(third).expect("rect").hit, moved_third);
}

#[test]
fn idle_frames_do_no_popup_settle_work() {
    let mut app = TestApp::new();
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0))),
        (120.0, 80.0),
        None,
    );
    frame(&mut app);
    stable_frame(&mut app);
    let window = win(&app);
    let outcome = app.app_mut().frame(window).expect("window").expect("frame");
    assert!(
        matches!(outcome, ui_core::window::FrameOutcome::Idle),
        "空闲帧必须 Idle"
    );
    let _ = id;
}

#[test]
fn a_removed_anchor_propagates_the_anchor_lost_dismissal() {
    let mut app = TestApp::new();
    let window = win(&app);
    let anchor = root_box(&mut app, (60.0, 20.0), Color::WHITE);
    frame(&mut app);
    let id = open_sized(
        &mut app,
        base_spec(Anchor::Node { node: anchor }),
        (120.0, 80.0),
        None,
    );
    frame(&mut app);
    assert!(app.app().popup_rect(id).is_some());
    app.app_mut().remove_node(window, anchor).expect("remove");
    frame(&mut app);
    assert!(
        app.app()
            .popup_stack(window)
            .expect("stack")
            .entries
            .is_empty(),
        "默认 dismiss 策略必须关闭 anchor 丢失的链"
    );
}

#[test]
fn window_deactivation_closes_auto_but_keeps_modal() {
    let mut app = TestApp::new();
    let window = win(&app);
    let auto = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (60.0, 30.0),
        None,
    );
    frame(&mut app);
    assert!(app.app().popup_rect(auto).is_some());
    app.app_mut().window_focus_lost(window);
    frame(&mut app);
    assert!(
        app.app().popup_rect(auto).is_none(),
        "窗口失焦必须关闭 Auto 链"
    );
    let mut modal_spec = base_spec(Anchor::WindowRect(rect(300.0, 100.0, 20.0, 20.0)));
    modal_spec.tier = PopupTier::Modal;
    modal_spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    let modal = open_sized(&mut app, modal_spec, (200.0, 120.0), None);
    frame(&mut app);
    app.app_mut().window_focus_lost(window);
    frame(&mut app);
    assert!(
        app.app().popup_rect(modal).is_some(),
        "窗口失焦必须保留 Modal"
    );
}

#[test]
fn opening_and_closing_a_non_modal_popup_never_forces_a_full_repaint() {
    let mut app = TestApp::new();
    let background = root_box(&mut app, (400.0, 300.0), Color::BLACK);
    frame(&mut app);
    let before = app.probe().bounds(background).expect("bounds");
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0))),
        (120.0, 80.0),
        None,
    );
    frame(&mut app);
    assert!(
        !app.frame_record().damage_full,
        "打开非模态 popup 不得整帧重绘"
    );
    assert_eq!(app.probe().bounds(background).expect("bounds"), before);
    assert!(app.app().popup_rect(id).is_some());
    app.app_mut().close_popup(id).expect("close");
    frame(&mut app);
    assert!(
        !app.frame_record().damage_full,
        "关闭非模态 popup 不得整帧重绘"
    );
    assert!(app.app().popup_rect(id).is_none());
    assert_eq!(
        app.probe().bounds(background).expect("bounds"),
        before,
        "I9：popup 开关不得改变主树布局"
    );
}

#[test]
fn pointer_down_on_the_opener_toggles_the_chain_and_consumes_the_press() {
    let mut app = TestApp::new();
    let trigger = root_button(&mut app, (100.0, 30.0), "open");
    frame(&mut app);
    let mut spec = base_spec(Anchor::NodeRect {
        node: trigger,
        rect: rect(0.0, 0.0, 100.0, 30.0),
    });
    spec.opener = Some(trigger);
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    let center = point(20.0, 15.0);
    let (_, down_consumed) = press(&mut app, center);
    assert!(down_consumed, "同一 opener 的 Toggle 必须消费按下");
    let (_, up_consumed) = release(&mut app, center);
    assert!(up_consumed, "被消费的按下必须同时消费抬起");
    assert!(app.app().popup_rect(id).is_none());
}

#[test]
fn outside_press_replays_the_click_to_the_background() {
    let mut app = TestApp::new();
    let background = root_button(&mut app, (300.0, 300.0), "background");
    frame(&mut app);
    let hits = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&hits);
    app.app_mut()
        .on_action(Action::custom("background"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(600.0, 500.0, 20.0, 20.0))),
        (60.0, 30.0),
        None,
    );
    frame(&mut app);
    let center = point(100.0, 100.0);
    let (_, down_consumed) = press(&mut app, center);
    assert!(!down_consumed, "默认 Replay 不得消费按下");
    let (_, up_consumed) = release(&mut app, center);
    assert!(!up_consumed, "默认 Replay 不得消费抬起");
    assert_eq!(hits.get(), 1, "press+release 必须重派发给下层控件");
    assert!(app.app().popup_rect(id).is_none());
    let _ = background;
}

#[test]
fn escape_closes_exactly_one_open_level() {
    let mut app = TestApp::new();
    let first = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (120.0, 160.0),
        None,
    );
    frame(&mut app);
    let first_node = popup_node(&app, first);
    let item = {
        let window = win(&app);
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(120.0), px(24.0)).build())
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                first_node,
            )
            .expect("item")
    };
    frame(&mut app);
    let second = open_child(&mut app, first, Anchor::Node { node: item }, (80.0, 60.0));
    frame(&mut app);
    let (_, consumed) = escape(&mut app);
    assert!(consumed, "Esc 必须返回 consumed");
    assert!(app.app().popup_rect(second).is_none());
    assert!(app.app().popup_rect(first).is_some(), "Esc 只关最上层");
}

#[test]
fn a_tooltip_escape_closes_only_the_tooltip() {
    let mut app = TestApp::new();
    let menu = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (120.0, 120.0),
        None,
    );
    let mut hint_spec = base_spec(Anchor::WindowRect(rect(300.0, 100.0, 20.0, 20.0)));
    hint_spec.tier = PopupTier::Hint;
    hint_spec.dismiss = DismissPolicy::Hint { escape: true };
    let hint = open_sized(&mut app, hint_spec, (80.0, 30.0), None);
    frame(&mut app);
    let (_, consumed) = escape(&mut app);
    assert!(consumed);
    assert!(app.app().popup_rect(hint).is_none(), "Hint 优先关闭");
    assert!(
        app.app().popup_rect(menu).is_some(),
        "Hint 的 Esc 不得关闭菜单"
    );
}

#[test]
fn opening_a_take_popup_moves_focus_and_closing_restores_it_once() {
    let mut app = TestApp::new();
    let window = win(&app);
    let trigger = root_button(&mut app, (100.0, 30.0), "trigger");
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(trigger)));
    assert_eq!(app.app().focused(window), Some(trigger));
    let mut spec = base_spec(Anchor::NodeRect {
        node: trigger,
        rect: rect(0.0, 0.0, 100.0, 30.0),
    });
    spec.focus = FocusPolicy::Take {
        restore: RestoreFocus::Opener,
    };
    spec.opener = Some(trigger);
    let target = app
        .app_mut()
        .open_popup(
            window,
            spec,
            |app, window, node| {
                NodeBuilder::boxed()
                    .style(StyleBuilder::new().size(px(120.0), px(80.0)).build())
                    .build(app.window_mut(window).expect("window").tree_mut(), node)?;
                Ok(())
            },
            None,
        )
        .expect("open");
    frame(&mut app);
    let popup_focus = app.app().focused(window);
    assert!(popup_focus.is_some());
    assert_ne!(popup_focus, Some(trigger), "Take 必须取走焦点");
    app.app_mut().close_popup(target).expect("close");
    frame(&mut app);
    assert_eq!(
        app.app().focused(window),
        Some(trigger),
        "关闭必须恢复 opener 焦点"
    );
}

#[test]
fn dialog_tab_cycles_inside_the_modal_scope() {
    let mut app = TestApp::new();
    let window = win(&app);
    let background = root_button(&mut app, (100.0, 30.0), "background");
    frame(&mut app);
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    spec.tier = PopupTier::Modal;
    spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::FirstFocusable,
    };
    let _dialog = app
        .app_mut()
        .open_popup(
            window,
            spec,
            |app, window, node| {
                for action in ["first", "second"] {
                    NodeBuilder::boxed()
                        .style(StyleBuilder::new().size(px(100.0), px(30.0)).build())
                        .on_click(Action::custom(action))
                        .build(app.window_mut(window).expect("window").tree_mut(), node)?;
                }
                Ok(())
            },
            None,
        )
        .expect("dialog");
    frame(&mut app);
    let first = app.app().focused(window).expect("initial focus");
    app.app_mut().focus_next(window);
    let second = app.app().focused(window).expect("second");
    assert_ne!(second, first);
    app.app_mut().focus_next(window);
    let wrapped = app.app().focused(window).expect("wrapped");
    assert_eq!(wrapped, first, "Tab 必须在 modal scope 内闭环");
    assert_ne!(wrapped, background);
    app.app_mut().focus_prev(window);
    assert_eq!(app.app().focused(window), Some(second));
}

#[test]
fn opening_a_popup_from_an_editable_cancels_ime_composition() {
    let mut app = TestApp::new();
    let window = win(&app);
    let editable = ui_core::text_edit::EditableSpec {
        role: ui_core::a11y::A11yRole::TextInput,
        label: "".into(),
        placeholder: "".into(),
        multiline: false,
        masked: false,
        readonly: false,
        disabled: false,
        max_length: None,
        submit: None,
        text_style: ui_core::text::TextStyle::new(px(16.0)),
        caret_color: Color::BLACK,
        selection_color: Color::BLACK,
        placeholder_color: Color::BLACK,
        composition_color: Color::BLACK,
    };
    let node = {
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        NodeBuilder::editable_text(editable)
            .build(tree, tree.root())
            .expect("editable")
    };
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(node)));
    app.dispatch_event(Event::Ime(ImeEvent::Preedit {
        text: "ni".into(),
        caret: None,
        clauses: Default::default(),
        replace: None,
    }));
    assert!(
        app.app()
            .text_edit_composition(window, node)
            .expect("composition")
            .is_some(),
        "前置条件：必须先进入组合"
    );
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    spec.focus = FocusPolicy::Take {
        restore: RestoreFocus::Opener,
    };
    let id = open_sized(&mut app, spec, (60.0, 30.0), None);
    frame(&mut app);
    assert!(app.app().popup_rect(id).is_some());
    assert_ne!(app.app().focused(window), Some(node));
    assert!(
        app.app()
            .text_edit_composition(window, node)
            .expect("composition")
            .is_none(),
        "打开 popup 抢焦必须取消 composition"
    );
}

#[test]
fn slide_animation_offsets_paint_and_hit_without_moving_the_settle_rect() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0)));
    spec.animation = PopupAnimation::FadeSlide;
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    let settled = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(settled, rect(100.0, 124.0, 120.0, 80.0));
    app.advance(Duration::from_millis(30));
    frame(&mut app);
    let entry = app.app().popup_stack(win(&app)).expect("stack").entries[0];
    assert!(
        entry.paint_offset.y > px(0.0) && entry.paint_offset.y < px(4.0),
        "paint_offset 必须随动画收敛：{:?}",
        entry.paint_offset
    );
    let offset = entry.paint_offset;
    let content = popup_node(&app, id);
    let painted_node = {
        let window = win(&app);
        let tree = app.app().window(window).expect("window").tree();
        tree.get(content).expect("content").children()[0]
    };
    let painted = app
        .rendered()
        .expect("rendered frame")
        .quads
        .iter()
        .find(|quad| quad.node == painted_node)
        .map(|quad| quad.bounds)
        .expect("动画帧必须绘制 popup 内容");
    assert_eq!(
        painted.size, settled.size,
        "动画不得改变绘制尺寸（paint_offset 不参与 layout）"
    );
    assert_eq!(
        painted.origin,
        LogicalPoint::new(settled.origin.x + offset.x, settled.origin.y + offset.y),
        "FrameRecord 的绘制位置必须跟随 paint_offset"
    );
    assert!(
        damage_covers(&app, settled.translate(offset)),
        "动画帧 damage 必须覆盖偏移后的绘制区域"
    );
    let background = {
        let window = win(&app);
        app.app().window(window).expect("window").tree().root()
    };
    let shifted_only = point(
        (settled.origin.x + 10.0).value(),
        (settled.max().y + offset.y.value() * 0.5).value(),
    );
    let shifted_hit = app
        .probe()
        .hit(shifted_only)
        .map(|result| result.node)
        .expect("偏移绘制区域必须可命中");
    assert_ne!(
        shifted_hit, background,
        "hit 必须跟随带 paint_offset 的绘制区域"
    );
    let settled_only = point(
        (settled.origin.x + 10.0).value(),
        (settled.origin.y + offset.y.value() * 0.5).value(),
    );
    assert_eq!(
        app.probe().hit(settled_only).map(|result| result.node),
        Some(background),
        "动画期间命中不得停留在最终位置"
    );
    app.advance(Duration::from_millis(120));
    frame(&mut app);
    let entry = app.app().popup_stack(win(&app)).expect("stack").entries[0];
    assert_eq!(entry.paint_offset, LogicalPoint::ZERO);
    assert_eq!(entry.state, PopupState::Open);
    assert_eq!(app.app().popup_rect(id).expect("rect").hit, settled);
}

#[test]
fn closing_animation_blocks_hit_immediately_then_removes_the_node() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0)));
    spec.animation = PopupAnimation::Fade;
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    app.advance(Duration::from_millis(130));
    frame(&mut app);
    let hit = app.app().popup_rect(id).expect("rect").hit;
    let (_, consumed) = escape(&mut app);
    assert!(consumed);
    let content = popup_node(&app, id);
    let stack = app.app().popup_stack(win(&app)).expect("stack");
    assert_eq!(stack.entries.len(), 1, "关闭动画期间节点保留");
    assert_eq!(stack.entries[0].state, PopupState::Closing);
    assert_ne!(
        app.probe()
            .hit(point(
                (hit.origin.x + 10.0).value(),
                (hit.origin.y + 10.0).value(),
            ))
            .map(|result| result.node),
        Some(content),
        "Closing/DismissPending 必须立即不可命中"
    );
    app.advance(Duration::from_millis(95));
    frame(&mut app);
    assert!(
        app.app()
            .popup_stack(win(&app))
            .expect("stack")
            .entries
            .is_empty(),
        "关闭动画结束后必须移除节点"
    );
}

#[test]
fn a_modal_scrim_closes_with_the_modal() {
    let mut app = TestApp::new();
    let window = win(&app);
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    spec.tier = PopupTier::Modal;
    spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    spec.scrim = Some(ScrimSpec {
        density: 0.5,
        ..ScrimSpec::default()
    });
    spec.animation = PopupAnimation::Fade;
    let _modal = open_sized(&mut app, spec, (200.0, 120.0), None);
    frame(&mut app);
    assert_eq!(
        app.app().popup_stack(window).expect("stack").entries.len(),
        2,
        "modal + scrim"
    );
    let (_, consumed) = escape(&mut app);
    assert!(consumed);
    app.advance(Duration::from_millis(95));
    frame(&mut app);
    assert!(
        app.app()
            .popup_stack(window)
            .expect("stack")
            .entries
            .is_empty(),
        "scrim 与 modal 生命周期绑定"
    );
}

#[test]
fn a_dialog_blocks_background_openers() {
    let mut app = TestApp::new();
    let window = win(&app);
    frame(&mut app);
    let mut modal_spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    modal_spec.tier = PopupTier::Modal;
    modal_spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    let _modal = open_sized(&mut app, modal_spec, (200.0, 120.0), None);
    frame(&mut app);
    let (_, down_consumed) = press(&mut app, point(20.0, 15.0));
    assert!(down_consumed, "模态 scope 外的按下必须被消费");
    let (_, up_consumed) = release(&mut app, point(20.0, 15.0));
    assert!(up_consumed, "模态 scope 外的抬起必须被消费");
    assert!(
        app.app()
            .popup_stack(window)
            .expect("stack")
            .entries
            .iter()
            .all(|entry| entry.tier == PopupTier::Modal),
        "背景 opener 不得被触发"
    );
}

#[test]
fn dismiss_topmost_keeps_its_programmatic_close_semantics() {
    let mut app = TestApp::new();
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (60.0, 30.0),
        Some(Box::new(|_app, dismiss| {
            assert_eq!(dismiss.reason, DismissReason::Programmatic);
            DismissOutcome::Close
        })),
    );
    frame(&mut app);
    let w = win(&app);
    assert!(
        app.app_mut()
            .dismiss_topmost(w, DismissReason::Programmatic)
    );
    assert!(app.app().popup_rect(id).is_none());
}

fn damage_covers(app: &TestApp, area: LogicalRect) -> bool {
    let mut union = ui_core::math::DeviceRect::default();
    for rect in &app.frame_record().damage {
        union = union.union(*rect);
    }
    let window = win(app);
    let dpr = app
        .app()
        .window(window)
        .expect("window")
        .device_pixel_ratio();
    let target = dpr.to_covering_device_rect(area);
    !target.is_empty()
        && union.x0 <= target.x0
        && union.y0 <= target.y0
        && union.x1 >= target.x1
        && union.y1 >= target.y1
}

#[test]
fn outside_press_consume_is_explicit_opt_in() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(600.0, 500.0, 20.0, 20.0)));
    spec.dismiss = DismissPolicy::Chain {
        outside: ui_core::popup::OutsidePress::Consume,
        escape: EscapePolicy::CloseTopmost,
        anchor_lost: true,
        focus_lost: true,
    };
    let id = open_sized(&mut app, spec, (60.0, 30.0), None);
    frame(&mut app);
    let center = point(100.0, 100.0);
    let (_, down_consumed) = press(&mut app, center);
    assert!(down_consumed, "Consume 策略必须消费按下");
    let (_, up_consumed) = release(&mut app, center);
    assert!(up_consumed, "Consume 策略必须消费抬起");
    assert!(app.app().popup_rect(id).is_none());
}

#[test]
fn a_different_opener_closes_the_old_chain_and_dispatches_the_press() {
    let mut app = TestApp::new();
    let window = win(&app);
    let first_trigger = {
        let tree = app.app_mut().window_mut(window).expect("window");
        let parent = tree.tree().root();
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .size(px(100.0), px(30.0))
                    .absolute()
                    .left(px(0.0))
                    .top(px(0.0))
                    .build(),
            )
            .build(tree.tree_mut(), parent)
            .expect("first trigger")
    };
    let second_trigger = {
        let tree = app.app_mut().window_mut(window).expect("window");
        let parent = tree.tree().root();
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .size(px(100.0), px(30.0))
                    .absolute()
                    .left(px(200.0))
                    .top(px(0.0))
                    .build(),
            )
            .on_click(Action::custom("second"))
            .build(tree.tree_mut(), parent)
            .expect("second trigger")
    };
    frame(&mut app);
    let hits = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&hits);
    app.app_mut()
        .on_action(Action::custom("second"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
    let mut spec = base_spec(Anchor::NodeRect {
        node: first_trigger,
        rect: rect(0.0, 0.0, 100.0, 30.0),
    });
    spec.opener = Some(first_trigger);
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    let second_center = point(250.0, 15.0);
    let (_, down_consumed) = press(&mut app, second_center);
    assert!(!down_consumed, "不同 opener 的按下不得被消费");
    let (_, up_consumed) = release(&mut app, second_center);
    assert!(!up_consumed);
    assert!(app.app().popup_rect(id).is_none(), "旧链必须先关闭");
    assert_eq!(hits.get(), 1, "按下必须正常派发给新 opener");
    let _ = second_trigger;
}

#[test]
fn pressing_inside_an_ancestor_menu_closes_only_its_descendants() {
    let mut app = TestApp::new();
    let first = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (120.0, 160.0),
        None,
    );
    frame(&mut app);
    let first_node = popup_node(&app, first);
    let item = {
        let window = win(&app);
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(120.0), px(24.0)).build())
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                first_node,
            )
            .expect("item")
    };
    frame(&mut app);
    let second = open_child(&mut app, first, Anchor::Node { node: item }, (80.0, 60.0));
    frame(&mut app);
    let second_node = popup_node(&app, second);
    let sub_item = {
        let window = win(&app);
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(80.0), px(24.0)).build())
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                second_node,
            )
            .expect("sub item")
    };
    frame(&mut app);
    let third = open_child(
        &mut app,
        second,
        Anchor::Node { node: sub_item },
        (70.0, 40.0),
    );
    frame(&mut app);
    assert!(app.app().popup_rect(third).is_some());
    let first_rect = app.app().popup_rect(first).expect("rect").hit;
    let inside_first = point(
        (first_rect.origin.x + 5.0).value(),
        (first_rect.origin.y + 5.0).value(),
    );
    let (_, down_consumed) = press(&mut app, inside_first);
    assert!(!down_consumed, "祖先面板内的按下正常派发");
    let (_, _) = release(&mut app, inside_first);
    assert!(app.app().popup_rect(third).is_none(), "必须关闭后代");
    assert!(app.app().popup_rect(second).is_none(), "必须关闭后代");
    assert!(app.app().popup_rect(first).is_some(), "祖先保持打开");
}

#[test]
fn opening_a_modal_closes_auto_roots_and_hints() {
    let mut app = TestApp::new();
    let auto = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (120.0, 80.0),
        None,
    );
    let mut hint_spec = base_spec(Anchor::WindowRect(rect(300.0, 100.0, 20.0, 20.0)));
    hint_spec.tier = PopupTier::Hint;
    hint_spec.dismiss = DismissPolicy::Hint { escape: true };
    let hint = open_sized(&mut app, hint_spec, (80.0, 30.0), None);
    frame(&mut app);
    let mut modal_spec = base_spec(Anchor::WindowRect(rect(200.0, 200.0, 20.0, 20.0)));
    modal_spec.tier = PopupTier::Modal;
    modal_spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    let modal = open_sized(&mut app, modal_spec, (200.0, 120.0), None);
    frame(&mut app);
    assert!(
        app.app().popup_rect(auto).is_none(),
        "Modal 必须关闭无关 Auto 链"
    );
    assert!(app.app().popup_rect(hint).is_none(), "Modal 必须关闭 Hint");
    assert!(app.app().popup_rect(modal).is_some());
}

#[test]
fn modal_outside_true_closes_the_chain_and_consumes_the_press() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    spec.tier = PopupTier::Modal;
    spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    spec.dismiss = DismissPolicy::Explicit {
        outside: true,
        escape: true,
    };
    let modal = open_sized(&mut app, spec, (200.0, 120.0), None);
    frame(&mut app);
    let (_, down_consumed) = press(&mut app, point(700.0, 500.0));
    assert!(down_consumed, "Modal scope 外按下必须消费");
    let (_, up_consumed) = release(&mut app, point(700.0, 500.0));
    assert!(up_consumed);
    assert!(
        app.app().popup_rect(modal).is_none(),
        "Explicit{{outside:true}} 必须关闭整链"
    );
}

#[test]
fn a_scrim_click_behaves_like_an_outside_press() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(200.0, 200.0, 20.0, 20.0)));
    spec.tier = PopupTier::Modal;
    spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    spec.dismiss = DismissPolicy::Explicit {
        outside: true,
        escape: true,
    };
    spec.scrim = Some(ScrimSpec {
        density: 0.5,
        ..ScrimSpec::default()
    });
    let modal = open_sized(&mut app, spec, (200.0, 120.0), None);
    frame(&mut app);
    let (_, down_consumed) = press(&mut app, point(700.0, 500.0));
    assert!(down_consumed);
    let (_, _) = release(&mut app, point(700.0, 500.0));
    assert!(
        app.app().popup_rect(modal).is_none(),
        "scrim 点击等价 OutsidePress"
    );
}

#[test]
fn closing_a_modal_closes_owner_modal_descendants() {
    let mut app = TestApp::new();
    let window = win(&app);
    let mut modal_spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    modal_spec.tier = PopupTier::Modal;
    modal_spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    let modal = app
        .app_mut()
        .open_popup(
            window,
            modal_spec,
            |app, window, node| {
                NodeBuilder::boxed()
                    .style(StyleBuilder::new().size(px(200.0), px(120.0)).build())
                    .on_click(Action::custom("inside"))
                    .build(app.window_mut(window).expect("window").tree_mut(), node)?;
                Ok(())
            },
            None,
        )
        .expect("modal");
    frame(&mut app);
    let content = popup_node(&app, modal);
    let inside = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(content)
        .expect("content")
        .children()[0];
    let select = {
        let mut spec = base_spec(Anchor::NodeRect {
            node: inside,
            rect: rect(0.0, 0.0, 100.0, 30.0),
        });
        spec.opener = Some(inside);
        open_sized(&mut app, spec, (100.0, 80.0), None)
    };
    frame(&mut app);
    assert!(app.app().popup_rect(select).is_some());
    app.app_mut().close_popup(modal).expect("close modal");
    assert!(
        app.app().popup_rect(select).is_none(),
        "关闭 Modal 必须递归关闭 owner_modal 后代"
    );
}

#[test]
fn escape_skips_closing_entries_and_closes_the_next_open_level() {
    let mut app = TestApp::new();
    let mut first_spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    first_spec.animation = PopupAnimation::Fade;
    let first = open_sized(&mut app, first_spec, (120.0, 160.0), None);
    frame(&mut app);
    let first_node = popup_node(&app, first);
    let item = {
        let window = win(&app);
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(120.0), px(24.0)).build())
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                first_node,
            )
            .expect("item")
    };
    frame(&mut app);
    let mut second_spec = base_spec(Anchor::Node { node: item });
    second_spec.parent = Some(first);
    second_spec.placement = menu_placement();
    second_spec.animation = PopupAnimation::Fade;
    let second = open_sized(&mut app, second_spec, (80.0, 60.0), None);
    frame(&mut app);
    let (_, consumed) = escape(&mut app);
    assert!(consumed);
    assert_eq!(
        app.app()
            .popup_stack(win(&app))
            .expect("stack")
            .entries
            .iter()
            .find(|entry| entry.id == second)
            .expect("second")
            .state,
        PopupState::Closing
    );
    let (_, consumed) = escape(&mut app);
    assert!(consumed, "第二次 Esc 必须跳过 Closing 并关闭下一层");
    assert_eq!(
        app.app()
            .popup_stack(win(&app))
            .expect("stack")
            .entries
            .iter()
            .find(|entry| entry.id == first)
            .expect("first")
            .state,
        PopupState::Closing
    );
}

#[test]
fn default_button_wins_the_trap_initial_focus() {
    let mut app = TestApp::new();
    let window = win(&app);
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    spec.tier = PopupTier::Modal;
    spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::DefaultButton,
    };
    let first_button = app
        .app_mut()
        .open_popup(
            window,
            spec,
            |app, window, node| {
                let roles = [
                    ui_core::a11y::A11yRole::Button,
                    ui_core::a11y::A11yRole::Button,
                ];
                for (index, role) in roles.into_iter().enumerate() {
                    NodeBuilder::boxed()
                        .style(StyleBuilder::new().size(px(100.0), px(30.0)).build())
                        .a11y(ui_core::a11y::A11yOverride {
                            role: Some(role),
                            ..Default::default()
                        })
                        .on_click(Action::custom(if index == 0 { "first" } else { "second" }))
                        .build(app.window_mut(window).expect("window").tree_mut(), node)?;
                }
                Ok(())
            },
            None,
        )
        .expect("dialog");
    frame(&mut app);
    let content = popup_node(&app, first_button);
    let children = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(content)
        .expect("content")
        .children()
        .to_vec();
    assert_eq!(
        app.app().focused(window),
        Some(children[0]),
        "DefaultButton 必须取第一个可聚焦按钮"
    );
}

#[test]
fn closing_a_popup_does_not_steal_the_newer_popup_focus() {
    let mut app = TestApp::new();
    let window = win(&app);
    let trigger = root_button(&mut app, (100.0, 30.0), "trigger");
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(trigger)));
    let mut first_spec = base_spec(Anchor::NodeRect {
        node: trigger,
        rect: rect(0.0, 0.0, 100.0, 30.0),
    });
    first_spec.focus = FocusPolicy::Take {
        restore: RestoreFocus::Opener,
    };
    first_spec.opener = Some(trigger);
    first_spec.animation = PopupAnimation::Fade;
    let first = open_sized(&mut app, first_spec, (120.0, 80.0), None);
    frame(&mut app);
    assert_ne!(app.app().focused(window), Some(trigger));
    let (_, consumed) = escape(&mut app);
    assert!(consumed, "Esc 启动第一个 popup 的关闭事务");
    let mut second_spec = base_spec(Anchor::WindowRect(rect(500.0, 100.0, 20.0, 20.0)));
    second_spec.focus = FocusPolicy::Take {
        restore: RestoreFocus::Opener,
    };
    let second = open_sized(&mut app, second_spec, (100.0, 40.0), None);
    frame(&mut app);
    let second_focus = app.app().focused(window);
    assert!(second_focus.is_some());
    app.advance(Duration::from_millis(120));
    frame(&mut app);
    assert!(app.app().popup_rect(first).is_none(), "旧 popup 已移除");
    assert_eq!(
        app.app().focused(window),
        second_focus,
        "关闭事务结束不得抢走新 popup 的焦点"
    );
    let _ = second;
}

#[test]
fn removing_the_opener_falls_back_to_the_root() {
    let mut app = TestApp::new();
    let window = win(&app);
    let trigger = root_button(&mut app, (100.0, 30.0), "trigger");
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(trigger)));
    let mut spec = base_spec(Anchor::NodeRect {
        node: trigger,
        rect: rect(0.0, 0.0, 100.0, 30.0),
    });
    spec.focus = FocusPolicy::Take {
        restore: RestoreFocus::Opener,
    };
    spec.opener = Some(trigger);
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    app.app_mut().remove_node(window, trigger).expect("remove");
    app.app_mut().close_popup(id).expect("close");
    frame(&mut app);
    assert_eq!(
        app.app().focused(window),
        None,
        "opener 失效时关闭必须回落到 root（None）"
    );
}

#[test]
fn escape_during_ime_composition_cancels_it_and_keeps_the_popup() {
    let mut app = TestApp::new();
    let window = win(&app);
    let editable = ui_core::text_edit::EditableSpec {
        role: ui_core::a11y::A11yRole::TextInput,
        label: "".into(),
        placeholder: "".into(),
        multiline: false,
        masked: false,
        readonly: false,
        disabled: false,
        max_length: None,
        submit: None,
        text_style: ui_core::text::TextStyle::new(px(16.0)),
        caret_color: Color::BLACK,
        selection_color: Color::BLACK,
        placeholder_color: Color::BLACK,
        composition_color: Color::BLACK,
    };
    let node = {
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        NodeBuilder::editable_text(editable)
            .build(tree, tree.root())
            .expect("editable")
    };
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(node)));
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(600.0, 500.0, 20.0, 20.0))),
        (60.0, 30.0),
        None,
    );
    frame(&mut app);
    app.dispatch_event(Event::Ime(ImeEvent::Preedit {
        text: "ni".into(),
        caret: None,
        clauses: Default::default(),
        replace: None,
    }));
    assert!(
        app.app()
            .text_edit_composition(window, node)
            .expect("composition")
            .is_some()
    );
    let (_, consumed) = escape(&mut app);
    assert!(consumed, "composing 时 Esc 必须由文本编辑消费");
    assert!(
        app.app()
            .text_edit_composition(window, node)
            .expect("composition")
            .is_none(),
        "Esc 必须先取消组合"
    );
    assert!(app.app().popup_rect(id).is_some(), "取消组合不得关闭 popup");
}

#[test]
fn slide_animation_damage_covers_the_offset_and_the_final_rect() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0)));
    spec.animation = PopupAnimation::FadeSlide;
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    let rect = app.app().popup_rect(id).expect("rect").hit;
    app.advance(Duration::from_millis(30));
    frame(&mut app);
    let offset = app.app().popup_stack(win(&app)).expect("stack").entries[0].paint_offset;
    assert!(offset.y > px(0.0) && offset.y < px(4.0));
    assert!(
        damage_covers(&app, rect.translate(LogicalPoint::new(px(0.0), px(4.0)))),
        "damage 必须覆盖起始偏移位置"
    );
    assert!(
        damage_covers(&app, rect.translate(offset)),
        "damage 必须覆盖当前偏移位置"
    );
    let painted = app
        .frame_record()
        .frame
        .as_ref()
        .expect("rendered frame")
        .quads
        .iter()
        .any(|quad| {
            (quad.bounds.origin.y - (rect.origin.y + offset.y))
                .value()
                .abs()
                < 0.01
                && quad.bounds.size.height > px(0.0)
        });
    assert!(painted, "绘制必须跟随 paint_offset（而非只改命中）");
    assert!(!app.frame_record().damage_full, "动画帧不得整帧重绘");
}

#[test]
fn closing_animation_frames_never_force_a_full_repaint() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0)));
    spec.animation = PopupAnimation::FadeSlide;
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    app.advance(Duration::from_millis(130));
    frame(&mut app);
    let (_, consumed) = escape(&mut app);
    assert!(consumed);
    app.advance(Duration::from_millis(45));
    frame(&mut app);
    assert!(!app.frame_record().damage_full, "关闭动画帧不得整帧重绘");
    app.advance(Duration::from_millis(60));
    frame(&mut app);
    assert!(!app.frame_record().damage_full, "关闭移除帧不得整帧重绘");
    assert!(app.app().popup_rect(id).is_none());
}

#[test]
fn modal_animation_is_fade_only() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    spec.tier = PopupTier::Modal;
    spec.animation = PopupAnimation::FadeSlide;
    spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    let id = open_sized(&mut app, spec, (200.0, 120.0), None);
    frame(&mut app);
    app.advance(Duration::from_millis(30));
    frame(&mut app);
    let offset = app
        .app()
        .popup_stack(win(&app))
        .expect("stack")
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .expect("modal")
        .paint_offset;
    assert_eq!(offset, LogicalPoint::ZERO, "Modal 只允许 fade，不得 slide");
}

#[test]
fn anchor_lost_policy_false_keeps_the_popup() {
    let mut app = TestApp::new();
    let window = win(&app);
    let anchor = root_box(&mut app, (60.0, 20.0), Color::WHITE);
    frame(&mut app);
    let mut spec = base_spec(Anchor::Node { node: anchor });
    spec.dismiss = DismissPolicy::Chain {
        outside: ui_core::popup::OutsidePress::Replay,
        escape: EscapePolicy::CloseTopmost,
        anchor_lost: false,
        focus_lost: true,
    };
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    app.app_mut().remove_node(window, anchor).expect("remove");
    frame(&mut app);
    assert!(
        app.app().popup_rect(id).is_some(),
        "anchor_lost=false 必须保留 popup"
    );
}

#[test]
fn hint_is_hidden_on_window_deactivation() {
    let mut app = TestApp::new();
    let window = win(&app);
    let mut hint_spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    hint_spec.tier = PopupTier::Hint;
    hint_spec.dismiss = DismissPolicy::Hint { escape: true };
    let hint = open_sized(&mut app, hint_spec, (80.0, 30.0), None);
    frame(&mut app);
    app.app_mut().window_focus_lost(window);
    frame(&mut app);
    assert!(
        app.app().popup_rect(hint).is_none(),
        "窗口失焦必须隐藏 Hint"
    );
}

#[test]
fn popup_settle_rounds_never_exceed_two() {
    let mut app = TestApp::new();
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0))),
        (80.0, 40.0),
        None,
    );
    frame(&mut app);
    assert!(app.frame_record().stats.popup_settle_rounds <= 2);
    let node = popup_node(&app, id);
    let window = win(&app);
    NodeBuilder::boxed()
        .style(StyleBuilder::new().size(px(300.0), px(200.0)).build())
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            node,
        )
        .expect("child");
    frame(&mut app);
    assert!(app.frame_record().stats.popup_settle_rounds <= 2);
    app.app_mut()
        .window_mut(window)
        .expect("window")
        .set_size(LogicalSize::new(px(320.0), px(240.0)));
    frame(&mut app);
    assert!(app.frame_record().stats.popup_settle_rounds <= 2);
}

#[test]
fn closing_a_popup_ends_the_editable_composition() {
    let mut app = TestApp::new();
    let window = win(&app);
    let editable = ui_core::text_edit::EditableSpec {
        role: ui_core::a11y::A11yRole::TextInput,
        label: "".into(),
        placeholder: "".into(),
        multiline: false,
        masked: false,
        readonly: false,
        disabled: false,
        max_length: None,
        submit: None,
        text_style: ui_core::text::TextStyle::new(px(16.0)),
        caret_color: Color::BLACK,
        selection_color: Color::BLACK,
        placeholder_color: Color::BLACK,
        composition_color: Color::BLACK,
    };
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    spec.focus = FocusPolicy::Take {
        restore: RestoreFocus::Opener,
    };
    spec.animation = PopupAnimation::Fade;
    let id = app
        .app_mut()
        .open_popup(
            window,
            spec,
            move |app, window, node| {
                NodeBuilder::editable_text(editable)
                    .build(app.window_mut(window).expect("window").tree_mut(), node)?;
                Ok(())
            },
            None,
        )
        .expect("popup");
    frame(&mut app);
    let content = popup_node(&app, id);
    let editable = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(content)
        .expect("content")
        .children()[0];
    assert!(app.app_mut().set_focus(window, Some(editable)));
    app.dispatch_event(Event::Ime(ImeEvent::Preedit {
        text: "ni".into(),
        caret: None,
        clauses: Default::default(),
        replace: None,
    }));
    assert!(
        app.app()
            .text_edit_composition(window, editable)
            .expect("composition")
            .is_some()
    );
    let (_, _) = press(&mut app, point(600.0, 500.0));
    let (_, _) = release(&mut app, point(600.0, 500.0));
    assert!(
        app.app().popup_rect(id).is_some(),
        "关闭动画期间节点仍在（可观测组合状态）"
    );
    assert!(
        app.app()
            .text_edit_composition(window, editable)
            .expect("composition")
            .is_none(),
        "关闭 popup 必须结束其内 editable 的组合状态"
    );
    app.advance(Duration::from_millis(95));
    frame(&mut app);
    assert!(app.app().popup_rect(id).is_none());
}

#[test]
fn programmatic_begin_close_animates_the_fade_and_removes_after_it() {
    let mut app = TestApp::new();
    let window = win(&app);
    let trigger = root_button(&mut app, (100.0, 30.0), "trigger");
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(trigger)));
    let reasons = std::rc::Rc::new(std::cell::Cell::new(None));
    let recorded = std::rc::Rc::clone(&reasons);
    let mut spec = base_spec(Anchor::NodeRect {
        node: trigger,
        rect: rect(0.0, 0.0, 100.0, 30.0),
    });
    spec.animation = PopupAnimation::Fade;
    spec.focus = FocusPolicy::Take {
        restore: RestoreFocus::Previous,
    };
    spec.opener = Some(trigger);
    let id = open_sized(
        &mut app,
        spec,
        (120.0, 80.0),
        Some(Box::new(move |_app, dismiss| {
            recorded.set(Some(dismiss.reason));
            DismissOutcome::Close
        })),
    );
    frame(&mut app);
    assert_eq!(app.app().popup_state(id), Some(PopupState::Open));
    assert!(
        app.app_mut()
            .begin_close_popup(id, DismissReason::Programmatic)
            .expect("begin close")
    );
    assert_eq!(app.app().popup_state(id), Some(PopupState::Closing));
    assert!(
        app.app().popup_rect(id).is_some(),
        "Closing 事务必须保留节点/几何"
    );
    assert_eq!(reasons.get(), Some(DismissReason::Programmatic));
    assert_eq!(
        app.app().focused(window),
        Some(trigger),
        "关闭事务必须先释放焦点"
    );
    app.advance(Duration::from_millis(95));
    frame(&mut app);
    assert!(app.app().popup_rect(id).is_none());
    assert_eq!(app.app().popup_state(id), None);
}

#[test]
fn begin_close_is_idempotent_while_the_transaction_runs() {
    let mut app = TestApp::new();
    let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&calls);
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    spec.animation = PopupAnimation::Fade;
    let id = open_sized(
        &mut app,
        spec,
        (120.0, 80.0),
        Some(Box::new(move |_app, _dismiss| {
            counter.set(counter.get() + 1);
            DismissOutcome::Close
        })),
    );
    frame(&mut app);
    assert!(
        app.app_mut()
            .begin_close_popup(id, DismissReason::Programmatic)
            .expect("first")
    );
    assert!(
        !app.app_mut()
            .begin_close_popup(id, DismissReason::Programmatic)
            .expect("second")
    );
    assert_eq!(calls.get(), 1, "重复 begin_close 不得再次回调 on_dismiss");
    assert_eq!(app.app().popup_state(id), Some(PopupState::Closing));
    app.advance(Duration::from_millis(95));
    frame(&mut app);
    assert!(app.app().popup_rect(id).is_none());
}

#[test]
fn begin_close_honours_an_ignore_handler() {
    let mut app = TestApp::new();
    let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&calls);
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (120.0, 80.0),
        Some(Box::new(move |_app, _dismiss| {
            counter.set(counter.get() + 1);
            DismissOutcome::Ignore
        })),
    );
    frame(&mut app);
    assert!(
        !app.app_mut()
            .begin_close_popup(id, DismissReason::Programmatic)
            .expect("ignored")
    );
    assert_eq!(app.app().popup_state(id), Some(PopupState::Open));
    assert!(app.app().popup_rect(id).is_some());
    assert!(
        !app.app_mut()
            .begin_close_popup(id, DismissReason::Programmatic)
            .expect("ignored again")
    );
    assert_eq!(calls.get(), 2, "Ignore 必须保留 handler 供下次调用");
}

#[test]
fn begin_close_rejects_an_unknown_popup() {
    let mut app = TestApp::new();
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (120.0, 80.0),
        None,
    );
    frame(&mut app);
    app.app_mut().close_popup(id).expect("immediate close");
    assert!(
        app.app_mut()
            .begin_close_popup(id, DismissReason::Programmatic)
            .is_err(),
        "未知 popup 必须返回强类型错误"
    );
    assert_eq!(app.app().popup_state(id), None);
}

fn open_take_for_restore(
    app: &mut TestApp,
    trigger: NodeId,
    opener: NodeId,
    restore: RestoreFocus,
) -> PopupId {
    let mut spec = base_spec(Anchor::NodeRect {
        node: opener,
        rect: rect(0.0, 0.0, 100.0, 30.0),
    });
    spec.focus = FocusPolicy::Take { restore };
    spec.opener = Some(opener);
    let id = open_sized(app, spec, (120.0, 80.0), None);
    frame(app);
    let _ = trigger;
    id
}

#[test]
fn restore_previous_ignores_the_opener() {
    let mut app = TestApp::new();
    let window = win(&app);
    let first = root_button(&mut app, (100.0, 30.0), "first");
    let second = root_button(&mut app, (100.0, 30.0), "second");
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(first)));
    let id = open_take_for_restore(&mut app, first, second, RestoreFocus::Previous);
    assert_ne!(app.app().focused(window), Some(first));
    app.app_mut().close_popup(id).expect("close");
    assert_eq!(
        app.app().focused(window),
        Some(first),
        "Previous 必须恢复关闭前焦点而不是 opener"
    );
}

#[test]
fn restore_opener_wins_over_the_previous_focus() {
    let mut app = TestApp::new();
    let window = win(&app);
    let first = root_button(&mut app, (100.0, 30.0), "first");
    let second = root_button(&mut app, (100.0, 30.0), "second");
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(first)));
    let id = open_take_for_restore(&mut app, first, second, RestoreFocus::Opener);
    app.app_mut().close_popup(id).expect("close");
    assert_eq!(
        app.app().focused(window),
        Some(second),
        "Opener 必须恢复到 opener 节点"
    );
}

#[test]
fn restore_opener_falls_back_to_the_previous_when_it_disappears() {
    let mut app = TestApp::new();
    let window = win(&app);
    let first = root_button(&mut app, (100.0, 30.0), "first");
    let second = root_button(&mut app, (100.0, 30.0), "second");
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(first)));
    let id = open_take_for_restore(&mut app, first, second, RestoreFocus::Opener);
    app.app_mut()
        .remove_node(window, second)
        .expect("remove opener");
    app.app_mut().close_popup(id).expect("close");
    assert_eq!(
        app.app().focused(window),
        Some(first),
        "opener 已被移除时必须回落到关闭前焦点"
    );
}

#[test]
fn restore_opener_walks_to_the_nearest_focusable_ancestor() {
    let mut app = TestApp::new();
    let window = win(&app);
    let first = root_button(&mut app, (100.0, 30.0), "first");
    let outer = root_button(&mut app, (100.0, 30.0), "outer");
    frame(&mut app);
    let label = {
        let tree = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(20.0), px(20.0)).build())
            .build(tree.tree_mut(), outer)
            .expect("label")
    };
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(first)));
    let id = open_take_for_restore(&mut app, first, label, RestoreFocus::Opener);
    app.app_mut().close_popup(id).expect("close");
    assert_eq!(
        app.app().focused(window),
        Some(outer),
        "不可聚焦 opener 必须回落到最近可聚焦祖先"
    );
}

#[test]
fn set_popup_a11y_updates_the_live_projection() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    spec.a11y.role = Some(ui_core::a11y::A11yRole::Dialog);
    spec.a11y.label = Some("Title A".into());
    let id = open_sized(&mut app, spec, (200.0, 120.0), None);
    frame(&mut app);
    app.enable_a11y_capture();
    app.drain_pump().expect("a11y frame");
    let node = popup_node(&app, id);
    let snapshot = app.a11y().expect("a11y snapshot");
    let record = snapshot.node_for(node).expect("popup a11y record");
    assert_eq!(record.role, ui_core::a11y::A11yRole::Dialog);
    assert_eq!(record.label_text(), Some("Title A"));
    assert!(
        app.app_mut()
            .set_popup_a11y(
                id,
                PopupA11y {
                    role: Some(ui_core::a11y::A11yRole::Dialog),
                    label: Some("Title B".into()),
                    ..Default::default()
                },
            )
            .expect("update a11y")
    );
    app.advance(Duration::from_millis(150));
    app.drain_pump().expect("a11y frame");
    assert!(
        app.a11y_update().is_some(),
        "set_popup_a11y 必须标脏并向 a11y 端投递"
    );
    let record = app
        .a11y()
        .expect("a11y snapshot")
        .node_for(node)
        .expect("popup a11y record");
    assert_eq!(record.label_text(), Some("Title B"));
    assert!(
        !app.app_mut()
            .set_popup_a11y(
                id,
                PopupA11y {
                    role: Some(ui_core::a11y::A11yRole::Dialog),
                    label: Some("Title B".into()),
                    ..Default::default()
                },
            )
            .expect("idempotent update"),
        "相同元数据不得重复标脏"
    );
    app.advance(Duration::from_millis(150));
    app.drain_pump().expect("idle frame");
    assert!(app.a11y_update().is_none(), "相同元数据不得产生 a11y 投递");
}

fn abs_button(
    app: &mut TestApp,
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    action: &'static str,
) -> NodeId {
    let window = win(app);
    let tree = app.app_mut().window_mut(window).expect("window");
    let parent = tree.tree().root();
    NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .size(px(width), px(height))
                .absolute()
                .left(px(left))
                .top(px(top))
                .build(),
        )
        .on_click(Action::custom(action))
        .build(tree.tree_mut(), parent)
        .expect("button")
}

fn tooltip_spec(button: NodeId) -> PopupSpec {
    let mut spec = base_spec(Anchor::WindowRect(rect(0.0, 0.0, 10.0, 10.0)));
    spec.tier = PopupTier::Hint;
    spec.dismiss = DismissPolicy::Hint { escape: true };
    spec.pointer_transparent = true;
    spec.opener = Some(button);
    spec.a11y = PopupA11y {
        role: Some(ui_core::a11y::A11yRole::Tooltip),
        described_by: Some(button),
        ..Default::default()
    };
    spec
}

#[test]
fn pointer_transparent_popup_lets_clicks_and_hover_reach_the_content_below() {
    let mut app = TestApp::new();
    let window = win(&app);
    let button = abs_button(&mut app, 0.0, 0.0, 100.0, 40.0, "under");
    frame(&mut app);
    let hits = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&hits);
    app.app_mut()
        .on_action(Action::custom("under"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
    let id = open_sized(&mut app, tooltip_spec(button), (300.0, 200.0), None);
    frame(&mut app);
    let tip = app.app().popup_rect(id).expect("tooltip rect").hit;
    let point = point(50.0, 20.0);
    assert!(tip.contains(point), "测试点必须同时落在 tooltip 与按钮内");
    assert_eq!(
        app.probe().hit(point).map(|hit| hit.node),
        Some(button),
        "pointer_transparent 子树不得参与命中"
    );
    app.move_pointer(point);
    assert_eq!(
        app.app().hovered(window),
        Some(button),
        "hover 必须穿透到下层按钮"
    );
    let (_, _) = press(&mut app, point);
    let (_, _) = release(&mut app, point);
    assert_eq!(hits.get(), 1, "点击必须落到下层按钮");
    assert!(
        app.app().popup_rect(id).is_some(),
        "穿透点击不得关闭 Hint/Tooltip"
    );
}

#[test]
fn a_non_transparent_popup_still_intercepts_the_pointer() {
    let mut app = TestApp::new();
    let window = win(&app);
    let _button = abs_button(&mut app, 0.0, 0.0, 100.0, 40.0, "under");
    frame(&mut app);
    let hits = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&hits);
    app.app_mut()
        .on_action(Action::custom("under"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(0.0, 0.0, 10.0, 10.0))),
        (300.0, 200.0),
        None,
    );
    frame(&mut app);
    let content = popup_node(&app, id);
    let child = app
        .app()
        .window(window)
        .expect("window")
        .tree()
        .get(content)
        .expect("content")
        .children()[0];
    let point = point(50.0, 20.0);
    assert_eq!(
        app.probe().hit(point).map(|hit| hit.node),
        Some(child),
        "非透明 popup 必须拦截命中"
    );
    app.move_pointer(point);
    assert_eq!(app.app().hovered(window), Some(child));
    let (_, _) = press(&mut app, point);
    let (_, _) = release(&mut app, point);
    assert_eq!(hits.get(), 0, "非透明 popup 不得把点击漏给下层");
}

#[test]
fn only_the_topmost_transparent_popup_passes_the_pointer_through() {
    let mut app = TestApp::new();
    let window = win(&app);
    let button = abs_button(&mut app, 0.0, 0.0, 100.0, 40.0, "under");
    frame(&mut app);
    let hits = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&hits);
    app.app_mut()
        .on_action(Action::custom("under"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
    let lower = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(0.0, 0.0, 10.0, 10.0))),
        (300.0, 200.0),
        None,
    );
    frame(&mut app);
    let lower_child = {
        let content = popup_node(&app, lower);
        app.app()
            .window(window)
            .expect("window")
            .tree()
            .get(content)
            .expect("content")
            .children()[0]
    };
    let upper = open_sized(&mut app, tooltip_spec(button), (300.0, 200.0), None);
    frame(&mut app);
    let upper_child = {
        let content = popup_node(&app, upper);
        app.app()
            .window(window)
            .expect("window")
            .tree()
            .get(content)
            .expect("content")
            .children()[0]
    };
    assert_ne!(lower_child, upper_child);
    let point = point(50.0, 20.0);
    assert_eq!(
        app.probe().hit(point).map(|hit| hit.node),
        Some(lower_child),
        "透明顶层必须把命中让给下层 popup，而不是漏到更下层"
    );
    app.move_pointer(point);
    assert_eq!(app.app().hovered(window), Some(lower_child));
    let (_, _) = press(&mut app, point);
    let (_, _) = release(&mut app, point);
    assert_eq!(hits.get(), 0, "命中下层 popup 时不得触发更下层按钮");
    app.app_mut().close_popup(upper).expect("close tooltip");
    frame(&mut app);
    assert_eq!(
        app.probe().hit(point).map(|hit| hit.node),
        Some(lower_child),
        "移除透明顶层后下层 popup 仍正常命中"
    );
}

#[test]
fn pointer_transparent_tooltip_keeps_focus_and_a11y_intact() {
    let mut app = TestApp::new();
    let window = win(&app);
    let button = abs_button(&mut app, 0.0, 0.0, 100.0, 40.0, "under");
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(button)));
    let hits = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&hits);
    app.app_mut()
        .on_action(Action::custom("under"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
    app.enable_a11y_capture();
    let id = open_sized(&mut app, tooltip_spec(button), (300.0, 200.0), None);
    frame(&mut app);
    assert_eq!(
        app.app().focused(window),
        Some(button),
        "Hint/Tooltip 的 FocusPolicy::None 不得改变焦点"
    );
    let content = popup_node(&app, id);
    let snapshot = app.a11y().expect("a11y snapshot");
    let tooltip = snapshot.node_for(content).expect("tooltip record");
    assert_eq!(tooltip.role, ui_core::a11y::A11yRole::Tooltip);
    assert!(!tooltip.hidden, "可关闭的 Tooltip 不得对 a11y 隐藏");
    assert!(
        !snapshot
            .node_for(button)
            .expect("button record")
            .described_by
            .is_empty(),
        "Tooltip 的 described_by 必须保留"
    );
    let point = point(50.0, 20.0);
    let (_, _) = press(&mut app, point);
    let (_, _) = release(&mut app, point);
    assert_eq!(hits.get(), 1);
    assert_eq!(
        app.app().focused(window),
        Some(button),
        "穿透点击不得改变焦点"
    );
    assert_eq!(
        app.a11y()
            .expect("a11y snapshot")
            .node_for(content)
            .map(|node| node.role),
        Some(ui_core::a11y::A11yRole::Tooltip)
    );
}

fn scrim_id(app: &TestApp) -> PopupId {
    app.app()
        .popup_stack(win(app))
        .expect("stack")
        .entries
        .iter()
        .find(|entry| entry.tier == PopupTier::Scrim)
        .map_or_else(|| panic!("scrim missing"), |entry| entry.id)
}

fn modal_spec(anchor: Anchor, scrim: bool) -> PopupSpec {
    let mut spec = base_spec(anchor);
    spec.tier = PopupTier::Modal;
    spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    spec.scrim = scrim.then_some(ScrimSpec {
        density: 0.5,
        ..ScrimSpec::default()
    });
    spec.animation = PopupAnimation::Fade;
    spec
}

#[test]
fn a_modal_scrim_covers_the_full_viewport_and_damages_it() {
    let mut app = TestApp::new();
    let window = win(&app);
    let modal = open_sized(
        &mut app,
        modal_spec(Anchor::WindowRect(rect(100.0, 100.0, 40.0, 30.0)), true),
        (200.0, 120.0),
        None,
    );
    frame(&mut app);
    assert!(
        app.frame_record().damage_full,
        "modal 打开必须全视口 damage"
    );
    let viewport = app.app().window(window).expect("window").size();
    let scrim = scrim_id(&app);
    let rect = app.app().popup_rect(scrim).expect("scrim rect");
    assert_eq!(
        rect.hit,
        LogicalRect::from_origin_size(LogicalPoint::ZERO, viewport),
        "scrim 必须全视口（不能被 settle 的 margin 夹小）"
    );
    assert_eq!(rect.paint, rect.hit);
    assert!(
        app.app_mut()
            .begin_close_popup(modal, DismissReason::Programmatic)
            .expect("begin close")
    );
    assert_eq!(
        app.app().popup_state(scrim),
        Some(PopupState::Closing),
        "scrim 与 modal 同帧进入关闭"
    );
    app.advance(Duration::from_millis(95));
    frame(&mut app);
    assert!(app.app().popup_rect(modal).is_none());
    assert_eq!(
        app.app().popup_state(scrim),
        None,
        "modal 移除时 scrim 一并回收"
    );
    assert!(
        app.frame_record().damage_full,
        "modal 关闭移除帧必须全视口 damage"
    );
}

#[test]
fn window_deactivation_keeps_a_modal_with_scrim() {
    let mut app = TestApp::new();
    let window = win(&app);
    let modal = open_sized(
        &mut app,
        modal_spec(Anchor::WindowRect(rect(100.0, 100.0, 40.0, 30.0)), true),
        (200.0, 120.0),
        None,
    );
    frame(&mut app);
    let scrim = scrim_id(&app);
    app.app_mut().window_focus_lost(window);
    frame(&mut app);
    assert!(
        app.app().popup_rect(modal).is_some(),
        "窗口失焦必须保留 Modal（即使有 scrim）"
    );
    assert_eq!(app.app().popup_state(scrim), Some(PopupState::Open));
}

#[test]
fn window_deactivation_emits_focus_lost() {
    let mut app = TestApp::new();
    let window = win(&app);
    let seen = std::rc::Rc::new(std::cell::Cell::new(None));
    let recorded = std::rc::Rc::clone(&seen);
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (120.0, 80.0),
        Some(Box::new(move |_app, dismiss| {
            recorded.set(Some(dismiss.reason));
            DismissOutcome::Close
        })),
    );
    frame(&mut app);
    app.app_mut().window_focus_lost(window);
    frame(&mut app);
    assert_eq!(seen.get(), Some(DismissReason::FocusLost));
    assert!(app.app().popup_rect(id).is_none());
}

#[test]
fn parent_close_emits_parent_closed_for_children() {
    let mut app = TestApp::new();
    let window = win(&app);
    let root_reason = std::rc::Rc::new(std::cell::Cell::new(None));
    let child_reason = std::rc::Rc::new(std::cell::Cell::new(None));
    let root_record = std::rc::Rc::clone(&root_reason);
    let child_record = std::rc::Rc::clone(&child_reason);
    let root = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (120.0, 160.0),
        Some(Box::new(move |_app, dismiss| {
            root_record.set(Some(dismiss.reason));
            DismissOutcome::CloseChain
        })),
    );
    frame(&mut app);
    let root_node = popup_node(&app, root);
    let item = {
        let tree = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(120.0), px(24.0)).build())
            .build(tree.tree_mut(), root_node)
            .expect("item")
    };
    frame(&mut app);
    let mut child_spec = base_spec(Anchor::Node { node: item });
    child_spec.parent = Some(root);
    child_spec.placement = menu_placement();
    let child = open_sized(
        &mut app,
        child_spec,
        (80.0, 60.0),
        Some(Box::new(move |_app, dismiss| {
            child_record.set(Some(dismiss.reason));
            DismissOutcome::Close
        })),
    );
    frame(&mut app);
    let (_, _) = press(&mut app, point(700.0, 500.0));
    let (_, _) = release(&mut app, point(700.0, 500.0));
    assert_eq!(root_reason.get(), Some(DismissReason::OutsidePress));
    assert_eq!(
        child_reason.get(),
        Some(DismissReason::ParentClosed),
        "父级级联关闭必须派发 ParentClosed"
    );
    assert_eq!(app.app().popup_state(child), None);
}

#[test]
fn pressing_the_opener_while_closing_cancels_the_close() {
    let mut app = TestApp::new();
    let trigger = root_button(&mut app, (100.0, 30.0), "trigger");
    frame(&mut app);
    let hits = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&hits);
    app.app_mut()
        .on_action(Action::custom("trigger"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
    let mut spec = base_spec(Anchor::NodeRect {
        node: trigger,
        rect: rect(0.0, 0.0, 100.0, 30.0),
    });
    spec.opener = Some(trigger);
    spec.animation = PopupAnimation::Fade;
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    let (_, consumed) = escape(&mut app);
    assert!(consumed);
    assert_eq!(app.app().popup_state(id), Some(PopupState::Closing));
    let (_, down_consumed) = press(&mut app, point(20.0, 10.0));
    assert!(down_consumed, "Closing 中命中 opener 必须取消关闭并消费");
    let (_, _) = release(&mut app, point(20.0, 10.0));
    assert_eq!(
        app.app().popup_state(id),
        Some(PopupState::Opening),
        "取消关闭必须回到 Opening"
    );
    assert!(app.app().popup_rect(id).is_some());
    app.advance(Duration::from_millis(130));
    frame(&mut app);
    assert_eq!(app.app().popup_state(id), Some(PopupState::Open));
    assert_eq!(hits.get(), 0, "取消关闭不得触发 opener 的点击 action");
}

#[test]
fn closing_an_opening_popup_keeps_later_nodes_clean() {
    let mut app = TestApp::new();
    let window = win(&app);
    let first = open_sized(
        &mut app,
        {
            let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
            spec.animation = PopupAnimation::Fade;
            spec
        },
        (120.0, 80.0),
        None,
    );
    frame(&mut app);
    app.app_mut().close_popup(first).expect("immediate close");
    let second = open_sized(
        &mut app,
        {
            let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
            spec.animation = PopupAnimation::Fade;
            spec
        },
        (120.0, 80.0),
        None,
    );
    frame(&mut app);
    let rect = app.app().popup_rect(second).expect("rect").hit;
    let center = point(
        (rect.origin.x + 10.0).value(),
        (rect.origin.y + 10.0).value(),
    );
    let hit = app.probe().hit(center).map(|result| result.node);
    let content = popup_node(&app, second);
    assert!(
        hit == Some(content)
            || app
                .app()
                .window(window)
                .expect("window")
                .tree()
                .get(content)
                .expect("content")
                .children()
                .contains(&hit.expect("hit")),
        "立即关闭 Opening 中的 popup 后，新 popup 必须可命中（槽位复用不继承 blocked）"
    );
}

fn open_text_popup(app: &mut TestApp) -> PopupId {
    let window = win(app);
    app.app_mut()
        .open_popup(
            window,
            base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
            |app, window, node| {
                let tree = app.window_mut(window).expect("window").tree_mut();
                NodeBuilder::text(
                    "a tooltip style sentence that must wrap onto several lines of text",
                    ui_core::text::TextStyle::new(px(16.0)),
                )
                .style(StyleBuilder::new().width(px(120.0)).build())
                .build(tree, node)?;
                Ok(())
            },
            None,
        )
        .expect("text popup")
}

#[test]
fn text_dominated_height_is_stable_on_the_first_frame() {
    let mut app = TestApp::new();
    let id = open_text_popup(&mut app);
    frame(&mut app);
    let first = app.app().popup_rect(id).expect("rect").hit;
    assert!(
        first.size.height > px(20.0),
        "文本必须在首帧完成 layout_text 并撑出多行高度"
    );
    assert_eq!(app.frame_record().stats.popup_settle_rounds, 2);
    assert!(app.frame_record().repainted, "首帧必须真正绘制");
    assert!(
        !app.frame_record().damage.is_empty() || app.frame_record().damage_full,
        "首帧必须产生 damage"
    );
    stable_frame(&mut app);
    assert_eq!(
        app.app().popup_rect(id).expect("rect").hit,
        first,
        "文本主导高度在首帧即必须等于稳定帧"
    );
}

#[test]
fn a_virtual_list_inside_a_popup_settles_after_item_changes() {
    let mut app = TestApp::new();
    let window = win(&app);
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (140.0, 100.0),
        None,
    );
    frame(&mut app);
    let content = popup_node(&app, id);
    let mut rows_style = StyleBuilder::new()
        .relative()
        .size(px(120.0), px(80.0))
        .build();
    rows_style.clip = Clip::Bounds;
    let container = {
        let tree = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(rows_style)
            .build(tree.tree_mut(), content)
            .expect("rows")
    };
    let list = app
        .app_mut()
        .make_virtual_list(
            window,
            container,
            ui_core::interaction::VirtualListConfig {
                extent: px(20.0),
                extents: None,
                overscan: 1,
                content_width: None,
            },
        )
        .expect("virtual list");
    app.app_mut()
        .set_virtual_items(&list, 400, |_| {
            NodeBuilder::boxed().style(StyleBuilder::new().size(px(120.0), px(20.0)).build())
        })
        .expect("items");
    frame(&mut app);
    let mounted = app.app().virtual_list_state(&list).expect("state").mounted;
    assert!(mounted < 400, "虚拟列表只能挂载可见窗口");
    assert!(
        app.app().popup_diagnostics(window).is_empty(),
        "虚拟列表窗口化后 popup 不得 Unsettled"
    );
    let first = app.app().popup_rect(id).expect("rect").hit;
    app.app_mut()
        .set_virtual_items(&list, 5, |_| {
            NodeBuilder::boxed().style(StyleBuilder::new().size(px(120.0), px(20.0)).build())
        })
        .expect("items");
    frame(&mut app);
    assert_eq!(
        app.app().popup_rect(id).expect("rect").hit,
        first,
        "虚拟列表增删不得改变 popup 尺寸"
    );
}

#[test]
fn a_dpi_change_keeps_the_popup_logical_geometry_stable() {
    let mut app = TestApp::new();
    let window = win(&app);
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0))),
        (120.0, 80.0),
        None,
    );
    frame(&mut app);
    let placed = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(placed, rect(100.0, 124.0, 120.0, 80.0));
    let dpr = ui_core::math::DevicePixelRatio::new(2.0).expect("dpr");
    app.app_mut()
        .window_mut(window)
        .expect("window")
        .set_device_pixel_ratio(dpr);
    frame(&mut app);
    assert_eq!(
        app.app().popup_rect(id).expect("rect").hit,
        placed,
        "DPI 变化后逻辑几何不得改变"
    );
    let center = point(
        (placed.origin.x + 10.0).value(),
        (placed.origin.y + 10.0).value(),
    );
    assert!(app.probe().hit(center).is_some(), "DPI 变化后命中必须工作");
    let back = ui_core::math::DevicePixelRatio::new(1.0).expect("dpr");
    app.app_mut()
        .window_mut(window)
        .expect("window")
        .set_device_pixel_ratio(back);
    frame(&mut app);
    assert_eq!(
        app.app().popup_rect(id).expect("rect").hit,
        placed,
        "回到 1x 后逻辑几何仍保持"
    );
    assert!(app.probe().hit(center).is_some());
}

#[test]
fn an_oversized_popup_is_clamped_to_the_viewport_margin() {
    let mut app = TestApp::new();
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (2000.0, 2000.0),
        None,
    );
    frame(&mut app);
    let clamped = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(
        clamped.size,
        LogicalSize::new(px(792.0), px(592.0)),
        "超出 viewport 的内容必须被 max_size 钳制"
    );
    assert!(clamped.origin.x >= px(4.0) && clamped.origin.y >= px(4.0));
    assert!(
        clamped.max().x <= px(796.0) && clamped.max().y <= px(596.0),
        "钳制后必须落在 viewport margin 内"
    );
    assert_eq!(app.frame_record().stats.popup_settle_rounds, 2);
    assert!(
        !app.app()
            .popup_diagnostics(win(&app))
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                ui_core::popup::PopupDiagnostic::Unsettled { .. }
            ))
    );
}

#[test]
fn a_modal_blocks_background_hover_and_scroll() {
    let mut app = TestApp::new();
    let window = win(&app);
    let mut container_style = StyleBuilder::new()
        .absolute()
        .left(px(0.0))
        .top(px(0.0))
        .size(px(200.0), px(100.0))
        .build();
    container_style.clip = Clip::Bounds;
    let container = {
        let tree = app.app_mut().window_mut(window).expect("window");
        let root = tree.tree().root();
        NodeBuilder::boxed()
            .style(container_style)
            .build(tree.tree_mut(), root)
            .expect("container")
    };
    let handle = app
        .app_mut()
        .make_scroll(
            window,
            container,
            ui_core::interaction::ScrollConfig::default(),
        )
        .expect("scroll");
    app.app_mut()
        .set_scroll_content(&handle, LogicalSize::new(px(200.0), px(600.0)))
        .expect("content");
    frame(&mut app);
    let modal = open_sized(
        &mut app,
        modal_spec(Anchor::WindowRect(rect(300.0, 200.0, 40.0, 30.0)), false),
        (200.0, 120.0),
        None,
    );
    frame(&mut app);
    let background = point(50.0, 50.0);
    app.move_pointer(background);
    assert_eq!(
        app.app().hovered(window),
        None,
        "Modal 外不得产生背景 hover"
    );
    app.inject(InputEvent::scroll_lines(
        background,
        LogicalPoint::new(px(0.0), px(-120.0)),
        Modifiers::empty(),
    ));
    frame(&mut app);
    let offset = app.app().scroll_state(&handle).expect("state").offset;
    assert_eq!(offset.y, px(0.0), "Modal 外不得滚动背景");
    app.app_mut().close_popup(modal).expect("close modal");
    frame(&mut app);
    app.inject(InputEvent::scroll_lines(
        background,
        LogicalPoint::new(px(0.0), px(-120.0)),
        Modifiers::empty(),
    ));
    app.advance(Duration::from_millis(400));
    frame(&mut app);
    assert!(
        app.app().scroll_state(&handle).expect("state").offset.y > px(0.0),
        "关闭 modal 后背景恢复滚动"
    );
}

#[test]
fn an_owner_modal_popup_stays_interactive_and_closes_from_inside_the_dialog() {
    let mut app = TestApp::new();
    let window = win(&app);
    let modal = open_sized(
        &mut app,
        modal_spec(Anchor::WindowRect(rect(100.0, 100.0, 40.0, 30.0)), false),
        (300.0, 200.0),
        None,
    );
    frame(&mut app);
    let dialog = popup_node(&app, modal);
    let trigger = {
        let tree = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .size(px(80.0), px(32.0))
                    .absolute()
                    .left(px(10.0))
                    .top(px(10.0))
                    .build(),
            )
            .on_click(Action::custom("trigger"))
            .build(tree.tree_mut(), dialog)
            .expect("trigger")
    };
    let inside_hits = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let inside_counter = std::rc::Rc::clone(&inside_hits);
    app.app_mut()
        .on_action(Action::custom("inside"), move |_ctx| {
            inside_counter.set(inside_counter.get() + 1);
        });
    frame(&mut app);
    let mut select_spec = base_spec(Anchor::NodeRect {
        node: trigger,
        rect: rect(0.0, 0.0, 80.0, 32.0),
    });
    select_spec.opener = Some(trigger);
    select_spec.focus = FocusPolicy::Take {
        restore: RestoreFocus::Opener,
    };
    let select = app
        .app_mut()
        .open_popup(
            window,
            select_spec,
            |app, window, node| {
                NodeBuilder::boxed()
                    .style(StyleBuilder::new().size(px(100.0), px(80.0)).build())
                    .on_click(Action::custom("inside"))
                    .build(app.window_mut(window).expect("window").tree_mut(), node)?;
                Ok(())
            },
            None,
        )
        .expect("select");
    frame(&mut app);
    assert_eq!(
        app.app()
            .popup_stack(window)
            .expect("stack")
            .entries
            .iter()
            .find(|entry| entry.id == select)
            .expect("select")
            .owner_modal,
        Some(modal),
        "Dialog 内打开的 Select 必须归属该 modal scope"
    );
    let select_rect = app.app().popup_rect(select).expect("rect").hit;
    let inside_point = point(
        (select_rect.origin.x + 10.0).value(),
        (select_rect.origin.y + 10.0).value(),
    );
    let (_, down_consumed) = press(&mut app, inside_point);
    assert!(!down_consumed, "scope 内 Select 上的按下必须正常派发");
    let (_, _) = release(&mut app, inside_point);
    assert_eq!(inside_hits.get(), 1, "owner_modal popup 内容必须可交互");
    let dialog_point = point(
        (app.app()
            .popup_rect(modal)
            .expect("modal rect")
            .hit
            .origin
            .x
            + 1.0)
            .value(),
        (app.app()
            .popup_rect(modal)
            .expect("modal rect")
            .hit
            .origin
            .y
            + 1.0)
            .value(),
    );
    app.app_mut()
        .close_popup(select)
        .expect("select must be closable from inside dialog");
    let _ = dialog_point;
    assert_eq!(app.app().popup_state(select), None);
    assert!(
        app.app().popup_rect(modal).is_some(),
        "关闭 Select 不得关闭 Dialog"
    );
}

#[test]
fn popup_stack_rects_are_finite_and_respect_z_and_scope_invariants() {
    let mut app = TestApp::new();
    let window = win(&app);
    let modal = open_sized(
        &mut app,
        modal_spec(Anchor::WindowRect(rect(100.0, 100.0, 40.0, 30.0)), true),
        (300.0, 200.0),
        None,
    );
    frame(&mut app);
    let dialog = popup_node(&app, modal);
    let trigger = {
        let tree = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .size(px(80.0), px(32.0))
                    .absolute()
                    .left(px(10.0))
                    .top(px(10.0))
                    .build(),
            )
            .build(tree.tree_mut(), dialog)
            .expect("trigger")
    };
    frame(&mut app);
    let mut select_spec = base_spec(Anchor::NodeRect {
        node: trigger,
        rect: rect(0.0, 0.0, 80.0, 32.0),
    });
    select_spec.opener = Some(trigger);
    let select = open_sized(&mut app, select_spec, (100.0, 60.0), None);
    frame(&mut app);
    let stack = app.app().popup_stack(window).expect("stack");
    let viewport = app.app().window(window).expect("window").size();
    for entry in &stack.entries {
        assert!(entry.hit_rect.is_finite(), "popup rect 必须有限");
        assert!(entry.paint_rect.is_finite(), "paint rect 必须有限");
        assert!(
            entry.hit_rect.origin.x >= LogicalPoint::ZERO.x
                && entry.hit_rect.origin.y >= LogicalPoint::ZERO.y,
            "popup 不得越出左上边界"
        );
        assert!(
            entry.hit_rect.max().x <= viewport.width && entry.hit_rect.max().y <= viewport.height,
            "popup 不得越出 viewport"
        );
    }
    let modal_z = stack
        .entries
        .iter()
        .find(|entry| entry.id == modal)
        .expect("modal")
        .z;
    let select_z = stack
        .entries
        .iter()
        .find(|entry| entry.id == select)
        .expect("select")
        .z;
    assert!(select_z > modal_z, "owner_modal 后代必须高于 modal");
}

#[test]
fn a_modal_blocks_new_hints_until_it_closes() {
    let mut app = TestApp::new();
    let window = win(&app);
    let modal = open_sized(
        &mut app,
        modal_spec(Anchor::WindowRect(rect(100.0, 100.0, 40.0, 30.0)), false),
        (200.0, 120.0),
        None,
    );
    frame(&mut app);
    let mut hint_spec = base_spec(Anchor::WindowRect(rect(400.0, 100.0, 20.0, 20.0)));
    hint_spec.tier = PopupTier::Hint;
    hint_spec.dismiss = DismissPolicy::Hint { escape: true };
    let blocked = app.app_mut().open_popup(
        window,
        hint_spec.clone(),
        |_app, _window, _node| Ok(()),
        None,
    );
    assert!(
        matches!(
            blocked,
            Err(ui_core::popup::PopupError::HintBlockedByModal { .. })
        ),
        "Modal 打开期间不得打开 Hint（含 z 叠高）"
    );
    app.app_mut().close_popup(modal).expect("close modal");
    frame(&mut app);
    let hint = app
        .app_mut()
        .open_popup(window, hint_spec, |_app, _window, _node| Ok(()), None)
        .expect("hint after modal closes");
    frame(&mut app);
    assert!(app.app().popup_rect(hint).is_some());
}

#[test]
fn a_submenu_ignores_vertical_fallbacks_and_opens_horizontally() {
    let mut app = TestApp::new();
    let window = win(&app);
    let parent = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (120.0, 120.0),
        None,
    );
    frame(&mut app);
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    spec.parent = Some(parent);
    spec.placement = PlacementSpec::default();
    let child = app
        .app_mut()
        .open_popup(window, spec, |_app, _window, _node| Ok(()), None)
        .expect("submenu coerces vertical primary to horizontal");
    frame(&mut app);
    let rect = app.app().popup_rect(child).expect("rect").hit;
    assert_eq!(
        rect.origin,
        LogicalPoint::new(px(224.0), px(100.0)),
        "子菜单必须水平开在 parent 面板右侧（垂直候选被忽略）"
    );
}

#[test]
fn submenu_placement_flips_right_to_left_with_exact_geometry() {
    let spec = PlacementSpec::submenu();
    let result = ui_core::popup::placement::solve_placement(ui_core::popup::PlacementInput {
        anchor: rect(760.0, 100.0, 20.0, 100.0),
        size: LogicalSize::new(px(120.0), px(60.0)),
        viewport: LogicalSize::new(px(800.0), px(600.0)),
        avoid: None,
        spec,
    })
    .expect("placement");
    assert_eq!(result.side, Side::Left, "右侧溢出必须翻到左侧");
    assert_eq!(
        result.rect.origin,
        LogicalPoint::new(px(760.0 - 4.0 - 120.0), px(100.0)),
        "翻到左侧后的几何必须是确定值（而不是被 clamp 在右侧）"
    );
    assert!(result.rect.max().x <= px(800.0));
}

#[test]
fn a_user_supplied_scrim_tier_is_rejected() {
    let mut app = TestApp::new();
    let window = win(&app);
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    spec.tier = PopupTier::Scrim;
    let result = app
        .app_mut()
        .open_popup(window, spec, |_app, _window, _node| Ok(()), None);
    assert!(
        matches!(result, Err(ui_core::popup::PopupError::InvalidScrim { .. })),
        "Scrim 只能由引擎合成，公开 spec 不得伪造 Scrim tier"
    );
    assert!(
        app.app()
            .popup_stack(window)
            .expect("stack")
            .entries
            .is_empty()
    );
}

#[test]
fn an_orphaned_scrim_is_reaped_when_the_modal_node_disappears() {
    let mut app = TestApp::new();
    let window = win(&app);
    let modal = open_sized(
        &mut app,
        modal_spec(Anchor::WindowRect(rect(100.0, 100.0, 40.0, 30.0)), true),
        (200.0, 120.0),
        None,
    );
    frame(&mut app);
    let scrim = scrim_id(&app);
    let modal_node = popup_node(&app, modal);
    app.app_mut()
        .remove_node(window, modal_node)
        .expect("external remove modal content");
    frame(&mut app);
    assert_eq!(
        app.app().popup_state(scrim),
        None,
        "modal 内容节点被外部删除后 scrim 必须被回收，不能留下全屏孤儿层"
    );
    let background = point(400.0, 300.0);
    assert_ne!(
        app.probe().hit(background).map(|hit| hit.node),
        Some(
            app.app()
                .popup_stack(window)
                .expect("stack")
                .entries
                .iter()
                .find(|entry| entry.tier == PopupTier::Scrim)
                .map(|entry| entry.node)
                .unwrap_or_else(|| app.app().popup_stack(window).expect("stack").root)
        ),
        "背景命中不得落在孤儿 scrim 上"
    );
}

#[test]
fn an_anchor_lost_chain_closes_the_whole_chain() {
    let mut app = TestApp::new();
    let window = win(&app);
    let anchor = root_box(&mut app, (60.0, 20.0), Color::WHITE);
    frame(&mut app);
    let mut root_spec = base_spec(Anchor::Node { node: anchor });
    root_spec.dismiss = DismissPolicy::Chain {
        outside: ui_core::popup::OutsidePress::Replay,
        escape: EscapePolicy::CloseTopmost,
        anchor_lost: true,
        focus_lost: true,
    };
    let root = open_sized(&mut app, root_spec, (120.0, 120.0), None);
    frame(&mut app);
    let root_node = popup_node(&app, root);
    let item = {
        let tree = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(120.0), px(24.0)).build())
            .build(tree.tree_mut(), root_node)
            .expect("item")
    };
    frame(&mut app);
    let mut child_spec = base_spec(Anchor::Node { node: item });
    child_spec.parent = Some(root);
    child_spec.placement = menu_placement();
    let child = open_sized(&mut app, child_spec, (80.0, 60.0), None);
    frame(&mut app);
    app.app_mut()
        .remove_node(window, anchor)
        .expect("remove anchor");
    frame(&mut app);
    assert_eq!(
        app.app().popup_state(root),
        None,
        "anchor_lost 必须关闭整条链"
    );
    assert_eq!(app.app().popup_state(child), None);
}

#[test]
fn a_popup_opened_inside_an_owner_modal_popup_keeps_the_modal_scope() {
    let mut app = TestApp::new();
    let window = win(&app);
    let modal = open_sized(
        &mut app,
        modal_spec(Anchor::WindowRect(rect(100.0, 100.0, 40.0, 30.0)), false),
        (300.0, 200.0),
        None,
    );
    frame(&mut app);
    let dialog = popup_node(&app, modal);
    let trigger = {
        let tree = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .size(px(80.0), px(32.0))
                    .absolute()
                    .left(px(10.0))
                    .top(px(10.0))
                    .build(),
            )
            .build(tree.tree_mut(), dialog)
            .expect("trigger")
    };
    frame(&mut app);
    let mut select_spec = base_spec(Anchor::NodeRect {
        node: trigger,
        rect: rect(0.0, 0.0, 80.0, 32.0),
    });
    select_spec.opener = Some(trigger);
    let select = open_sized(&mut app, select_spec, (100.0, 60.0), None);
    frame(&mut app);
    let select_node = popup_node(&app, select);
    let inside = {
        let tree = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(60.0), px(24.0)).build())
            .build(tree.tree_mut(), select_node)
            .expect("inside")
    };
    frame(&mut app);
    let mut nested_spec = base_spec(Anchor::NodeRect {
        node: inside,
        rect: rect(0.0, 0.0, 60.0, 24.0),
    });
    nested_spec.tier = PopupTier::Modal;
    nested_spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    nested_spec.opener = Some(inside);
    let nested = open_sized(&mut app, nested_spec, (80.0, 40.0), None);
    frame(&mut app);
    let owner = app
        .app()
        .popup_stack(window)
        .expect("stack")
        .entries
        .iter()
        .find(|entry| entry.id == nested)
        .expect("nested")
        .owner_modal;
    assert_eq!(
        owner,
        Some(modal),
        "经由 owner_modal popup 内 opener 打开的新 popup 必须继承 modal scope"
    );
}

fn dialog_with_button(app: &mut TestApp) -> (PopupId, NodeId) {
    let window = win(app);
    let modal = open_sized(
        app,
        modal_spec(Anchor::WindowRect(rect(100.0, 100.0, 40.0, 30.0)), false),
        (300.0, 200.0),
        None,
    );
    frame(app);
    let dialog = popup_node(app, modal);
    let button = {
        let tree = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .size(px(80.0), px(32.0))
                    .absolute()
                    .left(px(10.0))
                    .top(px(10.0))
                    .build(),
            )
            .on_click(Action::custom("dialog-button"))
            .build(tree.tree_mut(), dialog)
            .expect("button")
    };
    frame(app);
    (modal, button)
}

#[test]
fn a_tooltip_opened_from_dialog_content_is_allowed_above_the_modal() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = win(&app);
    let (modal, button) = dialog_with_button(&mut app);
    let mut hint_spec = base_spec(Anchor::NodeRect {
        node: button,
        rect: rect(0.0, 0.0, 80.0, 32.0),
    });
    hint_spec.tier = PopupTier::Hint;
    hint_spec.dismiss = DismissPolicy::Hint { escape: true };
    hint_spec.opener = Some(button);
    hint_spec.a11y = PopupA11y {
        role: Some(ui_core::a11y::A11yRole::Tooltip),
        described_by: Some(button),
        ..Default::default()
    };
    let hint = app
        .app_mut()
        .open_popup(window, hint_spec, |_app, _window, _node| Ok(()), None)
        .expect("in-scope tooltip");
    frame(&mut app);
    let modal_z = app
        .app()
        .popup_stack(window)
        .expect("stack")
        .entries
        .iter()
        .find(|entry| entry.id == modal)
        .expect("modal")
        .z;
    let snapshot = app.app().popup_stack(window).expect("stack");
    let hint_entry = snapshot
        .entries
        .iter()
        .find(|entry| entry.id == hint)
        .expect("hint");
    assert!(
        hint_entry.z > modal_z,
        "Dialog 内 tooltip 必须显示在 modal 之上"
    );
    assert_eq!(hint_entry.owner_modal, Some(modal));
    let hint_node = hint_entry.node;
    let record = app
        .a11y()
        .expect("a11y snapshot")
        .node_for(hint_node)
        .expect("tooltip record");
    assert_eq!(record.role, ui_core::a11y::A11yRole::Tooltip);
    assert!(!record.hidden);
    assert!(
        !app.a11y()
            .expect("a11y snapshot")
            .node_for(button)
            .expect("button record")
            .described_by
            .is_empty(),
        "Dialog 内 tooltip 的 described_by 必须保留"
    );
}

#[test]
fn a_background_tooltip_is_rejected_while_a_modal_is_open() {
    let mut app = TestApp::new();
    let window = win(&app);
    let background = root_button(&mut app, (100.0, 30.0), "background");
    frame(&mut app);
    let modal = open_sized(
        &mut app,
        modal_spec(Anchor::WindowRect(rect(500.0, 400.0, 40.0, 30.0)), false),
        (200.0, 120.0),
        None,
    );
    frame(&mut app);
    let mut hint_spec = base_spec(Anchor::NodeRect {
        node: background,
        rect: rect(0.0, 0.0, 100.0, 30.0),
    });
    hint_spec.tier = PopupTier::Hint;
    hint_spec.dismiss = DismissPolicy::Hint { escape: true };
    hint_spec.opener = Some(background);
    let blocked = app.app_mut().open_popup(
        window,
        hint_spec.clone(),
        |_app, _window, _node| Ok(()),
        None,
    );
    assert!(
        matches!(
            blocked,
            Err(ui_core::popup::PopupError::HintBlockedByModal { .. })
        ),
        "背景 tooltip 在 modal 打开期间必须被拒绝"
    );
    app.app_mut().close_popup(modal).expect("close modal");
    frame(&mut app);
    let hint = app
        .app_mut()
        .open_popup(window, hint_spec, |_app, _window, _node| Ok(()), None)
        .expect("background tooltip after modal closes");
    frame(&mut app);
    assert!(app.app().popup_rect(hint).is_some());
}

#[test]
fn opening_a_modal_closes_background_hints() {
    let mut app = TestApp::new();
    let window = win(&app);
    let background = root_button(&mut app, (100.0, 30.0), "background");
    frame(&mut app);
    let mut hint_spec = base_spec(Anchor::NodeRect {
        node: background,
        rect: rect(0.0, 0.0, 100.0, 30.0),
    });
    hint_spec.tier = PopupTier::Hint;
    hint_spec.dismiss = DismissPolicy::Hint { escape: true };
    hint_spec.opener = Some(background);
    let hint = app
        .app_mut()
        .open_popup(window, hint_spec, |_app, _window, _node| Ok(()), None)
        .expect("hint");
    frame(&mut app);
    assert!(app.app().popup_rect(hint).is_some());
    let _ = open_sized(
        &mut app,
        modal_spec(Anchor::WindowRect(rect(400.0, 300.0, 40.0, 30.0)), false),
        (200.0, 120.0),
        None,
    );
    frame(&mut app);
    assert_eq!(
        app.app().popup_state(hint),
        None,
        "modal 打开必须关闭背景 Hint"
    );
}

#[test]
fn dismiss_topmost_runs_the_closing_transaction() {
    let mut app = TestApp::new();
    let window = win(&app);
    let reasons = std::rc::Rc::new(std::cell::Cell::new(None));
    let recorded = std::rc::Rc::clone(&reasons);
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    spec.animation = PopupAnimation::Fade;
    let id = open_sized(
        &mut app,
        spec,
        (120.0, 80.0),
        Some(Box::new(move |_app, dismiss| {
            recorded.set(Some(dismiss.reason));
            DismissOutcome::Close
        })),
    );
    frame(&mut app);
    assert!(
        app.app_mut()
            .dismiss_topmost(window, DismissReason::Programmatic)
    );
    assert_eq!(
        app.app().popup_state(id),
        Some(PopupState::Closing),
        "程序化 dismiss 必须走 Closing 事务而不是立即删节点"
    );
    assert!(app.app().popup_rect(id).is_some());
    assert_eq!(reasons.get(), Some(DismissReason::Programmatic));
    app.advance(Duration::from_millis(95));
    frame(&mut app);
    assert!(app.app().popup_rect(id).is_none());
}

#[test]
fn a_constrained_popup_exposes_its_applied_size_without_unsettled_churn() {
    let mut app = TestApp::new();
    let window = win(&app);
    app.app_mut()
        .window_mut(window)
        .expect("window")
        .set_size(LogicalSize::new(px(300.0), px(200.0)));
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(10.0, 10.0, 20.0, 20.0))),
        (2000.0, 2000.0),
        None,
    );
    frame(&mut app);
    let snapshot = app.app().popup_stack(window).expect("stack");
    let entry = snapshot
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .expect("entry");
    assert_eq!(
        entry.constrained_size,
        Some(LogicalSize::new(px(292.0), px(192.0))),
        "resized 结果必须通过 constrained_size 暴露给 kit"
    );
    let diagnostics = app.app().popup_diagnostics(window);
    assert!(
        diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            ui_core::popup::PopupDiagnostic::Resized { .. }
        )),
        "placement resize 必须记录 Resized 诊断"
    );
    assert!(
        !diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            ui_core::popup::PopupDiagnostic::Unsettled { .. }
        )),
        "已应用的 resize 不得误报 Unsettled"
    );
    assert_eq!(app.app().popup_state(id), Some(PopupState::Open));
    frame(&mut app);
    assert_eq!(
        app.frame_record().stats.popup_settle_rounds,
        0,
        "resize 收敛后不得每帧重 settle"
    );
    assert!(
        app.app()
            .popup_diagnostics(window)
            .iter()
            .all(|diagnostic| !matches!(
                diagnostic,
                ui_core::popup::PopupDiagnostic::Unsettled { .. }
            )),
        "稳定后不得出现 Unsettled 诊断"
    );
}

#[test]
fn raise_damages_descendant_painted_extent() {
    let mut app = TestApp::new();
    let window = win(&app);
    let lower = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 40.0, 20.0))),
        (160.0, 60.0),
        None,
    );
    frame(&mut app);
    let lower_node = popup_node(&app, lower);
    let shadowed = {
        let tree = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(100.0), px(40.0)).build())
            .paint(PaintSpec {
                background: Color::WHITE,
                shadows: ui_core::paint::style::Shadows::new([ui_core::paint::style::ShadowSpec {
                    offset: LogicalPoint::new(px(0.0), px(40.0)),
                    blur: px(20.0),
                    spread: px(10.0),
                    color: Color::BLACK,
                }]),
                ..PaintSpec::default()
            })
            .build(tree.tree_mut(), lower_node)
            .expect("shadowed")
    };
    let _ = shadowed;
    let mut upper_spec = base_spec(Anchor::WindowRect(rect(400.0, 100.0, 40.0, 20.0)));
    upper_spec.tier = PopupTier::Hint;
    upper_spec.dismiss = DismissPolicy::Hint { escape: true };
    let upper = open_sized(&mut app, upper_spec, (160.0, 60.0), None);
    frame(&mut app);
    let _ = upper;
    app.app_mut().raise_popup(lower).expect("raise");
    frame(&mut app);
    let lower_rect = app.app().popup_rect(lower).expect("rect").hit;
    let expanded = LogicalRect::new(
        lower_rect.origin.x,
        lower_rect.origin.y,
        lower_rect.size.width,
        lower_rect.size.height + px(60.0),
    );
    assert!(
        damage_covers(&app, expanded),
        "reorder damage 必须包含后代 shadow 的 painted extent"
    );
}

#[test]
fn a_child_popup_anchor_follows_the_parent_paint_offset() {
    let mut app = TestApp::new();
    let window = win(&app);
    let mut parent_spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    parent_spec.animation = PopupAnimation::FadeSlide;
    let parent = open_sized(&mut app, parent_spec, (200.0, 160.0), None);
    frame(&mut app);
    let parent_node = popup_node(&app, parent);
    let item = {
        let tree = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .size(px(40.0), px(40.0))
                    .absolute()
                    .left(px(10.0))
                    .top(px(10.0))
                    .build(),
            )
            .build(tree.tree_mut(), parent_node)
            .expect("item")
    };
    frame(&mut app);
    app.advance(Duration::from_millis(30));
    frame(&mut app);
    let offset = app
        .app()
        .popup_stack(window)
        .expect("stack")
        .entries
        .iter()
        .find(|entry| entry.id == parent)
        .expect("parent")
        .paint_offset;
    assert!(offset.y > px(0.0));
    let parent_rect = app.app().popup_rect(parent).expect("rect").hit;
    let mut child_spec = base_spec(Anchor::NodeRect {
        node: item,
        rect: rect(0.0, 0.0, 40.0, 40.0),
    });
    child_spec.parent = Some(parent);
    child_spec.placement = menu_placement();
    let child = open_sized(&mut app, child_spec, (80.0, 60.0), None);
    frame(&mut app);
    let child_rect = app.app().popup_rect(child).expect("rect").hit;
    assert_eq!(
        child_rect.origin,
        LogicalPoint::new(
            px(parent_rect.max().x.value() + 4.0),
            px(parent_rect.origin.y.value() + 10.0 + offset.y.value()),
        ),
        "子 popup 锚点必须跟随父 paint_offset 的绘制位置"
    );
}

#[test]
fn external_max_size_writes_are_reapplied_by_the_next_settle() {
    let mut app = TestApp::new();
    let window = win(&app);
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (2000.0, 2000.0),
        None,
    );
    frame(&mut app);
    let content = popup_node(&app, id);
    app.app_mut()
        .window_mut(window)
        .expect("window")
        .set_max_size(content, ui_core::node::tree::MaxSize::default())
        .expect("external max size");
    frame(&mut app);
    let rect = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(
        rect.size,
        LogicalSize::new(px(792.0), px(592.0)),
        "外部写入 max_size 后 settle 必须重新施加 popup 约束"
    );
}

#[test]
fn an_anchor_lost_dismissal_records_a_diagnostic() {
    let mut app = TestApp::new();
    let window = win(&app);
    let anchor = root_box(&mut app, (60.0, 20.0), Color::WHITE);
    frame(&mut app);
    let mut spec = base_spec(Anchor::Node { node: anchor });
    spec.dismiss = DismissPolicy::Chain {
        outside: ui_core::popup::OutsidePress::Replay,
        escape: EscapePolicy::CloseTopmost,
        anchor_lost: true,
        focus_lost: true,
    };
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    app.app_mut().remove_node(window, anchor).expect("remove");
    frame(&mut app);
    assert_eq!(app.app().popup_state(id), None);
    assert!(
        app.app()
            .popup_diagnostics(window)
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                ui_core::popup::PopupDiagnostic::AnchorLost { .. }
            )),
        "AnchorLost 必须进入诊断面"
    );
}

fn node_placement(side: Side, align: Alignment) -> PlacementSpec {
    PlacementSpec {
        side,
        align,
        fallbacks: Default::default(),
        ..PlacementSpec::default()
    }
}

fn node_anchor(node: NodeId) -> Anchor {
    Anchor::Node { node }
}

#[test]
fn a_node_anchor_uses_the_full_border_box_for_all_four_sides() {
    let mut app = TestApp::new();
    let node = abs_button(&mut app, 100.0, 100.0, 80.0, 40.0, "anchor");
    frame(&mut app);
    for (side, align, expected) in [
        (Side::Bottom, Alignment::Start, (100.0, 144.0)),
        (Side::Bottom, Alignment::Center, (110.0, 144.0)),
        (Side::Bottom, Alignment::End, (120.0, 144.0)),
        (Side::Top, Alignment::Start, (100.0, 66.0)),
        (Side::Top, Alignment::Center, (110.0, 66.0)),
        (Side::Top, Alignment::End, (120.0, 66.0)),
        (Side::Right, Alignment::Start, (184.0, 100.0)),
        (Side::Right, Alignment::Center, (184.0, 105.0)),
        (Side::Right, Alignment::End, (184.0, 110.0)),
        (Side::Left, Alignment::Start, (36.0, 100.0)),
        (Side::Left, Alignment::Center, (36.0, 105.0)),
        (Side::Left, Alignment::End, (36.0, 110.0)),
    ] {
        let mut spec = base_spec(node_anchor(node));
        spec.placement = node_placement(side, align);
        let id = open_sized(&mut app, spec, (60.0, 30.0), None);
        frame(&mut app);
        let rect = app.app().popup_rect(id).expect("rect").hit;
        assert_eq!(
            (rect.origin.x.value(), rect.origin.y.value()),
            expected,
            "side={side:?} align={align:?} 必须以完整节点矩形定位"
        );
        app.app_mut().close_popup(id).expect("close");
        frame(&mut app);
    }
}

#[test]
fn a_node_anchor_flip_never_overlaps_the_anchor_border_box() {
    let mut app = TestApp::new();
    let bottom = abs_button(&mut app, 100.0, 520.0, 80.0, 40.0, "bottom");
    let top = abs_button(&mut app, 300.0, 10.0, 80.0, 40.0, "top");
    let right = abs_button(&mut app, 700.0, 200.0, 80.0, 40.0, "right");
    let left = abs_button(&mut app, 10.0, 300.0, 80.0, 40.0, "left");
    frame(&mut app);
    for (node, primary, fallback, size, expected_origin) in [
        (
            bottom,
            Side::Bottom,
            Side::Top,
            (60.0, 100.0),
            (100.0, 416.0),
        ),
        (top, Side::Top, Side::Bottom, (60.0, 100.0), (300.0, 54.0)),
        (
            right,
            Side::Right,
            Side::Left,
            (100.0, 30.0),
            (596.0, 200.0),
        ),
        (left, Side::Left, Side::Right, (100.0, 30.0), (94.0, 300.0)),
    ] {
        let mut spec = base_spec(node_anchor(node));
        spec.placement = PlacementSpec {
            side: primary,
            align: Alignment::Start,
            fallbacks: [Placement::new(fallback, Alignment::Start)]
                .into_iter()
                .collect(),
            ..PlacementSpec::default()
        };
        let id = open_sized(&mut app, spec, size, None);
        frame(&mut app);
        let rect = app.app().popup_rect(id).expect("rect").hit;
        let node_rect = app.probe().bounds(node).expect("node bounds");
        assert_eq!(
            app.app()
                .popup_stack(win(&app))
                .expect("stack")
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .expect("entry")
                .state,
            PopupState::Open
        );
        assert_eq!(
            (rect.origin.x.value(), rect.origin.y.value()),
            expected_origin,
            "flip 到对侧必须使用完整矩形对侧边"
        );
        assert!(
            !rect.intersects(node_rect),
            "flip 后面板不得覆盖锚点节点：panel={rect:?} node={node_rect:?}"
        );
        app.app_mut().close_popup(id).expect("close");
        frame(&mut app);
    }
}

#[test]
fn a_node_anchor_respects_gap_and_viewport_margin_clamp() {
    let mut app = TestApp::new();
    let node = abs_button(&mut app, 4.0, 300.0, 20.0, 20.0, "edge");
    frame(&mut app);
    let mut spec = base_spec(node_anchor(node));
    spec.placement = PlacementSpec {
        gap: px(10.0),
        fallbacks: Default::default(),
        ..node_placement(Side::Bottom, Alignment::Center)
    };
    let id = open_sized(&mut app, spec, (300.0, 30.0), None);
    frame(&mut app);
    let rect = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(rect.origin, LogicalPoint::new(px(4.0), px(330.0)));
    assert!(rect.max().x <= px(796.0));
    app.app_mut().close_popup(id).expect("close");
    frame(&mut app);
    let mut gap_spec = base_spec(node_anchor(node));
    gap_spec.placement = node_placement(Side::Bottom, Alignment::Start);
    gap_spec.placement.gap = px(12.0);
    let id = open_sized(&mut app, gap_spec, (40.0, 20.0), None);
    frame(&mut app);
    let rect = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(rect.origin.y, px(332.0), "gap 必须从节点矩形 bottom 起算");
}

#[test]
fn a_node_anchor_matches_the_equivalent_window_rect() {
    let mut app = TestApp::new();
    let node = abs_button(&mut app, 200.0, 150.0, 90.0, 44.0, "compare");
    frame(&mut app);
    let node_rect = app.probe().bounds(node).expect("bounds");
    let mut node_spec = base_spec(node_anchor(node));
    node_spec.tier = PopupTier::Hint;
    node_spec.dismiss = DismissPolicy::Hint { escape: true };
    node_spec.placement = node_placement(Side::Bottom, Alignment::End);
    let node_popup = open_sized(&mut app, node_spec, (100.0, 50.0), None);
    frame(&mut app);
    let node_geometry = app.app().popup_rect(node_popup).expect("node").hit;
    let mut rect_spec = base_spec(Anchor::WindowRect(node_rect));
    rect_spec.tier = PopupTier::Hint;
    rect_spec.dismiss = DismissPolicy::Hint { escape: true };
    rect_spec.placement = node_placement(Side::Bottom, Alignment::End);
    let rect_popup = open_sized(&mut app, rect_spec, (100.0, 50.0), None);
    frame(&mut app);
    let rect_geometry = app.app().popup_rect(rect_popup).expect("rect").hit;
    assert_eq!(
        node_geometry, rect_geometry,
        "Anchor::Node 必须与等价 WindowRect 产生同一几何"
    );
}

#[test]
fn a_node_anchor_inside_a_nested_popup_follows_parent_and_paint_offset() {
    let mut app = TestApp::new();
    let window = win(&app);
    let mut parent_spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    parent_spec.animation = PopupAnimation::FadeSlide;
    let parent = open_sized(&mut app, parent_spec, (200.0, 160.0), None);
    frame(&mut app);
    let parent_node = popup_node(&app, parent);
    let item = {
        let tree = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .size(px(40.0), px(40.0))
                    .absolute()
                    .left(px(10.0))
                    .top(px(10.0))
                    .build(),
            )
            .build(tree.tree_mut(), parent_node)
            .expect("item")
    };
    frame(&mut app);
    app.advance(Duration::from_millis(30));
    frame(&mut app);
    let offset = app
        .app()
        .popup_stack(window)
        .expect("stack")
        .entries
        .iter()
        .find(|entry| entry.id == parent)
        .expect("parent")
        .paint_offset;
    let parent_rect = app.app().popup_rect(parent).expect("rect").hit;
    let mut child_spec = base_spec(node_anchor(item));
    child_spec.parent = Some(parent);
    child_spec.placement = node_placement(Side::Bottom, Alignment::Start);
    let child = open_sized(&mut app, child_spec, (80.0, 60.0), None);
    frame(&mut app);
    let child_rect = app.app().popup_rect(child).expect("rect").hit;
    assert_eq!(
        child_rect.origin,
        LogicalPoint::new(
            px(parent_rect.max().x.value() + 4.0),
            px(parent_rect.origin.y.value() + 10.0 + offset.y.value()),
        ),
        "嵌套节点锚点（父级子菜单横向放置）必须跟随父内容位置与 paint_offset"
    );
    assert!(
        !child_rect.intersects(LogicalRect::from_origin_size(
            LogicalPoint::new(
                parent_rect.origin.x + px(10.0),
                parent_rect.origin.y + px(10.0) + offset.y
            ),
            LogicalSize::new(px(40.0), px(40.0))
        )),
        "子面板不得回叠到锚点项"
    );
}

#[test]
fn a_node_anchor_follows_scrolling_ancestors() {
    let mut app = TestApp::new();
    let window = win(&app);
    let container = {
        let tree = app.app_mut().window_mut(window).expect("window");
        let root = tree.tree().root();
        let mut style = StyleBuilder::new()
            .absolute()
            .left(px(0.0))
            .top(px(0.0))
            .size(px(200.0), px(100.0))
            .build();
        style.clip = Clip::Bounds;
        NodeBuilder::boxed()
            .style(style)
            .build(tree.tree_mut(), root)
            .expect("container")
    };
    let content = {
        let tree = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(200.0), px(600.0)).build())
            .build(tree.tree_mut(), container)
            .expect("content")
    };
    let node = {
        let tree = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .size(px(80.0), px(30.0))
                    .absolute()
                    .left(px(10.0))
                    .top(px(50.0))
                    .build(),
            )
            .build(tree.tree_mut(), content)
            .expect("node")
    };
    frame(&mut app);
    let handle = app
        .app_mut()
        .make_scroll(
            window,
            container,
            ui_core::interaction::ScrollConfig {
                axes: ui_core::interaction::ScrollAxes::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("scroll handle");
    app.app_mut()
        .set_scroll_content(&handle, LogicalSize::new(px(200.0), px(600.0)))
        .unwrap_or_else(|error| panic!("scroll content: {error:?}"));
    frame(&mut app);
    let mut spec = base_spec(node_anchor(node));
    spec.placement = node_placement(Side::Bottom, Alignment::Start);
    let id = open_sized(&mut app, spec, (60.0, 30.0), None);
    frame(&mut app);
    let before = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(before.origin, LogicalPoint::new(px(10.0), px(84.0)));
    app.app_mut()
        .scroll_to(&handle, LogicalPoint::new(px(0.0), px(40.0)))
        .expect("scroll");
    frame(&mut app);
    let after = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(
        after.origin,
        LogicalPoint::new(px(10.0), px(44.0)),
        "滚动后节点锚点必须同帧跟随"
    );
}

const E01_MODAL_ANIMATION: Duration = Duration::from_millis(130);

fn settled_modal(app: &mut TestApp, anchor: LogicalRect) -> PopupId {
    let id = open_sized(
        app,
        modal_spec(Anchor::WindowRect(anchor), false),
        (200.0, 120.0),
        None,
    );
    frame(app);
    app.advance(E01_MODAL_ANIMATION);
    frame(app);
    id
}

#[test]
fn a_closing_modal_swallows_background_presses_until_it_is_removed() {
    let mut app = TestApp::new();
    let background = root_button(&mut app, (200.0, 40.0), "background");
    frame(&mut app);
    let hits = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&hits);
    app.app_mut()
        .on_action(Action::custom("background"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
    let modal = settled_modal(&mut app, rect(400.0, 300.0, 20.0, 20.0));
    assert!(
        app.app_mut()
            .begin_close_popup(modal, DismissReason::Escape)
            .expect("begin close"),
        "关闭事务必须启动"
    );
    assert_eq!(
        app.app().popup_state(modal),
        Some(PopupState::Closing),
        "关闭开始即进入 DismissPending"
    );
    let center = point(100.0, 20.0);
    let (_, down) = press(&mut app, center);
    assert!(down, "DismissPending 的 modal 必须消费背景按下");
    let (_, up) = release(&mut app, center);
    assert!(up, "DismissPending 的 modal 必须消费背景抬起");
    assert_eq!(hits.get(), 0, "淡出期点击背景不得执行 action（E-01）");
    assert_eq!(
        app.app().focused(win(&app)),
        None,
        "关闭期焦点已归还，但背景点击仍不得落到焦点控件"
    );
    app.advance(Duration::from_millis(95));
    frame(&mut app);
    assert!(app.app().popup_rect(modal).is_none(), "淡出结束必须移除");
    let (_, down) = press(&mut app, center);
    assert!(!down, "modal 移除后背景按下必须恢复派发");
    let (_, up) = release(&mut app, center);
    assert!(!up, "modal 移除后背景抬起必须恢复派发");
    assert_eq!(hits.get(), 1, "modal 移除后点击必须执行 action");
    let _ = background;
}

#[test]
fn a_closing_modal_keeps_background_hover_and_scroll_blocked() {
    let mut app = TestApp::new();
    let window = win(&app);
    let mut container_style = StyleBuilder::new()
        .absolute()
        .left(px(0.0))
        .top(px(0.0))
        .size(px(200.0), px(100.0))
        .build();
    container_style.clip = Clip::Bounds;
    let container = {
        let state = app.app_mut().window_mut(window).expect("window");
        let root = state.tree().root();
        NodeBuilder::boxed()
            .style(container_style)
            .build(state.tree_mut(), root)
            .expect("container")
    };
    let handle = app
        .app_mut()
        .make_scroll(
            window,
            container,
            ui_core::interaction::ScrollConfig::default(),
        )
        .expect("scroll");
    app.app_mut()
        .set_scroll_content(&handle, LogicalSize::new(px(200.0), px(600.0)))
        .expect("content");
    frame(&mut app);
    let modal = settled_modal(&mut app, rect(400.0, 300.0, 20.0, 20.0));
    let background = point(50.0, 50.0);
    app.app_mut()
        .begin_close_popup(modal, DismissReason::Escape)
        .expect("begin close");
    app.move_pointer(background);
    assert_eq!(
        app.app().hovered(window),
        None,
        "DismissPending 的 modal 必须继续遮挡背景 hover"
    );
    app.inject(InputEvent::scroll_lines(
        background,
        LogicalPoint::new(px(0.0), px(-120.0)),
        Modifiers::empty(),
    ));
    app.advance(Duration::from_millis(30));
    frame(&mut app);
    assert_eq!(
        app.app().scroll_state(&handle).expect("state").offset.y,
        px(0.0),
        "DismissPending 的 modal 必须继续遮挡背景滚动"
    );
    app.advance(Duration::from_millis(80));
    frame(&mut app);
    assert!(app.app().popup_rect(modal).is_none());
    app.inject(InputEvent::scroll_lines(
        background,
        LogicalPoint::new(px(0.0), px(-120.0)),
        Modifiers::empty(),
    ));
    app.advance(Duration::from_millis(400));
    frame(&mut app);
    assert!(
        app.app().scroll_state(&handle).expect("state").offset.y > px(0.0),
        "modal 移除后背景滚动必须恢复"
    );
}

#[test]
fn a_nested_modal_escape_sequence_closes_topmost_then_the_parent() {
    let mut app = TestApp::new();
    let window = win(&app);
    let background = root_button(&mut app, (200.0, 40.0), "background");
    frame(&mut app);
    let hits = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&hits);
    app.app_mut()
        .on_action(Action::custom("background"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
    let outer = settled_modal(&mut app, rect(200.0, 120.0, 20.0, 20.0));
    let outer_content = popup_node(&app, outer);
    let inner_button = {
        let state = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(120.0), px(32.0)).build())
            .on_click(Action::custom("inner"))
            .build(state.tree_mut(), outer_content)
            .expect("inner button")
    };
    frame(&mut app);
    let mut inner_spec = modal_spec(Anchor::Node { node: inner_button }, false);
    inner_spec.opener = Some(inner_button);
    let inner = open_sized(&mut app, inner_spec, (160.0, 100.0), None);
    frame(&mut app);
    app.advance(E01_MODAL_ANIMATION);
    frame(&mut app);
    assert_eq!(
        app.app().focused(window),
        Some(popup_node(&app, inner)),
        "内层 modal 必须取焦"
    );
    let (_, consumed) = escape(&mut app);
    assert!(consumed, "Esc 必须关闭最上层 modal");
    assert_eq!(app.app().popup_state(inner), Some(PopupState::Closing));
    assert_eq!(
        app.app().popup_state(outer),
        Some(PopupState::Open),
        "Esc 只关最上层，父 modal 保持打开"
    );
    let center = point(100.0, 20.0);
    let (_, down) = press(&mut app, center);
    assert!(down, "内层淡出期背景按下必须被吞");
    let (_, up) = release(&mut app, center);
    assert!(up, "内层淡出期背景抬起必须被吞");
    assert_eq!(hits.get(), 0, "内层淡出期不得执行背景 action");
    app.advance(Duration::from_millis(95));
    frame(&mut app);
    assert!(app.app().popup_rect(inner).is_none());
    assert_eq!(
        app.app().popup_state(outer),
        Some(PopupState::Open),
        "内层移除后父 modal 仍打开"
    );
    let (_, consumed) = escape(&mut app);
    assert!(consumed, "父 modal 必须能被下一次 Esc 关闭");
    assert_eq!(app.app().popup_state(outer), Some(PopupState::Closing));
    let (_, down) = press(&mut app, center);
    assert!(down, "父 modal 淡出期背景按下必须被吞");
    let (_, up) = release(&mut app, center);
    assert!(up, "父 modal 淡出期背景抬起必须被吞");
    assert_eq!(hits.get(), 0);
    app.advance(Duration::from_millis(95));
    frame(&mut app);
    assert!(app.app().popup_rect(outer).is_none());
    let (_, down) = press(&mut app, center);
    assert!(!down);
    let (_, up) = release(&mut app, center);
    assert!(!up);
    assert_eq!(hits.get(), 1, "整条 modal 链移除后背景点击恢复");
    let _ = background;
}

#[test]
fn closing_the_lower_modal_cascades_and_keeps_input_blocked_until_all_leave() {
    let mut app = TestApp::new();
    let window = win(&app);
    let background = root_button(&mut app, (200.0, 40.0), "background");
    frame(&mut app);
    let hits = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&hits);
    app.app_mut()
        .on_action(Action::custom("background"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
    let outer = settled_modal(&mut app, rect(300.0, 200.0, 20.0, 20.0));
    let outer_content = popup_node(&app, outer);
    let item = {
        let state = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(100.0), px(30.0)).build())
            .build(state.tree_mut(), outer_content)
            .expect("item")
    };
    frame(&mut app);
    let mut child_spec = base_spec(Anchor::NodeRect {
        node: item,
        rect: rect(0.0, 0.0, 100.0, 30.0),
    });
    child_spec.parent = Some(outer);
    child_spec.animation = PopupAnimation::Fade;
    let child = open_sized(&mut app, child_spec, (120.0, 80.0), None);
    frame(&mut app);
    app.advance(E01_MODAL_ANIMATION);
    frame(&mut app);
    assert!(
        app.app_mut()
            .begin_close_popup(outer, DismissReason::Programmatic)
            .expect("begin close")
    );
    assert_eq!(app.app().popup_state(outer), Some(PopupState::Closing));
    assert_eq!(
        app.app().popup_state(child),
        Some(PopupState::Closing),
        "modal 关闭必须级联其内容链"
    );
    let center = point(100.0, 20.0);
    let (_, down) = press(&mut app, center);
    assert!(down, "级联关闭期背景按下必须被吞");
    let (_, up) = release(&mut app, center);
    assert!(up, "级联关闭期背景抬起必须被吞");
    assert_eq!(hits.get(), 0);
    app.advance(Duration::from_millis(95));
    frame(&mut app);
    assert!(app.app().popup_rect(outer).is_none());
    assert!(app.app().popup_rect(child).is_none());
    let (_, down) = press(&mut app, center);
    assert!(!down);
    let (_, up) = release(&mut app, center);
    assert!(!up);
    assert_eq!(hits.get(), 1, "级联关闭完成后背景点击恢复");
    let _ = background;
}

#[test]
fn a_stale_focus_owner_is_repaired_when_the_next_tab_runs() {
    let mut app = TestApp::new();
    let window = win(&app);
    let first = root_button(&mut app, (100.0, 30.0), "first");
    let second = root_button(&mut app, (100.0, 30.0), "second");
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(first)));
    let mut spec = base_spec(Anchor::WindowRect(rect(500.0, 400.0, 20.0, 20.0)));
    spec.focus = FocusPolicy::Take {
        restore: RestoreFocus::Previous,
    };
    spec.animation = PopupAnimation::None;
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    let content = popup_node(&app, id);
    assert_eq!(app.app().focused(window), Some(content));
    app.app_mut()
        .window_mut(window)
        .expect("window")
        .tree_mut()
        .remove(content)
        .expect("remove content node directly");
    assert_eq!(
        app.app().focused(window),
        None,
        "内容节点消失后 stale owner 让焦点读数为 None"
    );
    let next = app.app_mut().focus_next(window);
    assert_eq!(
        next,
        Some(second),
        "validate_owner 必须清除失效 owner 并让 Tab 继续遍历主树"
    );
    assert_eq!(app.app().focused(window), Some(second));
}

#[test]
fn removing_a_take_popups_content_node_restores_main_focus_and_tab() {
    let mut app = TestApp::new();
    let window = win(&app);
    let first = root_button(&mut app, (100.0, 30.0), "first");
    let second = root_button(&mut app, (100.0, 30.0), "second");
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(first)));
    let mut spec = base_spec(Anchor::WindowRect(rect(500.0, 400.0, 20.0, 20.0)));
    spec.focus = FocusPolicy::Take {
        restore: RestoreFocus::Previous,
    };
    spec.animation = PopupAnimation::None;
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    let content = popup_node(&app, id);
    assert_eq!(app.app().focused(window), Some(content));
    app.app_mut().remove_node(window, content).expect("remove");
    assert!(
        app.app()
            .popup_stack(window)
            .expect("stack")
            .entries
            .is_empty(),
        "外部删除 content 必须同时修剪 popup 栈条目"
    );
    assert_eq!(
        app.app().focused(window),
        Some(first),
        "外部删除 popup content 必须恢复主树焦点"
    );
    assert_eq!(
        app.app_mut().focus_next(window),
        Some(second),
        "焦点恢复后 Tab 必须可用"
    );
}

#[test]
fn window_focus_loss_does_not_swallow_the_next_background_click() {
    let mut app = TestApp::new();
    let window = win(&app);
    let trigger = root_button(&mut app, (100.0, 30.0), "trigger");
    let background = {
        let state = app.app_mut().window_mut(window).expect("window");
        let root = state.tree().root();
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .absolute()
                    .left(px(300.0))
                    .top(px(300.0))
                    .size(px(100.0), px(40.0))
                    .build(),
            )
            .on_click(Action::custom("background"))
            .build(state.tree_mut(), root)
            .expect("background")
    };
    frame(&mut app);
    let hits = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&hits);
    app.app_mut()
        .on_action(Action::custom("background"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
    let mut spec = base_spec(Anchor::NodeRect {
        node: trigger,
        rect: rect(0.0, 0.0, 100.0, 30.0),
    });
    spec.opener = Some(trigger);
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    let (_, consumed) = press(&mut app, point(50.0, 15.0));
    assert!(consumed, "opener Toggle 必须消费按下");
    assert!(app.app().popup_rect(id).is_none());
    app.app_mut().window_focus_lost(window);
    let target = point(350.0, 320.0);
    let (_, down) = press(&mut app, target);
    assert!(!down, "失焦后第一次点击不得被 suppress_release 吞掉按下");
    let (_, up) = release(&mut app, target);
    assert!(!up);
    assert_eq!(
        hits.get(),
        1,
        "窗口失焦必须重置 suppress_release，第一次点击即生效"
    );
    let _ = background;
}

#[test]
fn window_scroll_epoch_advances_only_when_an_offset_changes() {
    let mut app = TestApp::new();
    let window = win(&app);
    let mut container_style = StyleBuilder::new()
        .absolute()
        .left(px(0.0))
        .top(px(0.0))
        .size(px(200.0), px(100.0))
        .build();
    container_style.clip = Clip::Bounds;
    let container = {
        let state = app.app_mut().window_mut(window).expect("window");
        let root = state.tree().root();
        NodeBuilder::boxed()
            .style(container_style)
            .build(state.tree_mut(), root)
            .expect("container")
    };
    let handle = app
        .app_mut()
        .make_scroll(
            window,
            container,
            ui_core::interaction::ScrollConfig::default(),
        )
        .expect("scroll");
    app.app_mut()
        .set_scroll_content(&handle, LogicalSize::new(px(200.0), px(600.0)))
        .expect("content");
    frame(&mut app);
    let epoch = app.app().window_scroll_epoch(window).expect("epoch");
    app.app_mut()
        .scroll_to(&handle, LogicalPoint::new(px(0.0), px(40.0)))
        .expect("scroll");
    frame(&mut app);
    let moved = app.app().window_scroll_epoch(window).expect("epoch");
    assert_ne!(moved, epoch, "滚动 offset 变化必须推进 epoch");
    app.app_mut()
        .scroll_to(&handle, LogicalPoint::new(px(0.0), px(40.0)))
        .expect("same offset");
    frame(&mut app);
    assert_eq!(
        app.app().window_scroll_epoch(window).expect("epoch"),
        moved,
        "未改变 offset 的滚动不得推进 epoch"
    );
}

#[test]
fn popup_anchor_queries_track_scrolling_and_visibility() {
    let mut app = TestApp::new();
    let window = win(&app);
    let mut container_style = StyleBuilder::new()
        .absolute()
        .left(px(0.0))
        .top(px(0.0))
        .size(px(200.0), px(100.0))
        .build();
    container_style.clip = Clip::Bounds;
    let container = {
        let state = app.app_mut().window_mut(window).expect("window");
        let root = state.tree().root();
        NodeBuilder::boxed()
            .style(container_style)
            .build(state.tree_mut(), root)
            .expect("container")
    };
    let content = {
        let state = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(200.0), px(600.0)).build())
            .build(state.tree_mut(), container)
            .expect("content")
    };
    let node = {
        let state = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .absolute()
                    .left(px(10.0))
                    .top(px(50.0))
                    .size(px(80.0), px(30.0))
                    .build(),
            )
            .on_click(Action::custom("anchor"))
            .build(state.tree_mut(), content)
            .expect("node")
    };
    frame(&mut app);
    let handle = app
        .app_mut()
        .make_scroll(
            window,
            container,
            ui_core::interaction::ScrollConfig::default(),
        )
        .expect("scroll");
    app.app_mut()
        .set_scroll_content(&handle, LogicalSize::new(px(200.0), px(600.0)))
        .expect("content");
    frame(&mut app);
    let mut spec = base_spec(Anchor::Node { node });
    spec.placement = node_placement(Side::Bottom, Alignment::Start);
    let id = open_sized(&mut app, spec, (60.0, 30.0), None);
    frame(&mut app);
    assert_eq!(
        app.app().popup_anchor_rect(id),
        Some(rect(10.0, 50.0, 80.0, 30.0)),
        "Anchor::Node 查询返回节点完整矩形"
    );
    assert_eq!(app.app().popup_anchor_visible(id), Some(true));
    let epoch = app.app().window_scroll_epoch(window).expect("epoch");
    app.app_mut()
        .scroll_to(&handle, LogicalPoint::new(px(0.0), px(40.0)))
        .expect("scroll");
    frame(&mut app);
    assert_eq!(
        app.app().popup_anchor_rect(id),
        Some(rect(10.0, 10.0, 80.0, 30.0)),
        "滚动后锚点矩形必须同帧更新"
    );
    assert_eq!(app.app().popup_anchor_visible(id), Some(true));
    assert_ne!(
        app.app().window_scroll_epoch(window).expect("epoch"),
        epoch,
        "滚动必须推进 epoch 供 kit 检测祖先滚动"
    );
    app.app_mut()
        .scroll_to(&handle, LogicalPoint::new(px(0.0), px(500.0)))
        .expect("scroll out");
    frame(&mut app);
    assert_eq!(
        app.app().popup_anchor_visible(id),
        Some(false),
        "锚点滚出容器后 visible 必须为 false"
    );
    let point_anchor = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (60.0, 30.0),
        None,
    );
    frame(&mut app);
    assert_eq!(app.app().popup_anchor_visible(point_anchor), Some(true));
    app.app_mut()
        .set_popup_anchor(
            point_anchor,
            Anchor::WindowRect(rect(2000.0, 2000.0, 20.0, 20.0)),
        )
        .expect("anchor");
    assert_eq!(
        app.app().popup_anchor_visible(point_anchor),
        Some(false),
        "视口外的 WindowRect 锚点不可见"
    );
}

#[test]
fn scrim_paints_the_configured_color_and_density() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(400.0, 300.0, 20.0, 20.0)));
    spec.tier = PopupTier::Modal;
    spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    spec.animation = PopupAnimation::None;
    spec.scrim = Some(ScrimSpec {
        density: 0.5,
        color: ui_core::paint::color::Color::srgb(1.0, 0.0, 0.0, 1.0),
    });
    let id = open_sized(&mut app, spec, (200.0, 120.0), None);
    frame(&mut app);
    let scrim = popup_node(&app, scrim_id(&app));
    let fill = app
        .rendered()
        .expect("rendered")
        .quads
        .iter()
        .find(|quad| quad.node == scrim)
        .map(|quad| quad.fill)
        .expect("scrim quad");
    assert!(
        (fill.components[3] - 0.5).abs() < 0.02,
        "scrim alpha 必须是 density：{:?}",
        fill.components
    );
    assert!(
        fill.components[0] > 0.45 && fill.components[1] < 0.02 && fill.components[2] < 0.02,
        "scrim 必须使用 spec.color（红色）：{:?}",
        fill.components
    );
    app.app_mut().close_popup(id).expect("close");
    frame(&mut app);
    let mut default_spec = base_spec(Anchor::WindowRect(rect(400.0, 300.0, 20.0, 20.0)));
    default_spec.tier = PopupTier::Modal;
    default_spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    default_spec.animation = PopupAnimation::None;
    default_spec.scrim = Some(ScrimSpec {
        density: 0.5,
        ..ScrimSpec::default()
    });
    open_sized(&mut app, default_spec, (200.0, 120.0), None);
    frame(&mut app);
    let scrim = popup_node(&app, scrim_id(&app));
    let fill = app
        .rendered()
        .expect("rendered")
        .quads
        .iter()
        .find(|quad| quad.node == scrim)
        .map(|quad| quad.fill)
        .expect("scrim quad");
    assert!(
        fill.components[0].abs() < 0.001
            && fill.components[1].abs() < 0.001
            && fill.components[2].abs() < 0.001,
        "ScrimSpec 默认色必须是黑色：{:?}",
        fill.components
    );
    assert!((fill.components[3] - 0.5).abs() < 0.02);
}

#[test]
fn set_popup_initial_focus_moves_the_focus_within_a_trap() {
    let mut app = TestApp::new();
    let window = win(&app);
    let captured = std::rc::Rc::new(std::cell::RefCell::new(Vec::<NodeId>::new()));
    let sink = std::rc::Rc::clone(&captured);
    let mut spec = base_spec(Anchor::WindowRect(rect(400.0, 300.0, 20.0, 20.0)));
    spec.tier = PopupTier::Modal;
    spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::FirstFocusable,
    };
    spec.animation = PopupAnimation::None;
    let id = app
        .app_mut()
        .open_popup(
            window,
            spec,
            move |app, window, content| {
                let state = app.window_mut(window).expect("window");
                for action in ["first", "second"] {
                    let node = NodeBuilder::boxed()
                        .on_click(Action::custom(action))
                        .build(state.tree_mut(), content)?;
                    sink.borrow_mut().push(node);
                }
                let plain = NodeBuilder::boxed().build(state.tree_mut(), content)?;
                sink.borrow_mut().push(plain);
                Ok(())
            },
            None,
        )
        .expect("modal");
    frame(&mut app);
    let nodes = captured.borrow().clone();
    let (first, second, plain) = (nodes[0], nodes[1], nodes[2]);
    assert_eq!(
        app.app().focused(window),
        Some(first),
        "Trap 初始焦点先按策略取第一可聚焦"
    );
    assert!(
        app.app_mut()
            .set_popup_initial_focus(id, second)
            .expect("setter"),
        "Trap 弹层必须接受初始焦点修正"
    );
    assert_eq!(app.app().focused(window), Some(second));
    assert!(
        !app.app_mut()
            .set_popup_initial_focus(id, plain)
            .expect("setter"),
        "不可聚焦节点必须被拒绝"
    );
    assert_eq!(app.app().focused(window), Some(second));
    app.app_mut().close_popup(id).expect("close");
    assert!(
        app.app_mut().set_popup_initial_focus(id, second).is_err(),
        "已关闭 popup 必须返回强类型错误"
    );
}

#[test]
fn set_popup_initial_focus_rejects_non_trap_popups() {
    let mut app = TestApp::new();
    let window = win(&app);
    let captured = std::rc::Rc::new(std::cell::RefCell::new(None::<NodeId>));
    let sink = std::rc::Rc::clone(&captured);
    let mut spec = base_spec(Anchor::WindowRect(rect(400.0, 300.0, 20.0, 20.0)));
    spec.focus = FocusPolicy::Take {
        restore: RestoreFocus::Opener,
    };
    spec.animation = PopupAnimation::None;
    let id = app
        .app_mut()
        .open_popup(
            window,
            spec,
            move |app, window, content| {
                let state = app.window_mut(window).expect("window");
                let node = NodeBuilder::boxed()
                    .on_click(Action::custom("only"))
                    .build(state.tree_mut(), content)?;
                *sink.borrow_mut() = Some(node);
                Ok(())
            },
            None,
        )
        .expect("popup");
    frame(&mut app);
    let node = captured.borrow().expect("content node");
    assert!(
        !app.app_mut()
            .set_popup_initial_focus(id, node)
            .expect("setter"),
        "Take 弹层不得接受初始焦点修正"
    );
}

#[test]
fn raising_a_modal_moves_its_scrim_to_the_adjacent_z_slot() {
    let mut app = TestApp::new();
    let window = win(&app);
    let modal = open_sized(
        &mut app,
        modal_spec(Anchor::WindowRect(rect(300.0, 200.0, 20.0, 20.0)), true),
        (200.0, 120.0),
        None,
    );
    frame(&mut app);
    app.advance(E01_MODAL_ANIMATION);
    frame(&mut app);
    let auto = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (80.0, 40.0),
        None,
    );
    frame(&mut app);
    let z_of = |app: &TestApp, id: PopupId| {
        app.app()
            .popup_stack(window)
            .expect("stack")
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .expect("entry")
            .z
    };
    assert!(z_of(&app, auto) > z_of(&app, modal));
    let scrim = scrim_id(&app);
    app.app_mut().raise_popup(modal).expect("raise");
    frame(&mut app);
    assert!(
        z_of(&app, scrim) + 1 == z_of(&app, modal),
        "raise modal 必须把 scrim 移到 modal 正下方（E-16）"
    );
    assert!(z_of(&app, modal) > z_of(&app, auto));
}

#[test]
fn immediate_modal_close_notifies_owned_descendants_children_first() {
    let mut app = TestApp::new();
    let window = win(&app);
    let modal = open_sized(
        &mut app,
        modal_spec(Anchor::WindowRect(rect(300.0, 200.0, 20.0, 20.0)), false),
        (300.0, 200.0),
        None,
    );
    frame(&mut app);
    app.advance(E01_MODAL_ANIMATION);
    frame(&mut app);
    let outer_content = popup_node(&app, modal);
    let first_item = {
        let state = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(120.0), px(30.0)).build())
            .build(state.tree_mut(), outer_content)
            .expect("first item")
    };
    frame(&mut app);
    let order = std::rc::Rc::new(std::cell::RefCell::new(Vec::<PopupId>::new()));
    let record_b = std::rc::Rc::clone(&order);
    let mut child_b_spec = base_spec(Anchor::Node { node: first_item });
    child_b_spec.opener = Some(first_item);
    let child_b = open_sized(
        &mut app,
        child_b_spec,
        (140.0, 80.0),
        Some(Box::new(move |_app, dismiss| {
            record_b.borrow_mut().push(dismiss.id);
            DismissOutcome::Close
        })),
    );
    frame(&mut app);
    let child_b_content = popup_node(&app, child_b);
    let second_item = {
        let state = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(100.0), px(24.0)).build())
            .build(state.tree_mut(), child_b_content)
            .expect("second item")
    };
    frame(&mut app);
    let record_c = std::rc::Rc::clone(&order);
    let mut child_c_spec = base_spec(Anchor::Node { node: second_item });
    child_c_spec.parent = Some(child_b);
    child_c_spec.opener = Some(second_item);
    let child_c = open_sized(
        &mut app,
        child_c_spec,
        (100.0, 60.0),
        Some(Box::new(move |_app, dismiss| {
            record_c.borrow_mut().push(dismiss.id);
            DismissOutcome::Close
        })),
    );
    frame(&mut app);
    app.app_mut().close_popup(modal).expect("close modal");
    assert!(
        app.app()
            .popup_stack(window)
            .expect("stack")
            .entries
            .is_empty()
    );
    assert_eq!(
        order.borrow().as_slice(),
        &[child_c, child_b],
        "立即关闭 modal 时 owner_modal 后代必须按深度降序通知（子先于父，E-15）"
    );
}

#[test]
fn a_zero_size_point_anchor_is_visible_inside_the_viewport_and_lost_outside() {
    let mut app = TestApp::new();
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(200.0, 150.0, 0.0, 0.0))),
        (120.0, 80.0),
        None,
    );
    frame(&mut app);
    assert_eq!(
        app.app().popup_anchor_rect(id),
        Some(rect(200.0, 150.0, 0.0, 0.0)),
        "零尺寸点锚点矩形必须原样返回"
    );
    assert_eq!(
        app.app().popup_anchor_visible(id),
        Some(true),
        "视口内的零尺寸点锚点（ContextMenu）必须可见"
    );
    app.app_mut()
        .set_popup_anchor(id, Anchor::WindowRect(rect(2000.0, 150.0, 0.0, 0.0)))
        .expect("anchor");
    frame(&mut app);
    assert_eq!(
        app.app().popup_anchor_visible(id),
        Some(false),
        "移出视口的零尺寸点锚点必须不可见"
    );
}

#[test]
fn a_zero_size_node_rect_anchor_follows_the_node_clip() {
    let mut app = TestApp::new();
    let window = win(&app);
    let mut container_style = StyleBuilder::new()
        .absolute()
        .left(px(0.0))
        .top(px(0.0))
        .size(px(200.0), px(100.0))
        .build();
    container_style.clip = Clip::Bounds;
    let (container, node) = {
        let state = app.app_mut().window_mut(window).expect("window");
        let root = state.tree().root();
        let container = NodeBuilder::boxed()
            .style(container_style)
            .build(state.tree_mut(), root)
            .expect("container");
        let content = NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(200.0), px(600.0)).build())
            .build(state.tree_mut(), container)
            .expect("content");
        let node = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .absolute()
                    .left(px(10.0))
                    .top(px(50.0))
                    .size(px(80.0), px(30.0))
                    .build(),
            )
            .build(state.tree_mut(), content)
            .expect("node");
        (container, node)
    };
    frame(&mut app);
    let handle = app
        .app_mut()
        .make_scroll(
            window,
            container,
            ui_core::interaction::ScrollConfig::default(),
        )
        .expect("scroll");
    app.app_mut()
        .set_scroll_content(&handle, LogicalSize::new(px(200.0), px(600.0)))
        .expect("content");
    frame(&mut app);
    let mut spec = base_spec(Anchor::NodeRect {
        node,
        rect: rect(20.0, 10.0, 0.0, 0.0),
    });
    spec.placement = node_placement(Side::Bottom, Alignment::Start);
    let id = open_sized(&mut app, spec, (60.0, 30.0), None);
    frame(&mut app);
    assert_eq!(
        app.app().popup_anchor_visible(id),
        Some(true),
        "节点可见区内的零尺寸 NodeRect 子锚点必须可见"
    );
    app.app_mut()
        .scroll_to(&handle, LogicalPoint::new(px(0.0), px(500.0)))
        .expect("scroll out");
    frame(&mut app);
    assert_eq!(
        app.app().popup_anchor_visible(id),
        Some(false),
        "节点滚出容器后零尺寸 NodeRect 子锚点必须不可见"
    );
}

#[test]
fn popup_internal_scroll_does_not_advance_the_window_scroll_epoch() {
    let mut app = TestApp::new();
    let window = win(&app);
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (220.0, 160.0),
        None,
    );
    frame(&mut app);
    let content_node = popup_node(&app, id);
    let mut container_style = StyleBuilder::new()
        .absolute()
        .left(px(0.0))
        .top(px(0.0))
        .size(px(200.0), px(100.0))
        .build();
    container_style.clip = Clip::Bounds;
    let container = {
        let state = app.app_mut().window_mut(window).expect("window");
        let container = NodeBuilder::boxed()
            .style(container_style)
            .build(state.tree_mut(), content_node)
            .expect("container");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(200.0), px(600.0)).build())
            .build(state.tree_mut(), container)
            .expect("content");
        container
    };
    frame(&mut app);
    let handle = app
        .app_mut()
        .make_scroll(
            window,
            container,
            ui_core::interaction::ScrollConfig::default(),
        )
        .expect("scroll");
    app.app_mut()
        .set_scroll_content(&handle, LogicalSize::new(px(200.0), px(600.0)))
        .expect("content");
    frame(&mut app);
    let epoch = app.app().window_scroll_epoch(window).expect("epoch");
    app.app_mut()
        .scroll_to(&handle, LogicalPoint::new(px(0.0), px(40.0)))
        .expect("scroll");
    frame(&mut app);
    assert_eq!(
        app.app().scroll_state(&handle).expect("state").offset.y,
        px(40.0),
        "popup 内部滚动必须生效"
    );
    assert_eq!(
        app.app().window_scroll_epoch(window).expect("epoch"),
        epoch,
        "popup 子树内部滚动不得推进 window_scroll_epoch（kit 菜单内部滚动不得被当作祖先滚动）"
    );
    let anchor = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(400.0, 300.0, 20.0, 20.0))),
        (60.0, 30.0),
        None,
    );
    frame(&mut app);
    let outer_epoch = app.app().window_scroll_epoch(window).expect("epoch");
    let mut main_style = StyleBuilder::new()
        .absolute()
        .left(px(0.0))
        .top(px(0.0))
        .size(px(120.0), px(60.0))
        .build();
    main_style.clip = Clip::Bounds;
    let main_scroll = {
        let state = app.app_mut().window_mut(window).expect("window");
        let root = state.tree().root();
        let container = NodeBuilder::boxed()
            .style(main_style)
            .build(state.tree_mut(), root)
            .expect("main container");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(120.0), px(400.0)).build())
            .build(state.tree_mut(), container)
            .expect("main content");
        container
    };
    frame(&mut app);
    let handle = app
        .app_mut()
        .make_scroll(
            window,
            main_scroll,
            ui_core::interaction::ScrollConfig::default(),
        )
        .expect("main scroll");
    app.app_mut()
        .set_scroll_content(&handle, LogicalSize::new(px(120.0), px(400.0)))
        .expect("content");
    frame(&mut app);
    app.app_mut()
        .scroll_to(&handle, LogicalPoint::new(px(0.0), px(20.0)))
        .expect("main scroll");
    frame(&mut app);
    assert_ne!(
        app.app().window_scroll_epoch(window).expect("epoch"),
        outer_epoch,
        "主内容树滚动必须推进 epoch（ContextMenu 关链依据）"
    );
    let _ = (anchor, content_node);
}

#[test]
fn raising_a_modal_keeps_its_owner_modal_chain_above_it() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = win(&app);
    let mut a_spec = modal_spec(Anchor::WindowRect(rect(200.0, 120.0, 40.0, 30.0)), true);
    a_spec.animation = PopupAnimation::None;
    let a = open_sized(&mut app, a_spec, (300.0, 200.0), None);
    frame(&mut app);
    let a_content = popup_node(&app, a);
    let a_button = {
        let state = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(120.0), px(30.0)).build())
            .on_click(Action::custom("inside-a"))
            .build(state.tree_mut(), a_content)
            .expect("a button")
    };
    frame(&mut app);
    let mut c_spec = modal_spec(Anchor::WindowRect(rect(600.0, 400.0, 30.0, 30.0)), false);
    c_spec.animation = PopupAnimation::None;
    let c = open_sized(&mut app, c_spec, (160.0, 100.0), None);
    frame(&mut app);
    let mut p_spec = base_spec(Anchor::NodeRect {
        node: a_button,
        rect: rect(0.0, 0.0, 120.0, 30.0),
    });
    p_spec.opener = Some(a_button);
    p_spec.animation = PopupAnimation::None;
    let p = open_sized(&mut app, p_spec, (140.0, 60.0), None);
    frame(&mut app);
    let b_clicked = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let b_counter = std::rc::Rc::clone(&b_clicked);
    app.app_mut()
        .on_action(Action::custom("inside-b"), move |_ctx| {
            b_counter.set(b_counter.get() + 1);
        });
    let b_button_slot = std::rc::Rc::new(std::cell::Cell::new(None::<NodeId>));
    let b_slot = std::rc::Rc::clone(&b_button_slot);
    let mut b_spec = modal_spec(
        Anchor::NodeRect {
            node: a_button,
            rect: rect(0.0, 0.0, 120.0, 30.0),
        },
        true,
    );
    b_spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::FirstFocusable,
    };
    b_spec.opener = Some(a_button);
    b_spec.animation = PopupAnimation::None;
    let b = app
        .app_mut()
        .open_popup(
            window,
            b_spec,
            move |app, window, content| {
                let state = app.window_mut(window).expect("window");
                let button = NodeBuilder::boxed()
                    .style(StyleBuilder::new().size(px(100.0), px(30.0)).build())
                    .on_click(Action::custom("inside-b"))
                    .build(state.tree_mut(), content)?;
                b_slot.set(Some(button));
                Ok(())
            },
            None,
        )
        .expect("modal b");
    frame(&mut app);
    let b_content = popup_node(&app, b);
    let b_button = b_button_slot.get().expect("b button");
    let z_of = |app: &TestApp, id: PopupId| {
        app.app()
            .popup_stack(window)
            .expect("stack")
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .map_or_else(|| panic!("entry {id:?} missing"), |entry| entry.z)
    };
    let a_scrim = scrim_id(&app);
    let z = |id: PopupId| z_of(&app, id);
    assert_eq!(z(a_scrim) + 1, z(a));
    assert_eq!(z(p), z(a) + 2);
    assert!(z(b) > z(c));
    assert_eq!(
        app.app().modal_scope(window),
        Some(b_content),
        "raise 前 topmost modal 必须是内层 B"
    );
    assert_eq!(app.app().focused(window), Some(b_button));
    let center = {
        let rect = app
            .app()
            .window(window)
            .expect("window")
            .node_screen_rect(b_button)
            .expect("b button rect");
        point(
            (rect.origin.x + rect.size.width.value() * 0.5).value(),
            (rect.origin.y + rect.size.height.value() * 0.5).value(),
        )
    };
    let (_, down) = press(&mut app, center);
    let (_, up) = release(&mut app, center);
    assert!(!down && !up);
    assert_eq!(b_clicked.get(), 1, "raise 前 B 内容必须可交互");
    app.app_mut().raise_popup(a).expect("raise a");
    frame(&mut app);
    let z = |id: PopupId| z_of(&app, id);
    assert_eq!(z(a_scrim) + 1, z(a), "raise 后 scrim 仍紧贴 A");
    assert!(z(a) < z(p), "A 内已展开的 Auto popup 必须仍在 A 之上");
    assert!(z(p) < z(b), "Auto popup 必须仍在 B 之下");
    assert_eq!(z(b), z(a) + 3, "raise 必须整组抬升并保持组内相对顺序");
    assert!(z(b) > z(c), "raise A 不得让 A 越过内层 modal B");
    assert_eq!(
        app.app().modal_scope(window),
        Some(b_content),
        "raise A 后 topmost modal 必须仍是 B（E-17）"
    );
    assert_eq!(
        app.app().focused(window),
        Some(b_button),
        "raise A 不得夺走 B 的焦点"
    );
    assert!(
        app.probe().hit(center).is_some(),
        "raise A 后 B 内容区域必须仍可命中"
    );
    let (_, down) = press(&mut app, center);
    let (_, up) = release(&mut app, center);
    assert!(!down && !up);
    assert_eq!(
        b_clicked.get(),
        2,
        "raise A 后 B 内容输入不得被 A 的 scrim 阻断"
    );
    assert!(
        app.app().popup_rect(p).is_some(),
        "raise A 不得关闭 A 内已展开的 Auto popup"
    );
    if let Some(snapshot) = app.a11y() {
        let record = snapshot.node_for(b_button).expect("b button a11y record");
        assert!(!record.hidden, "raise A 后 B 的 a11y 不得被隐藏");
    }
}

#[test]
fn popup_a11y_active_descendant_is_validated_and_cleared() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = win(&app);
    let popup = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (200.0, 120.0),
        None,
    );
    frame(&mut app);
    let content = popup_node(&app, popup);
    let target = {
        let state = app.app_mut().window_mut(window).expect("window");
        NodeBuilder::boxed()
            .a11y(ui_core::a11y::A11yOverride {
                role: Some(ui_core::a11y::A11yRole::MenuItem),
                label: Some("行".into()),
                ..ui_core::a11y::A11yOverride::default()
            })
            .build(state.tree_mut(), content)
            .expect("row")
    };
    frame(&mut app);
    let a11y = |active: Option<NodeId>| ui_core::popup::PopupA11y {
        role: Some(ui_core::a11y::A11yRole::ListBox),
        active_descendant: active,
        ..ui_core::popup::PopupA11y::default()
    };
    assert!(
        app.app_mut()
            .set_popup_a11y(popup, a11y(Some(target)))
            .expect("valid active descendant"),
        "content 后代必须被接受"
    );
    frame(&mut app);
    let active = app
        .a11y()
        .expect("a11y snapshot")
        .node_for(content)
        .expect("content a11y")
        .active_descendant
        .expect("合法 active_descendant 必须投影到 popup 内容根");
    let content_record = app
        .a11y()
        .expect("a11y snapshot")
        .node_for(content)
        .expect("content a11y");
    assert!(
        content_record.children.contains(&active) || content_record.owns.contains(&active),
        "active_descendant target 必须位于 content 的后代或 owns 内"
    );
    let root = root_node(&app);
    assert!(
        app.app_mut()
            .set_popup_a11y(popup, a11y(Some(root)))
            .expect("invalid target is cleared"),
        "非法 active_descendant 必须被清空并应用其它字段"
    );
    frame(&mut app);
    assert!(
        app.a11y()
            .expect("a11y snapshot")
            .node_for(content)
            .expect("content a11y")
            .active_descendant
            .is_none(),
        "非法 active_descendant 不得投影"
    );
    assert!(
        app.app()
            .a11y_diagnostics()
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                ui_core::a11y::A11yDiagnostic::InvalidActiveDescendant { node, target: reported }
                    if *node == content && *reported == root
            )),
        "非法 active_descendant 必须留诊断：{:?}",
        app.app().a11y_diagnostics()
    );
    let stale = root_box(&mut app, (10.0, 10.0), Color::BLACK);
    app.app_mut().remove_node(window, stale).expect("remove");
    assert!(
        app.app_mut()
            .set_popup_a11y(popup, a11y(Some(stale)))
            .expect("missing target is cleared"),
    );
    assert!(
        app.app()
            .a11y_diagnostics()
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                ui_core::a11y::A11yDiagnostic::InvalidActiveDescendant { node, target: reported }
                    if *node == content && *reported == stale
            )),
        "不存在的 active_descendant 必须留诊断"
    );
    let owned = root_box(&mut app, (10.0, 10.0), Color::BLACK);
    let mut owned_a11y = a11y(Some(owned));
    owned_a11y.owns.push(owned);
    assert!(
        app.app_mut()
            .set_popup_a11y(popup, owned_a11y)
            .expect("owned active descendant"),
        "owns 列表内的节点即使不在 content 子树也必须被接受"
    );
    frame(&mut app);
    let content_record = app
        .a11y()
        .expect("a11y snapshot")
        .node_for(content)
        .expect("content a11y");
    let active = content_record
        .active_descendant
        .expect("owns 内的 active_descendant 必须投影");
    assert!(
        content_record.owns.contains(&active),
        "owns 内的 active_descendant 必须出现在 owns 关系里"
    );
    let mut invalid_spec = base_spec(Anchor::WindowRect(rect(400.0, 300.0, 20.0, 20.0)));
    invalid_spec.a11y = a11y(Some(root));
    let opened = app
        .app_mut()
        .open_popup(window, invalid_spec, |_app, _window, _content| Ok(()), None)
        .expect("open with invalid active descendant");
    frame(&mut app);
    let opened_content = popup_node(&app, opened);
    assert!(
        app.app()
            .a11y_diagnostics()
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                ui_core::a11y::A11yDiagnostic::InvalidActiveDescendant { node, target: reported }
                    if *node == opened_content && *reported == root
            )),
        "open 时非法 active_descendant 必须清空并留诊断"
    );
    assert!(
        app.a11y()
            .expect("a11y snapshot")
            .node_for(opened_content)
            .expect("opened content a11y")
            .active_descendant
            .is_none(),
        "open 时的非法 active_descendant 不得进入 a11y 树"
    );
}

fn popup_snapshot(app: &TestApp, id: PopupId) -> ui_core::popup::PopupSnapshotEntry {
    app.app()
        .popup_stack(win(app))
        .expect("stack")
        .entries
        .into_iter()
        .find(|entry| entry.id == id)
        .expect("popup entry")
}

fn popup_painted_child(app: &TestApp, id: PopupId) -> NodeId {
    let content = popup_node(app, id);
    let window = win(app);
    app.app()
        .window(window)
        .expect("window")
        .tree()
        .get(content)
        .expect("popup content")
        .children()
        .first()
        .copied()
        .expect("popup content child")
}

fn painted_quad(app: &TestApp, node: NodeId) -> LogicalRect {
    app.rendered()
        .expect("rendered frame")
        .quads
        .iter()
        .find(|quad| quad.node == node)
        .map(|quad| quad.bounds)
        .expect("popup content quad must be painted")
}

fn scaled_about(rect: LogicalRect, factor: f32) -> LogicalRect {
    let center = rect_center(rect);
    LogicalRect::new(
        px(center.x.value() - rect.size.width.value() * factor / 2.0),
        px(center.y.value() - rect.size.height.value() * factor / 2.0),
        px(rect.size.width.value() * factor),
        px(rect.size.height.value() * factor),
    )
}

fn rect_center(rect: LogicalRect) -> LogicalPoint {
    point(
        (rect.origin.x + rect.size.width / 2.0).value(),
        (rect.origin.y + rect.size.height / 2.0).value(),
    )
}

fn assert_rect_close(actual: LogicalRect, expected: LogicalRect) {
    let close = |left: f32, right: f32| (left - right).abs() < 0.05;
    assert!(
        close(actual.origin.x.value(), expected.origin.x.value())
            && close(actual.origin.y.value(), expected.origin.y.value())
            && close(actual.size.width.value(), expected.size.width.value())
            && close(actual.size.height.value(), expected.size.height.value()),
        "绘制矩形不一致：actual={actual:?} expected={expected:?}"
    );
}

fn percentile(samples: &[Duration], percentile: f32) -> Duration {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() as f32 - 1.0) * percentile / 100.0).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn scaled_rect_about(rect: LogicalRect, pivot: LogicalPoint, factor: f32) -> LogicalRect {
    LogicalRect::new(
        px(pivot.x.value() + factor * (rect.origin.x - pivot.x).value()),
        px(pivot.y.value() + factor * (rect.origin.y - pivot.y).value()),
        px(rect.size.width.value() * factor),
        px(rect.size.height.value() * factor),
    )
}

#[test]
fn fade_scale_open_paints_the_panel_scaled_around_its_center() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0)));
    spec.animation = PopupAnimation::FadeScale;
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    let settled = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(settled, rect(100.0, 124.0, 120.0, 80.0));
    assert_eq!(app.app().popup_rect(id).expect("rect").a11y, settled);
    let center = point(160.0, 164.0);
    let child = popup_painted_child(&app, id);
    let entry = popup_snapshot(&app, id);
    assert_eq!(
        entry.paint_offset,
        LogicalPoint::ZERO,
        "FadeScale 只做 fade+scale"
    );
    assert!(
        (entry.paint_scale - 0.98).abs() < 0.001,
        "首帧（opacity 0）即处于 0.98：{}",
        entry.paint_scale
    );
    assert_eq!(
        app.app().popup_rect(id).expect("rect").hit,
        settled,
        "scale 不得改变 settle/hit/a11y rect"
    );
    assert_eq!(app.app().popup_rect(id).expect("rect").a11y, settled);
    assert!(!app.frame_record().damage_full, "open 缩放帧不得整帧重绘");

    app.advance(Duration::from_millis(6));
    frame(&mut app);
    let early = popup_snapshot(&app, id).paint_scale;
    assert!(
        early > 0.98 && early < 1.0,
        "首可见帧 scale 必须收敛：{early}"
    );
    let first = painted_quad(&app, child);
    assert_rect_close(first, scaled_about(settled, early));
    let first_center = rect_center(first);
    assert!(
        (first_center.x - center.x).value().abs() < 0.05
            && (first_center.y - center.y).value().abs() < 0.05,
        "缩放中心必须保持 popup 中心：{first_center:?} vs {center:?}"
    );
    let hit = app
        .probe()
        .hit(point(
            settled.origin.x.value() + 1.0,
            settled.origin.y.value() + 1.0,
        ))
        .map(|result| result.node)
        .expect("稳定 rect 内必须可命中");
    assert_ne!(hit, root_node(&app), "命中不得跟随绘制缩放");
    assert!(damage_covers(&app, settled), "damage 必须覆盖缩放外延");
    assert!(!app.frame_record().damage_full, "首可见缩放帧不得整帧重绘");

    app.advance(Duration::from_millis(24));
    frame(&mut app);
    let mid = popup_snapshot(&app, id).paint_scale;
    assert!(mid > 0.98 && mid < 1.0, "中段 scale 必须收敛：{mid}");
    let mid_quad = painted_quad(&app, child);
    assert_rect_close(mid_quad, scaled_about(settled, mid));
    let mid_center = rect_center(mid_quad);
    assert!(
        (mid_center.x - center.x).value().abs() < 0.05
            && (mid_center.y - center.y).value().abs() < 0.05,
        "动画中段中心必须不变：{mid_center:?}"
    );
    assert_eq!(app.app().popup_rect(id).expect("rect").hit, settled);
    assert!(damage_covers(&app, settled));
    assert!(!app.frame_record().damage_full);

    app.advance(Duration::from_millis(120));
    frame(&mut app);
    assert_eq!(popup_snapshot(&app, id).paint_scale, 1.0);
    assert_eq!(
        painted_quad(&app, child),
        settled,
        "动画结束后绘制回到稳定 rect"
    );
    assert_eq!(app.app().popup_state(id), Some(PopupState::Open));
}

#[test]
fn fade_scale_close_reverses_the_scale_and_keeps_damage_local() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0)));
    spec.animation = PopupAnimation::FadeScale;
    let id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    app.advance(Duration::from_millis(130));
    frame(&mut app);
    assert_eq!(popup_snapshot(&app, id).paint_scale, 1.0);
    let settled = app.app().popup_rect(id).expect("rect").hit;
    let child = popup_painted_child(&app, id);
    assert!(
        app.app_mut()
            .begin_close_popup(id, DismissReason::Programmatic)
            .expect("begin close")
    );
    assert_eq!(app.app().popup_state(id), Some(PopupState::Closing));
    app.advance(Duration::from_millis(45));
    frame(&mut app);
    let scale = popup_snapshot(&app, id).paint_scale;
    assert!(scale > 0.98 && scale < 1.0, "关闭必须 1→0.98 反向：{scale}");
    let quad = painted_quad(&app, child);
    assert_rect_close(quad, scaled_about(settled, scale));
    let center = rect_center(settled);
    let quad_center = rect_center(quad);
    assert!(
        (quad_center.x - center.x).value().abs() < 0.05
            && (quad_center.y - center.y).value().abs() < 0.05,
        "关闭动画中心必须不变：{quad_center:?} vs {center:?}"
    );
    assert!(
        damage_covers(&app, settled),
        "关闭帧 damage 必须覆盖稳定 rect"
    );
    assert!(!app.frame_record().damage_full, "关闭缩放帧不得整帧重绘");
    app.advance(Duration::from_millis(60));
    frame(&mut app);
    assert!(app.app().popup_rect(id).is_none(), "关闭动画结束后必须移除");
    assert!(!app.frame_record().damage_full, "关闭移除帧不得整帧重绘");
    assert!(damage_covers(&app, settled));
}

#[test]
fn fade_scale_paints_the_same_logical_geometry_at_dpr_one_and_two() {
    let mut painted = Vec::new();
    for factor in [1.0_f32, 2.0] {
        let dpr = DevicePixelRatio::new(factor).expect("dpr");
        let mut app = TestApp::with_scale(dpr);
        let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0)));
        spec.animation = PopupAnimation::FadeScale;
        let id = open_sized(&mut app, spec, (120.0, 80.0), None);
        frame(&mut app);
        app.advance(Duration::from_millis(30));
        frame(&mut app);
        let settled = app.app().popup_rect(id).expect("rect").hit;
        let scale = popup_snapshot(&app, id).paint_scale;
        assert!(scale > 0.98 && scale < 1.0);
        let child = popup_painted_child(&app, id);
        let quad = painted_quad(&app, child);
        assert_rect_close(quad, scaled_about(settled, scale));
        let dump = ui_core::test_support::diagnostics::frame_dump(&app);
        let expected = [
            quad.origin.x.value() * factor,
            quad.origin.y.value() * factor,
            quad.size.width.value() * factor,
            quad.size.height.value() * factor,
        ];
        let matched = dump.instances.iter().any(|instance| {
            (0..4).all(|index| (instance.bounds[index] - expected[index]).abs() <= 1.5)
        });
        assert!(
            matched,
            "DPR {factor} 必须把缩放后的逻辑矩形落到对应物理像素：{expected:?} in {:?}",
            dump.instances
                .iter()
                .map(|instance| instance.bounds)
                .collect::<Vec<_>>()
        );
        painted.push(quad);
    }
    assert_eq!(
        painted[0], painted[1],
        "缩放变换是逻辑空间变换，逻辑绘制几何必须与 DPR 无关"
    );
}

#[test]
fn modal_fade_scale_is_fade_only() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    spec.tier = PopupTier::Modal;
    spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    spec.animation = PopupAnimation::FadeScale;
    let id = open_sized(&mut app, spec, (200.0, 120.0), None);
    frame(&mut app);
    app.advance(Duration::from_millis(30));
    frame(&mut app);
    assert_eq!(
        popup_snapshot(&app, id).paint_scale,
        1.0,
        "Modal 只允许 fade，不得 scale"
    );
}

#[test]
fn fade_scale_coexists_with_fade_and_fade_slide() {
    let mut app = TestApp::new();
    let mut root_spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0)));
    root_spec.animation = PopupAnimation::FadeSlide;
    let root = open_sized(&mut app, root_spec, (120.0, 80.0), None);
    frame(&mut app);
    let mut child_spec = base_spec(Anchor::WindowRect(rect(300.0, 100.0, 40.0, 20.0)));
    child_spec.parent = Some(root);
    child_spec.placement = menu_placement();
    child_spec.animation = PopupAnimation::FadeScale;
    let child = open_sized(&mut app, child_spec, (80.0, 60.0), None);
    let mut hint_spec = base_spec(Anchor::WindowRect(rect(500.0, 100.0, 30.0, 20.0)));
    hint_spec.tier = PopupTier::Hint;
    hint_spec.dismiss = DismissPolicy::Hint { escape: true };
    hint_spec.animation = PopupAnimation::Fade;
    let hint = open_sized(&mut app, hint_spec, (60.0, 24.0), None);
    frame(&mut app);
    let root_entry = popup_snapshot(&app, root);
    let child_entry = popup_snapshot(&app, child);
    let hint_entry = popup_snapshot(&app, hint);
    assert!(
        root_entry.paint_offset.y > px(0.0),
        "FadeSlide 必须仍有 slide 偏移：{:?}",
        root_entry.paint_offset
    );
    assert!(
        child_entry.paint_scale < 1.0,
        "FadeScale 子层必须有缩放：{}",
        child_entry.paint_scale
    );
    assert_eq!(hint_entry.paint_scale, 1.0, "Fade 不得被 scale 污染");
    assert!(
        !app.frame_record().damage_full,
        "三类动画共存帧不得整帧重绘"
    );
    app.advance(Duration::from_millis(130));
    frame(&mut app);
    assert_eq!(popup_snapshot(&app, root).paint_offset, LogicalPoint::ZERO);
    assert_eq!(popup_snapshot(&app, child).paint_scale, 1.0);
    assert_eq!(popup_snapshot(&app, hint).paint_scale, 1.0);
    assert_eq!(app.app().popup_state(child), Some(PopupState::Open));
}

#[test]
fn begin_close_popup_forwards_an_explicit_reason_to_the_dismiss_handler() {
    let mut app = TestApp::new();
    let reasons = std::rc::Rc::new(std::cell::Cell::new(None));
    let recorded = std::rc::Rc::clone(&reasons);
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0))),
        (120.0, 80.0),
        Some(Box::new(move |_app, dismiss| {
            recorded.set(Some(dismiss.reason));
            DismissOutcome::Close
        })),
    );
    frame(&mut app);
    assert!(
        app.app_mut()
            .begin_close_popup(id, DismissReason::AnchorLost)
            .expect("begin close")
    );
    assert_eq!(
        reasons.get(),
        Some(DismissReason::AnchorLost),
        "begin_close_popup 必须把调用方 reason 原样交给 on_dismiss"
    );
    frame(&mut app);
    assert!(app.app().popup_rect(id).is_none(), "无动画时关闭立即生效");
}

#[test]
fn idle_frames_do_no_popup_settle_or_damage_work() {
    let mut app = TestApp::new();
    let window = win(&app);
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0)));
    spec.animation = PopupAnimation::FadeScale;
    let _id = open_sized(&mut app, spec, (120.0, 80.0), None);
    frame(&mut app);
    app.advance(Duration::from_millis(130));
    frame(&mut app);
    assert!(
        !app.app().animations_running(window),
        "稳定后不得残留动画源"
    );
    for _ in 0..8 {
        frame(&mut app);
        let record = app.frame_record().clone();
        assert!(!record.rendered, "稳定后必须是空闲帧");
        assert!(record.damage.is_empty(), "空闲帧不得产生 damage");
        assert!(!record.damage_full, "空闲帧不得整帧重绘");
        assert_eq!(
            record.stats.popup_animation_records, 0,
            "空闲帧的 popup 动画 tick 记录数必须为 0（tick 真实回填，非默认值）"
        );
        assert!(
            !record.stats.popup_opening && !record.stats.popup_closing,
            "空闲帧不得有 open/close popup 动画工作（tick 真实回填）"
        );
        assert_eq!(
            record.stats.popup_settle_rounds, 0,
            "空闲帧的 settle 轮数必须为 0（settle.rs 真实回填，非默认构造）"
        );
        assert!(!record.stats.popup_unsettled);
        assert!(
            record.stats.frame_cpu.is_some(),
            "空闲帧也必须记录 CPU 测量入口"
        );
        app.advance(Duration::from_millis(16));
    }
}

#[test]
fn popup_open_and_close_frames_stay_within_the_frame_budget() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0)));
    spec.animation = PopupAnimation::FadeScale;
    let id = open_sized(&mut app, spec, (240.0, 160.0), None);
    let mut open_frames = Vec::new();
    for index in 0..10 {
        frame(&mut app);
        if app.frame_record().stats.popup_opening {
            if index == 0 {
                assert!(
                    app.frame_record().stats.popup_animation_records > 0,
                    "open 帧的动画 tick 记录数必须真实回填"
                );
            }
            open_frames.push(app.frame_record().stats.frame_cpu.expect("open frame cpu"));
        }
        app.advance(Duration::from_millis(12));
    }
    frame(&mut app);
    assert_eq!(app.app().popup_state(id), Some(PopupState::Open));
    let mut idle_frames = Vec::new();
    for _ in 0..12 {
        frame(&mut app);
        assert!(!app.frame_record().rendered, "稳定后必须是空闲帧");
        idle_frames.push(app.frame_record().stats.frame_cpu.expect("idle frame cpu"));
        app.advance(Duration::from_millis(8));
    }
    assert!(
        app.app_mut()
            .begin_close_popup(id, DismissReason::Programmatic)
            .expect("begin close")
    );
    let mut close_frames = Vec::new();
    for _ in 0..12 {
        app.advance(Duration::from_millis(9));
        frame(&mut app);
        if app.frame_record().stats.popup_closing {
            close_frames.push(app.frame_record().stats.frame_cpu.expect("close frame cpu"));
        }
        if app.app().popup_rect(id).is_none() {
            break;
        }
    }
    assert!(!open_frames.is_empty(), "必须采样到 open 帧");
    assert!(!close_frames.is_empty(), "必须采样到 close 帧");
    let open_median = percentile(&open_frames, 50.0);
    let open_p95 = percentile(&open_frames, 95.0);
    let close_median = percentile(&close_frames, 50.0);
    let close_p95 = percentile(&close_frames, 95.0);
    let idle_p95 = percentile(&idle_frames, 95.0);
    let budget = idle_p95 + Duration::from_millis(2);
    assert!(
        open_median <= budget,
        "popup open 中位帧 {open_median:?} 超出 idle p95 {idle_p95:?} + 2ms（样本 {open_frames:?}）"
    );
    assert!(
        close_median <= budget,
        "popup close 中位帧 {close_median:?} 超出 idle p95 {idle_p95:?} + 2ms（样本 {close_frames:?}）"
    );
    println!(
        "popup frame budget: idle_p95={idle_p95:?} open_median={open_median:?} open_p95={open_p95:?} \
         close_median={close_median:?} close_p95={close_p95:?} \
         open_samples={open_frames:?} close_samples={close_frames:?} \
         idle_samples={idle_frames:?}（open 首帧含 atlas/text 冷启动，故用中位数守卫设计目标 idle p95 + 2ms，p95 仅记录）"
    );
}

#[test]
fn fade_scale_scales_canvas_content_about_the_popup_center() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0)));
    spec.animation = PopupAnimation::FadeScale;
    let window = win(&app);
    let id = app
        .app_mut()
        .open_popup(
            window,
            spec,
            move |app, window, node| {
                let mut canvas =
                    Canvas::new(LogicalRect::new(px(0.0), px(0.0), px(60.0), px(40.0)));
                canvas.push_quad(Quad {
                    bounds: LogicalRect::new(px(10.0), px(8.0), px(30.0), px(20.0)),
                    corner_radius: [px(0.0); 4],
                    paint: QuadPaint::Solid {
                        fill: Color::WHITE,
                        border: None,
                    },
                });
                NodeBuilder::canvas(canvas)
                    .style(StyleBuilder::new().size(px(60.0), px(40.0)).build())
                    .build(app.window_mut(window).expect("window").tree_mut(), node)?;
                Ok(())
            },
            None,
        )
        .expect("open canvas popup");
    frame(&mut app);
    app.advance(Duration::from_millis(30));
    frame(&mut app);
    let settled = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(settled, rect(100.0, 124.0, 60.0, 40.0));
    let scale = popup_snapshot(&app, id).paint_scale;
    assert!(scale > 0.98 && scale < 1.0, "scale 必须收敛：{scale}");
    let canvas_node = popup_painted_child(&app, id);
    let quad_world = LogicalRect::new(px(110.0), px(132.0), px(30.0), px(20.0));
    let expected = scaled_about(quad_world, scale);
    let recorded = app
        .rendered()
        .expect("rendered frame")
        .primitives
        .iter()
        .find(|primitive| primitive.node == canvas_node && primitive.kind == PrimitiveKind::Quad)
        .map(|primitive| primitive.bounds)
        .expect("canvas content quad must be recorded");
    assert_rect_close(recorded, expected);
    let center = rect_center(settled);
    assert!(
        (center.x - px(130.0)).value().abs() < 0.05 && (center.y - px(144.0)).value().abs() < 0.05,
        "Canvas popup 中心保持稳定 rect 中心"
    );
    assert!(!app.frame_record().damage_full);
}

#[test]
fn fade_scale_scales_the_shadow_offset_as_a_vector() {
    let mut app = TestApp::new();
    let window = win(&app);
    let mut spec = base_spec(Anchor::WindowRect(rect(576.5, 450.0, 0.0, 0.0)));
    spec.animation = PopupAnimation::FadeScale;
    let id = app
        .app_mut()
        .open_popup(
            window,
            spec,
            move |app, window, node| {
                NodeBuilder::boxed()
                    .style(StyleBuilder::new().size(px(200.0), px(100.0)).build())
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        shadows: Shadows::new([ShadowSpec {
                            offset: point(0.0, 13.0),
                            blur: px(8.0),
                            spread: px(2.0),
                            color: Color::BLACK,
                        }]),
                        ..PaintSpec::default()
                    })
                    .build(app.window_mut(window).expect("window").tree_mut(), node)?;
                Ok(())
            },
            None,
        )
        .expect("open shadow popup");
    frame(&mut app);
    app.advance(Duration::from_millis(30));
    frame(&mut app);
    let settled = app.app().popup_rect(id).expect("rect").hit;
    assert_eq!(settled, rect(576.0, 454.0, 200.0, 100.0));
    let pivot = point(676.0, 504.0);
    assert!(
        (rect_center(settled).x - pivot.x).value().abs() < 0.05
            && (rect_center(settled).y - pivot.y).value().abs() < 0.05,
        "测试场景的 pivot 必须是 (676,504)：{:?}",
        rect_center(settled)
    );
    let scale = popup_snapshot(&app, id).paint_scale;
    assert!(
        (scale - 0.991_562_5).abs() < 1e-5,
        "30ms 处 scale 必须是 0.9915625：{scale}"
    );
    let child = popup_painted_child(&app, id);
    let shadow_rect = settled.outset(px(2.0));
    let shadow_offset = point(0.0, 13.0);
    let scaled_rect = scaled_rect_about(shadow_rect, pivot, scale);
    let expected_origin = point(
        scaled_rect.origin.x.value() + scale * shadow_offset.x.value(),
        scaled_rect.origin.y.value() + scale * shadow_offset.y.value(),
    );
    let expected = LogicalRect::new(
        expected_origin.x,
        expected_origin.y,
        scaled_rect.size.width,
        scaled_rect.size.height,
    );
    let recorded = app
        .rendered()
        .expect("rendered frame")
        .primitives
        .iter()
        .find(|primitive| primitive.node == child && primitive.kind == PrimitiveKind::Shadow)
        .map(|primitive| primitive.bounds)
        .expect("shadow primitive must be recorded");
    assert_rect_close(recorded, scaled_rect);
    let buggy_offset = point(
        pivot.x.value() + scale * (shadow_offset.x - pivot.x).value(),
        pivot.y.value() + scale * (shadow_offset.y - pivot.y).value(),
    );
    assert!(
        (expected.origin.y - (scaled_rect.origin.y + buggy_offset.y))
            .value()
            .abs()
            > 2.0,
        "回归守卫：把阴影 offset 当点变换会产生 >2px 的漂移"
    );
    let dump = ui_core::test_support::diagnostics::frame_dump(&app);
    let expected_device = [
        expected.origin.x.value(),
        expected.origin.y.value(),
        expected.size.width.value(),
        expected.size.height.value(),
    ];
    let matched = dump.instances.iter().any(|instance| {
        instance.kind == 3u32
            && (0..4).all(|index| (instance.bounds[index] - expected_device[index]).abs() <= 1.5)
    });
    assert!(
        matched,
        "阴影 GPU 实例必须等于 f*offset + pivot 变换（DPR1，pivot 676,504，f={scale}）：期望 {expected_device:?}，实际 {:?}",
        dump.instances
            .iter()
            .filter(|instance| instance.kind == 3u32)
            .map(|instance| instance.bounds)
            .collect::<Vec<_>>()
    );
    assert!(!app.frame_record().damage_full);
}

#[test]
fn fade_scale_modal_scrim_is_fade_only() {
    let mut app = TestApp::new();
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 20.0, 20.0)));
    spec.tier = PopupTier::Modal;
    spec.focus = FocusPolicy::Trap {
        restore: RestoreFocus::Opener,
        initial: InitialFocus::Container,
    };
    spec.scrim = Some(ScrimSpec {
        density: 0.5,
        ..ScrimSpec::default()
    });
    spec.animation = PopupAnimation::FadeScale;
    let modal = open_sized(&mut app, spec, (200.0, 120.0), None);
    frame(&mut app);
    let window = win(&app);
    let scrim = app
        .app()
        .popup_stack(window)
        .expect("stack")
        .entries
        .iter()
        .find(|entry| entry.tier == PopupTier::Scrim)
        .map(|entry| entry.id)
        .expect("scrim entry");
    app.advance(Duration::from_millis(30));
    frame(&mut app);
    assert_eq!(
        popup_snapshot(&app, modal).paint_scale,
        1.0,
        "modal 的 FadeScale 必须退化为 fade-only"
    );
    assert_eq!(
        popup_snapshot(&app, scrim).paint_scale,
        1.0,
        "打开期间 scrim 不得被缩放"
    );
    assert!(
        app.app_mut()
            .begin_close_popup(modal, DismissReason::Programmatic)
            .expect("begin close")
    );
    app.advance(Duration::from_millis(45));
    frame(&mut app);
    assert_eq!(
        popup_snapshot(&app, modal).paint_scale,
        1.0,
        "关闭帧 modal 不得缩放"
    );
    assert_eq!(
        popup_snapshot(&app, scrim).paint_scale,
        1.0,
        "关闭帧 scrim 不得缩放（FadeScale 对 Scrim 必须 fade-only）"
    );
    app.advance(Duration::from_millis(60));
    frame(&mut app);
    assert!(
        app.app().popup_rect(scrim).is_none(),
        "关闭动画结束后 scrim 必须移除"
    );
}

fn open_glyph_image_popup(app: &mut TestApp, animation: PopupAnimation) -> PopupId {
    let window = win(app);
    let mut spec = base_spec(Anchor::WindowRect(rect(100.0, 100.0, 60.0, 20.0)));
    spec.animation = animation;
    app.app_mut()
        .open_popup(
            window,
            spec,
            move |app, window, node| {
                {
                    let tree = app.window_mut(window).expect("window").tree_mut();
                    NodeBuilder::text("Scale", ui_core::text::TextStyle::new(px(16.0)))
                        .style(StyleBuilder::new().width(px(90.0)).build())
                        .build(tree, node)?;
                }
                let mut canvas =
                    Canvas::new(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(30.0)));
                canvas.push_image(PaintImage {
                    bounds: LogicalRect::new(px(4.0), px(6.0), px(20.0), px(10.0)),
                    tile: AtlasTile {
                        atlas: AtlasKind::Image,
                        page: 0,
                        u0: 0.0,
                        v0: 0.0,
                        u1: 0.5,
                        v1: 0.5,
                        generation: 1,
                        slot: 0,
                        slot_epoch: 0,
                    },
                    modulation: Color::WHITE,
                });
                NodeBuilder::canvas(canvas)
                    .style(StyleBuilder::new().size(px(40.0), px(30.0)).build())
                    .build(app.window_mut(window).expect("window").tree_mut(), node)?;
                Ok(())
            },
            None,
        )
        .expect("open glyph/image popup")
}

#[test]
fn fade_scale_scales_glyph_runs_and_images_in_animation_frames() {
    let mut baseline_app = TestApp::new();
    let baseline = open_glyph_image_popup(&mut baseline_app, PopupAnimation::None);
    frame(&mut baseline_app);
    let settled = baseline_app.app().popup_rect(baseline).expect("rect").hit;
    let glyph_node = baseline_app
        .rendered()
        .expect("baseline frame")
        .primitives
        .iter()
        .find(|primitive| primitive.kind == PrimitiveKind::GlyphRun)
        .map(|primitive| (primitive.node, primitive.bounds))
        .expect("baseline glyph run");
    let image_node = baseline_app
        .rendered()
        .expect("baseline frame")
        .primitives
        .iter()
        .find(|primitive| primitive.kind == PrimitiveKind::Image)
        .map(|primitive| (primitive.node, primitive.bounds))
        .expect("baseline image");

    let mut app = TestApp::new();
    let id = open_glyph_image_popup(&mut app, PopupAnimation::FadeScale);
    frame(&mut app);
    app.advance(Duration::from_millis(30));
    frame(&mut app);
    assert_eq!(app.app().popup_rect(id).expect("rect").hit, settled);
    let scale = popup_snapshot(&app, id).paint_scale;
    assert!(scale > 0.98 && scale < 1.0, "scale 必须收敛：{scale}");
    let pivot = rect_center(settled);
    let rendered = app.rendered().expect("scaled frame");
    let glyph = rendered
        .primitives
        .iter()
        .find(|primitive| {
            primitive.node == glyph_node.0 && primitive.kind == PrimitiveKind::GlyphRun
        })
        .map(|primitive| primitive.bounds)
        .expect("scaled glyph run");
    let image = rendered
        .primitives
        .iter()
        .find(|primitive| primitive.node == image_node.0 && primitive.kind == PrimitiveKind::Image)
        .map(|primitive| primitive.bounds)
        .expect("scaled image");
    assert_rect_close(glyph, scaled_rect_about(glyph_node.1, pivot, scale));
    assert_rect_close(image, scaled_rect_about(image_node.1, pivot, scale));
    assert!(
        glyph.size.width < glyph_node.1.size.width && glyph.size.height < glyph_node.1.size.height,
        "GlyphRun 尺寸必须随 scale 收缩：{glyph:?} vs {:?}",
        glyph_node.1
    );
    assert!(
        image.size.width < image_node.1.size.width,
        "Image 尺寸必须随 scale 收缩：{image:?} vs {:?}",
        image_node.1
    );
    assert!(!app.frame_record().damage_full, "scale 动画帧不得整帧重绘");
}

#[test]
fn an_ignored_outside_press_is_consumed_and_never_reaches_the_background() {
    let mut app = TestApp::new();
    let _background = root_button(&mut app, (300.0, 300.0), "background");
    frame(&mut app);
    let hits = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = std::rc::Rc::clone(&hits);
    app.app_mut()
        .on_action(Action::custom("background"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(600.0, 500.0, 20.0, 20.0))),
        (60.0, 30.0),
        Some(Box::new(|_app, dismiss| {
            assert_eq!(dismiss.reason, DismissReason::OutsidePress);
            DismissOutcome::Ignore
        })),
    );
    frame(&mut app);
    let center = point(100.0, 100.0);
    let (_, down_consumed) = press(&mut app, center);
    assert!(
        down_consumed,
        "safe triangle/grace 内 Ignore 必须 Consume，不得把按下 replay 给背景"
    );
    let (_, up_consumed) = release(&mut app, center);
    assert!(up_consumed, "Ignore 的按下必须同时吞掉抬起");
    assert_eq!(hits.get(), 0, "背景控件不得收到 Ignore 的 click");
    assert!(app.app().popup_rect(id).is_some(), "Ignore 不得关闭 popup");
}

#[test]
fn an_ignored_outside_press_does_not_drop_the_popup_or_leave_it_stuck() {
    let mut app = TestApp::new();
    let _background = root_button(&mut app, (300.0, 300.0), "background");
    frame(&mut app);
    let id = open_sized(
        &mut app,
        base_spec(Anchor::WindowRect(rect(600.0, 500.0, 20.0, 20.0))),
        (60.0, 30.0),
        Some(Box::new(|_app, dismiss| {
            if dismiss.reason == DismissReason::OutsidePress {
                DismissOutcome::Ignore
            } else {
                DismissOutcome::Close
            }
        })),
    );
    frame(&mut app);
    let center = point(100.0, 100.0);
    press(&mut app, center);
    release(&mut app, center);
    assert_eq!(
        app.app().popup_state(id),
        Some(PopupState::Open),
        "Ignore 后 popup 必须保持 Open 且可继续交互"
    );
    assert!(
        app.app_mut()
            .begin_close_popup(id, DismissReason::Programmatic)
            .expect("close")
    );
    frame(&mut app);
    assert!(
        app.app().popup_rect(id).is_none(),
        "Ignore 不得破坏后续正常关闭"
    );
}

#[test]
fn a_non_modal_take_popup_tabs_out_to_the_main_tree_instead_of_wrapping() {
    let mut app = TestApp::new();
    let window = win(&app);
    let first = root_button(&mut app, (100.0, 30.0), "first");
    let second = root_button(&mut app, (100.0, 30.0), "second");
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(first)));
    let mut spec = base_spec(Anchor::WindowRect(rect(400.0, 400.0, 20.0, 20.0)));
    spec.focus = FocusPolicy::Take {
        restore: RestoreFocus::Previous,
    };
    let popup = app
        .app_mut()
        .open_popup(
            window,
            spec,
            |app, window, node| {
                for action in ["a", "b"] {
                    NodeBuilder::boxed()
                        .style(StyleBuilder::new().size(px(100.0), px(30.0)).build())
                        .on_click(Action::custom(action))
                        .build(app.window_mut(window).expect("window").tree_mut(), node)?;
                }
                Ok(())
            },
            None,
        )
        .expect("take popup");
    frame(&mut app);
    let a = app.app().focused(window).expect("take focus");
    assert_ne!(a, first, "Take 必须取走主树焦点");
    app.app_mut().focus_next(window);
    let b = app.app().focused(window).expect("second popup focusable");
    assert_ne!(b, a);
    app.app_mut().focus_next(window);
    assert_eq!(
        app.app().focused(window),
        Some(second),
        "非 Trap popup 的 Tab 到边界必须跳到主树下一个可聚焦（打开前焦点之后）"
    );
    assert_eq!(
        app.app().popup_state(popup),
        Some(PopupState::Open),
        "Tab 跳出不得关闭 popup"
    );
    app.app_mut().focus_prev(window);
    assert_eq!(
        app.app().focused(window),
        Some(first),
        "焦点已在主树后 Shift+Tab 继续主树顺序"
    );
}

#[test]
fn a_non_modal_take_popup_shift_tab_escapes_backwards() {
    let mut app = TestApp::new();
    let window = win(&app);
    let first = root_button(&mut app, (100.0, 30.0), "first");
    let second = root_button(&mut app, (100.0, 30.0), "second");
    frame(&mut app);
    assert!(app.app_mut().set_focus(window, Some(first)));
    let mut spec = base_spec(Anchor::WindowRect(rect(400.0, 400.0, 20.0, 20.0)));
    spec.focus = FocusPolicy::Take {
        restore: RestoreFocus::Previous,
    };
    let popup = app
        .app_mut()
        .open_popup(
            window,
            spec,
            |app, window, node| {
                for action in ["a", "b"] {
                    NodeBuilder::boxed()
                        .style(StyleBuilder::new().size(px(100.0), px(30.0)).build())
                        .on_click(Action::custom(action))
                        .build(app.window_mut(window).expect("window").tree_mut(), node)?;
                }
                Ok(())
            },
            None,
        )
        .expect("take popup");
    frame(&mut app);
    let a = app.app().focused(window).expect("take focus");
    assert_ne!(a, first, "Take 必须取走主树焦点");
    app.app_mut().focus_prev(window);
    assert_eq!(
        app.app().focused(window),
        Some(second),
        "Shift+Tab 必须离开 popup scope，落到主树中打开前焦点之前的下一个可聚焦（环绕到 second）"
    );
    assert_eq!(
        app.app().popup_state(popup),
        Some(PopupState::Open),
        "Shift+Tab 跳出不得关闭 popup"
    );
}

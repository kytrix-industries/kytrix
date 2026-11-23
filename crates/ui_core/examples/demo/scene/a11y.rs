use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yAction, A11yActionData, A11yOverride, A11yPatch, A11yRole},
    app::App,
    event::Action,
    math::Rect,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        primitive::Border,
        style::{Cursor, PaintSpec, StylePatch},
    },
    window::WindowId,
};

use super::{
    ACCENT_RGBA, DIM_RGBA, MUTED_RGBA, RAIL_RGBA, TEXT_RGBA, card, color, ring, text_style,
};

const RADIOS: [&str; 2] = ["选项 A", "选项 B"];

const RADIO_ACTIONS: [&str; 2] = ["a11y.radio.a", "a11y.radio.b"];

const SLIDER_SEGMENTS: u8 = 10;

const SEGMENT_WIDTH: f32 = 24.0;
const SEGMENT_GAP: f32 = 3.0;
const SEGMENT_HEIGHT: f32 = 16.0;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct A11yState {
    radio: u8,
    scale: u8,
    last_action: SmolStr,
}

impl Default for A11yState {
    fn default() -> Self {
        Self {
            radio: 0,
            scale: 6,
            last_action: SmolStr::new_static("—"),
        }
    }
}

impl A11yState {
    pub(crate) fn sample() -> Self {
        Self {
            radio: 1,
            scale: 7,
            last_action: SmolStr::new_static("SetValue → 70%"),
        }
    }

    pub(crate) fn select_radio(&mut self, index: u8, label: &str) {
        self.radio = index;
        self.last_action = format_smolstr!("Click → {label}");
    }

    pub(crate) fn next_scale(&mut self) {
        self.scale = if self.scale >= SLIDER_SEGMENTS {
            1
        } else {
            self.scale + 1
        };
        self.last_action = format_smolstr!("Click → {}%", self.scale * 10);
    }

    pub(crate) fn step_scale(&mut self, step: i8) {
        let next = (i16::from(self.scale) + i16::from(step)).clamp(1, i16::from(SLIDER_SEGMENTS));
        self.scale = u8::try_from(next).unwrap_or(self.scale);
        self.last_action = format_smolstr!("方向键 → {}%", self.scale * 10);
    }

    pub(crate) fn set_scale_from_text(&mut self, text: &str) -> bool {
        let trimmed = text.trim().trim_end_matches('%');
        let Ok(percent) = trimmed.parse::<u16>() else {
            return false;
        };
        let scale = (percent / 10).clamp(1, u16::from(SLIDER_SEGMENTS));
        self.scale = u8::try_from(scale).unwrap_or(self.scale);
        self.last_action = format_smolstr!("SetValue → {}%", self.scale * 10);
        true
    }

    pub(crate) fn scale_line(&self) -> SmolStr {
        format_smolstr!("缩放 {}%", self.scale * 10)
    }

    pub(crate) fn status_line(&self) -> SmolStr {
        format_smolstr!("最近 AT 动作：{}", self.last_action)
    }
}

pub(crate) fn build(
    app: &mut App,
    window: WindowId,
    panel: NodeId,
    state: Signal<A11yState>,
) -> Controls {
    let radios = [
        radio_row(app, window, panel, 40.0, 0, state),
        radio_row(app, window, panel, 70.0, 1, state),
    ];
    let slider = slider(app, window, panel, state);
    let _ = card::bound_label(
        app,
        window,
        panel,
        Rect::new(18.0, 146.0, 276.0, 18.0),
        text_style(11.5, 400, MUTED_RGBA),
        move |cx| cx.read(&state).status_line(),
    );
    let _ = card::label(
        app,
        window,
        panel,
        Rect::new(18.0, 166.0, 276.0, 16.0),
        "角色 / 名称 / 选中态 / 数值可被 UIA 读取",
        text_style(11.0, 400, DIM_RGBA),
    );
    Controls { radios, slider }
}

fn radio_row(
    app: &mut App,
    window: WindowId,
    panel: NodeId,
    y: f32,
    index: u8,
    state: Signal<A11yState>,
) -> NodeId {
    let label = RADIOS[usize::from(index)];
    let row = NodeBuilder::boxed()
        .style(card::absolute_style(Rect::new(18.0, y, 276.0, 24.0)))
        .on_click(Action::custom(RADIO_ACTIONS[usize::from(index)]))
        .focus_ring(ring())
        .a11y(A11yOverride {
            role: Some(A11yRole::RadioButton),
            label: Some(SmolStr::new_static(label)),
            actions: Some(vec![A11yAction::Click, A11yAction::Focus]),
            ..A11yOverride::default()
        })
        .bind_style(|_cx| StylePatch {
            cursor: Some(Cursor::Pointer),
            ..StylePatch::default()
        })
        .bind_a11y(move |cx| A11yPatch {
            checked: Some(cx.read(&state).radio == index),
            ..A11yPatch::default()
        })
        .build(card::tree(app, window), panel)
        .expect("单选行挂载");

    let _ = NodeBuilder::boxed()
        .layout_override(Rect::new(2.0, 5.0, 14.0, 14.0))
        .paint(PaintSpec {
            border: Some(Border {
                width: 1.5,
                color: color(RAIL_RGBA),
            }),
            corner_radius: [7.0; 4],
            ..PaintSpec::default()
        })
        .build(card::tree(app, window), row)
        .expect("单选圆环挂载");

    let _ = NodeBuilder::boxed()
        .layout_override(Rect::new(5.0, 8.0, 8.0, 8.0))
        .paint(PaintSpec {
            background: color(ACCENT_RGBA),
            corner_radius: [4.0; 4],
            ..PaintSpec::default()
        })
        .bind_style(move |cx| StylePatch {
            visible: Some(cx.read(&state).radio == index),
            ..StylePatch::default()
        })
        .build(card::tree(app, window), row)
        .expect("单选圆点挂载");

    let _ = card::hidden_label(
        app,
        window,
        row,
        Rect::new(26.0, 2.0, 236.0, 20.0),
        label,
        text_style(13.0, 400, TEXT_RGBA),
    );
    row
}

fn slider(app: &mut App, window: WindowId, panel: NodeId, state: Signal<A11yState>) -> NodeId {
    let slider = NodeBuilder::boxed()
        .style(card::absolute_style(Rect::new(18.0, 98.0, 276.0, 42.0)))
        .on_click(Action::custom("a11y.slider.next"))
        .focus_ring(ring())
        .a11y(A11yOverride {
            role: Some(A11yRole::Slider),
            label: Some(SmolStr::new_static("缩放比例")),
            description: Some(SmolStr::new_static("点击循环取值，方向键微调")),
            actions: Some(vec![A11yAction::SetValue, A11yAction::Focus]),
            ..A11yOverride::default()
        })
        .bind_style(|_cx| StylePatch {
            cursor: Some(Cursor::Pointer),
            ..StylePatch::default()
        })
        .bind_a11y(move |cx| A11yPatch {
            value: Some(format_smolstr!("{}", cx.read(&state).scale * 10)),
            ..A11yPatch::default()
        })
        .build(card::tree(app, window), panel)
        .expect("滑块挂载");

    let _ = card::bound_hidden_label(
        app,
        window,
        slider,
        Rect::new(0.0, 0.0, 200.0, 18.0),
        text_style(12.5, 400, MUTED_RGBA),
        move |cx| cx.read(&state).scale_line(),
    );

    for index in 0..SLIDER_SEGMENTS {
        let _ = NodeBuilder::boxed()
            .layout_override(Rect::new(
                f32::from(index) * (SEGMENT_WIDTH + SEGMENT_GAP),
                22.0,
                SEGMENT_WIDTH,
                SEGMENT_HEIGHT,
            ))
            .paint(PaintSpec {
                background: color(RAIL_RGBA),
                corner_radius: [3.0; 4],
                ..PaintSpec::default()
            })
            .bind_style(move |cx| {
                let lit = cx.read(&state).scale > index;
                StylePatch {
                    background: Some(color(if lit { ACCENT_RGBA } else { RAIL_RGBA })),
                    ..StylePatch::default()
                }
            })
            .build(card::tree(app, window), slider)
            .expect("滑块分段挂载");
    }
    slider
}

pub(crate) fn install(app: &mut App, scene: &super::Scene) {
    let state = scene.a11y;
    for (index, action) in RADIO_ACTIONS.iter().enumerate() {
        let index = u8::try_from(index).expect("两个选项的下标必然适合 u8");
        app.on_action(Action::custom(action), move |ctx| {
            let mut next = (*ctx.read(&state)).clone();
            next.select_radio(index, RADIOS[usize::from(index)]);
            ctx.write(&state, next);
        });
    }
    app.on_action(Action::custom("a11y.slider.next"), move |ctx| {
        let mut next = (*ctx.read(&state)).clone();
        next.next_scale();
        ctx.write(&state, next);
    });

    let value_state = scene.a11y;
    let ime = scene.ime;
    let input = scene.ime_field;
    let slider = scene.slider;
    let registered = app.on_a11y_action(A11yAction::SetValue, move |ctx| {
        let A11yActionData::Value(text) = ctx.data() else {
            return;
        };
        if ctx.node() == input {
            let mut field = (*ctx.read(&ime)).clone();
            field.set_text(text);
            ctx.write(&ime, field);
        } else if ctx.node() == slider {
            let mut next = (*ctx.read(&value_state)).clone();
            if next.set_scale_from_text(text) {
                ctx.write(&value_state, next);
            }
        }
    });
    debug_assert!(
        registered,
        "SetValue 必须可注册（只有 Click 被引擎保留给 Action 表）"
    );
}

pub(crate) struct Controls {
    pub radios: [NodeId; 2],
    pub slider: NodeId,
}

#[cfg(test)]
mod tests {
    use super::A11yState;

    #[test]
    fn scale_text_accepts_percent_and_plain_numbers() {
        let mut state = A11yState::default();

        assert!(state.set_scale_from_text("70"));
        assert_eq!(state.scale_line().as_str(), "缩放 70%");
        assert!(state.set_scale_from_text(" 45% "));
        assert_eq!(state.scale_line().as_str(), "缩放 40%");
        assert!(!state.set_scale_from_text("abc"));
        assert_eq!(state.scale_line().as_str(), "缩放 40%", "非法输入不改状态");
    }

    #[test]
    fn scale_text_clamps_to_the_ten_segments() {
        let mut state = A11yState::default();

        assert!(state.set_scale_from_text("0"));
        assert_eq!(state.scale_line().as_str(), "缩放 10%");
        assert!(state.set_scale_from_text("999"));
        assert_eq!(state.scale_line().as_str(), "缩放 100%");
    }

    #[test]
    fn click_cycles_and_arrow_keys_clamp() {
        let mut state = A11yState::default();
        assert_eq!(state.scale, 6);

        state.next_scale();
        assert_eq!(state.scale_line().as_str(), "缩放 70%");
        assert_eq!(state.status_line().as_str(), "最近 AT 动作：Click → 70%");

        state.step_scale(-100);
        assert_eq!(state.scale, 1, "方向键钳制在 1，不回绕");
        state.step_scale(100);
        assert_eq!(state.scale, 10);
        state.next_scale();
        assert_eq!(state.scale, 1, "点击是循环的");
    }

    #[test]
    fn selecting_a_radio_updates_the_status() {
        let mut state = A11yState::default();

        state.select_radio(1, "选项 B");
        assert_eq!(state.radio, 1);
        assert_eq!(state.status_line().as_str(), "最近 AT 动作：Click → 选项 B");
    }

    #[test]
    fn sample_seeds_a_visible_state_for_the_snapshot() {
        let state = A11yState::sample();

        assert_eq!(state.radio, 1);
        assert_eq!(state.scale_line().as_str(), "缩放 70%");
        assert_eq!(state.status_line().as_str(), "最近 AT 动作：SetValue → 70%");
    }
}

#[cfg(all(test, feature = "test-support"))]
mod showcase_tests {
    use ui_core::{
        a11y::{A11yAction, A11yRole},
        event::{Event, KeyCode},
    };

    use crate::scene::{
        dispatch_event,
        test_support::{frame, press, setup, setup_installed},
    };

    #[test]
    fn the_a11y_tree_exposes_roles_names_and_values() {
        let (mut test, _window, _scene) = setup();
        test.enable_a11y_capture();
        frame(&mut test);

        let snapshot = test.a11y().expect("捕获打开时首帧必有 a11y 交付");
        assert!(snapshot.full, "首笔必须是全树");

        let labels: Vec<(&str, A11yRole)> = snapshot
            .nodes()
            .iter()
            .filter_map(|node| node.label_text().map(|text| (text, node.role)))
            .collect();
        for expected in [
            ("输入法测试输入框", A11yRole::TextInput),
            ("缩放比例", A11yRole::Slider),
            ("选项 A", A11yRole::RadioButton),
            ("选项 B", A11yRole::RadioButton),
            ("色块 1（红）", A11yRole::RadioButton),
            ("色块 3（蓝）", A11yRole::RadioButton),
            ("Kytrix 图标（2× 放大）", A11yRole::Image),
        ] {
            assert!(
                labels.contains(&expected),
                "a11y 树缺少 {expected:?}；实际标签：{labels:?}"
            );
        }

        let input = snapshot
            .nodes()
            .iter()
            .find(|node| node.role == A11yRole::TextInput)
            .expect("输入框必须在 a11y 树里");
        assert_eq!(input.value.as_deref(), Some(""), "未输入时 value 是空串");
        assert!(
            input.actions.contains(&A11yAction::SetValue),
            "TextInput 必须支持 SetValue：{:?}",
            input.actions
        );

        let slider = snapshot
            .nodes()
            .iter()
            .find(|node| node.role == A11yRole::Slider)
            .expect("滑块必须在 a11y 树里");
        assert_eq!(slider.value.as_deref(), Some("60"), "默认 6 档 = 60%");

        let radio_a = snapshot
            .nodes()
            .iter()
            .find(|node| node.label_text() == Some("选项 A"))
            .expect("选项 A");
        assert_eq!(radio_a.checked, Some(true), "默认选中选项 A");
        let radio_b = snapshot
            .nodes()
            .iter()
            .find(|node| node.label_text() == Some("选项 B"))
            .expect("选项 B");
        assert_eq!(radio_b.checked, Some(false), "选项 B 必须显式未勾选");
    }

    #[test]
    fn arrow_key_updates_the_slider_and_its_a11y_value() {
        let (mut test, window, mut scene) = setup_installed();
        test.enable_a11y_capture();
        frame(&mut test);

        {
            let app = test.app_mut();
            let _ = app.focus_prev(window).expect("输入框");
            let focused = app.focus_prev(window).expect("滑块");
            assert_eq!(app.focused(window), Some(focused));
            dispatch_event(&mut scene, app, &Event::Input(press(KeyCode::ArrowRight)));
        }
        frame(&mut test);
        {
            let app = test.app_mut();
            dispatch_event(&mut scene, app, &Event::Input(press(KeyCode::ArrowRight)));
        }
        frame(&mut test);

        let snapshot = test.a11y().expect("A11Y 变化必须产生交付");
        let slider = snapshot
            .nodes()
            .iter()
            .find(|node| node.role == A11yRole::Slider)
            .expect("滑块的增量记录");
        assert_eq!(
            slider.value.as_deref(),
            Some("80"),
            "方向键改状态后 `bind_a11y` 的 value 必须重发"
        );
        let status = snapshot
            .nodes()
            .iter()
            .find(|node| {
                node.label_text()
                    .is_some_and(|text| text.starts_with("最近 AT 动作"))
            })
            .expect("等宽替换后的状态行也必须重发（文本内容变化 → A11Y）");
        assert_eq!(status.value.as_deref(), Some("最近 AT 动作：方向键 → 80%"));
    }
}

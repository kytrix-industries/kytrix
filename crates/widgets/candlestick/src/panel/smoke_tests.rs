use std::time::Instant;

use i18n::language::codes::LanguageCode;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    event::{Action, ActionSource, Event, InputEvent},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{color::Color, style_builder::StyleBuilder},
    test_support::diagnostics::SignalSlotStats,
};
use ui_kit::{
    kit::size::UnifiedSize,
    theme::{appearance::Appearance, resolved::Theme, role::Role},
};
use widget::test_support::WidgetTestApp;

use super::{
    PANEL_WIDTH,
    rows::{self, PERIOD_FIELD_WIDTH},
    view::{IndicatorPanel, PanelRequest},
};
use crate::indicator::{CandlestickIndicatorConfig, IndicatorId, LineColor};

fn harness() -> (
    WidgetTestApp,
    IndicatorPanel,
    NodeId,
    Signal<Option<PanelRequest>>,
    SignalSlotStats,
) {
    let i18n = i18n::embedded::load().expect("the product catalogue loads");
    let translate = i18n::translate::Translate::new(std::rc::Rc::new(i18n), LanguageCode::En);
    let mut harness = WidgetTestApp::with_translate(translate);
    let baseline = harness.signal_slot_stats();
    let requests = harness.with_cx(|cx| cx.signal(None));
    let mut panel = IndicatorPanel::new(requests);
    let parent = harness.with_cx(|cx| {
        let root = cx.tree().root();
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .width(super::PANEL_WIDTH)
                    .height_full()
                    .flex_col()
                    .build(),
            )
            .build(cx.tree_mut(), root)
            .expect("the window root is alive")
    });
    harness.with_cx(|cx| {
        panel.build(cx, parent).expect("the panel builds");
    });
    (harness, panel, parent, requests, baseline)
}

fn click(harness: &mut WidgetTestApp, node: NodeId, action: Action) -> bool {
    let window = harness.with_cx(|cx| cx.window());
    harness
        .app_mut()
        .dispatch_action(window, node, action, ActionSource::Programmatic)
}

fn click_action(index: u32, suffix: &str) -> Action {
    Action::Custom(format_smolstr!("panel.{suffix}@{index}.click"))
}

fn node(harness: &WidgetTestApp, root: NodeId, suffix: &str, index: u32) -> NodeId {
    harness
        .find_by_debug_prefix(root, &format_smolstr!("panel.{suffix}@{index}"))
        .unwrap_or_else(|| panic!("the control `panel.{suffix}` was built"))
}

#[test]
fn the_panel_builds_syncs_and_releases_cleanly() {
    let (mut harness, mut panel, parent, _requests, baseline) = harness();
    let config = CandlestickIndicatorConfig::default();
    let root = panel.root().expect("the shell was built");
    assert!(harness.tree().get(root).is_ok(), "the shell node is alive");

    assert!(
        harness.with_cx(|cx| panel.sync(cx, &config).expect("first sync")),
        "the first sync builds the rows"
    );
    harness.drain();
    assert!(
        harness
            .find_by_debug_prefix(root, "TEST_PANEL_CARD_MA")
            .is_some(),
        "MA's card was built"
    );
    assert!(
        harness
            .find_by_debug_prefix(root, "TEST_PANEL_LINE_LIST_MA")
            .is_some(),
        "MA's line list was built"
    );

    harness.with_cx(|cx| panel.sync(cx, &config).expect("measure sync"));
    harness.drain();
    assert!(
        !harness.with_cx(|cx| panel.sync(cx, &config).expect("steady sync")),
        "a steady sync does no work"
    );
    let requests = harness.frame_requests();
    harness.drain();
    assert_eq!(
        harness.frame_requests(),
        requests,
        "a steady sync asks for no frame"
    );

    harness.switch_language(LanguageCode::ZhCn);
    assert!(
        harness.with_cx(|cx| panel.sync(cx, &config).expect("language sync")),
        "a language switch rebuilds the rows"
    );
    harness.drain();

    let theme = harness.services().theme.clone();
    theme.set_appearance(harness.app_mut(), Appearance::Dark);
    harness.drain();
    assert!(
        !harness.with_cx(|cx| panel.sync(cx, &config).expect("theme sync")),
        "an appearance change is drawn by the bindings, not a rebuild"
    );
    harness.drain();

    let palette = theme.theme_for(Appearance::Light).components().chart.series;
    let mut next = config.clone();
    assert!(next.add_line(IndicatorId::Ma, &palette));
    assert!(
        harness.with_cx(|cx| panel.sync(cx, &next).expect("config sync")),
        "a configuration change rebuilds the rows"
    );
    harness.drain();
    assert!(
        harness
            .find_by_debug_prefix(root, "TEST_PANEL_LINE_ROW_MA_3")
            .is_some(),
        "the added line has a row"
    );

    harness.with_cx(|cx| panel.release(cx));
    assert!(panel.root().is_none(), "the panel forgets its root");
    assert!(
        harness.tree().get(root).is_err(),
        "the panel removed its own subtree"
    );
    assert!(
        harness.tree().get(parent).is_ok(),
        "the widget's column stays"
    );
    assert!(
        !harness.with_cx(|cx| panel.sync(cx, &config).expect("sync after release")),
        "a released panel stays idle"
    );
    assert_eq!(panel.root(), None, "a released panel has no root to remove");
    harness.with_cx(|cx| panel.release(cx));
    harness.drain();
    assert_eq!(
        harness.signal_slot_stats().live,
        baseline.live + 1,
        "releasing the panel returns every scope it minted (the widget's requests signal stays)"
    );
}

#[test]
fn building_the_same_panel_twice_keeps_one_shell() {
    let (mut harness, mut panel, _parent, _requests, _baseline) = harness();
    let config = CandlestickIndicatorConfig::default();
    let root = panel.root().expect("the shell was built");
    harness.with_cx(|cx| {
        panel.build(cx, cx.tree().root()).expect("second build");
    });
    assert_eq!(
        panel.root(),
        Some(root),
        "a second build is a no-op while the shell is alive"
    );
    harness.with_cx(|cx| panel.sync(cx, &config).expect("sync"));
    harness.drain();
    harness.with_cx(|cx| panel.release(cx));
    assert!(harness.tree().get(root).is_err());
}

#[test]
fn the_rows_line_up_on_the_checkbox_s_line() {
    let (mut harness, mut panel, _parent, _requests, _baseline) = harness();
    let config = CandlestickIndicatorConfig::default();
    let root = panel.root().expect("the shell was built");
    harness.with_cx(|cx| panel.sync(cx, &config).expect("first sync"));
    harness.drain();
    let bounds = |harness: &WidgetTestApp, node: NodeId| {
        harness
            .tree()
            .get(node)
            .expect("the node is alive")
            .layout()
            .bounds
    };

    let root_bounds = bounds(&harness, root);
    assert!(
        (root_bounds.size.width - PANEL_WIDTH).abs() <= 1.0,
        "the panel fills the column the body gives it: {root_bounds:?}"
    );
    let card = harness
        .find_by_debug_prefix(root, "TEST_PANEL_CARD_MA")
        .expect("MA's card was built");
    let card_bounds = bounds(&harness, card);
    assert!(
        card_bounds.size.width > 200.0 && card_bounds.size.height > 20.0,
        "the card is laid out inside the panel: {card_bounds:?}"
    );

    let index = harness.with_cx(|cx| cx.scope().index());
    let toggle = node(&harness, root, "node.MA.enabled", index);
    let toggle_bounds = bounds(&harness, toggle);
    let field = harness
        .find_by_debug_prefix(root, "TEST_PANEL_LINE_FIELD_MA_0")
        .expect("MA's first field was built");
    let field_bounds = bounds(&harness, field);
    assert!(
        (field_bounds.size.width - PERIOD_FIELD_WIDTH).abs() <= 1.0,
        "the field takes the width the row gives it: {field_bounds:?}"
    );
    let checkbox_line = toggle_bounds.origin.x + UnifiedSize::Small.px();
    assert!(
        (field_bounds.origin.x - checkbox_line).abs() <= 1.0,
        "the field starts on the checkbox's line: field={field_bounds:?} toggle={toggle_bounds:?}"
    );

    let hint = harness
        .find_by_debug_prefix(root, "TEST_PANEL_INVALID_MA_0")
        .expect("the hint was built");
    assert!(
        bounds(&harness, hint).size.height < 1.0,
        "a valid field reserves no hint space: {:?}",
        bounds(&harness, hint)
    );
    harness.with_cx(|cx| panel.release(cx));
}

#[test]
fn a_period_field_commits_only_a_period_it_may_take() {
    let (mut harness, mut panel, _parent, requests, _baseline) = harness();
    let mut config = CandlestickIndicatorConfig::default();
    let root = panel.root().expect("the shell was built");
    harness.with_cx(|cx| panel.sync(cx, &config).expect("first sync"));
    harness.drain();
    let index = harness.with_cx(|cx| cx.scope().index());
    let window = harness.with_cx(|cx| cx.window());
    let field = harness
        .find_by_debug_prefix(
            root,
            &format_smolstr!("panel.line.MA.0.period@{index}.root"),
        )
        .expect("MA's first period field was built");

    assert!(harness.with_cx(|cx| cx.app_mut().set_focus(window, Some(field))));
    harness.with_cx(|cx| {
        let event = Event::Input(InputEvent::Text {
            text: SmolStr::new_static("0"),
            timestamp: Instant::now(),
        });
        assert!(
            panel.dispatch(cx, &event),
            "the focused field takes the text"
        );
    });
    harness.drain();
    harness.with_cx(|cx| panel.sync(cx, &config).expect("typing sync"));
    harness.drain();

    harness.with_cx(|cx| cx.app_mut().set_focus(window, None));
    harness.drain();
    assert!(
        harness.with_cx(|cx| panel.sync(cx, &config).expect("blur sync")),
        "the blur committed the period"
    );
    let request = harness.with_cx(|cx| cx.app().read_with(&requests, |value| value.clone()));
    let Some(PanelRequest::SetIndicator(next)) = request else {
        panic!("the field asked the widget to store the typed period");
    };
    let period = next
        .entry(IndicatorId::Ma)
        .expect("MA is configured")
        .settings
        .lines()
        .expect("MA holds a line list")
        .rows[0]
        .period;
    assert_eq!(period, 50, "the typed period was committed");
    harness.with_cx(|cx| cx.write(&requests, None));
    config = next;
    harness.with_cx(|cx| panel.sync(cx, &config).expect("apply sync"));
    harness.drain();

    let field = harness
        .find_by_debug_prefix(
            root,
            &format_smolstr!("panel.line.MA.0.period@{index}.root"),
        )
        .expect("the rebuilt field was built");
    assert!(harness.with_cx(|cx| cx.app_mut().set_focus(window, Some(field))));
    harness.with_cx(|cx| {
        let event = Event::Input(InputEvent::Text {
            text: SmolStr::new_static("x"),
            timestamp: Instant::now(),
        });
        assert!(panel.dispatch(cx, &event));
    });
    harness.drain();
    harness.with_cx(|cx| panel.sync(cx, &config).expect("invalid sync"));
    harness.drain();
    let hint = harness
        .find_by_debug_prefix(root, "TEST_PANEL_INVALID_MA_0")
        .expect("the hint was built");
    assert!(
        harness.has_text(hint, "The period must be an integer from 1 to 500"),
        "the hint states the range a period may take: {:?}",
        harness.text_of(hint)
    );
    assert!(
        harness
            .tree()
            .get(hint)
            .expect("the hint is alive")
            .layout()
            .bounds
            .size
            .height
            > 1.0,
        "the hint is drawn while the text is not a period"
    );
    assert!(
        harness.with_cx(|cx| cx.app().read_with(&requests, |value| value.is_none())),
        "nothing is written while the text is invalid"
    );

    harness.with_cx(|cx| cx.app_mut().set_focus(window, None));
    harness.drain();
    harness.with_cx(|cx| panel.sync(cx, &config).expect("refused sync"));
    assert!(
        harness.with_cx(|cx| cx.app().read_with(&requests, |value| value.is_none())),
        "an invalid period is never written back"
    );
    harness.drain();
    assert!(
        harness
            .tree()
            .get(hint)
            .expect("the hint is alive")
            .layout()
            .bounds
            .size
            .height
            < 1.0,
        "the hint goes away once the stored value is put back"
    );
    harness.with_cx(|cx| panel.release(cx));
}

#[test]
fn the_panel_controls_turn_into_requests_and_rebuilds() {
    let (mut harness, mut panel, _parent, requests, _baseline) = harness();
    let mut config = CandlestickIndicatorConfig::default();
    let root = panel.root().expect("the shell was built");
    assert!(harness.with_cx(|cx| panel.sync(cx, &config).expect("first sync")));
    harness.drain();
    harness.with_cx(|cx| panel.sync(cx, &config).expect("measure sync"));
    harness.drain();

    let index = harness.with_cx(|cx| cx.scope().index());
    let toggle = node(&harness, root, "node.MA.enabled", index);
    let action = click_action(index, "node.MA.enabled");
    assert!(
        click(&mut harness, toggle, action),
        "the toggle's click handler ran"
    );
    harness.drain();
    assert!(
        harness.with_cx(|cx| panel.sync(cx, &config).expect("edit sync")),
        "the click produced an edit the sync drained"
    );
    let request = harness.with_cx(|cx| cx.app().read_with(&requests, |value| value.clone()));
    let Some(PanelRequest::SetIndicator(next)) = request else {
        panic!("the toggle asked the widget to store the configuration");
    };
    assert!(
        !next
            .entry(IndicatorId::Ma)
            .expect("MA is configured")
            .enabled,
        "the request turns MA off"
    );
    assert!(
        next.entry(IndicatorId::Ema)
            .expect("EMA is configured")
            .enabled
            == config.entry(IndicatorId::Ema).expect("EMA").enabled,
        "the request changes nothing else"
    );
    config = next;
    harness.with_cx(|cx| panel.sync(cx, &config).expect("config sync"));
    harness.drain();

    let caret = node(&harness, root, "node.MA.caret", index);
    let action = click_action(index, "node.MA.caret");
    assert!(
        click(&mut harness, caret, action),
        "the caret's handler ran"
    );
    harness.drain();
    assert!(harness.with_cx(|cx| panel.sync(cx, &config).expect("fold sync")));
    harness.drain();
    assert!(
        harness
            .find_by_debug_prefix(root, "TEST_PANEL_LINE_LIST_MA")
            .is_none(),
        "a folded card draws no settings"
    );
    let caret = node(&harness, root, "node.MA.caret", index);
    let action = click_action(index, "node.MA.caret");
    assert!(click(&mut harness, caret, action));
    harness.drain();
    assert!(harness.with_cx(|cx| panel.sync(cx, &config).expect("unfold sync")));
    harness.drain();
    assert!(
        harness
            .find_by_debug_prefix(root, "TEST_PANEL_LINE_LIST_MA")
            .is_some(),
        "the settings come back"
    );

    let add = node(&harness, root, "add_line.MA", index);
    let action = click_action(index, "add_line.MA");
    assert!(click(&mut harness, add, action), "the add-line handler ran");
    harness.drain();
    assert!(harness.with_cx(|cx| panel.sync(cx, &config).expect("add sync")));
    let request = harness.with_cx(|cx| cx.app().read_with(&requests, |value| value.clone()));
    let Some(PanelRequest::SetIndicator(next)) = request else {
        panic!("the add-line button asked the widget to store the configuration");
    };
    let lines = next
        .entry(IndicatorId::Ma)
        .expect("MA is configured")
        .settings
        .lines()
        .expect("MA holds a line list");
    assert_eq!(
        lines.rows.len(),
        config
            .entry(IndicatorId::Ma)
            .expect("MA is configured")
            .settings
            .lines()
            .expect("MA holds a line list")
            .rows
            .len()
            + 1,
        "the request appends one line"
    );
    harness.with_cx(|cx| panel.release(cx));
    harness.drain();
}

#[test]
fn a_line_row_should_delete_the_line_it_belongs_to() {
    let (mut harness, mut panel, _parent, requests, _baseline) = harness();
    let config = CandlestickIndicatorConfig::default();
    let root = panel.root().expect("the shell was built");
    harness.with_cx(|cx| panel.sync(cx, &config).expect("first sync"));
    harness.drain();
    harness.with_cx(|cx| panel.sync(cx, &config).expect("measure sync"));
    harness.drain();

    let index = harness.with_cx(|cx| cx.scope().index());
    let delete = harness
        .find_by_debug_prefix(root, "TEST_PANEL_DELETE_LINE_MA_1")
        .expect("MA's second line can be removed");
    assert!(
        click(
            &mut harness,
            delete,
            click_action(index, "line.MA.1.delete")
        ),
        "the delete button ran its handler"
    );
    harness.drain();
    assert!(
        harness.with_cx(|cx| panel.sync(cx, &config).expect("edit sync")),
        "the delete is an edit the sync drained"
    );
    let request = harness.with_cx(|cx| cx.app().read_with(&requests, |value| value.clone()));
    let Some(PanelRequest::SetIndicator(next)) = request else {
        panic!("the delete button asked the widget to store the configuration");
    };
    let before = config
        .entry(IndicatorId::Ma)
        .and_then(|entry| entry.settings.lines())
        .expect("MA holds a line list");
    let remaining = next
        .entry(IndicatorId::Ma)
        .and_then(|entry| entry.settings.lines())
        .expect("MA still holds a line list");
    assert_eq!(
        remaining.rows.len(),
        before.rows.len() - 1,
        "the request drops the line it names"
    );
    assert_eq!(
        remaining.rows[0], before.rows[0],
        "the line above it is untouched"
    );
    assert_eq!(
        remaining.rows[1], before.rows[2],
        "the line below it moved up"
    );

    harness.with_cx(|cx| cx.write(&requests, None));
    assert!(harness.with_cx(|cx| panel.sync(cx, &next).expect("apply sync")));
    harness.drain();
    assert!(
        harness
            .find_by_debug_prefix(root, "TEST_PANEL_LINE_ROW_MA_2")
            .is_none(),
        "the removed line's row is gone"
    );
    assert!(
        harness
            .find_by_debug_prefix(root, &format_smolstr!("panel.color.MA.2@{index}:trigger"),)
            .is_none(),
        "and so is its picker"
    );
    harness.with_cx(|cx| panel.release(cx));
}

#[test]
fn the_last_line_of_a_line_list_should_not_offer_a_delete() {
    let (mut harness, mut panel, _parent, _requests, _baseline) = harness();
    let mut config = CandlestickIndicatorConfig::default();
    while config
        .entry(IndicatorId::Ma)
        .and_then(|entry| entry.settings.lines())
        .expect("MA holds a line list")
        .rows
        .len()
        > 1
    {
        assert!(
            config.remove_line(IndicatorId::Ma, 0),
            "the fixture trims MA down to one line"
        );
    }
    let root = panel.root().expect("the shell was built");
    harness.with_cx(|cx| panel.sync(cx, &config).expect("first sync"));
    harness.drain();
    harness.with_cx(|cx| panel.sync(cx, &config).expect("measure sync"));
    harness.drain();

    assert!(
        harness
            .find_by_debug_prefix(root, "TEST_PANEL_LINE_ROW_MA_0")
            .is_some(),
        "the last line is drawn"
    );
    assert!(
        harness
            .find_by_debug_prefix(root, "TEST_PANEL_DELETE_LINE_MA_0")
            .is_none(),
        "but it offers no delete"
    );
    harness.with_cx(|cx| panel.release(cx));
}

#[test]
fn every_colour_setting_should_have_one_picker_showing_its_colour() {
    let (mut harness, mut panel, _parent, _requests, _baseline) = harness();
    let mut config = CandlestickIndicatorConfig::default();
    let stored = LineColor {
        h: 0.33,
        s: 0.8,
        l: 0.4,
        a: 1.0,
    };
    assert!(
        config.set_line_color(IndicatorId::Ma, 0, stored),
        "MA's first line takes a colour"
    );
    let root = panel.root().expect("the shell was built");
    let index = harness.with_cx(|cx| cx.scope().index());
    harness.with_cx(|cx| panel.sync(cx, &config).expect("first sync"));
    harness.drain();
    harness.with_cx(|cx| panel.sync(cx, &config).expect("picker sync"));
    harness.drain();

    let fields = super::colors::color_fields(&config);
    assert_eq!(
        fields.len(),
        12,
        "the default configuration offers twelve colours"
    );
    for field in &fields {
        let name = rows::indicator_name(field.id);
        let (holder, suffix) = match field.row {
            Some(row) => (
                format_smolstr!("TEST_PANEL_COLOR_{name}_{row}"),
                format_smolstr!("panel.color.{name}.{row}"),
            ),
            None => (
                format_smolstr!("TEST_PANEL_COLOR_{name}_{}", rows::setting_name(field.key)),
                format_smolstr!("panel.color.{name}.{}", rows::setting_name(field.key)),
            ),
        };
        assert!(
            harness.find_by_debug_prefix(root, &holder).is_some(),
            "the colour setting `{holder}` has a field"
        );
        assert!(
            harness
                .find_by_debug_prefix(root, &format_smolstr!("{suffix}@{index}:trigger"))
                .is_some(),
            "and exactly that field holds the picker `{suffix}`"
        );
    }

    let well = harness
        .find_by_debug_prefix(
            root,
            &format_smolstr!("panel.color.MA.0@{index}:trigger.well"),
        )
        .expect("MA's first line has a picker well");
    let primitives = harness.canvas_primitives(well);
    assert!(
        primitives
            .iter()
            .any(|primitive| primitive.color == Some(Color::from(stored))),
        "the well paints the colour the configuration holds: {primitives:?}"
    );
    harness.with_cx(|cx| panel.release(cx));
}

#[test]
fn a_folded_card_should_light_up_and_an_open_one_should_not() {
    let theme = Theme::of(Appearance::Light);
    let (rest_background, rest_border) = rows::card_colors(&theme, true, false);
    let (hovered_background, hovered_border) = rows::card_colors(&theme, true, true);

    assert_ne!(
        rest_background, hovered_background,
        "a hovered folded card draws a lighter surface"
    );
    assert_ne!(rest_border, hovered_border, "and a stronger border");
    assert_eq!(hovered_background, theme.role(Role::SurfaceControl));
    assert_eq!(hovered_border, theme.role(Role::LineStrong));
    assert_eq!(
        rows::card_colors(&theme, false, true),
        rows::card_colors(&theme, false, false),
        "an open card draws no hover"
    );
}

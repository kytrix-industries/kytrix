//! The panel's smoke tests.
//!
//! They cover what the old toolkit panel's tests could not survive the migration with — the panel's
//! life (build / sync / rebuild / release), the steady frame, the scroll range, the row geometry
//! and the two interaction paths that turn a control into a configuration request — rather than the
//! full behaviour table, which the later test stage rewrites against the engine.

use std::time::Instant;

use base_ui::{kit::size::UnifiedSize, style_builder::StyleBuilder, theme::appearance::Appearance};
use i18n::language::codes::LanguageCode;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    event::{Action, ActionSource, Event, InputEvent},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    test_support::diagnostics::SignalSlotStats,
};
use widget::test_support::WidgetTestApp;

use super::{
    PANEL_WIDTH,
    rows::PERIOD_FIELD_WIDTH,
    view::{IndicatorPanel, PanelRequest},
};
use crate::indicator::{CandlestickIndicatorConfig, IndicatorId};

fn harness() -> (
    WidgetTestApp,
    IndicatorPanel,
    NodeId,
    Signal<Option<PanelRequest>>,
    SignalSlotStats,
) {
    let i18n = i18n::global::embedded::load().expect("the product catalogue loads");
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

    // The frame after the layout publishes the column's measured height as the scroll range;
    // after it the panel is past its first measurement.
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

    // The language moves: the block is rebuilt, and the old tree is gone.
    harness.switch_language(LanguageCode::ZhCn);
    assert!(
        harness.with_cx(|cx| panel.sync(cx, &config).expect("language sync")),
        "a language switch rebuilds the rows"
    );
    harness.drain();

    // The appearance moves: colours are dynamic channels, not a rebuild.
    let theme = harness.services().theme.clone();
    theme.set_appearance(harness.app_mut(), Appearance::Dark);
    harness.drain();
    assert!(
        !harness.with_cx(|cx| panel.sync(cx, &config).expect("theme sync")),
        "an appearance change is drawn by the bindings, not a rebuild"
    );
    harness.drain();

    // The configuration moves: the rows are rebuilt with it.
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

    // Releasing removes the panel's own subtree, releases its scopes and leaves the widget's
    // column in place.
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
    // Releasing again is a no-op, not a panic.
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

    // A settings row starts on the vertical line the checkbox's square is drawn on.
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

    // The invalid hint takes no room while the field holds a period it may take.
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

    // Focus the field and type one digit: the text is now "50".
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

    // The caret leaves: the period is committed and asked for.
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

    // Text that is not a period is refused: the hint appears and nothing is written back.
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

    // The checkbox that enables MA: its click becomes an edit, and the edit becomes a request.
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
    // The widget applies and hands the configuration back.
    config = next;
    harness.with_cx(|cx| panel.sync(cx, &config).expect("config sync"));
    harness.drain();

    // The caret folds MA: the rows under it go away, and come back when it is pressed again.
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

    // The add-line button appends a line to MA's list.
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

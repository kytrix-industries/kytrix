//! The panel itself: its state, its build / sync / release life, and what it asks the widget to do.
//!
//! The panel is a sub-view of the candlestick widget, not an entity of its own: it has no
//! `Render` and no deferred work, it builds its nodes into the widget's body once, and afterwards
//! the frame's `sync` keeps it current. It never stores the indicator configuration — the widget
//! owns it, and the panel reads the copy it is handed on every sync. A control that the user
//! operates writes a [`PanelEdit`]; `sync` applies the edit to a copy of the published
//! configuration and reports the result through [`PanelRequest::SetIndicator`] into the signal the
//! widget minted, which is the widget's cue to store and publish it (a change that is not
//! published only lives in memory). The rows themselves are rebuilt from the configuration the
//! widget publishes next, which is why the panel never disagrees with the chart about a value.
//!
//! # The three signals
//!
//! - `requests` is the widget's: the panel writes committed configuration changes into it.
//! - `edits` and `invalid` are the panel's own, minted in the panel's scope by [`build`] and
//!   released by [`release`]. `edits` is how a control's callback — which runs with an
//!   application context and no panel — hands over what the user did; `invalid` is the field
//!   currently in error, which every row's hint and error frame read.
//!
//! # Rebuilds
//!
//! The row tree is a function of `(configuration, language)`: which indicators exist, how many
//! lines a list has, and what a control's copy says. The panel remembers the pair it built and
//! rebuilds the block when either moves (AGENTS §15.1 — a kit component's copy is build-time).
//! Expansion is the panel's own state and rebuilds the block it opens or folds. A field being
//! typed into is never rebuilt: typing does not change the configuration.
//!
//! [`build`]: IndicatorPanel::build
//! [`release`]: IndicatorPanel::release

use base_ui::{
    kit::{
        button::Button,
        scroll_bar::{ScrollBar, ScrollbarAxis, ScrollbarOptions},
        toggle::Toggle,
    },
    style_builder::StyleBuilder,
    theme::role::Role,
};
use i18n::language::codes::LanguageCode;
use ui_core::{
    error::EngineError,
    event::{Action, Event},
    interaction::{ScrollConfig, ScrollHandle},
    math::Size,
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
    },
    paint::{color::Color, style::StylePatch},
};
use widget::cx::WidgetCx;

use super::{
    colors::{ColorKey, FieldPicker},
    inputs::{FieldInput, InputKey},
    rows::{self, BuiltRows, Wiring},
};
use crate::indicator::{
    CandlestickIndicatorConfig, INDICATOR_CATALOG, IndicatorId, LineColor, SettingKey,
};

/// The width the panel takes out of the pane body.
///
/// Fixed rather than draggable. The body gives the panel a column of exactly this width and hides
/// or shows that column, so opening and closing the panel never resizes the chart's plot.
pub(crate) const PANEL_WIDTH: f32 = 260.0;

/// The padding around the panel's cards.
const PANEL_PADDING: f32 = 8.0;
/// The gap between two cards, and between a section heading and the first card under it.
const CARD_GAP: f32 = 8.0;

/// The selector the panel's root node carries.
const PANEL_ROOT_SELECTOR: &str = "TEST_CANDLESTICK_INDICATOR_PANEL";
/// The selector the panel's scroll container carries.
const PANEL_SCROLL_SELECTOR: &str = "TEST_CANDLESTICK_INDICATOR_PANEL_SCROLL";
/// The selector the column the cards are built into carries.
const PANEL_CONTENT_SELECTOR: &str = "TEST_CANDLESTICK_INDICATOR_PANEL_CONTENT";

/// What the panel asks the widget to do.
///
/// The panel never stores the configuration, so a committed change is a request: the widget
/// stores it, publishes it, and hands it back on the next sync.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PanelRequest {
    /// A control committed an edit; this is the configuration to store and publish.
    SetIndicator(CandlestickIndicatorConfig),
}

/// One thing a control, or the panel's own keyboard-free interaction, did.
///
/// Written by callbacks into the panel's `edits` signal and applied by `sync` to a copy of the
/// published configuration. An expansion never reaches the configuration: it only changes which
/// rows the panel draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum PanelEdit {
    /// Enable or disable one indicator.
    SetEnabled { id: IndicatorId, enabled: bool },
    /// Turn one named switch on or off.
    SetSwitch {
        id: IndicatorId,
        key: SettingKey,
        on: bool,
    },
    /// Append one line to an indicator's line list.
    AddLine(IndicatorId),
    /// Remove one line from an indicator's line list.
    RemoveLine { id: IndicatorId, index: usize },
    /// Write a period the user typed: a line's, or one named setting's.
    SetPeriod { key: InputKey, period: u32 },
    /// Write a colour the user confirmed: a line's, or one switch row's.
    SetColor { key: ColorKey, color: Color },
    /// Open or fold one indicator's settings; never reaches the configuration.
    ToggleExpanded(IndicatorId),
}

impl PanelEdit {
    /// Applies one committed edit to `config`, and returns whether the configuration took it.
    ///
    /// `palette` is the current appearance's chart-series palette, which is where a line the user
    /// adds takes its first colour from. Whether the value really moved is decided by comparing
    /// the whole configuration afterwards, not here: an edit can be accepted and change nothing.
    fn apply(self, config: &mut CandlestickIndicatorConfig, palette: &[Color]) -> bool {
        match self {
            Self::SetEnabled { id, enabled } => {
                let Some(entry) = config.entries.iter_mut().find(|entry| entry.id == id) else {
                    return false;
                };
                if entry.enabled == enabled {
                    return false;
                }
                entry.enabled = enabled;
                true
            }
            Self::SetSwitch { id, key, on } => config.set_switch(id, key, on),
            Self::AddLine(id) => config.add_line(id, palette),
            Self::RemoveLine { id, index } => config.remove_line(id, index),
            Self::SetPeriod { key, period } => match key.row {
                Some(index) => config.set_line_period(key.id, index, period),
                None => config.set_setting_period(key.id, key.key, period),
            },
            Self::SetColor { key, color } => match key.row {
                Some(index) => config.set_line_color(key.id, index, LineColor::from(color)),
                None => config.set_color(key.id, key.key, LineColor::from(color)),
            },
            Self::ToggleExpanded(_) => false,
        }
    }
}

/// One control the built rows own.
///
/// The panel keeps the component handle rather than only its node: the kit's `release` unregisters
/// the action handler **and** releases the component's signal scope, which is what keeps a
/// rebuild from leaking a slot per control.
pub(super) enum Control {
    Button(Button),
    Toggle(Toggle),
    Input(FieldInput),
    Picker(FieldPicker),
}

impl Control {
    /// Routes one platform event into the control.
    fn dispatch(&mut self, cx: &mut WidgetCx<'_>, event: &Event) -> bool {
        match self {
            Self::Button(button) => button.dispatch(cx.app_mut(), event),
            Self::Toggle(toggle) => toggle.dispatch(cx.app_mut(), event),
            Self::Input(input) => input.dispatch(cx, event),
            Self::Picker(picker) => picker.dispatch(cx, event),
        }
    }

    /// Releases the control's handlers and signal scope. The row it was built into is removed by
    /// the panel before this runs.
    fn release(self, cx: &mut WidgetCx<'_>) {
        match self {
            Self::Button(button) => {
                let _ = button.release(cx.app_mut());
            }
            Self::Toggle(toggle) => {
                let _ = toggle.release(cx.app_mut());
            }
            Self::Input(input) => input.release(cx),
            Self::Picker(picker) => picker.release(cx),
        }
    }
}

/// The indicator settings panel.
pub(crate) struct IndicatorPanel {
    /// The widget's signal: where a committed configuration change is handed over.
    requests: Signal<Option<PanelRequest>>,
    /// The panel's own control edits and error mark, minted in [`IndicatorPanel::build`] and
    /// released with the panel's scope.
    edits: Option<Signal<Option<PanelEdit>>>,
    invalid: Option<Signal<Option<InputKey>>>,
    /// One hover mirror per catalogue indicator, read by that card's `bind_style`.
    hover: Vec<(IndicatorId, Signal<bool>)>,
    /// The scope the panel's signals belong to.
    scope: Option<SignalScope>,
    /// The panel's shell: the root, the scroll container and the card column.
    root: Option<NodeId>,
    scroll: Option<NodeId>,
    content: Option<NodeId>,
    scroll_handle: Option<ScrollHandle>,
    scrollbar: Option<ScrollBar>,
    /// The controls of the built rows, in build order.
    controls: Vec<Control>,
    /// The application-level action handlers the node rows registered.
    actions: Vec<Action>,
    /// The card node of each indicator, for the pointer mirror.
    cards: Vec<(IndicatorId, NodeId)>,
    /// Which indicators are open. Not persisted (design D9): opening the panel always starts with
    /// every indicator that has settings expanded.
    expanded: Vec<(IndicatorId, bool)>,
    /// The configuration and language the current rows were built from, so a steady frame does
    /// no work and a moved value rebuilds the block.
    built_config: Option<CandlestickIndicatorConfig>,
    built_language: Option<LanguageCode>,
    /// The scrollable content size written last, so a steady frame writes nothing.
    scroll_content: Option<Size>,
    /// The card the pointer is on, so a hover write happens only when it moves.
    hovered_card: Option<IndicatorId>,
}

impl IndicatorPanel {
    /// Creates the panel's state: no nodes, no work.
    ///
    /// `requests` is the widget's signal: the panel writes a committed configuration change into
    /// it, and the widget stores, publishes and hands the configuration back on the next sync.
    pub(crate) fn new(requests: Signal<Option<PanelRequest>>) -> Self {
        Self {
            requests,
            edits: None,
            invalid: None,
            hover: Vec::new(),
            scope: None,
            root: None,
            scroll: None,
            content: None,
            scroll_handle: None,
            scrollbar: None,
            controls: Vec::new(),
            actions: Vec::new(),
            cards: Vec::new(),
            // From the catalogue rather than a literal: an indicator added to it would otherwise
            // start folded, which no compile error would catch.
            expanded: INDICATOR_CATALOG
                .iter()
                .map(|definition| (definition.id, true))
                .collect(),
            built_config: None,
            built_language: None,
            scroll_content: None,
            hovered_card: None,
        }
    }

    /// Builds the panel's nodes into `parent`.
    ///
    /// Only the shell — the root, its scroll container and the card column — is built here: the
    /// cards are a function of the configuration, which the first `sync` is handed. The call is
    /// idempotent: a second `build` while the shell is alive does nothing, and a shell whose root
    /// was removed is released and rebuilt.
    pub(crate) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        if let Some(root) = self.root
            && cx.tree().get(root).is_ok()
        {
            return Ok(());
        }
        if self.root.is_some() || self.scope.is_some() {
            self.release(cx);
        }
        let theme = cx.theme().clone();
        let window = cx.window();
        let scope = cx.app_mut().signal_scope();
        let edits = cx.app_mut().signal_scoped(scope, None);
        let invalid = cx.app_mut().signal_scoped(scope, None);
        let mut hover = Vec::with_capacity(INDICATOR_CATALOG.len());
        for definition in INDICATOR_CATALOG {
            hover.push((definition.id, cx.app_mut().signal_scoped(scope, false)));
        }
        self.scope = Some(scope);
        self.edits = Some(edits);
        self.invalid = Some(invalid);
        self.hover = hover;

        let root_theme = theme.clone();
        let root = NodeBuilder::boxed()
            .id(PANEL_ROOT_SELECTOR)
            .style(
                StyleBuilder::new()
                    .width_full()
                    .height_full()
                    .min_height_0()
                    .flex_col()
                    .relative()
                    .build(),
            )
            // The body's own background belongs to the framework; this is a region inside the
            // body, which is allowed to state its own surface.
            .bind_style(move |read| StylePatch {
                background: Some(root_theme.role(read, Role::SurfaceRaised)),
                ..StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        let scroll = NodeBuilder::boxed()
            .id(PANEL_SCROLL_SELECTOR)
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .flex_1()
                    .min_height_0()
                    .width_full()
                    .relative()
                    .overflow_hidden()
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        let handle = cx
            .app_mut()
            .make_scroll(window, scroll, ScrollConfig::default())?;
        let content = NodeBuilder::boxed()
            .id(PANEL_CONTENT_SELECTOR)
            .style(
                StyleBuilder::new()
                    .width_full()
                    .flex_col()
                    .gap(CARD_GAP)
                    .padding(PANEL_PADDING)
                    // The column keeps its natural height inside the clipped scroll container;
                    // its measured height is what makes the panel scrollable.
                    .flex_none()
                    .build(),
            )
            .build(cx.tree_mut(), scroll)?;
        let scrollbar_key = cx.unique_key("panel.scrollbar");
        let scrollbar = ScrollBar::new(
            cx.app_mut(),
            theme,
            scrollbar_key,
            ScrollbarAxis::Vertical,
            ScrollbarOptions::default(),
        );
        scrollbar.build(cx.tree_mut(), root)?;
        scrollbar.attach(cx.app_mut(), window, scroll);
        self.root = Some(root);
        self.scroll = Some(scroll);
        self.content = Some(content);
        self.scroll_handle = Some(handle);
        self.scrollbar = Some(scrollbar);
        // The card column has no measured height until the frame that lays the shell out, so the
        // scroll range cannot be written yet: ask for the frame that can.
        cx.request_frame()?;
        Ok(())
    }

    /// Keeps the panel current from `config`, and returns whether anything changed.
    ///
    /// A steady frame does no work: no signal write, no node rebuild, no frame request. A stale
    /// root (the widget removed the column) is a release, not an error — the panel stays idle
    /// until it is built again.
    pub(crate) fn sync(
        &mut self,
        cx: &mut WidgetCx<'_>,
        config: &CandlestickIndicatorConfig,
    ) -> Result<bool, EngineError> {
        let Some(root) = self.root else {
            return Ok(false);
        };
        if cx.tree().get(root).is_err() {
            self.release(cx);
            return Ok(false);
        }
        let (Some(edits), Some(invalid)) = (self.edits, self.invalid) else {
            return Ok(false);
        };
        let mut changed = false;
        let mut intents: Vec<PanelEdit> = Vec::with_capacity(2);
        // A control's callback cannot reach the panel; what it did is taken out of the signal on
        // the first sync after the frame it asked for. The read is taken out of the signal before
        // the write: an engine signal is not copied on read, so a write while a read guard is
        // alive would alias the value it is reading.
        let edit = *cx.read(&edits);
        if let Some(edit) = edit {
            cx.write(&edits, None);
            intents.push(edit);
        }
        for control in &mut self.controls {
            match control {
                Control::Input(input) => {
                    if let Some(edit) = input.sync(cx, invalid) {
                        intents.push(edit);
                    }
                }
                Control::Picker(picker) => picker.sync(cx)?,
                Control::Toggle(toggle) => {
                    toggle.sync(cx.app_mut())?;
                }
                Control::Button(button) => {
                    button.sync(cx.app_mut())?;
                }
            }
        }
        if let Some(scrollbar) = &self.scrollbar {
            scrollbar.sync(cx.app_mut())?;
        }
        let mut expansion = false;
        let mut next: Option<CandlestickIndicatorConfig> = None;
        if !intents.is_empty() {
            let theme = cx.theme().clone();
            let palette = theme
                .theme_for(theme.current(cx.app()))
                .components()
                .chart
                .series;
            for intent in intents {
                match intent {
                    PanelEdit::ToggleExpanded(id) => {
                        self.toggle_expanded(id);
                        expansion = true;
                    }
                    intent => {
                        let next = next.get_or_insert_with(|| config.clone());
                        intent.apply(next, &palette);
                    }
                }
            }
        }
        // Only a configuration that really moved is handed over: every field commits when its
        // caret leaves, and most of those commits change nothing.
        if let Some(next) = next
            && next != *config
        {
            cx.write(&self.requests, Some(PanelRequest::SetIndicator(next)));
            changed = true;
        }
        let language = *cx.read(&cx.language());
        if expansion
            || self.built_language != Some(language)
            || self.built_config.as_ref() != Some(config)
        {
            self.rebuild_rows(cx, config, language)?;
            changed = true;
            cx.request_frame()?;
        }
        changed |= self.sync_hover(cx);
        changed |= self.sync_scroll(cx)?;
        Ok(changed)
    }

    /// Forwards one platform event to the panel's controls; returns whether one consumed it.
    ///
    /// The widget offers the event here before its own handlers see it: a focused period field
    /// and an open colour picker both own their keystrokes.
    pub(crate) fn dispatch(&mut self, cx: &mut WidgetCx<'_>, event: &Event) -> bool {
        let Some(root) = self.root else {
            return false;
        };
        if cx.tree().get(root).is_err() {
            return false;
        }
        let mut consumed = false;
        for control in &mut self.controls {
            consumed |= control.dispatch(cx, event);
        }
        consumed
    }

    /// Releases every kit component the panel built and its own signal scope, and removes the
    /// nodes it built.
    ///
    /// Safe to call when [`IndicatorPanel::build`] was never called, and safe to call twice: the
    /// components are taken out of their fields first, so each scope is released exactly once, and
    /// a root that is already gone is not removed again.
    ///
    /// Nodes go before components: a kit component's `bind_*` closures read signals its own scope
    /// owns, so the nodes that hold those closures have to be gone before the scope is released.
    /// The panel removes its own subtree here rather than leaving it to the widget, so a panel
    /// that is released but not handed straight to [`IndicatorPanel::root`] cannot leave a dead
    /// tree painting stale content next to a fresh one.
    pub(crate) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        if let Some(root) = self.root.take()
            && cx.tree().get(root).is_ok()
        {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, root);
        }
        for control in self.controls.drain(..) {
            control.release(cx);
        }
        for action in self.actions.drain(..) {
            cx.app_mut().off_action(&action);
        }
        if let Some(scrollbar) = self.scrollbar.take() {
            scrollbar.release(cx.app_mut());
        }
        if let Some(scope) = self.scope.take() {
            cx.app_mut().release_scope(scope);
        }
        self.scroll = None;
        self.content = None;
        self.scroll_handle = None;
        self.edits = None;
        self.invalid = None;
        self.hover.clear();
        self.cards.clear();
        self.built_config = None;
        self.built_language = None;
        self.scroll_content = None;
        self.hovered_card = None;
    }

    /// The panel's own root node, while it is built.
    ///
    /// `None` before the first [`IndicatorPanel::build`] and after [`IndicatorPanel::release`],
    /// which removes the subtree itself. The widget uses this only for the alive-guarded removal
    /// it would otherwise do itself; a release has already done it.
    #[cfg(test)]
    pub(crate) fn root(&self) -> Option<NodeId> {
        self.root
    }

    /// Rebuilds the row tree from `config` in `language`, releasing the previous one.
    ///
    /// Expansion never leaves the panel, which is why this is the method that consumes it: the
    /// other edits were applied to the working copy `sync` builds and are already on their way to
    /// the widget, so the rows built here still show the configuration that was published.
    fn rebuild_rows(
        &mut self,
        cx: &mut WidgetCx<'_>,
        config: &CandlestickIndicatorConfig,
        language: LanguageCode,
    ) -> Result<(), EngineError> {
        let Some(content) = self.content else {
            return Ok(());
        };
        let Some(edits) = self.edits else {
            return Ok(());
        };
        let Some(invalid) = self.invalid else {
            return Ok(());
        };
        let window = cx.window();
        // Nodes first, then the components that built them: a handler reads signals its component
        // scope owns, so the closures have to be gone before the scope is.
        let children: Vec<NodeId> = cx.tree().get(content)?.children().to_vec();
        for child in children {
            let _ = cx.app_mut().remove_node(window, child);
        }
        for control in self.controls.drain(..) {
            control.release(cx);
        }
        for action in self.actions.drain(..) {
            cx.app_mut().off_action(&action);
        }
        self.cards.clear();
        // A mark on a field that is no longer drawn would keep the panel believing a row is in
        // error, and would outlive the line it was about.
        cx.write(&invalid, None);
        let theme = cx.theme().clone();
        let built: BuiltRows = rows::build_rows(
            cx,
            config,
            &theme,
            Wiring { edits, invalid },
            self.hover.clone(),
            self.expanded.clone(),
            content,
        )?;
        self.controls = built.controls;
        self.actions = built.actions;
        self.cards = built.cards;
        self.hovered_card = None;
        self.built_config = Some(config.clone());
        self.built_language = Some(language);
        Ok(())
    }

    /// Mirrors which folded card the pointer is on into that card's hover signal.
    fn sync_hover(&mut self, cx: &mut WidgetCx<'_>) -> bool {
        let mut current = cx.app().hovered(cx.window());
        let mut inside = None;
        while let Some(node) = current {
            if let Some((id, _)) = self.cards.iter().find(|(_, card)| *card == node) {
                inside = Some(*id);
                break;
            }
            current = cx.tree().get(node).ok().and_then(|node| node.parent());
        }
        // An open card draws no hover at all: the controls under its title are controls of their
        // own, and a box around them that lit up would say the whole box is one control.
        let target = inside.filter(|id| {
            let definition = crate::indicator::definition_of(*id);
            !(self.is_expanded(*id) && !definition.rows.is_empty())
        });
        if target == self.hovered_card {
            return false;
        }
        if let Some(previous) = self.hovered_card.take()
            && let Some((_, signal)) = self.hover.iter().find(|(id, _)| *id == previous)
        {
            cx.write(signal, false);
        }
        if let Some(id) = target {
            if let Some((_, signal)) = self.hover.iter().find(|(candidate, _)| *candidate == id) {
                cx.write(signal, true);
            }
            self.hovered_card = Some(id);
        }
        true
    }

    /// Publishes the card column's measured height as the scroll container's content size.
    ///
    /// The measurement is one frame behind a rebuild — the column is laid out by the frame the
    /// rebuild requested — so the range settles on the frame after the rows changed, and a steady
    /// frame writes nothing.
    fn sync_scroll(&mut self, cx: &mut WidgetCx<'_>) -> Result<bool, EngineError> {
        let (Some(scroll), Some(content), Some(handle)) =
            (self.scroll, self.content, self.scroll_handle)
        else {
            return Ok(false);
        };
        let (viewport, height) = {
            let tree = cx.tree();
            let Ok(scroll_node) = tree.get(scroll) else {
                return Ok(false);
            };
            let viewport = scroll_node.layout().bounds.size;
            let Ok(content_node) = tree.get(content) else {
                return Ok(false);
            };
            (viewport, content_node.layout().bounds.size.height)
        };
        if viewport.height <= 0.0 || !height.is_finite() {
            return Ok(false);
        }
        let size = Size::new(viewport.width, height.max(viewport.height));
        if self.scroll_content == Some(size) {
            return Ok(false);
        }
        cx.app_mut().set_scroll_content(&handle, size)?;
        self.scroll_content = Some(size);
        Ok(true)
    }

    /// Whether `id`'s settings are open.
    fn is_expanded(&self, id: IndicatorId) -> bool {
        self.expanded
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .is_none_or(|(_, open)| *open)
    }

    /// Opens or folds `id`'s settings.
    fn toggle_expanded(&mut self, id: IndicatorId) {
        match self
            .expanded
            .iter_mut()
            .find(|(candidate, _)| *candidate == id)
        {
            Some((_, open)) => *open = !*open,
            // An indicator the panel has not seen starts open, so the first toggle folds it.
            None => self.expanded.push((id, false)),
        }
    }
}

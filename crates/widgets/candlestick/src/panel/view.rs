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

pub(crate) const PANEL_WIDTH: f32 = 260.0;

const PANEL_PADDING: f32 = 8.0;
const CARD_GAP: f32 = 8.0;

const PANEL_ROOT_SELECTOR: &str = "TEST_CANDLESTICK_INDICATOR_PANEL";
const PANEL_SCROLL_SELECTOR: &str = "TEST_CANDLESTICK_INDICATOR_PANEL_SCROLL";
const PANEL_CONTENT_SELECTOR: &str = "TEST_CANDLESTICK_INDICATOR_PANEL_CONTENT";

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PanelRequest {
    SetIndicator(CandlestickIndicatorConfig),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum PanelEdit {
    SetEnabled {
        id: IndicatorId,
        enabled: bool,
    },
    SetSwitch {
        id: IndicatorId,
        key: SettingKey,
        on: bool,
    },
    AddLine(IndicatorId),
    RemoveLine {
        id: IndicatorId,
        index: usize,
    },
    SetPeriod {
        key: InputKey,
        period: u32,
    },
    SetColor {
        key: ColorKey,
        color: Color,
    },
    ToggleExpanded(IndicatorId),
}

impl PanelEdit {
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

pub(super) enum Control {
    Button(Button),
    Toggle(Toggle),
    Input(FieldInput),
    Picker(FieldPicker),
}

impl Control {
    fn dispatch(&mut self, cx: &mut WidgetCx<'_>, event: &Event) -> bool {
        match self {
            Self::Button(button) => button.dispatch(cx.app_mut(), event),
            Self::Toggle(toggle) => toggle.dispatch(cx.app_mut(), event),
            Self::Input(input) => input.dispatch(cx, event),
            Self::Picker(picker) => picker.dispatch(cx, event),
        }
    }

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

pub(crate) struct IndicatorPanel {
    requests: Signal<Option<PanelRequest>>,
    edits: Option<Signal<Option<PanelEdit>>>,
    invalid: Option<Signal<Option<InputKey>>>,
    hover: Vec<(IndicatorId, Signal<bool>)>,
    scope: Option<SignalScope>,
    root: Option<NodeId>,
    scroll: Option<NodeId>,
    content: Option<NodeId>,
    scroll_handle: Option<ScrollHandle>,
    scrollbar: Option<ScrollBar>,
    controls: Vec<Control>,
    actions: Vec<Action>,
    cards: Vec<(IndicatorId, NodeId)>,
    expanded: Vec<(IndicatorId, bool)>,
    built_config: Option<CandlestickIndicatorConfig>,
    built_language: Option<LanguageCode>,
    scroll_content: Option<Size>,
    hovered_card: Option<IndicatorId>,
}

impl IndicatorPanel {
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
        cx.request_frame()?;
        Ok(())
    }

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

    #[cfg(test)]
    pub(crate) fn root(&self) -> Option<NodeId> {
        self.root
    }

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

    fn is_expanded(&self, id: IndicatorId) -> bool {
        self.expanded
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .is_none_or(|(_, open)| *open)
    }

    fn toggle_expanded(&mut self, id: IndicatorId) {
        match self
            .expanded
            .iter_mut()
            .find(|(candidate, _)| *candidate == id)
        {
            Some((_, open)) => *open = !*open,
            None => self.expanded.push((id, false)),
        }
    }
}

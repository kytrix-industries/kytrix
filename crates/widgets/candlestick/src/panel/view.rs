//! The panel view itself: the configured indicators and the controls that edit
//! them.
//!
//! The panel is an entity of its own rather than a corner of the widget's body:
//! editing a setting is then one entity's change instead of a repaint of the
//! chart, and the panel's state — which indicators are open, which field is in
//! error, the scroll position — is not part of the chart's. It holds the widget
//! weakly and reads the configuration from it every frame, so the widget stays
//! the one owner of what is stored; the strong edge the other way (the widget
//! keeps the panel alive) is what keeps this entity from being dropped the
//! moment it is built, and the weak one here keeps the two from forming a cycle.
//!
//! The panel's own dimensions are its business; the width it takes out of the
//! body is [`PANEL_WIDTH`], and the body decides where it goes.

use base_ui::{
    animation::ANIMATION_TICK,
    scroll_bar::Scrollbar,
    style::{roles::Roles, theme::ThemeExt},
};
use gpui_kit::{
    AnyElement, App, Context, ElementId, Hsla, InteractiveElement, IntoElement, ParentElement,
    Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window,
    base::{Easing, Transition, transition},
    div,
    prelude::FluentBuilder,
    px,
};
use i18n::t;
use smol_str::format_smolstr;

use super::{
    colors::{self, ColorKey, LinePicker},
    inputs::{self, InputKey, LineInput},
    rows,
};
use crate::{
    indicator::{
        CandlestickIndicatorConfig, INDICATOR_CATALOG, IndicatorDefinition, IndicatorId,
        IndicatorPane, LineColor, SettingControl, SettingKey,
    },
    view::CandlestickWidget,
};

/// The width the panel takes out of the pane body.
///
/// Fixed rather than draggable, and the budget is counted from the widest thing in
/// it: a line row. The panel's own [`PANEL_PADDING`] takes 16, a card's 1px border and
/// [`CARD_PADDING`] take 18 more, and [`rows::SETTING_INSET`] takes 24 inside the row,
/// which leaves 202 for the row's content. The controls take 102 of those — a 60px
/// period field, a 20px colour picker and a 22px delete button — and the three 8px gaps
/// between them take 24, so the spacer that holds the controls at the right edge still
/// has 76 to give. The delete button's 22 rather than a stated width is the icon's 12
/// plus its own two 4px paddings and 1px borders: the button is as wide as what it
/// draws, so the number tracks the size it is given. A pane narrower than this presses
/// the chart down to its own minimum plot width and the body clips the panel's right
/// edge — the panel's visibility is decided by whether it is open, not by how wide the
/// pane is, because `Render` cannot measure the box it is drawn in.
pub(crate) const PANEL_WIDTH: f32 = 260.0;

/// The padding around the panel's rows.
///
/// It is also the margin a card has on every side: the column is inset by this, and
/// the gap the column puts between two cards is [`ROW_GAP`], which is stated as the
/// same number. A card inset by less than the gap beside it reads as if the gap were
/// the card's margin and the padding something else — which is what the panel looked
/// like while the two numbers were 4 and 8.
const PANEL_PADDING: f32 = 8.0;
/// The padding inside one indicator's card, on all four sides.
///
/// The same number as [`PANEL_PADDING`]: what the eye compares is a card's own inset
/// to the space around it, and a card that held its content 4px from its edge while
/// the next card began 8px below it looked like two different margins.
pub(super) const CARD_PADDING: f32 = PANEL_PADDING;
/// The gap between two cards, and between a section heading and the first card
/// under it.
///
/// One gap for both: the column is one flex container whose `gap` places the
/// headings and the cards alike, so the space above a group of cards is the space
/// between them.
///
/// The same number as [`CARD_PADDING`]: the gap between two cards and the inset each
/// card holds its content by are one measure, so no two neighbours in the panel look
/// further apart than two others.
const ROW_GAP: f32 = CARD_PADDING;

/// The indicator settings panel.
pub(crate) struct IndicatorPanel {
    /// The chart the settings belong to. Weak: the widget owns this entity, so a
    /// strong edge here would be a cycle.
    widget: WeakEntity<CandlestickWidget>,
    /// Which indicators are open. Not persisted (design D9), so opening the panel
    /// always starts with every indicator that has settings expanded.
    expanded: Vec<(IndicatorId, bool)>,
    /// The text fields the period rows edit, one per editable period.
    inputs: Vec<LineInput>,
    /// The colour pickers the colour settings edit, one per configurable colour.
    pickers: Vec<LinePicker>,
    /// The field whose text is not a period it may take, if there is one. Draws
    /// the red frame and the hint; nothing is written back while it is set.
    invalid: Option<InputKey>,
    /// A field whose text has to be put back to the stored value on the next
    /// frame: restoring it needs the window, so it waits for the frame that has
    /// one.
    pending_reset: Option<InputKey>,
    /// The next number a field's elements are told apart by. Handed out once and
    /// never reused, so a field drawn again is a new element.
    next_slot: u64,
    scroll: ScrollHandle,
    /// How many frames this panel has drawn.
    ///
    /// A test that asks whether the panel was rebuilt reads this: `debug_bounds`
    /// answers "was this drawn in the frame that painted it", not "was this
    /// rebuilt", and the panel is a child entity that is only drawn when it is
    /// notified.
    #[cfg(test)]
    renders: usize,
}

impl IndicatorPanel {
    /// The panel over `widget`.
    ///
    /// It takes no configuration: what it draws is read from the widget on the
    /// frame that draws it, so "who owns the settings" has one answer.
    pub(crate) fn new(widget: WeakEntity<CandlestickWidget>, _cx: &mut Context<Self>) -> Self {
        Self {
            widget,
            // From the catalogue rather than a literal: an indicator added to it
            // would otherwise start folded, which no compile error would catch.
            expanded: INDICATOR_CATALOG
                .iter()
                .map(|definition| (definition.id, true))
                .collect(),
            inputs: Vec::new(),
            pickers: Vec::new(),
            invalid: None,
            pending_reset: None,
            next_slot: 0,
            scroll: ScrollHandle::new(),
            #[cfg(test)]
            renders: 0,
        }
    }

    /// The settings to draw, read from the widget on the frame that draws them.
    pub(super) fn indicator_config(&self, cx: &App) -> Option<CandlestickIndicatorConfig> {
        Some(self.widget.upgrade()?.read(cx).indicator_config().clone())
    }

    /// Whether `id`'s settings are open.
    ///
    /// An indicator the list does not name is open: that is the same answer the
    /// constructor gives, so a catalogue entry added later starts open rather
    /// than folded and unreachable.
    fn is_expanded(&self, id: IndicatorId) -> bool {
        self.expanded
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .is_none_or(|(_, open)| *open)
    }

    /// Opens or folds `id`'s settings.
    pub(super) fn toggle_expanded(&mut self, id: IndicatorId, cx: &mut Context<Self>) {
        if let Some((_, open)) = self
            .expanded
            .iter_mut()
            .find(|(candidate, _)| *candidate == id)
        {
            *open = !*open;
        }
        cx.notify();
    }

    /// Enables or disables one indicator.
    pub(super) fn set_enabled(&mut self, id: IndicatorId, enabled: bool, cx: &mut Context<Self>) {
        let Some(mut next) = self.indicator_config(cx) else {
            return;
        };
        let Some(entry) = next.entries.iter_mut().find(|entry| entry.id == id) else {
            return;
        };
        entry.enabled = enabled;
        self.write(next, cx);
    }

    /// Reads one named switch out of `id`'s settings.
    fn switch(&self, id: IndicatorId, key: SettingKey, cx: &App) -> bool {
        self.indicator_config(cx)
            .and_then(|config| config.entry(id)?.settings.switch(key))
            .unwrap_or(false)
    }

    /// Turns one named switch of `id` on or off.
    pub(super) fn set_switch(
        &mut self,
        id: IndicatorId,
        key: SettingKey,
        on: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(mut next) = self.indicator_config(cx) else {
            return;
        };
        if next.set_switch(id, key, on) {
            self.write(next, cx);
        }
    }

    /// Appends a line to `id`'s list.
    pub(super) fn add_line(&mut self, id: IndicatorId, cx: &mut Context<Self>) {
        let Some(mut next) = self.indicator_config(cx) else {
            return;
        };
        let palette = cx.theme().components.chart.series.map(|color| color.hsla);
        if next.add_line(id, &palette) {
            self.write(next, cx);
        }
    }

    /// Removes one line from `id`'s list.
    ///
    /// The last line stays: at the minimum its catalogue row declares the panel
    /// draws no delete button at all, and the configuration refuses the removal
    /// anyway, so an action that slipped past the panel cannot empty the list.
    pub(super) fn remove_line(&mut self, id: IndicatorId, index: usize, cx: &mut Context<Self>) {
        let Some(mut next) = self.indicator_config(cx) else {
            return;
        };
        if next.remove_line(id, index) {
            self.write(next, cx);
        }
    }

    /// Records the text a field holds while it is being typed into.
    ///
    /// Typing never writes: a half-typed number is a value the user has not given
    /// yet, and the configuration only ever holds a period the panel accepted.
    /// What this decides is whether the row is in error, so the red frame and the
    /// hint follow the text as it is typed.
    pub(super) fn on_input_change(&mut self, key: InputKey, text: &str, cx: &mut Context<Self>) {
        let invalid = self
            .field_bounds(key, cx)
            .is_some_and(|(min, max)| parse_period(text, min, max).is_none())
            .then_some(key);
        if self.invalid != invalid {
            self.invalid = invalid;
            cx.notify();
        }
    }

    /// Commits a field when the caret leaves it.
    ///
    /// A period is written back through the widget, which is the one place that
    /// publishes; anything else leaves the configuration alone and asks the next
    /// frame to put the stored value back into the field and take the red frame
    /// down.
    pub(super) fn on_input_blur(&mut self, key: InputKey, text: &str, cx: &mut Context<Self>) {
        let Some((min, max)) = self.field_bounds(key, cx) else {
            return;
        };
        let Some(period) = parse_period(text, min, max) else {
            self.invalid = Some(key);
            self.pending_reset = Some(key);
            cx.notify();
            return;
        };
        if self.invalid == Some(key) {
            self.invalid = None;
        }
        let Some(mut next) = self.indicator_config(cx) else {
            return;
        };
        let applied = match key.row {
            Some(index) => next.set_line_period(key.id, index, period),
            None => next.set_setting_period(key.id, key.key, period),
        };
        if applied {
            self.write(next, cx);
        }
    }

    /// The range the field `key` names accepts.
    fn field_bounds(&self, key: InputKey, cx: &App) -> Option<(u32, u32)> {
        let config = self.indicator_config(cx)?;
        let field = inputs::field_of(&config, key)?;
        Some((field.min, field.max))
    }

    /// Hands an edited configuration to the widget.
    ///
    /// The panel keeps no copy of what it changed: the next frame draws what the
    /// widget reports, so the field the user just left and the chart cannot
    /// disagree. `publish` is the widget's, because a change that is not
    /// published only lives in memory.
    ///
    /// A configuration that says what the stored one already says is dropped
    /// here: every field of the panel commits when it is left, and most of those
    /// commits change nothing. The widget would not publish again, but the panel
    /// would still draw another frame.
    fn write(&mut self, next: CandlestickIndicatorConfig, cx: &mut Context<Self>) {
        if self
            .indicator_config(cx)
            .is_some_and(|current| current == next)
        {
            return;
        }
        if let Some(widget) = self.widget.upgrade() {
            widget.update(cx, |widget, cx| widget.set_indicator_config(next, cx));
        }
        cx.notify();
    }

    /// Puts a field the user left in error back to the stored value.
    ///
    /// The frame between the blur and this one keeps the red row, so the user
    /// sees which field was refused; this frame is the one that has a window, and
    /// therefore the one that can write text into an input.
    fn restore_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.pending_reset.take() else {
            return;
        };
        let Some(period) = self
            .indicator_config(cx)
            .and_then(|config| inputs::field_of(&config, key))
            .map(|field| field.period)
        else {
            return;
        };
        let text = format_smolstr!("{period}");
        if let Some(input) = self
            .inputs
            .iter()
            .find(|input| input.id == key.id && input.row == key.row && input.key == key.key)
        {
            input.entity.update(cx, |state, cx| {
                state.update_value(text.as_str(), window, cx);
            });
        }
        if self.invalid == Some(key) {
            self.invalid = None;
        }
    }

    /// Creates, reuses and drops the input entities the fields need this frame.
    ///
    /// Three rules, each of which is a bug if it is missing: a field drawn again
    /// gets the entity it had (a rebuilt one loses the caret), a field whose
    /// stored value moved under it gets a new one (the old one shows a number
    /// that is no longer stored), and a field whose line is gone loses its entity
    /// (the table would otherwise grow without bound and a line added later would
    /// come back holding the deleted line's text).
    fn sync_inputs(
        &mut self,
        config: &CandlestickIndicatorConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fields = inputs::input_fields(config);
        self.inputs.retain(|input| {
            fields.iter().any(|field| {
                field.id == input.id
                    && field.row == input.row
                    && field.key == input.key
                    && field.period == input.built_from
            })
        });
        for field in &fields {
            if self.inputs.iter().any(|input| {
                input.id == field.id && input.row == field.row && input.key == field.key
            }) {
                continue;
            }
            let slot = self.next_slot;
            self.next_slot += 1;
            self.inputs
                .push(LineInput::connect(field, slot, window, cx));
        }
        // A mark on a field that is no longer drawn would keep the panel
        // believing a row is in error, and would outlive the line it was about.
        let live = |key: &InputKey| {
            fields
                .iter()
                .any(|field| field.id == key.id && field.row == key.row && field.key == key.key)
        };
        self.invalid = self.invalid.filter(|key| live(key));
        self.pending_reset = self.pending_reset.filter(|key| live(key));
    }

    /// Creates, reuses and drops the colour pickers the settings need this frame.
    ///
    /// Retention follows the same rule as the inputs': a field drawn again keeps
    /// its entity, and a field the configuration no longer holds loses it. What
    /// differs is the colour: a picker whose stored colour moved is written back
    /// rather than rebuilt, because the picker's own state — the tab it is on,
    /// the caret in its hex field — belongs to the user, and `set_color` emits no
    /// change, so writing the stored colour back cannot loop through its handler.
    fn sync_pickers(
        &mut self,
        config: &CandlestickIndicatorConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fields = colors::color_fields(config);
        let labels = colors::color_labels(cx);
        self.pickers
            .retain(|picker| fields.iter().any(|field| field.key() == picker.key()));
        for field in &fields {
            let color = Hsla::from(field.color);
            if let Some(picker) = self
                .pickers
                .iter()
                .find(|picker| picker.key() == field.key())
            {
                picker.entity().update(cx, |picker, cx| {
                    picker.set_labels(labels.clone(), cx);
                    if picker.color() != color {
                        picker.set_color(color, window, cx);
                    }
                });
                continue;
            }
            let slot = self.next_slot;
            self.next_slot += 1;
            self.pickers
                .push(LinePicker::connect(field, slot, &labels, window, cx));
        }
    }

    /// Stores the colour the picker confirmed, and publishes it once.
    ///
    /// The key says which control the picker edits: a line list applies the
    /// colour to that line, and a switch row to the one field its colour key
    /// names. A key the configuration no longer holds is ignored, because the
    /// picker that reported it is about to be dropped with its line.
    ///
    /// The picker reports a colour only when the user confirms it in its panel,
    /// so this is one write per interaction and the widget's own publish is the
    /// only one: a panel dismissed without confirming never reaches here.
    pub(super) fn commit_color(&mut self, key: ColorKey, color: Hsla, cx: &mut Context<Self>) {
        let Some(mut next) = self.indicator_config(cx) else {
            return;
        };
        let applied = match key.row {
            Some(index) => next.set_line_color(key.id, index, LineColor::from(color)),
            None => next.set_color(key.id, key.key, LineColor::from(color)),
        };
        if !applied {
            return;
        }
        if let Some(widget) = self.widget.upgrade() {
            widget.update(cx, |widget, cx| widget.set_indicator_config(next, cx));
        }
    }

    /// The picker of one colour field, if the panel holds it.
    fn color_picker(
        &self,
        id: IndicatorId,
        row: Option<usize>,
        key: SettingKey,
    ) -> Option<&LinePicker> {
        self.pickers
            .iter()
            .find(|picker| picker.key() == ColorKey { id, row, key })
    }

    /// One indicator's card: its node row, and its settings while it is open, in
    /// the one box that holds everything the indicator draws.
    ///
    /// The card is returned whole rather than as the rows it is made of, because
    /// the box is what says "these rows belong to this indicator": an expanded
    /// card that stopped at the node row would draw the settings of one indicator
    /// between the cards of two. Its padding is the inset the node row starts
    /// from; a settings row under it is that much plus `rows::SETTING_INSET`, so
    /// that its first control lands on the line the square inside that checkbox is
    /// drawn on.
    fn indicator_card(
        &self,
        index: usize,
        definition: &'static IndicatorDefinition,
        config: &CandlestickIndicatorConfig,
        panel: &WeakEntity<IndicatorPanel>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let entry = config.entry(definition.id)?;
        let expanded = self.is_expanded(definition.id);
        // Whether the card is showing anything under its node row. An indicator
        // with nothing to configure has no caret to open, so its card is its node
        // row for as long as the panel is on screen: it counts as folded, and it
        // answers the pointer like every other card that is only a title.
        let open = expanded && !definition.rows.is_empty();
        let node = rows::node_row(index, definition, entry.enabled, expanded, panel, cx);
        let mut settings = Vec::with_capacity(definition.rows.len() + 1);
        if open {
            // Enumerated because a period row is named after its place in the
            // definition: two of an indicator's rows can both be periods, so the
            // key alone would not tell their labels apart.
            for (row_index, row) in definition.rows.iter().enumerate() {
                match row.control {
                    SettingControl::LineList { .. } => {
                        let Some(lines) = entry.settings.lines() else {
                            continue;
                        };
                        if let Some(list) = rows::line_list(
                            index,
                            definition.id,
                            row,
                            lines,
                            &self.inputs,
                            &self.pickers,
                            self.invalid,
                            panel,
                            cx,
                        ) {
                            settings.push(list);
                        }
                    }
                    SettingControl::Period { .. } => {
                        let Some(input) = self.line_input(definition.id, None, row.key) else {
                            continue;
                        };
                        settings.push(rows::period_row(
                            row_index,
                            row,
                            input,
                            self.is_invalid(definition.id, None, row.key),
                            cx,
                        ));
                    }
                    SettingControl::Switch => {
                        // The catalogue gives every switch row a colour, which is
                        // what puts a picker on its right. A switch without one
                        // would be a row the panel cannot draw as designed.
                        let color_key = row
                            .color_key
                            .expect("every switch row declares the colour it controls");
                        let Some(picker) = self.color_picker(definition.id, None, color_key) else {
                            continue;
                        };
                        settings.push(rows::switch_row(
                            index,
                            definition.id,
                            row,
                            self.switch(definition.id, row.key, cx),
                            picker,
                            panel,
                            cx,
                        ));
                    }
                }
            }
        }
        let mut children = Vec::with_capacity(settings.len() + 1);
        children.push(node);
        children.extend(settings);
        // The card answers the pointer while it is folded — which includes an
        // indicator that has nothing to open, since its card is only ever a title.
        // An expanded card attaches no listener at all: the settings under the
        // title are controls of their own, and a box around them that lit up would
        // say the whole box is one control.
        Some(card(index, definition.id, !open, children, window, cx))
    }

    /// The entity of one field, if the panel drew it this frame.
    fn line_input(
        &self,
        id: IndicatorId,
        row: Option<usize>,
        key: SettingKey,
    ) -> Option<&LineInput> {
        self.inputs
            .iter()
            .find(|input| input.id == id && input.row == row && input.key == key)
    }

    /// Whether the row of one field is the one in error.
    fn is_invalid(&self, id: IndicatorId, row: Option<usize>, key: SettingKey) -> bool {
        self.invalid == Some(InputKey { id, row, key })
    }
}

/// Reads a period out of the text a field holds.
///
/// `trim` because a field the user leaves with a trailing space holds a number
/// they typed rather than a different answer; the range is inclusive on both
/// ends, which is what the hint's sentence names.
fn parse_period(text: &str, min: u32, max: u32) -> Option<u32> {
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|period| (min..=max).contains(period))
}

/// The colours one card is painted in, given what it is and whether the pointer is
/// on it.
///
/// A pure function so the three states the user asked for are one piece of logic a
/// test can call: a folded card under the pointer, a folded card at rest, and a card
/// whose settings are open (which draws no hover at all). The hover is a veil
/// composited over the resting surface rather than a colour of its own, so one
/// recipe darkens a light surface and lightens a dark one by itself.
pub(super) fn card_colors(roles: &Roles, folded: bool, hovered: bool) -> (Hsla, Hsla) {
    let background = roles.surface.sunken.hsla;
    if !folded || !hovered {
        return (background, roles.line.divider.hsla);
    }
    (
        background.blend(roles.veil.hover.hsla),
        roles.line.strong.hsla,
    )
}

/// One indicator's card: the box holding its node row and everything under it.
///
/// A card is drawn in one of two states, and the difference is not only colour:
/// a folded card answers the pointer (its background and border lift), while an
/// expanded one draws no hover at all — its listener is not attached, so the
/// pointer over it lands on nothing that reacts. That is why `on_hover` is
/// conditional rather than attached always, and why the state it writes lives in
/// the window under `(card_id, "hover")`: the panel is redrawn on every change, so
/// a field on it would be a second copy per frame.
fn card(
    index: usize,
    id: IndicatorId,
    folded: bool,
    children: Vec<AnyElement>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let card_id = rows::card_id(index);
    let hover_state = window.use_keyed_state((card_id.clone(), "hover"), cx, |_, _| false);
    let hovered = *hover_state.read(cx);
    let (background, border) = card_colors(&cx.theme().roles, folded, hovered);
    let bg = transition(
        (card_id.clone(), "bg"),
        background,
        Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
        window,
        cx,
    );
    let border_color = transition(
        (card_id.clone(), "border"),
        border,
        Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
        window,
        cx,
    );

    div()
        .debug_selector(move || rows::card_selector(id))
        .id(card_id)
        .w_full()
        .flex()
        .flex_col()
        .rounded_sm()
        .border_1()
        .border_color(border_color)
        .bg(bg)
        .p(px(CARD_PADDING))
        .children(children)
        .when(folded, |card| {
            card.on_hover(move |hovered, _, cx| hover_state.write(cx, *hovered))
        })
        .into_any_element()
}

impl Render for IndicatorPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let Some(config) = self.indicator_config(cx) else {
            // The chart is gone, so the settings it would edit are too. This
            // entity is dropped with it.
            return div().size_full().min_h_0().into_any_element();
        };
        self.restore_pending(window, cx);
        self.sync_inputs(&config, window, cx);
        self.sync_pickers(&config, window, cx);
        let panel = cx.entity().downgrade();
        let mut content = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(ROW_GAP))
            .p(px(PANEL_PADDING));
        for pane in [IndicatorPane::Main, IndicatorPane::Sub] {
            content = content.child(rows::section_title(pane, t!(cx, section_key(pane)), cx));
            for (index, definition) in INDICATOR_CATALOG.iter().enumerate() {
                if definition.pane != pane {
                    continue;
                }
                let card = self.indicator_card(index, definition, &config, &panel, window, cx);
                content = content.children(card);
            }
        }

        div()
            .debug_selector(|| "TEST_CANDLESTICK_INDICATOR_PANEL".into())
            .relative()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            // The body's own background belongs to the framework; this is a
            // region inside the body, which is allowed to state its own surface.
            .bg(cx.theme().roles.surface.raised.hsla)
            .child(
                div()
                    .id(ElementId::Name(SharedString::new_static(
                        "CandlestickIndicatorPanelScroll",
                    )))
                    .size_full()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(content),
            )
            .child(Scrollbar::vertical(self.scroll.clone()))
            .into_any_element()
    }
}

/// The heading key of one region of the chart.
fn section_key(pane: IndicatorPane) -> &'static str {
    match pane {
        IndicatorPane::Main => "widget.candlestick.indicator.main",
        IndicatorPane::Sub => "widget.candlestick.indicator.sub",
    }
}

#[cfg(test)]
impl IndicatorPanel {
    /// How many frames this panel has drawn.
    pub(super) fn rendered_frames(&self) -> usize {
        self.renders
    }

    /// How many input entities the panel is holding.
    pub(super) fn input_count(&self) -> usize {
        self.inputs.len()
    }

    /// The entity of one field, so that a test can drive what the window would.
    pub(super) fn input_entity(
        &self,
        id: IndicatorId,
        row: Option<usize>,
        key: SettingKey,
    ) -> Option<gpui_kit::Entity<base_ui::input::InputState>> {
        self.line_input(id, row, key)
            .map(|input| input.entity.clone())
    }

    /// How many colour pickers the panel is holding.
    pub(super) fn color_picker_count(&self) -> usize {
        self.pickers.len()
    }

    /// The picker of one colour field, so that a test can drive what a user does.
    pub(super) fn color_picker_entity(
        &self,
        key: ColorKey,
    ) -> Option<gpui_kit::Entity<base_ui::color_picker::ColorPicker>> {
        self.pickers
            .iter()
            .find(|picker| picker.key() == key)
            .map(|picker| picker.entity().clone())
    }
}

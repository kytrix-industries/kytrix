//! A color picker: a stateful view, the events it emits and the panel it presents.
//!
//! [`ColorPicker`] is a view the caller owns. The parent creates it once with `cx.new`, stores
//! the [`Entity<ColorPicker>`] in its own state and subscribes to [`ColorPickerEvent`]; the
//! picker then owns the selected color, the active tab, the localized labels and the hex input
//! the panel draws, so a user action repaints the trigger and the panel without the parent
//! rebuilding either. This is the split [`crate::input::InputState`] uses: the caller holds the
//! entity and listens to it.
//!
//! It reports two events. [`ColorPickerEvent::ColorChanged`] fires on every user change, so a
//! caller can paint the color live; [`ColorPickerEvent::ColorCommitted`] fires once, when the popup
//! closes on a color different from the baseline it opened with, so a caller can persist one value
//! per interaction. [`ColorPicker::set_color`] emits neither: a programmatic change is not a user
//! action.
//!
//! The popup is not the picker's private machinery: [`crate::overlay::Overlay`] owns when the
//! panel is mounted, where it is placed and how it fades, and [`crate::input::InputState`] owns
//! the hex field's editing state. The picker only decides what the trigger and the panel
//! contain, and builds both on every render of its own view.
//!
//! The palette and the four HSLA tracks are drawn by canvases rather than by one element per
//! swatch or slice, which is where the frame cost of a picker usually comes from.
//!
//! Both `gpui-base` and `gpui-component` ship a color picker, and this module deliberately uses
//! neither: it keeps base_ui's [`crate::input::Input`], [`crate::button::Button`], theme roles
//! and event naming (`ColorChanged`), reusing only implementation ideas from those components
//! (hex parsing and formatting, a canvas-drawn palette and canvas-drawn tracks). The imports
//! are explicit for the same reason: `gpui_kit::base` exports its own `ColorPicker` and
//! `ColorPickerEvent`, so a glob import would collide with the types defined here.

use std::{cell::Cell, rc::Rc, sync::LazyLock};

use gpui_kit::{
    AnyElement, App, AppContext, Bounds, Context, ElementId, Empty, Entity, EntityId, EventEmitter,
    Hsla, InteractiveElement, IntoElement, MouseButton, Orientation, ParentElement, Pixels, Point,
    Render, RenderOnce, Rgba, Role, SharedString, StatefulInteractiveElement, Styled, Subscription,
    TextAlign, Window,
    base::{Align, Placement, Selectable, box_shadow},
    canvas, checkerboard, div, fill, hsla, point,
    prelude::FluentBuilder,
    px, rems, size,
};
use smol_str::{SmolStr, format_smolstr};

use crate::{
    button::Button,
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    interactive::InteractiveVariantExt,
    join::Join,
    overlay::Overlay,
    separator::Separator,
    size::{UnifiedSizable, UnifiedSize},
    style::theme::ThemeExt,
};

/// The fixed width every row of the panel is laid out in.
const PANEL_CONTENT_WIDTH: Pixels = px(284.);

/// The height of one HSLA slider row.
const SLIDER_ROW_HEIGHT: Pixels = px(24.);
/// The width of a row's label column.
const SLIDER_LABEL_WIDTH: Pixels = px(72.);
/// The width of a row's numeric value column.
const SLIDER_VALUE_WIDTH: Pixels = px(40.);
/// The horizontal gap between a row's label, its track and its value.
const SLIDER_ROW_INNER_GAP: Pixels = px(8.);
/// The height of a slider's track.
const TRACK_HEIGHT: f32 = 14.0;
/// A track's corner radius; half its height, so its caps are round.
const TRACK_RADIUS: f32 = 7.0;
/// How many solid slices a track is drawn with.
const TRACK_SLICES: usize = 256;
/// The diameter of a slider's thumb.
const THUMB_DIAMETER: f32 = 12.0;

/// The palette's columns: one lightness step each.
const PALETTE_COLS: usize = 12;
/// The palette's rows: one neutral row and eleven hue families.
const PALETTE_ROWS: usize = 12;
/// One swatch's edge.
const PALETTE_CELL: f32 = 20.0;
/// The gap between two neighbouring swatches.
const PALETTE_GAP: f32 = 4.0;
/// The pitch of a swatch: its edge plus the gap that follows it.
const PALETTE_STEP: f32 = 24.0;
/// The palette's edge; the same as [`PANEL_CONTENT_WIDTH`] by design.
const PALETTE_SIZE: f32 =
    PALETTE_CELL * PALETTE_COLS as f32 + PALETTE_GAP * (PALETTE_COLS as f32 - 1.0);
/// The saturation every hue row is built with; the neutral row is unsaturated.
const PALETTE_SATURATION: f32 = 0.72;
/// The twelve lightness steps, dark to light; they are denser at the dark end.
const PALETTE_LIGHTNESS: [f32; PALETTE_COLS] = [
    0.06, 0.12, 0.18, 0.26, 0.34, 0.42, 0.50, 0.60, 0.70, 0.80, 0.90, 0.96,
];

/// The two tabs the panel can show.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub enum ColorPickerTab {
    /// The palette of swatches.
    #[default]
    Palette,
    /// The hue, saturation, lightness and alpha sliders.
    Hsla,
}

/// What a [`ColorPicker`] reports to its subscribers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColorPickerEvent {
    /// A user action changed the color.
    ///
    /// It fires on every user change, so a caller can paint a live preview.
    /// [`ColorPicker::set_color`] does not emit this: a programmatic change is silent.
    ColorChanged(Hsla),
    /// The popup closed on a color different from the baseline it opened with.
    ///
    /// It reports the final color, so a caller can persist once per interaction rather than on
    /// every [`ColorPickerEvent::ColorChanged`]. The baseline is the color the popup opened with;
    /// a programmatic [`ColorPicker::set_color`] while the popup is open moves that baseline, so
    /// closing after only a programmatic change emits nothing.
    ColorCommitted(Hsla),
}

/// Every piece of text the panel shows, so a caller can localize it.
#[derive(Debug, Clone, PartialEq)]
pub struct ColorPickerLabels {
    /// The palette tab's label.
    pub palette: SharedString,
    /// The HSLA tab's label.
    pub hsla: SharedString,
    /// The hue slider's label.
    pub hue: SharedString,
    /// The saturation slider's label.
    pub saturation: SharedString,
    /// The lightness slider's label.
    pub lightness: SharedString,
    /// The alpha slider's label.
    pub alpha: SharedString,
}

impl Default for ColorPickerLabels {
    fn default() -> Self {
        Self {
            palette: SharedString::new_static("Palette"),
            hsla: SharedString::new_static("HSLA"),
            hue: SharedString::new_static("Hue"),
            saturation: SharedString::new_static("Saturation"),
            lightness: SharedString::new_static("Lightness"),
            alpha: SharedString::new_static("Alpha"),
        }
    }
}

/// The color picker: the trigger, the popup and the state they show.
///
/// A parent creates the picker once with `cx.new`, keeps the entity in its own state and
/// subscribes to [`ColorPickerEvent`]:
///
/// ```ignore
/// let picker = cx.new(|cx| ColorPicker::new(accent, UnifiedSize::Small, window, cx));
/// let subscription = cx.subscribe(&picker, |this, _, event: &ColorPickerEvent, cx| {
///     match event {
///         ColorPickerEvent::ColorChanged(color) => {
///             this.accent = *color;
///             cx.notify();
///         }
///         ColorPickerEvent::ColorCommitted(color) => {
///             this.accent = *color;
///             cx.notify();
///         }
///     }
/// });
/// ```
///
/// The picker owns the selected color, the active tab, the labels and the hex field. The popup's
/// placement and dismissal belong to [`Overlay`], and the field's editing state belongs to
/// [`InputState`], so neither is duplicated here. The trigger and the panel are rebuilt on every
/// render of this view, which means a color change, a tab switch, a theme change or the popup
/// opening repaints them, and the caller only has to draw the entity it stored.
pub struct ColorPicker {
    color: Hsla,
    /// The baseline the popup opened with, taken back when it closes to decide whether the user
    /// changed anything worth committing. A programmatic `set_color` while the popup is open moves
    /// it.
    opened_with: Option<Hsla>,
    hovered_swatch: Option<usize>,
    active_tab: ColorPickerTab,
    labels: ColorPickerLabels,
    size: UnifiedSize,
    open: Entity<bool>,
    hex_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl ColorPicker {
    /// Creates a closed picker showing `default_color` at `size`.
    ///
    /// This needs a window because it builds the hex field and subscribes to it. Call it from a
    /// context that owns the entity — typically `cx.new` in the parent's first render — and keep
    /// the returned handle: creating a picker per frame would reset its color, its tab and the
    /// caret in its hex field.
    pub fn new(
        default_color: Hsla,
        size: UnifiedSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The open flag is a plain entity: the trigger writes it and the picker observes it, so a
        // click that opens or dismisses the popup repaints the trigger and mounts or unmounts the
        // panel through the same path a color change takes.
        let open = cx.new(|_| false);
        let input_id: ElementId = ("color-picker-hex", cx.entity_id()).into();
        let hex_input = cx.new(|cx| {
            InputStateBuilder::new(input_id)
                .default_value(hex_string(default_color).as_str())
                .placeholder("#RRGGBB")
                .build(window, cx)
        });

        // The subscription has to outlive `new`: dropping it would leave the field with no one
        // parsing its text. `set_value`, which `write_hex` uses, does not emit a change, so the
        // panel writing the field back never loops through this handler.
        let hex_subscription = cx.subscribe_in(
            &hex_input,
            window,
            |this, _, event: &InputStateEvent, window, cx| match event {
                InputStateEvent::Change(text) => this.on_hex_change(text, window, cx),
                InputStateEvent::PressEnter(text) | InputStateEvent::Blur(text) => {
                    this.on_hex_commit(text, window, cx)
                }
            },
        );
        // Closing the panel has to clear a hover preview as well: the pointer may have left the
        // palette before the outside press, but the footer should still show the real color. The
        // same observer records the color the popup opened with and reports the final one when it
        // closes, so a caller persists once per interaction instead of on every slider tick.
        let open_subscription = cx.observe_in(&open, window, |this, _, window, cx| {
            this.clear_preview(window, cx);
            this.sync_open_state(cx);
            cx.notify();
        });

        Self {
            color: default_color,
            opened_with: None,
            hovered_swatch: None,
            active_tab: ColorPickerTab::default(),
            labels: ColorPickerLabels::default(),
            size,
            open,
            hex_input,
            _subscriptions: vec![hex_subscription, open_subscription],
        }
    }

    /// The color currently selected.
    pub fn color(&self) -> Hsla {
        self.color
    }

    /// The tab the panel currently shows.
    pub fn active_tab(&self) -> ColorPickerTab {
        self.active_tab
    }

    /// Replaces the panel's text.
    ///
    /// Equal labels are a no-op, so a caller may hand the same labels over every frame.
    pub fn set_labels(&mut self, labels: ColorPickerLabels, cx: &mut Context<Self>) {
        if self.labels == labels {
            return;
        }

        self.labels = labels;
        cx.notify();
    }

    /// Sets the color programmatically.
    ///
    /// The hex field is synchronized and the picker repaints, but no
    /// [`ColorPickerEvent::ColorChanged`] is emitted: a programmatic change is not a user action.
    /// If the popup is open, the color also becomes the one it is considered to have opened with,
    /// so closing it emits no [`ColorPickerEvent::ColorCommitted`] either.
    pub fn set_color(&mut self, color: Hsla, window: &mut Window, cx: &mut Context<Self>) {
        self.hovered_swatch = None;
        self.color = color;

        if let Some(opened_with) = self.opened_with.as_mut() {
            *opened_with = color;
        }

        self.write_hex(color, window, cx);
        cx.notify();
    }

    /// Records the color the popup opened with, or emits the one it closed on.
    ///
    /// The open flag is written by the trigger, by a palette choice and by the overlay's outside
    /// press (`overlay.rs`), which is the usual close path, so one interaction emits at most one
    /// [`ColorPickerEvent::ColorCommitted`]: a real open transition stores the color the popup
    /// starts from (a repeated open notify leaves that baseline alone), and closing takes it back
    /// and reports the current color only when the two differ. The flag is copied out before
    /// anything is written, so reading it does not borrow the picker across the mutation.
    fn sync_open_state(&mut self, cx: &mut Context<Self>) {
        let opened = *self.open.read(cx);

        if opened {
            if self.opened_with.is_none() {
                self.opened_with = Some(self.color);
            }

            return;
        }

        let Some(opened_with) = self.opened_with.take() else {
            return;
        };

        if self.color != opened_with {
            cx.emit(ColorPickerEvent::ColorCommitted(self.color));
        }
    }

    /// Switches the panel to `tab`.
    ///
    /// Selecting the tab already shown is a no-op.
    pub fn set_active_tab(&mut self, tab: ColorPickerTab, cx: &mut Context<Self>) {
        if self.active_tab == tab {
            return;
        }

        self.active_tab = tab;
        cx.notify();
    }

    /// Applies a color a user action produced.
    ///
    /// Every user path converges here, so the event contract lives in one place: the event is
    /// emitted only when the color actually changes, and `sync_hex` decides whether the field is
    /// rewritten. Typing in the field passes `false` so the program does not move the caret;
    /// palette and slider interactions pass `true`.
    fn set_user_color(
        &mut self,
        color: Hsla,
        sync_hex: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if color == self.color {
            return;
        }

        self.hovered_swatch = None;
        self.color = color;

        if sync_hex {
            self.write_hex(color, window, cx);
        }

        cx.emit(ColorPickerEvent::ColorChanged(color));
        cx.notify();
    }

    /// Applies the palette swatch at `index` and closes the popup.
    ///
    /// This is a user action, so it goes through [`ColorPicker::set_user_color`]: the event is
    /// emitted only when the swatch differs from the current color, and the hex field is
    /// synchronized. Choosing a swatch ends the interaction, so the panel closes (writing the
    /// open flag also clears the hover preview through the picker's observer). An index outside
    /// the palette is ignored rather than panicking; the only caller passes an index a hit test
    /// from the same palette produced.
    fn select_palette(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(swatch) = PALETTE.get(index) else {
            return;
        };

        self.set_user_color(*swatch, true, window, cx);
        self.open.write(cx, false);
    }

    /// Shows the swatch at `index` in the footer and marks it as hovered.
    ///
    /// This is what a palette hover uses: the large swatch and the hex field follow the pointer,
    /// while the palette draws a ring around the hovered cell. No
    /// [`ColorPickerEvent::ColorChanged`] is emitted and [`ColorPicker::color`] stays the real
    /// selection. Passing `None` restores the footer to the real color and removes the ring.
    fn set_preview_swatch(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = index.filter(|index| *index < PALETTE.len());

        if self.hovered_swatch == index {
            return;
        }

        self.hovered_swatch = index;
        let preview = index.and_then(|index| PALETTE.get(index).copied());
        self.write_hex(preview.unwrap_or(self.color), window, cx);
        cx.notify();
    }

    /// Restores the footer to the real color after a palette hover.
    fn clear_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_preview_swatch(None, window, cx);
    }

    /// Moves one HSLA channel to `t`, leaving the other three untouched.
    ///
    /// `t` is the fraction a slider's track maps a pointer position to, so it is clamped to the
    /// unit range before it becomes a component. Like every other user path this goes through
    /// [`ColorPicker::set_user_color`]: an unchanged color emits nothing, and a change emits
    /// [`ColorPickerEvent::ColorChanged`] and synchronizes the hex field.
    fn set_channel(
        &mut self,
        channel: HslaChannel,
        t: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let t = t.clamp(0., 1.);
        let mut color = self.color;

        match channel {
            HslaChannel::Hue => color.h = t,
            HslaChannel::Saturation => color.s = t,
            HslaChannel::Lightness => color.l = t,
            HslaChannel::Alpha => color.a = t,
        }

        self.set_user_color(color, true, window, cx);
    }

    /// Writes `color` into the hex field in its normalized uppercase form.
    fn write_hex(&mut self, color: Hsla, window: &mut Window, cx: &mut Context<Self>) {
        let hex = hex_string(color);
        self.hex_input.update(cx, |input, cx| {
            input.update_value(hex.as_str(), window, cx);
        });
    }

    /// Handles the text of an in-progress edit.
    ///
    /// A legal value becomes the color but is not written back: the caret and the text the user
    /// typed stay untouched. Illegal text is left alone until the edit is committed.
    fn on_hex_change(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(color) = parse_hex(text) else {
            return;
        };

        self.set_user_color(color, false, window, cx);
    }

    /// Handles the end of an edit, whether Enter was pressed or the field lost focus.
    ///
    /// A legal value is applied and then normalized even when it did not change the color, so
    /// `#fff` becomes `#FFFFFF`; an illegal value is replaced with the current color's hex.
    fn on_hex_commit(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(color) = parse_hex(text) {
            self.set_user_color(color, false, window, cx);
        }

        let color = self.color;
        self.write_hex(color, window, cx);
    }

    /// Builds one of the two tab buttons for `state`.
    ///
    /// The panel is built by the overlay with a plain [`App`], so this is an associated function:
    /// it has no picker borrow to read and routes the click through the entity instead.
    fn tab_button(
        state: Entity<Self>,
        tab: ColorPickerTab,
        active: ColorPickerTab,
        label: SharedString,
    ) -> Button {
        let selected = tab == active;
        let id: ElementId = match tab {
            ColorPickerTab::Palette => ("color-picker-tab-palette", state.entity_id()).into(),
            ColorPickerTab::Hsla => ("color-picker-tab-hsla", state.entity_id()).into(),
        };

        Button::new(id)
            .tonal()
            .small()
            .flex_1()
            .justify_center()
            .selected(selected)
            .aria_selected(selected)
            .debug_selector(move || match tab {
                ColorPickerTab::Palette => "TEST_COLOR_PICKER_TAB_PALETTE".into(),
                ColorPickerTab::Hsla => "TEST_COLOR_PICKER_TAB_HSLA".into(),
            })
            .child(label)
            .on_click(move |_, window, cx| {
                state.update(cx, |state, cx| {
                    state.clear_preview(window, cx);
                    state.set_active_tab(tab, cx);
                });
            })
    }
}

impl EventEmitter<ColorPickerEvent> for ColorPicker {}

impl Render for ColorPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Read the theme through Copy values before anything borrows the context mutably; the
        // panel must paint from the values of this frame, not from a cached theme.
        let (panel_background, panel_border, panel_shadow) = {
            let theme = cx.theme();
            (
                theme.components.menu.background.hsla,
                theme.roles.line.divider.hsla,
                theme.components.menu.shadow.hsla,
            )
        };

        let opened = *self.open.read(cx);
        let picker = cx.entity();
        let picker_entity_id = picker.entity_id();
        let picker_id: ElementId = ("color-picker", picker_entity_id).into();
        let color = self.color;
        let hovered_swatch = self.hovered_swatch;
        let active_tab = self.active_tab;
        let labels = self.labels.clone();
        let size = self.size;
        let hex_input = self.hex_input.clone();

        Overlay::new(
            (picker_id, "overlay"),
            self.open.clone(),
            move |open_state, _, _| {
                Button::new(("color-picker-trigger", picker_entity_id))
                    .tonal()
                    // `with_unified_size` fixes the height but leaves the width automatic, so
                    // the horizontal padding and borders were added on top of the swatch and the
                    // button was 22/32/42 wide against a 20/30/40 height. Size the button to the
                    // UnifiedSize square instead, keep the same 2px padding at every size, and
                    // let the swatch fill what is left of the content box.
                    .size(size.h())
                    .p_0p5()
                    .selected(opened)
                    .aria_label(hex_string(color).as_str())
                    .debug_selector(|| "TEST_COLOR_PICKER_TRIGGER".into())
                    .child(
                        ColorWell::new(color)
                            .size_full()
                            .radius(px(4.))
                            .debug_selector("TEST_COLOR_PICKER_TRIGGER_WELL"),
                    )
                    .when(!opened, |this| {
                        this.on_click(move |_, _, cx| open_state.write(cx, true))
                    })
                    .into_any_element()
            },
            move |_, _, _| {
                render_panel(
                    picker,
                    color,
                    hovered_swatch,
                    active_tab,
                    labels,
                    hex_input,
                    panel_background,
                    panel_border,
                    panel_shadow,
                )
            },
        )
        .placement(Placement::Bottom)
        .align(Align::Start)
    }
}

/// Builds the popup panel: the tabs, the selected tab's content and the hex footer.
///
/// This is a free function because the popup is built by the [`Overlay`] with a plain [`App`],
/// where the picker's own context is not available. Everything the panel draws or calls back
/// into is handed in: the picker entity routes the interactions, and the hex field is the
/// [`InputState`] the picker owns.
fn render_panel(
    state: Entity<ColorPicker>,
    color: Hsla,
    hovered_swatch: Option<usize>,
    active: ColorPickerTab,
    labels: ColorPickerLabels,
    hex_input: Entity<InputState>,
    panel_background: Hsla,
    panel_border: Hsla,
    panel_shadow: Hsla,
) -> AnyElement {
    let content: AnyElement = match active {
        ColorPickerTab::Palette => {
            PaletteGrid::new(state.clone(), labels.palette.clone(), hovered_swatch)
                .into_any_element()
        }
        ColorPickerTab::Hsla => div()
            .flex()
            .flex_col()
            .gap_2()
            .child(HslaSlider::new(
                state.clone(),
                HslaChannel::Hue,
                color,
                HslaChannel::Hue.label(&labels),
            ))
            .child(HslaSlider::new(
                state.clone(),
                HslaChannel::Saturation,
                color,
                HslaChannel::Saturation.label(&labels),
            ))
            .child(HslaSlider::new(
                state.clone(),
                HslaChannel::Lightness,
                color,
                HslaChannel::Lightness.label(&labels),
            ))
            .child(HslaSlider::new(
                state.clone(),
                HslaChannel::Alpha,
                color,
                HslaChannel::Alpha.label(&labels),
            ))
            .into_any_element(),
    };

    div()
        .p_2()
        .bg(panel_background)
        .border_1()
        .border_color(panel_border)
        .rounded_sm()
        .shadow(vec![
            box_shadow(0., 1., 2., 0., panel_shadow),
            box_shadow(0., 3., 6., -2., panel_shadow),
        ])
        .debug_selector(|| "TEST_COLOR_PICKER_PANEL".into())
        .child(
            div()
                .w(PANEL_CONTENT_WIDTH)
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    Join::default()
                        .w_full()
                        .button(ColorPicker::tab_button(
                            state.clone(),
                            ColorPickerTab::Palette,
                            active,
                            labels.palette.clone(),
                        ))
                        .button(ColorPicker::tab_button(
                            state,
                            ColorPickerTab::Hsla,
                            active,
                            labels.hsla,
                        )),
                )
                .child(content)
                .child(Separator::horizontal())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .debug_selector(|| "TEST_COLOR_PICKER_HEX_INPUT".into())
                        .child(
                            ColorWell::new(
                                hovered_swatch
                                    .and_then(|index| PALETTE.get(index).copied())
                                    .unwrap_or(color),
                            )
                            .size(px(30.))
                            .radius(px(6.)),
                        )
                        .child(div().flex_1().child(Input::new(&hex_input).medium())),
                ),
        )
        .into_any_element()
}

/// A small color block: the picker's trigger and its footer preview.
///
/// It paints everything in one canvas so a swatch costs one element, and a translucent color is
/// shown over a checkerboard rather than over whatever happens to be behind the block. It either
/// has a fixed size or fills its parent, depending on which builder the caller used.
#[derive(IntoElement)]
struct ColorWell {
    color: Hsla,
    size: Option<Pixels>,
    radius: Pixels,
    debug_selector: Option<&'static str>,
}

impl ColorWell {
    /// A well showing `color`; callers choose a fixed size or let it fill its parent.
    fn new(color: Hsla) -> Self {
        Self {
            color,
            size: Some(px(18.)),
            radius: px(4.),
            debug_selector: None,
        }
    }

    /// Sets the size of the block.
    fn size(mut self, size: Pixels) -> Self {
        self.size = Some(size);
        self
    }

    /// Makes the block fill its parent instead of using a fixed size.
    fn size_full(mut self) -> Self {
        self.size = None;
        self
    }

    /// Sets the corner radius of the block.
    fn radius(mut self, radius: Pixels) -> Self {
        self.radius = radius;
        self
    }

    /// Sets the test selector on the block.
    fn debug_selector(mut self, selector: &'static str) -> Self {
        self.debug_selector = Some(selector);
        self
    }
}

impl RenderOnce for ColorWell {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            color,
            size,
            radius,
            debug_selector,
        } = self;
        let (base, checker) = {
            let theme = cx.theme();
            (
                theme.roles.surface.overlay.hsla,
                theme.roles.surface.sunken.hsla,
            )
        };
        // A color that is not fully opaque shows the checkerboard, exactly as the alpha slider
        // reads it; the hex field's eight-bit rounding is a display detail, not this test.
        let translucent = color.a < 1.0;

        let well = div().relative();
        let well = match size {
            Some(size) => well.w(size).h(size),
            None => well.size_full(),
        };
        let well = well.when_some(debug_selector, |this, selector| {
            this.debug_selector(move || selector.into())
        });

        well.child(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    window.paint_quad(fill(bounds, base).corner_radii(radius));

                    if translucent {
                        window.paint_quad(
                            fill(bounds, checkerboard(checker, 4.0)).corner_radii(radius),
                        );
                    }

                    window.paint_quad(fill(bounds, color).corner_radii(radius));
                },
            )
            .absolute()
            .size_full(),
        )
    }
}

/// The 12×12 palette of swatches, drawn by a single canvas.
///
/// The picker entity only routes hover and click back to it; the label it announces is handed in
/// by the panel, so this element never reads the picker while rendering.
#[derive(IntoElement)]
struct PaletteGrid {
    state: Entity<ColorPicker>,
    label: SharedString,
    hovered: Option<usize>,
}

impl PaletteGrid {
    /// Builds the grid for `state`, outlining the hovered swatch if there is one.
    fn new(state: Entity<ColorPicker>, label: SharedString, hovered: Option<usize>) -> Self {
        Self {
            state,
            label,
            hovered,
        }
    }
}

impl RenderOnce for PaletteGrid {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let Self {
            state,
            label,
            hovered,
        } = self;
        let bounds = Rc::new(Cell::new(Bounds::default()));
        let prepaint_bounds = bounds.clone();

        div()
            .relative()
            .w(px(PALETTE_SIZE))
            .h(px(PALETTE_SIZE))
            .id(("color-picker-palette", state.entity_id()))
            .role(Role::Group)
            .aria_label(label)
            .cursor_pointer()
            .debug_selector(|| "TEST_COLOR_PICKER_GRID".into())
            .child(
                canvas(
                    move |bounds, _, _| prepaint_bounds.set(bounds),
                    move |bounds, (), window, _| {
                        // The cells are scaled exactly as `palette_index_at` scales its hit test:
                        // the grid is laid out PALETTE_SIZE wide, but the canvas may be measured
                        // at another size, and drawing at the nominal geometry would then
                        // disagree with the hit areas.
                        let scale = (bounds.size.width.as_f32() / PALETTE_SIZE)
                            .min(bounds.size.height.as_f32() / PALETTE_SIZE);
                        let cell_radius = px(4.0 * scale);
                        let ring_inset = px(scale);
                        let ring_width = px(2.0 * scale);
                        let ring_radius = px(3.0 * scale);

                        for (index, color) in PALETTE.iter().enumerate() {
                            let cell = palette_cell_bounds(
                                bounds,
                                index / PALETTE_COLS,
                                index % PALETTE_COLS,
                            );

                            window.paint_quad(fill(cell, *color).corner_radii(cell_radius));

                            if hovered == Some(index) {
                                let inner = Bounds::new(
                                    point(cell.origin.x + ring_inset, cell.origin.y + ring_inset),
                                    size(
                                        cell.size.width - ring_inset * 2.0,
                                        cell.size.height - ring_inset * 2.0,
                                    ),
                                );
                                window.paint_quad(
                                    fill(inner, hsla(0., 0., 0., 0.))
                                        .border_widths(ring_width)
                                        .border_color(hover_ring_color(*color))
                                        .corner_radii(ring_radius),
                                );
                            }
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
            .on_mouse_move({
                let bounds = bounds.clone();
                let state = state.clone();

                move |event, window, cx| {
                    // A move that is still inside the grid but not over a cell is over a gap.
                    // Keeping the previous preview and ring there avoids the A -> real color -> B
                    // flash the pointer would otherwise produce while crossing two swatches.
                    let Some(index) = palette_index_at(bounds.get(), event.position) else {
                        return;
                    };

                    state.update(cx, |state, cx| {
                        state.set_preview_swatch(Some(index), window, cx)
                    });
                }
            })
            .on_hover({
                let state = state.clone();

                move |hovered, window, cx| {
                    // Leaving the grid is the only pointer move that restores the real color;
                    // gaps between its cells are still inside this hitbox.
                    if !*hovered {
                        state.update(cx, |state, cx| state.clear_preview(window, cx));
                    }
                }
            })
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                let Some(index) = palette_index_at(bounds.get(), event.position) else {
                    return;
                };

                state.update(cx, |state, cx| state.select_palette(index, window, cx));
            })
    }
}

/// The color of the ring around the hovered swatch.
///
/// The ring has to read at both ends of the ramp, so it flips with the swatch's luma rather than
/// with the theme: a light swatch gets a dark ring and a dark swatch a light one.
fn hover_ring_color(color: Hsla) -> Hsla {
    let [r, g, b, _] = rgba8(color);
    let luma = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;

    if luma > 140.0 {
        hsla(0., 0., 0., 0.85)
    } else {
        hsla(0., 0., 1., 0.95)
    }
}

/// Which component of an [`Hsla`] a slider edits.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum HslaChannel {
    Hue,
    Saturation,
    Lightness,
    Alpha,
}

impl HslaChannel {
    /// The element id prefix of the channel's track.
    fn id_prefix(self) -> &'static str {
        match self {
            Self::Hue => "color-picker-hue-track",
            Self::Saturation => "color-picker-saturation-track",
            Self::Lightness => "color-picker-lightness-track",
            Self::Alpha => "color-picker-alpha-track",
        }
    }

    /// The debug selector of the channel's track.
    fn debug_selector(self) -> &'static str {
        match self {
            Self::Hue => "TEST_COLOR_PICKER_HUE_TRACK",
            Self::Saturation => "TEST_COLOR_PICKER_SATURATION_TRACK",
            Self::Lightness => "TEST_COLOR_PICKER_LIGHTNESS_TRACK",
            Self::Alpha => "TEST_COLOR_PICKER_ALPHA_TRACK",
        }
    }

    /// The channel's label, taken from the panel's labels.
    fn label(self, labels: &ColorPickerLabels) -> SharedString {
        match self {
            Self::Hue => labels.hue.clone(),
            Self::Saturation => labels.saturation.clone(),
            Self::Lightness => labels.lightness.clone(),
            Self::Alpha => labels.alpha.clone(),
        }
    }

    /// The component of `color` the channel edits, as the `0..=1` fraction a track maps to.
    fn value(self, color: Hsla) -> f32 {
        match self {
            Self::Hue => color.h,
            Self::Saturation => color.s,
            Self::Lightness => color.l,
            Self::Alpha => color.a,
        }
    }

    /// The number the channel shows and announces: degrees for hue, percent for the rest.
    fn display_value(self, color: Hsla) -> i32 {
        match self {
            Self::Hue => (self.value(color) * 360.0).round() as i32,
            Self::Saturation | Self::Lightness | Self::Alpha => {
                (self.value(color) * 100.0).round() as i32
            }
        }
    }

    /// The channel's value text.
    fn value_text(self, color: Hsla) -> SharedString {
        SharedString::new(format_smolstr!("{}", self.display_value(color)))
    }
}

/// The payload of a track drag, so a drag only updates the track it started on.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
struct HslaDrag {
    picker: EntityId,
    channel: HslaChannel,
}

/// The preview of a track drag: empty, so dragging a track paints no ghost.
struct HslaDragPreview;

impl Render for HslaDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// One hue, saturation, lightness or alpha slider row.
///
/// The track is a single canvas: [`TRACK_SLICES`] solid slices and one thumb, not a run of
/// per-slice elements and not a gradient. The picker entity is only used to route pointer events
/// back to it; the color and the text the row draws are handed in by the panel.
#[derive(IntoElement)]
struct HslaSlider {
    state: Entity<ColorPicker>,
    channel: HslaChannel,
    color: Hsla,
    label: SharedString,
}

impl HslaSlider {
    /// Builds the row for `channel`, reading the current `color`.
    fn new(
        state: Entity<ColorPicker>,
        channel: HslaChannel,
        color: Hsla,
        label: SharedString,
    ) -> Self {
        Self {
            state,
            channel,
            color,
            label,
        }
    }
}

impl RenderOnce for HslaSlider {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            state,
            channel,
            color,
            label,
        } = self;
        // The track's paint callback has to be `'static`, so the two theme colors it needs are
        // copied out here rather than borrowed from the theme inside it.
        let (label_color, thumb_fill) = {
            let theme = cx.theme();
            (theme.roles.fg.muted.hsla, theme.roles.surface.overlay.hsla)
        };
        let thumb_border = hsla(0., 0., 0., 0.35);
        let value = channel.value(color);
        let display_value = channel.display_value(color);
        let value_text = channel.value_text(color);
        let max = match channel {
            HslaChannel::Hue => 360.,
            HslaChannel::Saturation | HslaChannel::Lightness | HslaChannel::Alpha => 100.,
        };
        let bounds = Rc::new(Cell::new(Bounds::default()));
        let prepaint_bounds = bounds.clone();
        let picker = state.entity_id();
        let drag_state = state.clone();

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(SLIDER_ROW_INNER_GAP)
            .h(SLIDER_ROW_HEIGHT)
            .child(
                div()
                    .w(SLIDER_LABEL_WIDTH)
                    .text_size(rems(0.75))
                    .text_color(label_color)
                    .child(label.clone()),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .h(px(TRACK_HEIGHT))
                    .id((channel.id_prefix(), picker))
                    .role(Role::Slider)
                    .aria_label(label)
                    .aria_numeric_value(display_value as f64)
                    .aria_min_numeric_value(0.)
                    .aria_max_numeric_value(max)
                    .aria_orientation(Orientation::Horizontal)
                    .cursor_pointer()
                    .debug_selector(move || channel.debug_selector().into())
                    .child(
                        canvas(
                            move |bounds, _, _| prepaint_bounds.set(bounds),
                            move |bounds, (), window, _| {
                                paint_track(
                                    window,
                                    bounds,
                                    channel,
                                    color,
                                    value,
                                    thumb_fill,
                                    thumb_border,
                                );
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
                    .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                        let bounds = bounds.get();
                        let width = bounds.size.width.as_f32();
                        if width <= 0.0 {
                            return;
                        }

                        let t = ((event.position.x - bounds.left()).as_f32() / width).clamp(0., 1.);
                        state.update(cx, |state, cx| state.set_channel(channel, t, window, cx));
                    })
                    .on_drag(HslaDrag { picker, channel }, |_, _, _, cx| {
                        cx.new(|_| HslaDragPreview)
                    })
                    .on_drag_move::<HslaDrag>(move |event, window, cx| {
                        let drag = *event.drag(cx);
                        if drag.picker != picker || drag.channel != channel {
                            return;
                        }

                        // The element's own bounds, not the prepaint cell: the pointer may be
                        // anywhere in the window, and `event.bounds` is where this track was
                        // laid out.
                        let bounds = event.bounds;
                        let width = bounds.size.width.as_f32();
                        if width <= 0.0 {
                            return;
                        }

                        let t = ((event.event.position.x - bounds.left()).as_f32() / width)
                            .clamp(0., 1.);
                        drag_state
                            .update(cx, |state, cx| state.set_channel(channel, t, window, cx));
                    }),
            )
            .child(
                div()
                    .w(SLIDER_VALUE_WIDTH)
                    .text_size(rems(0.75))
                    .text_color(label_color)
                    .text_align(TextAlign::Right)
                    .child(value_text),
            )
    }
}

/// Paints one track: [`TRACK_SLICES`] solid slices with rounded caps, then the thumb.
///
/// Every slice is a plain quad, so no gradient shader is involved and the color is exact on every
/// backend. Near the two ends a slice shrinks from both sides by the sagitta of the track's
/// radius, which draws the caps without needing a clip.
fn paint_track(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    channel: HslaChannel,
    base: Hsla,
    value: f32,
    thumb_fill: Hsla,
    thumb_border: Hsla,
) {
    let left = bounds.left().as_f32();
    let top = bounds.top().as_f32();
    let width = bounds.size.width.as_f32();
    let height = bounds.size.height.as_f32();

    if width <= 0.0 || height <= 0.0 {
        return;
    }

    for index in 0..TRACK_SLICES {
        let t0 = index as f32 / TRACK_SLICES as f32;
        let t1 = (index + 1) as f32 / TRACK_SLICES as f32;
        let t = (t0 + t1) / 2.0;
        let color = match channel {
            HslaChannel::Hue => hsla(t, 1.0, 0.5, 1.0),
            HslaChannel::Saturation => hsla(base.h, t, base.l, 1.0),
            HslaChannel::Lightness => hsla(base.h, base.s, t, 1.0),
            HslaChannel::Alpha => hsla(base.h, base.s, base.l, t),
        };
        let x0 = left + width * t0;
        let x1 = left + width * t1;
        let center = (x0 + x1) / 2.0;
        let distance = (center - left).min((left + width) - center);
        let inset = if distance < TRACK_RADIUS {
            TRACK_RADIUS
                - (TRACK_RADIUS * TRACK_RADIUS
                    - (TRACK_RADIUS - distance) * (TRACK_RADIUS - distance))
                    .sqrt()
        } else {
            0.0
        };
        let slice_height = height - inset * 2.0;

        if slice_height <= 0.0 {
            continue;
        }

        window.paint_quad(fill(
            Bounds::new(
                point(px(x0), px(top + inset)),
                size(px(x1 - x0), px(slice_height)),
            ),
            color,
        ));
    }

    // A track narrower than the thumb has no room for it, and the two clamp bounds would cross.
    // The slices above are what carries the track's color, so a narrow track can simply do
    // without the thumb instead of panicking on an inverted range.
    if width < THUMB_DIAMETER {
        return;
    }

    let half = THUMB_DIAMETER / 2.0;
    let x = (left + width * value).clamp(left + half, left + width - half);
    let y = top + height / 2.0;

    window.paint_quad(
        fill(
            Bounds::new(
                point(px(x - half), px(y - half)),
                size(px(THUMB_DIAMETER), px(THUMB_DIAMETER)),
            ),
            thumb_fill,
        )
        .border_widths(px(1.))
        .border_color(thumb_border)
        .corner_radii(px(half)),
    );
}

/// The bounds of one palette cell inside a canvas measured at `bounds`.
///
/// The grid is scaled as a whole, exactly like [`palette_index_at`]: it is laid out
/// [`PALETTE_SIZE`] wide, and every cell and gap keeps its proportion when the canvas is measured
/// at another size.
fn palette_cell_bounds(bounds: Bounds<Pixels>, row: usize, col: usize) -> Bounds<Pixels> {
    let scale_x = bounds.size.width.as_f32() / PALETTE_SIZE;
    let scale_y = bounds.size.height.as_f32() / PALETTE_SIZE;
    let origin = point(
        bounds.left() + px(PALETTE_STEP * col as f32 * scale_x),
        bounds.top() + px(PALETTE_STEP * row as f32 * scale_y),
    );

    Bounds::new(
        origin,
        size(px(PALETTE_CELL * scale_x), px(PALETTE_CELL * scale_y)),
    )
}

/// Quantizes `color` to the eight-bit sRGB channels the hex notation can express.
fn rgba8(color: Hsla) -> [u8; 4] {
    let rgba = Rgba::from(color);
    let quantize = |value: f32| (value.clamp(0., 1.) * 255.0).round() as u8;

    [
        quantize(rgba.r),
        quantize(rgba.g),
        quantize(rgba.b),
        quantize(rgba.a),
    ]
}

/// Formats `color` as uppercase `#RRGGBB`, or `#RRGGBBAA` when the quantized alpha is not opaque.
///
/// Alpha is compared after quantization, so a color that displays as opaque never shows an alpha
/// component. Rounding rather than truncation keeps the displayed value faithful to the color:
/// truncation would turn `0.999` into `254`.
fn hex_string(color: Hsla) -> SmolStr {
    let [r, g, b, a] = rgba8(color);

    if a < 255 {
        format_smolstr!("#{r:02X}{g:02X}{b:02X}{a:02X}")
    } else {
        format_smolstr!("#{r:02X}{g:02X}{b:02X}")
    }
}

/// Parses `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` into a color.
///
/// The leading hash is optional, the text is trimmed and the digits are case-insensitive. Short
/// forms duplicate each nibble (`#f00` is `#ff0000`), as the CSS notation defines, rather than
/// scaling it. Anything else — the wrong length, a non-hex character, a sign, a prefix — is
/// rejected, because the field advertises hex and promises nothing more.
fn parse_hex(text: &str) -> Option<Hsla> {
    let text = text.trim();
    let text = text.strip_prefix('#').unwrap_or(text);

    if !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }

    let (width, has_alpha) = match text.len() {
        3 => (1, false),
        4 => (1, true),
        6 => (2, false),
        8 => (2, true),
        _ => return None,
    };

    let component = |index: usize| -> Option<f32> {
        let start = index * width;
        let raw = u8::from_str_radix(&text[start..start + width], 16).ok()?;
        let raw = if width == 1 { raw * 0x11 } else { raw };

        Some(raw as f32 / 255.0)
    };

    Some(Hsla::from(Rgba {
        r: component(0)?,
        g: component(1)?,
        b: component(2)?,
        a: if has_alpha { component(3)? } else { 1.0 },
    }))
}

/// The hue of a palette row, or `None` for the neutral row.
fn palette_hue(row: usize) -> Option<f32> {
    (row > 0).then(|| (row - 1) as f32 / (PALETTE_ROWS as f32 - 1.0))
}

/// The twelve by twelve palette, generated once.
///
/// Row 0 is the neutral ramp and each following row is one hue family; each column is one
/// lightness step. The table is a process-wide static because every picker shares the same
/// generated colors.
static PALETTE: LazyLock<[Hsla; PALETTE_ROWS * PALETTE_COLS]> = LazyLock::new(|| {
    std::array::from_fn(|index| {
        let row = index / PALETTE_COLS;
        let col = index % PALETTE_COLS;

        match palette_hue(row) {
            Some(hue) => hsla(hue, PALETTE_SATURATION, PALETTE_LIGHTNESS[col], 1.0),
            None => hsla(0., 0., PALETTE_LIGHTNESS[col], 1.0),
        }
    })
});

/// Maps a window position to the palette swatch under it.
///
/// The grid is scaled as a whole: it is laid out [`PALETTE_SIZE`] wide, and a canvas measured at
/// any other size keeps the same twelve cells and eleven gaps. A position in a gap, outside the
/// grid, or in the trailing space past the last cell is not a hit.
fn palette_index_at(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<usize> {
    let width = bounds.size.width.as_f32();
    let height = bounds.size.height.as_f32();
    let x = (position.x - bounds.left()).as_f32();
    let y = (position.y - bounds.top()).as_f32();

    if !(0.0..width).contains(&x) || !(0.0..height).contains(&y) {
        return None;
    }

    // The pitch is 24, not `PALETTE_SIZE / 12`: twelve cells and eleven gaps leave no trailing
    // gap, and `PALETTE_SIZE / 12` would drift the rightmost column.
    let scale_x = width / PALETTE_SIZE;
    let scale_y = height / PALETTE_SIZE;
    let step_x = PALETTE_STEP * scale_x;
    let step_y = PALETTE_STEP * scale_y;
    let cell_x = PALETTE_CELL * scale_x;
    let cell_y = PALETTE_CELL * scale_y;
    let col = (x / step_x).floor() as usize;
    let row = (y / step_y).floor() as usize;

    if col >= PALETTE_COLS || row >= PALETTE_ROWS {
        return None;
    }

    let within_x = x - col as f32 * step_x;
    let within_y = y - row as f32 * step_y;

    if within_x >= cell_x || within_y >= cell_y {
        return None;
    }

    Some(row * PALETTE_COLS + col)
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    use gpui_kit::{
        AppContext, Bounds, Context, Entity, Hsla, IntoElement, KeyDownEvent, Keystroke, Modifiers,
        MouseButton, ParentElement, Pixels, PlatformInput, Point, Render, SharedString, Styled,
        Subscription, TestAppContext, VisualTestContext, Window, canvas, div, hsla, point, px,
        size,
    };

    use super::{
        ColorPicker, ColorPickerEvent, ColorPickerLabels, ColorPickerTab, HslaChannel, PALETTE,
        PALETTE_COLS, PALETTE_LIGHTNESS, PALETTE_ROWS, PALETTE_SATURATION, PALETTE_SIZE,
        PALETTE_STEP, PANEL_CONTENT_WIDTH, SLIDER_LABEL_WIDTH, SLIDER_ROW_HEIGHT,
        SLIDER_ROW_INNER_GAP, SLIDER_VALUE_WIDTH, THUMB_DIAMETER, TRACK_HEIGHT, TRACK_RADIUS,
        TRACK_SLICES, hex_string, hover_ring_color, paint_track, palette_cell_bounds, palette_hue,
        palette_index_at, parse_hex, rgba8,
    };
    use crate::{
        animation::ANIMATION_TICK,
        size::UnifiedSize,
        style::{
            palette::Appearance,
            theme::{self, Theme},
        },
    };

    #[test]
    fn the_hover_ring_should_contrast_with_the_swatch() {
        assert_eq!(
            hover_ring_color(hsla(0., 0., 1., 1.)),
            hsla(0., 0., 0., 0.85),
            "a white swatch gets a dark ring"
        );
        assert_eq!(
            hover_ring_color(hsla(0., 0., 0., 1.)),
            hsla(0., 0., 1., 0.95),
            "a black swatch gets a light ring"
        );
        assert_eq!(
            hover_ring_color(PALETTE[PALETTE_COLS - 1]),
            hsla(0., 0., 0., 0.85),
            "the light end of the ramp gets a dark ring"
        );
        assert_eq!(
            hover_ring_color(PALETTE[0]),
            hsla(0., 0., 1., 0.95),
            "the dark end of the ramp gets a light ring"
        );
    }

    #[test]
    fn the_palette_should_be_twelve_lightness_steps_by_twelve_rows() {
        assert_eq!(PALETTE.len(), 144);
        assert_eq!(PALETTE.len(), PALETTE_ROWS * PALETTE_COLS);
        assert_eq!(PALETTE_SIZE, 284., "twelve 20px cells and eleven 4px gaps");
        assert_eq!(
            PANEL_CONTENT_WIDTH.as_f32(),
            PALETTE_SIZE,
            "the palette fills the panel's content width exactly"
        );
        assert!(
            PALETTE_LIGHTNESS.windows(2).all(|pair| pair[0] < pair[1]),
            "the lightness column runs dark to light"
        );

        for col in 0..PALETTE_COLS {
            let color = PALETTE[col];

            assert_eq!(color.s, 0.0, "row 0 is the neutral ramp");
            assert_eq!(color.l, PALETTE_LIGHTNESS[col]);
            assert_eq!(color.a, 1.0);
        }

        for row in 1..PALETTE_ROWS {
            let hue = palette_hue(row).expect("every row after the neutral one has a hue");
            assert!((hue - (row - 1) as f32 / (PALETTE_ROWS - 1) as f32).abs() < 1e-6);

            for col in 0..PALETTE_COLS {
                let color = PALETTE[row * PALETTE_COLS + col];

                assert_eq!(color.h, hue);
                assert_eq!(color.s, PALETTE_SATURATION);
                assert_eq!(color.l, PALETTE_LIGHTNESS[col]);
                assert_eq!(color.a, 1.0);
            }
        }

        assert_eq!(palette_hue(0), None);
    }

    fn palette_bounds(origin: Point<Pixels>, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(origin, size(px(width), px(height)))
    }

    fn cell_center(row: usize, col: usize) -> Point<Pixels> {
        point(
            px(PALETTE_STEP * col as f32 + 10.),
            px(PALETTE_STEP * row as f32 + 10.),
        )
    }

    #[test]
    fn palette_index_at_should_map_a_cell_center_to_its_index() {
        let grid = palette_bounds(point(px(0.), px(0.)), PALETTE_SIZE, PALETTE_SIZE);

        assert_eq!(palette_index_at(grid, cell_center(2, 4)), Some(28));
        assert_eq!(palette_index_at(grid, cell_center(0, 0)), Some(0));
    }

    #[test]
    fn palette_index_at_should_hit_the_last_cell_and_reject_its_trailing_edge() {
        let grid = palette_bounds(point(px(0.), px(0.)), PALETTE_SIZE, PALETTE_SIZE);

        assert_eq!(
            palette_index_at(grid, cell_center(PALETTE_ROWS - 1, PALETTE_COLS - 1)),
            Some(143)
        );
        assert_eq!(
            palette_index_at(grid, point(px(PALETTE_SIZE), px(PALETTE_SIZE))),
            None,
            "the bottom-right corner is outside the grid's half-open bounds"
        );
    }

    #[test]
    fn palette_index_at_should_reject_the_gap_between_cells() {
        let grid = palette_bounds(point(px(0.), px(0.)), PALETTE_SIZE, PALETTE_SIZE);

        assert_eq!(
            palette_index_at(grid, point(px(22.), px(10.))),
            None,
            "x = 22 lies in the gap after the first 20px cell"
        );
        assert_eq!(
            palette_index_at(grid, point(px(10.), px(22.))),
            None,
            "y = 22 lies in the gap after the first row"
        );
    }

    #[test]
    fn palette_index_at_should_reject_positions_outside_the_grid() {
        let grid = palette_bounds(point(px(0.), px(0.)), PALETTE_SIZE, PALETTE_SIZE);

        for position in [
            point(px(PALETTE_SIZE), px(10.)),
            point(px(10.), px(PALETTE_SIZE)),
            point(px(-1.), px(10.)),
            point(px(10.), px(-1.)),
        ] {
            assert_eq!(palette_index_at(grid, position), None, "{position:?}");
        }
    }

    #[test]
    fn palette_index_at_should_follow_a_scaled_and_offset_grid() {
        let grid = palette_bounds(
            point(px(100.), px(50.)),
            PALETTE_SIZE / 2.0,
            PALETTE_SIZE / 2.0,
        );
        let pitch = PALETTE_STEP / 2.0;
        let half_cell = 10.0 / 2.0;

        assert_eq!(
            palette_index_at(
                grid,
                point(
                    px(100. + 4.0 * pitch + half_cell),
                    px(50. + 2.0 * pitch + half_cell),
                ),
            ),
            Some(28),
            "the same cell maps to the same index in a scaled and offset canvas"
        );
    }

    #[test]
    fn palette_index_at_should_pin_the_24_pixel_pitch_to_the_right_edge() {
        let grid = palette_bounds(point(px(0.), px(0.)), PALETTE_SIZE, PALETTE_SIZE);
        // The vertical centre of the last row; the last cell spans x = 264..284, so its index is
        // 11 * 12 + 11 = 143.
        let y = px(PALETTE_STEP * (PALETTE_ROWS - 1) as f32 + 10.);
        assert_eq!(y, px(274.));

        assert_eq!(
            palette_index_at(grid, point(px(263.999), y)),
            None,
            "the 4px gap before the last cell ends where 264 starts"
        );
        assert_eq!(
            palette_index_at(grid, point(px(264.0), y)),
            Some(143),
            "the last cell starts at 264, not at PALETTE_SIZE / 12 * 11"
        );
        assert_eq!(
            palette_index_at(grid, point(px(283.999), y)),
            Some(143),
            "the last cell reaches 284"
        );
        assert_eq!(
            palette_index_at(grid, point(px(284.0), y)),
            None,
            "the grid's right edge is exclusive"
        );
        // The same boundaries one pitch to the left: column 10 starts at 240.
        assert_eq!(palette_index_at(grid, point(px(239.999), y)), None);
        assert_eq!(palette_index_at(grid, point(px(240.0), y)), Some(142));
    }

    #[test]
    fn drawn_cells_should_map_back_to_their_own_index_at_any_scale() {
        for edge in [PALETTE_SIZE, PALETTE_SIZE / 2.0] {
            let grid = palette_bounds(point(px(100.), px(50.)), edge, edge);

            for (index, _) in PALETTE.iter().enumerate() {
                let cell = palette_cell_bounds(grid, index / PALETTE_COLS, index % PALETTE_COLS);

                assert_eq!(
                    palette_index_at(grid, cell.center()),
                    Some(index),
                    "the centre of the drawn cell {index} should hit that cell at {edge}px"
                );
            }
        }
    }

    #[test]
    fn hsla_channel_should_name_its_track_and_selector() {
        assert_eq!(HslaChannel::Hue.id_prefix(), "color-picker-hue-track");
        assert_eq!(
            HslaChannel::Saturation.id_prefix(),
            "color-picker-saturation-track"
        );
        assert_eq!(
            HslaChannel::Lightness.id_prefix(),
            "color-picker-lightness-track"
        );
        assert_eq!(HslaChannel::Alpha.id_prefix(), "color-picker-alpha-track");

        assert_eq!(
            HslaChannel::Hue.debug_selector(),
            "TEST_COLOR_PICKER_HUE_TRACK"
        );
        assert_eq!(
            HslaChannel::Saturation.debug_selector(),
            "TEST_COLOR_PICKER_SATURATION_TRACK"
        );
        assert_eq!(
            HslaChannel::Lightness.debug_selector(),
            "TEST_COLOR_PICKER_LIGHTNESS_TRACK"
        );
        assert_eq!(
            HslaChannel::Alpha.debug_selector(),
            "TEST_COLOR_PICKER_ALPHA_TRACK"
        );
    }

    #[test]
    fn hsla_channel_should_read_its_label_from_the_panel_labels() {
        let labels = ColorPickerLabels {
            palette: SharedString::new_static("palette!"),
            hsla: SharedString::new_static("hsla!"),
            hue: SharedString::new_static("hue!"),
            saturation: SharedString::new_static("saturation!"),
            lightness: SharedString::new_static("lightness!"),
            alpha: SharedString::new_static("alpha!"),
        };

        assert_eq!(HslaChannel::Hue.label(&labels), labels.hue);
        assert_eq!(HslaChannel::Saturation.label(&labels), labels.saturation);
        assert_eq!(HslaChannel::Lightness.label(&labels), labels.lightness);
        assert_eq!(HslaChannel::Alpha.label(&labels), labels.alpha);
    }

    #[test]
    fn hsla_channel_should_read_the_component_it_edits() {
        let color = hsla(0.1, 0.2, 0.3, 0.4);

        assert_eq!(HslaChannel::Hue.value(color), 0.1);
        assert_eq!(HslaChannel::Saturation.value(color), 0.2);
        assert_eq!(HslaChannel::Lightness.value(color), 0.3);
        assert_eq!(HslaChannel::Alpha.value(color), 0.4);
    }

    #[test]
    fn hsla_channel_should_render_zero_half_and_full() {
        let zero = hsla(0., 0., 0., 0.);
        let half = hsla(0.5, 0.5, 0.5, 0.5);
        let full = hsla(1., 1., 1., 1.);

        assert_eq!(HslaChannel::Hue.value_text(zero).as_str(), "0");
        assert_eq!(HslaChannel::Saturation.value_text(zero).as_str(), "0");
        assert_eq!(HslaChannel::Lightness.value_text(zero).as_str(), "0");
        assert_eq!(HslaChannel::Alpha.value_text(zero).as_str(), "0");

        assert_eq!(HslaChannel::Hue.value_text(half).as_str(), "180");
        assert_eq!(HslaChannel::Saturation.value_text(half).as_str(), "50");
        assert_eq!(HslaChannel::Lightness.value_text(half).as_str(), "50");
        assert_eq!(HslaChannel::Alpha.value_text(half).as_str(), "50");

        assert_eq!(HslaChannel::Hue.value_text(full).as_str(), "360");
        assert_eq!(HslaChannel::Saturation.value_text(full).as_str(), "100");
        assert_eq!(HslaChannel::Lightness.value_text(full).as_str(), "100");
        assert_eq!(HslaChannel::Alpha.value_text(full).as_str(), "100");
    }

    #[test]
    fn hsla_channel_should_round_the_displayed_number() {
        let almost_full = hsla(0.9999, 0.9999, 0.9999, 0.9999);

        assert_eq!(
            HslaChannel::Hue.value_text(almost_full).as_str(),
            "360",
            "hue rounds up into the 360 the display allows"
        );
        assert_eq!(
            HslaChannel::Saturation.value_text(almost_full).as_str(),
            "100"
        );
        assert_eq!(
            HslaChannel::Lightness.value_text(almost_full).as_str(),
            "100"
        );
        assert_eq!(HslaChannel::Alpha.value_text(almost_full).as_str(), "100");
        assert_eq!(
            HslaChannel::Hue
                .value_text(hsla(0.0001, 0., 0., 0.))
                .as_str(),
            "0",
            "hue rounds down rather than truncating a tiny value up"
        );
        assert_eq!(
            HslaChannel::Hue
                .value_text(hsla(0.5555, 0., 0., 0.))
                .as_str(),
            "200",
            "0.5555 * 360 = 199.98"
        );
    }

    #[test]
    fn the_slider_rows_should_fill_the_panel_content_width() {
        assert_eq!(SLIDER_ROW_HEIGHT, px(24.));
        assert_eq!(SLIDER_LABEL_WIDTH, px(72.));
        assert_eq!(SLIDER_VALUE_WIDTH, px(40.));
        assert_eq!(SLIDER_ROW_INNER_GAP, px(8.));
        assert_eq!(TRACK_HEIGHT, 14.0);
        assert_eq!(TRACK_RADIUS * 2.0, TRACK_HEIGHT);
        assert_eq!(TRACK_SLICES, 256);
        assert_eq!(THUMB_DIAMETER, 12.0);
        assert_eq!(
            SLIDER_LABEL_WIDTH.as_f32()
                + SLIDER_ROW_INNER_GAP.as_f32() * 2.0
                + SLIDER_VALUE_WIDTH.as_f32()
                + 156.0,
            PANEL_CONTENT_WIDTH.as_f32(),
            "72 (label) + 8 + 156 (track) + 8 + 40 (value) = 284"
        );
        assert_eq!(PANEL_CONTENT_WIDTH, px(284.));
    }

    #[test]
    fn parse_hex_should_accept_every_supported_length() {
        assert_eq!(
            rgba8(parse_hex("#f00").expect("#rgb parses")),
            [255, 0, 0, 255]
        );
        assert_eq!(
            rgba8(parse_hex("#f008").expect("#rgba parses")),
            [255, 0, 0, 136]
        );
        assert_eq!(
            rgba8(parse_hex("#00ff00").expect("#rrggbb parses")),
            [0, 255, 0, 255]
        );
        assert_eq!(
            rgba8(parse_hex("#00ff0080").expect("#rrggbbaa parses")),
            [0, 255, 0, 128]
        );
        assert_eq!(
            rgba8(parse_hex("#a1b2c3").expect("#rrggbb parses")),
            [161, 178, 195, 255]
        );
        assert_eq!(
            rgba8(parse_hex("#a1b2c3d4").expect("#rrggbbaa parses")),
            [161, 178, 195, 212]
        );
        assert_eq!(
            rgba8(parse_hex("#f00").expect("#rgb parses")),
            rgba8(parse_hex("#ff0000").expect("#rrggbb parses")),
            "a short digit is duplicated, not scaled"
        );
    }

    #[test]
    fn parse_hex_should_ignore_the_hash_surrounding_whitespace_and_case() {
        for text in ["#ff0000", "ff0000", "#FF0000", "  #Ff0000  "] {
            assert_eq!(
                rgba8(parse_hex(text).expect("red parses")),
                [255, 0, 0, 255],
                "{text:?}"
            );
        }

        for text in ["#abcdef", "abcdef", "#ABCDEF", "  #AbCdEf  "] {
            assert_eq!(
                rgba8(parse_hex(text).expect("blue-grey parses")),
                [171, 205, 239, 255],
                "{text:?}"
            );
        }

        for text in ["#abcd", "abcd", "#ABCD"] {
            assert_eq!(
                rgba8(parse_hex(text).expect("translucent white parses")),
                [170, 187, 204, 221],
                "{text:?}"
            );
        }
    }

    #[test]
    fn parse_hex_should_reject_anything_that_is_not_hex() {
        for text in [
            "", "#", "#12", "#12345", "#1234567", "#ggg", "#+f0000", "#-fffff", "0x123456",
            "0xabcdef", "#0x123",
        ] {
            assert!(parse_hex(text).is_none(), "{text:?} must not parse");
        }
    }

    #[test]
    fn hex_string_should_be_uppercase_and_omit_an_opaque_alpha() {
        assert_eq!(hex_string(hsla(0.0, 1.0, 0.5, 1.0)).as_str(), "#FF0000");
        assert_eq!(hex_string(hsla(0.0, 0.0, 0.0, 1.0)).as_str(), "#000000");
    }

    #[test]
    fn hex_string_should_include_the_alpha_when_it_is_not_opaque() {
        assert_eq!(hex_string(hsla(0.0, 1.0, 0.5, 0.5)).as_str(), "#FF000080");
        assert_eq!(hex_string(hsla(0.0, 0.0, 0.0, 0.0)).as_str(), "#00000000");
    }

    #[test]
    fn hex_string_should_round_rather_than_truncate() {
        // 0.5 * 255 is 127.5: rounding writes 80, truncation would write 7F.
        assert_eq!(hex_string(hsla(0.0, 1.0, 0.5, 0.5)).as_str(), "#FF000080");

        // An alpha that rounds up to opaque is dropped, not written as FE.
        assert_eq!(
            hex_string(hsla(0.0, 0.0, 0.0, 254.5 / 255.0)).as_str(),
            "#000000"
        );
    }

    #[test]
    fn hex_should_round_trip_through_eight_bit_quantization() {
        let colors = [
            hsla(0.0, 1.0, 0.5, 1.0),
            hsla(0.55, 0.8, 0.53, 1.0),
            hsla(0.33, 0.42, 0.61, 0.5),
            hsla(0.0, 0.0, 0.0, 0.0),
        ];

        for color in colors {
            let hex = hex_string(color);
            let parsed = parse_hex(hex.as_str()).expect("the picker's own hex parses");

            assert_eq!(
                rgba8(parsed),
                rgba8(color),
                "{hex} should round-trip through eight-bit channels"
            );
        }
    }

    const DEFAULT_COLOR: Hsla = hsla(0.2, 0.5, 0.4, 1.0);

    /// A parent that holds one picker entity and records the events it emits.
    ///
    /// The picker is created once, in `with_size`, and rendering this harness draws that same
    /// entity again: that is the lifetime a real parent gives it. The column centers its
    /// children horizontally, so the trigger keeps the content-sized width a caller gives it
    /// instead of being stretched to the window's width.
    struct Harness {
        picker: Entity<ColorPicker>,
        changes: Rc<RefCell<Vec<Hsla>>>,
        commits: Rc<RefCell<Vec<Hsla>>>,
        _subscription: Subscription,
    }

    impl Harness {
        fn with_size(size: UnifiedSize, window: &mut Window, cx: &mut Context<Self>) -> Self {
            let picker = cx.new(|cx| ColorPicker::new(DEFAULT_COLOR, size, window, cx));
            let changes = Rc::new(RefCell::new(Vec::new()));
            let commits = Rc::new(RefCell::new(Vec::new()));
            let change_sink = changes.clone();
            let commit_sink = commits.clone();
            let subscription =
                cx.subscribe(
                    &picker,
                    move |_, _, event: &ColorPickerEvent, _| match event {
                        ColorPickerEvent::ColorChanged(color) => {
                            change_sink.borrow_mut().push(*color);
                        }
                        ColorPickerEvent::ColorCommitted(color) => {
                            commit_sink.borrow_mut().push(*color);
                        }
                    },
                );

            Self {
                picker,
                changes,
                commits,
                _subscription: subscription,
            }
        }
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .child(self.picker.clone())
        }
    }

    /// A parent that creates its picker lazily on the first render and stores it, like
    /// `examples/color_picker.rs`.
    ///
    /// The picker cannot be built when the harness is constructed because [`ColorPicker::new`]
    /// needs a window, so it is `None` until the first render fills it in. `creations` counts the
    /// lazy branch: a later render that rebuilt the picker instead of drawing the stored one
    /// would increment it and hand the window a new entity, losing the color and the hex field.
    /// The subscription is installed in that same branch, so it is installed exactly once too.
    /// `renders` counts every render, so a test can prove the parent really drew again.
    struct LazyHarness {
        picker: Option<Entity<ColorPicker>>,
        creations: Rc<Cell<usize>>,
        renders: Rc<Cell<usize>>,
        subscription: Option<Subscription>,
    }

    impl Render for LazyHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.renders.set(self.renders.get() + 1);

            if self.picker.is_none() || self.subscription.is_none() {
                let picker =
                    cx.new(|cx| ColorPicker::new(DEFAULT_COLOR, UnifiedSize::Medium, window, cx));
                self.subscription = Some(cx.subscribe(&picker, |_, _, _: &ColorPickerEvent, _| {}));
                self.creations.set(self.creations.get() + 1);
                self.picker = Some(picker);
            }

            let picker = self
                .picker
                .as_ref()
                .expect("the lazy branch above stores the picker")
                .clone();

            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .child(picker)
        }
    }

    /// Installs the theme and draws the picker at `unified_size` in a window large enough for
    /// the panel.
    fn harness_with_size(
        cx: &mut TestAppContext,
        unified_size: UnifiedSize,
    ) -> (Entity<Harness>, &mut VisualTestContext) {
        cx.update(|cx| {
            // The hex tests deliver Enter through the real key dispatch, so the input's key
            // bindings have to be installed; the other tests do not care either way.
            gpui_kit::base::init(cx);
            theme::install(cx);
        });
        let (harness, cx) =
            cx.add_window_view(move |window, cx| Harness::with_size(unified_size, window, cx));
        cx.simulate_resize(size(px(900.), px(800.)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        (harness, cx)
    }

    /// Draws the picker at the default [`UnifiedSize::Medium`].
    fn harness(cx: &mut TestAppContext) -> (Entity<Harness>, &mut VisualTestContext) {
        harness_with_size(cx, UnifiedSize::Medium)
    }

    /// Draws a parent whose picker is created in its first render, like the example.
    ///
    /// The first refresh is what runs that render, so the returned view already holds its picker.
    fn lazy_harness(cx: &mut TestAppContext) -> (Entity<LazyHarness>, &mut VisualTestContext) {
        cx.update(|cx| {
            gpui_kit::base::init(cx);
            theme::install(cx);
        });
        let (harness, cx) = cx.add_window_view(|_window, _cx| LazyHarness {
            picker: None,
            creations: Rc::new(Cell::new(0)),
            renders: Rc::new(Cell::new(0)),
            subscription: None,
        });
        cx.simulate_resize(size(px(900.), px(800.)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        (harness, cx)
    }

    /// Clicks the centre of the element with `selector` and redraws the window.
    fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .expect("the selector is drawn")
            .center();
        cx.simulate_click(target, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// The window position of `fraction` along a slider's track.
    fn track_point(
        cx: &mut VisualTestContext,
        channel: HslaChannel,
        fraction: f32,
    ) -> Point<Pixels> {
        let bounds = cx
            .debug_bounds(channel.debug_selector())
            .expect("the HSLA tab draws the track");

        point(
            bounds.origin.x + px(bounds.size.width.as_f32() * fraction),
            bounds.origin.y + px(bounds.size.height.as_f32() / 2.0),
        )
    }

    #[gpui_kit::test]
    fn select_palette_should_apply_the_swatch_emit_once_and_sync_the_hex_field(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        let swatch = PALETTE[28];

        cx.update(|window, cx| {
            state.update(cx, |state, cx| state.select_palette(28, window, cx));
        });

        assert_eq!(changes.borrow().as_slice(), &[swatch]);

        let (color, hex) = state.read_with(cx, |state, cx| {
            (state.color(), state.hex_input.read(cx).value(cx))
        });
        assert_eq!(color, swatch);
        assert_eq!(hex.as_str(), hex_string(swatch).as_str());

        cx.update(|window, cx| {
            state.update(cx, |state, cx| state.select_palette(28, window, cx));
        });
        assert_eq!(
            changes.borrow().as_slice(),
            &[swatch],
            "selecting the swatch already selected is a no-op"
        );
    }

    #[gpui_kit::test]
    fn set_channel_should_change_only_the_named_component(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());

        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_channel(HslaChannel::Lightness, 0.75, window, cx)
            });
        });
        let lightness = state.read_with(cx, |state, _| state.color());
        assert!((lightness.l - 0.75).abs() < 1e-6);
        assert_eq!(lightness.h, DEFAULT_COLOR.h);
        assert_eq!(lightness.s, DEFAULT_COLOR.s);
        assert_eq!(lightness.a, DEFAULT_COLOR.a);
        assert_eq!(changes.borrow().as_slice(), &[lightness]);

        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_channel(HslaChannel::Alpha, 0.25, window, cx)
            });
        });
        let alpha = state.read_with(cx, |state, _| state.color());
        assert!((alpha.a - 0.25).abs() < 1e-6);
        assert_eq!(alpha.h, lightness.h);
        assert_eq!(alpha.s, lightness.s);
        assert_eq!(alpha.l, lightness.l);
        assert_eq!(
            changes.borrow().as_slice(),
            &[lightness, alpha],
            "each change of the color emits once"
        );
    }

    #[gpui_kit::test]
    fn set_channel_should_clamp_and_ignore_an_unchanged_value(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());

        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_channel(HslaChannel::Saturation, -0.5, window, cx)
            });
        });
        assert_eq!(
            state.read_with(cx, |state, _| state.color().s),
            0.0,
            "a fraction below zero clamps to zero"
        );

        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_channel(HslaChannel::Saturation, 1.5, window, cx)
            });
        });
        assert_eq!(
            state.read_with(cx, |state, _| state.color().s),
            1.0,
            "a fraction above one clamps to one"
        );
        assert_eq!(changes.borrow().len(), 2);

        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_channel(HslaChannel::Saturation, 1.0, window, cx)
            });
        });
        assert_eq!(
            changes.borrow().len(),
            2,
            "setting the value already selected is a no-op"
        );
    }

    /// Presses what `selector` names the way a window delivers a press: down, a frame, up, then
    /// the frame the press asked for.
    ///
    /// The frame in the middle is the point. While the panel is open the trigger is rendered
    /// without an `on_click` listener, and GPUI 0.3.8's `Div` arms its `pending_mouse_down` only
    /// for an element that has a click or drag listener. This press is therefore recorded only as
    /// the outside press that closes the panel: the release arrives after the closed trigger has
    /// been given its listener back, but with no pending press for it to complete, so it cannot
    /// reopen what the press closed.
    fn press(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .expect("the picker draws what it is asked to press")
            .center();
        cx.simulate_mouse_down(target, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// Lets a closing overlay finish fading out before asking whether its panel is gone.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(ANIMATION_TICK * 2);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    fn panel_is_drawn(cx: &mut VisualTestContext) -> bool {
        cx.debug_bounds("TEST_COLOR_PICKER_PANEL").is_some()
    }

    /// Empties the hex field and puts the caret in it.
    ///
    /// The draw is what makes the field the window's input handler: typing before it would go
    /// nowhere. `update_value` is the panel's own write-back path and emits no change, so
    /// clearing the field does not touch the color.
    fn clear_and_focus_hex(cx: &mut VisualTestContext, state: &Entity<ColorPicker>) {
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.hex_input.update(cx, |input, cx| {
                    input.update_value("", window, cx);
                });
            });
            let focus = state.read(cx).hex_input.read(cx).engine_focus_handle(cx);
            focus.focus(window, cx);
            window.draw(cx).clear(cx);
        });
        cx.run_until_parked();
    }

    /// Delivers Enter to the focused field as a real key-down.
    ///
    /// `simulate_keystrokes("enter")` also feeds the key's simulated IME text (`"\n"`) to the
    /// focused input after the key-down. That text-input event emits a `Change` carrying the
    /// value already in the field, which would apply it through `on_hex_change` before the
    /// commit runs and hide the commit's own apply branch. This dispatches the same key-down,
    /// and therefore the same key binding, without the simulated text.
    fn press_enter(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.dispatch_event(
                PlatformInput::KeyDown(KeyDownEvent {
                    keystroke: Keystroke::parse("enter").expect("enter is a keystroke"),
                    is_held: false,
                    prefer_character_input: false,
                }),
                cx,
            );
        });
        cx.run_until_parked();
    }

    /// Puts `text` in the hex field the way the panel's own write-back does: no change event.
    ///
    /// Typing the text first would let `on_hex_change` apply the color and emit, so a commit test
    /// has to use this path to leave the apply branch in `on_hex_commit` as the only thing that
    /// can change the color.
    fn set_hex_text_without_change_event(
        cx: &mut VisualTestContext,
        state: &Entity<ColorPicker>,
        text: &str,
    ) {
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.hex_input.update(cx, |input, cx| {
                    input.update_value(text, window, cx);
                });
            });
        });
        cx.run_until_parked();
    }

    /// Asserts the open panel draws the HSLA tab and none of the palette grid.
    fn assert_hsla_tab_is_drawn(cx: &mut VisualTestContext, context: &str) {
        assert!(
            cx.debug_bounds("TEST_COLOR_PICKER_GRID").is_none(),
            "{context} must not fall back to the palette grid"
        );
        for channel in [
            HslaChannel::Hue,
            HslaChannel::Saturation,
            HslaChannel::Lightness,
            HslaChannel::Alpha,
        ] {
            assert!(
                cx.debug_bounds(channel.debug_selector()).is_some(),
                "{context} must draw the {} track",
                channel.id_prefix()
            );
        }
    }

    /// Asserts the open panel draws the palette tab and none of the four HSLA tracks.
    fn assert_palette_tab_is_drawn(cx: &mut VisualTestContext, context: &str) {
        assert!(
            cx.debug_bounds("TEST_COLOR_PICKER_GRID").is_some(),
            "{context} must draw the palette grid"
        );
        for channel in [
            HslaChannel::Hue,
            HslaChannel::Saturation,
            HslaChannel::Lightness,
            HslaChannel::Alpha,
        ] {
            assert!(
                cx.debug_bounds(channel.debug_selector()).is_none(),
                "{context} must not draw the {} track",
                channel.id_prefix()
            );
        }
    }

    #[gpui_kit::test]
    fn the_picker_should_start_closed_on_the_default_color_and_palette_tab(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());

        let (color, tab, hex) = state.read_with(cx, |state, cx| {
            (
                state.color(),
                state.active_tab(),
                state.hex_input.read(cx).value(cx),
            )
        });

        assert_eq!(color, DEFAULT_COLOR);
        assert_eq!(tab, ColorPickerTab::Palette);
        assert_eq!(hex.as_str(), hex_string(DEFAULT_COLOR).as_str());
        assert!(cx.debug_bounds("TEST_COLOR_PICKER_TRIGGER").is_some());
        assert!(!panel_is_drawn(cx), "the panel starts closed");
    }

    /// A parent that already holds its picker keeps drawing that entity across a render.
    ///
    /// This covers the state half of the contract only: the picker exists before the first
    /// render, so this test cannot observe the lazy creation the example performs. The lazy
    /// harness below covers that path.
    #[gpui_kit::test]
    fn an_eagerly_created_picker_should_keep_its_state_across_a_parent_render(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let picker = harness.read_with(cx, |harness, _| harness.picker.clone());
        let picker_id = picker.entity_id();

        let blue = hsla(2.0 / 3.0, 1.0, 0.5, 1.0);
        cx.update(|window, cx| {
            picker.update(cx, |picker, cx| picker.set_color(blue, window, cx));
        });

        // A parent render must draw the entity it stored, not build a picker per frame: a new
        // one would lose the color, the field and the caret.
        cx.update(|window, cx| {
            harness.update(cx, |_, cx| cx.notify());
            window.refresh();
        });
        cx.run_until_parked();

        let (id_after, color_after) = harness.read_with(cx, |harness, cx| {
            (harness.picker.entity_id(), harness.picker.read(cx).color())
        });
        assert_eq!(
            id_after, picker_id,
            "the picker entity survives a parent render"
        );
        assert_eq!(
            color_after, blue,
            "the picker's state survives a parent render"
        );
        assert_eq!(
            picker
                .read_with(cx, |picker, cx| picker.hex_input.read(cx).value(cx))
                .as_str(),
            "#0000FF",
            "the hex field still belongs to the stored picker"
        );
    }

    #[gpui_kit::test]
    fn a_parent_should_lazily_create_the_picker_once_and_keep_it_across_a_render(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = lazy_harness(cx);
        let creations = harness.read_with(cx, |harness, _| harness.creations.clone());
        let renders = harness.read_with(cx, |harness, _| harness.renders.clone());
        let picker = harness.read_with(cx, |harness, _| {
            harness
                .picker
                .clone()
                .expect("the first render creates the picker")
        });
        let picker_id = picker.entity_id();

        assert_eq!(
            creations.get(),
            1,
            "the first render creates the picker exactly once"
        );
        assert!(
            harness.read_with(cx, |harness, _| harness.subscription.is_some()),
            "the first render installs the picker's subscription"
        );
        assert!(
            cx.debug_bounds("TEST_COLOR_PICKER_TRIGGER").is_some(),
            "the parent draws the picker its first render created"
        );

        let blue = hsla(2.0 / 3.0, 1.0, 0.5, 1.0);
        cx.update(|window, cx| {
            picker.update(cx, |picker, cx| picker.set_color(blue, window, cx));
        });

        // A second parent render must draw the stored entity: a render that re-entered the
        // lazy branch would replace the picker and reset its color and its hex field.
        let renders_before = renders.get();
        cx.update(|window, cx| {
            harness.update(cx, |_, cx| cx.notify());
            window.refresh();
        });
        cx.run_until_parked();

        assert!(
            renders.get() > renders_before,
            "the forced refresh renders the parent again"
        );
        assert_eq!(
            creations.get(),
            1,
            "a parent re-render must not create a second picker"
        );
        assert!(
            harness.read_with(cx, |harness, _| harness.subscription.is_some()),
            "the stored subscription survives the parent re-render"
        );
        assert_eq!(
            harness.read_with(cx, |harness, _| {
                harness
                    .picker
                    .as_ref()
                    .expect("the parent still stores the picker")
                    .entity_id()
            }),
            picker_id,
            "the parent keeps the picker entity its first render created"
        );
        assert!(
            cx.debug_bounds("TEST_COLOR_PICKER_TRIGGER").is_some(),
            "the reused picker entity is still drawn after the parent's second render"
        );

        let (color, hex) = picker.read_with(cx, |picker, cx| {
            (picker.color(), picker.hex_input.read(cx).value(cx))
        });
        assert_eq!(
            color, blue,
            "the color set on the stored picker survives the parent re-render"
        );
        assert_eq!(
            hex.as_str(),
            "#0000FF",
            "the hex field still belongs to the picker the first render stored"
        );
    }

    #[gpui_kit::test]
    fn pressing_the_trigger_should_open_the_panel(cx: &mut TestAppContext) {
        let (_harness, cx) = harness(cx);
        assert!(!panel_is_drawn(cx));

        press(cx, "TEST_COLOR_PICKER_TRIGGER");

        assert!(panel_is_drawn(cx), "pressing the trigger mounts the panel");
        assert!(
            cx.debug_bounds("TEST_COLOR_PICKER_GRID").is_some(),
            "the open panel shows its default palette tab"
        );
    }

    #[gpui_kit::test]
    fn the_trigger_should_be_square_at_every_unified_size(cx: &mut TestAppContext) {
        // The well is inset by the button's 2px padding on each side and its 1px border on each
        // side: 20/30/40 minus 6 is 14/24/34.
        for (unified_size, edge, well_edge) in [
            (UnifiedSize::Small, 20., 14.),
            (UnifiedSize::Medium, 30., 24.),
            (UnifiedSize::Large, 40., 34.),
        ] {
            let (_harness, cx) = harness_with_size(cx, unified_size);
            let bounds = cx
                .debug_bounds("TEST_COLOR_PICKER_TRIGGER")
                .expect("the trigger is drawn");
            assert_eq!(
                bounds.size.width, bounds.size.height,
                "{unified_size:?}: the trigger's width must match its UnifiedSize height"
            );
            assert_eq!(
                bounds.size,
                size(px(edge), px(edge)),
                "{unified_size:?}: the trigger must be the UnifiedSize square"
            );

            let well = cx
                .debug_bounds("TEST_COLOR_PICKER_TRIGGER_WELL")
                .expect("the trigger's color well is drawn");
            assert_eq!(
                well.size,
                size(px(well_edge), px(well_edge)),
                "{unified_size:?}: the color well fills the 2px padding and 1px border of the \
                 button's content box"
            );

            press(cx, "TEST_COLOR_PICKER_TRIGGER");
            assert!(
                panel_is_drawn(cx),
                "{unified_size:?}: a square trigger must still open the panel"
            );
        }
    }

    #[gpui_kit::test]
    fn pressing_outside_the_panel_should_close_it(cx: &mut TestAppContext) {
        let (_harness, cx) = harness(cx);
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        assert!(panel_is_drawn(cx));

        let outside = point(px(850.), px(750.));
        cx.simulate_mouse_down(outside, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_up(outside, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        settle(cx);

        assert!(
            !panel_is_drawn(cx),
            "a press outside the panel dismisses it"
        );
    }

    #[gpui_kit::test]
    fn pressing_the_trigger_of_an_open_panel_should_close_it_without_reopening_it(
        cx: &mut TestAppContext,
    ) {
        let (_harness, cx) = harness(cx);
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        assert!(panel_is_drawn(cx));

        // The trigger is outside the panel, so this press closes the panel while the button is
        // down. The release must not arrive as a click on a trigger that is closed again.
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        settle(cx);

        assert!(
            !panel_is_drawn(cx),
            "the release of the closing press must not reopen the panel"
        );

        settle(cx);
        assert!(
            !panel_is_drawn(cx),
            "the panel stays closed on later frames"
        );
    }

    #[gpui_kit::test]
    fn the_open_panel_should_switch_between_the_palette_and_the_hsla_tracks(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        assert_palette_tab_is_drawn(cx, "the open panel's default palette tab");
        let grid = cx
            .debug_bounds("TEST_COLOR_PICKER_GRID")
            .expect("the open panel draws its default palette tab");
        assert_eq!(
            grid.size,
            size(px(PALETTE_SIZE), px(PALETTE_SIZE)),
            "the palette fills the panel's 284px content width"
        );

        click_selector(cx, "TEST_COLOR_PICKER_TAB_HSLA");

        assert_eq!(
            state.read_with(cx, |state, _| state.active_tab()),
            ColorPickerTab::Hsla
        );
        assert!(cx.debug_bounds("TEST_COLOR_PICKER_GRID").is_none());
        for channel in [
            HslaChannel::Hue,
            HslaChannel::Saturation,
            HslaChannel::Lightness,
            HslaChannel::Alpha,
        ] {
            assert!(
                cx.debug_bounds(channel.debug_selector()).is_some(),
                "the open panel draws the {} in its HSLA tab",
                channel.id_prefix()
            );
        }
        let hue_track = cx
            .debug_bounds(HslaChannel::Hue.debug_selector())
            .expect("the HSLA tab draws the hue track");
        assert_eq!(
            hue_track.size,
            size(px(156.), px(TRACK_HEIGHT)),
            "72 (label) + 8 + 156 (track) + 8 + 40 (value) fills the 284px content width"
        );

        click_selector(cx, "TEST_COLOR_PICKER_TAB_PALETTE");

        assert_eq!(
            state.read_with(cx, |state, _| state.active_tab()),
            ColorPickerTab::Palette
        );
        assert_palette_tab_is_drawn(cx, "switching back to the palette tab");
        assert!(panel_is_drawn(cx), "switching tabs keeps the panel open");
    }

    #[gpui_kit::test]
    fn clicking_a_palette_cell_should_change_the_color_and_close_the_panel(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");

        let grid = cx
            .debug_bounds("TEST_COLOR_PICKER_GRID")
            .expect("the palette tab draws the grid");
        assert_eq!(
            grid.size,
            size(px(PALETTE_SIZE), px(PALETTE_SIZE)),
            "the grid draws at its nominal edge"
        );
        let gap = point(
            grid.origin.x + px(PALETTE_STEP * 4.0 + 22.0),
            grid.origin.y + px(PALETTE_STEP * 2.0 + 10.0),
        );
        cx.simulate_click(gap, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert_eq!(
            changes.borrow().len(),
            0,
            "a press in the gap between swatches emits nothing"
        );
        assert!(
            panel_is_drawn(cx),
            "a press in the gap does not choose a color or close the panel"
        );

        let cell = point(
            grid.origin.x + px(PALETTE_STEP * 4.0 + 10.0),
            grid.origin.y + px(PALETTE_STEP * 2.0 + 10.0),
        );
        cx.simulate_click(cell, Modifiers::none());
        settle(cx);

        let swatch = PALETTE[28];
        assert_eq!(state.read_with(cx, |state, _| state.color()), swatch);
        assert_eq!(changes.borrow().as_slice(), &[swatch]);
        assert!(!panel_is_drawn(cx), "choosing a swatch closes the panel");

        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        let grid = cx
            .debug_bounds("TEST_COLOR_PICKER_GRID")
            .expect("the picker opens the palette again");
        let same_cell = point(
            grid.origin.x + px(PALETTE_STEP * 4.0 + 10.0),
            grid.origin.y + px(PALETTE_STEP * 2.0 + 10.0),
        );
        cx.simulate_click(same_cell, Modifiers::none());
        settle(cx);

        assert_eq!(
            changes.borrow().len(),
            1,
            "choosing the swatch already selected emits nothing"
        );
        assert!(
            !panel_is_drawn(cx),
            "choosing the already selected swatch still closes the panel"
        );
    }

    #[gpui_kit::test]
    fn hovering_a_palette_cell_should_preview_the_footer_and_hold_across_gaps(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");

        let grid = cx
            .debug_bounds("TEST_COLOR_PICKER_GRID")
            .expect("the palette tab draws the grid");
        let cell = point(
            grid.origin.x + px(PALETTE_STEP * 4.0 + 10.0),
            grid.origin.y + px(PALETTE_STEP * 2.0 + 10.0),
        );
        let hovered = PALETTE[28];

        cx.simulate_mouse_move(cell, None, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert_eq!(
            state.read_with(cx, |state, cx| state.hex_input.read(cx).value(cx)),
            hex_string(hovered).as_str(),
            "hover previews the hovered swatch in the hex field"
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.hovered_swatch),
            Some(28),
            "the hovered swatch is tracked for the palette ring and footer preview"
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.color()),
            DEFAULT_COLOR,
            "hover must not change the picker's color"
        );
        assert!(
            changes.borrow().is_empty(),
            "hover must not emit ColorChanged"
        );
        assert!(panel_is_drawn(cx), "hover must not close the panel");

        let gap = point(
            grid.origin.x + px(PALETTE_STEP * 4.0 + 22.0),
            grid.origin.y + px(PALETTE_STEP * 2.0 + 10.0),
        );
        cx.simulate_mouse_move(gap, None, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert_eq!(
            state.read_with(cx, |state, cx| state.hex_input.read(cx).value(cx)),
            hex_string(hovered).as_str(),
            "moving into a gap keeps the previous preview instead of flashing the real color"
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.hovered_swatch),
            Some(28),
            "the ring and footer keep the previous swatch while crossing the gap"
        );
        assert!(changes.borrow().is_empty());

        let next_cell = point(
            grid.origin.x + px(PALETTE_STEP * 5.0 + 10.0),
            grid.origin.y + px(PALETTE_STEP * 2.0 + 10.0),
        );
        let next = PALETTE[29];
        cx.simulate_mouse_move(next_cell, None, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert_eq!(
            state.read_with(cx, |state, cx| state.hex_input.read(cx).value(cx)),
            hex_string(next).as_str(),
            "entering the next cell replaces the preview directly"
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.hovered_swatch),
            Some(29)
        );
        assert!(changes.borrow().is_empty());

        let outside = point(px(850.), px(750.));
        cx.simulate_mouse_move(outside, None, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert_eq!(
            state.read_with(cx, |state, cx| state.hex_input.read(cx).value(cx)),
            hex_string(DEFAULT_COLOR).as_str(),
            "leaving the palette restores the footer to the real color"
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.hovered_swatch),
            None,
            "leaving the palette clears the hovered swatch and its ring"
        );
        assert!(changes.borrow().is_empty());
    }

    #[gpui_kit::test]
    fn closing_the_panel_should_clear_a_palette_hover_preview(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");

        let grid = cx
            .debug_bounds("TEST_COLOR_PICKER_GRID")
            .expect("the palette tab draws the grid");
        let cell = point(
            grid.origin.x + px(PALETTE_STEP * 4.0 + 10.0),
            grid.origin.y + px(PALETTE_STEP * 2.0 + 10.0),
        );
        cx.simulate_mouse_move(cell, None, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert_eq!(
            state.read_with(cx, |state, cx| state.hex_input.read(cx).value(cx)),
            hex_string(PALETTE[28]).as_str()
        );

        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        settle(cx);

        assert_eq!(
            state.read_with(cx, |state, cx| state.hex_input.read(cx).value(cx)),
            hex_string(DEFAULT_COLOR).as_str(),
            "closing the panel restores the real color in the field"
        );
        assert_eq!(state.read_with(cx, |state, _| state.color()), DEFAULT_COLOR);
        assert!(changes.borrow().is_empty());
    }

    #[gpui_kit::test]
    fn clicking_a_previewed_palette_cell_should_commit_the_color_and_close_the_panel(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");

        let grid = cx
            .debug_bounds("TEST_COLOR_PICKER_GRID")
            .expect("the palette tab draws the grid");
        let cell = point(
            grid.origin.x + px(PALETTE_STEP * 4.0 + 10.0),
            grid.origin.y + px(PALETTE_STEP * 2.0 + 10.0),
        );
        cx.simulate_mouse_move(cell, None, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        cx.simulate_click(cell, Modifiers::none());
        settle(cx);

        let swatch = PALETTE[28];
        assert_eq!(state.read_with(cx, |state, _| state.color()), swatch);
        assert_eq!(changes.borrow().as_slice(), &[swatch]);
        assert_eq!(
            state.read_with(cx, |state, cx| state.hex_input.read(cx).value(cx)),
            hex_string(swatch).as_str(),
            "committing keeps the chosen color in the field"
        );
        assert!(
            state.read_with(cx, |state, _| state.hovered_swatch.is_none()),
            "closing the panel clears the hover preview"
        );
        assert!(!panel_is_drawn(cx), "choosing a swatch closes the panel");
    }

    /// Choosing a swatch is the shortest commit path: the palette selection emits the change and
    /// closes the popup in one update, so the commit the observer reports carries the final color.
    #[gpui_kit::test]
    fn choosing_a_palette_swatch_should_commit_the_final_color_once(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        let commits = harness.read_with(cx, |harness, _| harness.commits.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");

        let grid = cx
            .debug_bounds("TEST_COLOR_PICKER_GRID")
            .expect("the palette tab draws the grid");
        let cell = point(
            grid.origin.x + px(PALETTE_STEP * 4.0 + 10.0),
            grid.origin.y + px(PALETTE_STEP * 2.0 + 10.0),
        );
        cx.simulate_click(cell, Modifiers::none());
        settle(cx);

        let swatch = PALETTE[28];
        assert_eq!(
            state.read_with(cx, |state, _| state.color()),
            swatch,
            "the chosen swatch is the color the popup closes on"
        );
        assert_eq!(changes.borrow().as_slice(), &[swatch]);
        assert_eq!(
            commits.borrow().as_slice(),
            &[swatch],
            "choosing a swatch emits exactly one commit carrying the final color"
        );
        assert!(!panel_is_drawn(cx), "choosing a swatch closes the panel");
    }

    /// A slider change is a live edit, not a commit: only closing the popup reports the color,
    /// and the value it reports is the one the interaction ended on.
    #[gpui_kit::test]
    fn closing_after_a_slider_change_should_commit_the_final_color_once(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let commits = harness.read_with(cx, |harness, _| harness.commits.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        click_selector(cx, "TEST_COLOR_PICKER_TAB_HSLA");

        let target = track_point(cx, HslaChannel::Hue, 0.25);
        cx.simulate_click(target, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let changed = state.read_with(cx, |state, _| state.color());
        assert!(
            (changed.h - 0.25).abs() < 1e-4,
            "the slider click changes the color, not just the preview"
        );
        assert!(
            commits.borrow().is_empty(),
            "an open popup commits nothing while the user is still editing"
        );

        let outside = point(px(850.), px(750.));
        cx.simulate_mouse_down(outside, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_up(outside, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        settle(cx);

        assert!(!panel_is_drawn(cx), "the outside press closes the panel");
        assert_eq!(
            state.read_with(cx, |state, _| state.color()),
            changed,
            "closing does not change the color"
        );
        assert_eq!(
            commits.borrow().as_slice(),
            &[changed],
            "closing after a slider change emits exactly one commit with the final color"
        );
    }

    /// Opening and closing without touching the color is not an interaction to persist.
    #[gpui_kit::test]
    fn opening_and_closing_on_the_same_color_should_not_commit(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        let commits = harness.read_with(cx, |harness, _| harness.commits.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        assert!(panel_is_drawn(cx));

        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        settle(cx);

        assert!(!panel_is_drawn(cx), "the second press closes the panel");
        assert!(changes.borrow().is_empty());
        assert!(
            commits.borrow().is_empty(),
            "a popup closed on the color it opened with commits nothing"
        );
    }

    /// `set_color` is silent and does not turn a programmatic change into an interaction.
    #[gpui_kit::test]
    fn a_programmatic_change_should_not_commit_when_the_popup_closes(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        let commits = harness.read_with(cx, |harness, _| harness.commits.clone());

        let blue = hsla(2.0 / 3.0, 1.0, 0.5, 1.0);
        cx.update(|window, cx| {
            state.update(cx, |state, cx| state.set_color(blue, window, cx));
        });
        cx.run_until_parked();

        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        let green = hsla(1.0 / 3.0, 1.0, 0.5, 1.0);
        cx.update(|window, cx| {
            state.update(cx, |state, cx| state.set_color(green, window, cx));
        });
        settle(cx);

        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        settle(cx);

        assert!(
            !panel_is_drawn(cx),
            "the second trigger press closes the panel"
        );
        assert_eq!(state.read_with(cx, |state, _| state.color()), green);
        assert!(
            changes.borrow().is_empty(),
            "set_color emits no ColorChanged, whether closed or open"
        );
        assert!(
            commits.borrow().is_empty(),
            "a programmatic change is not committed when the popup closes"
        );
    }

    /// The commit compares the final color with the one the popup opened with, so undoing the
    /// edit before closing leaves nothing to persist.
    #[gpui_kit::test]
    fn restoring_the_opening_color_should_not_commit(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        let commits = harness.read_with(cx, |harness, _| harness.commits.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        click_selector(cx, "TEST_COLOR_PICKER_TAB_HSLA");

        let target = track_point(cx, HslaChannel::Hue, 0.75);
        cx.simulate_click(target, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert_ne!(
            state.read_with(cx, |state, _| state.color()),
            DEFAULT_COLOR,
            "the edit moves the color away from the one the popup opened with"
        );

        // A track is addressed in pixels, so a click cannot land on the opening hue exactly; the
        // channel's own entry point takes the exact fraction a drag back would aim for.
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_channel(HslaChannel::Hue, DEFAULT_COLOR.h, window, cx)
            });
        });
        settle(cx);

        assert_eq!(
            state.read_with(cx, |state, _| state.color()),
            DEFAULT_COLOR,
            "the restored color is exactly the one the popup opened with"
        );
        assert!(
            commits.borrow().is_empty(),
            "an open popup commits nothing while the user is still editing"
        );

        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        settle(cx);

        assert!(!panel_is_drawn(cx));
        assert_eq!(
            changes.borrow().len(),
            2,
            "the edit and the restore are both ColorChanged events"
        );
        assert!(
            commits.borrow().is_empty(),
            "closing on the color the popup opened with commits nothing"
        );
    }

    /// A repeated open notify must not move the baseline, and every interaction commits its own
    /// final color: the first commit is not repeated by a no-op reopen, and a later edit commits
    /// once more.
    #[gpui_kit::test]
    fn a_second_interaction_should_commit_on_its_own_after_the_first(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let commits = harness.read_with(cx, |harness, _| harness.commits.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");

        let grid = cx
            .debug_bounds("TEST_COLOR_PICKER_GRID")
            .expect("the palette tab draws the grid");
        let cell = point(
            grid.origin.x + px(PALETTE_STEP * 4.0 + 10.0),
            grid.origin.y + px(PALETTE_STEP * 2.0 + 10.0),
        );
        cx.simulate_click(cell, Modifiers::none());
        settle(cx);

        let first = PALETTE[28];
        assert_eq!(commits.borrow().as_slice(), &[first]);

        // Reopen and close without changing anything: the first commit is the only one.
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        settle(cx);
        assert!(
            !panel_is_drawn(cx),
            "the second trigger press closes the reopened panel"
        );
        assert_eq!(
            commits.borrow().as_slice(),
            &[first],
            "closing without a change must not commit the color again"
        );

        // A third interaction edits the color and commits it once more.
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        click_selector(cx, "TEST_COLOR_PICKER_TAB_HSLA");
        let target = track_point(cx, HslaChannel::Hue, 0.75);
        cx.simulate_click(target, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let second = state.read_with(cx, |state, _| state.color());
        assert_ne!(second, first, "the hue track moved the color");

        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        settle(cx);

        assert_eq!(
            commits.borrow().as_slice(),
            &[first, second],
            "each interaction commits exactly its own final color"
        );
    }

    /// `Entity::write` notifies even when the open flag already says true; that repeat must not
    /// reset the baseline and swallow the commit of an edit made before it.
    #[gpui_kit::test]
    fn a_repeated_open_notify_should_not_swallow_the_commit(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let commits = harness.read_with(cx, |harness, _| harness.commits.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        click_selector(cx, "TEST_COLOR_PICKER_TAB_HSLA");

        let target = track_point(cx, HslaChannel::Hue, 0.75);
        cx.simulate_click(target, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let edited = state.read_with(cx, |state, _| state.color());
        assert!(
            (edited.h - 0.75).abs() < 1e-4,
            "the hue track changed the color before the repeated open notify"
        );

        state.update(cx, |state, cx| state.open.write(cx, true));
        settle(cx);
        assert!(
            panel_is_drawn(cx),
            "writing the open flag it already holds leaves the panel open"
        );
        assert!(
            commits.borrow().is_empty(),
            "an open popup commits nothing while the user is still editing"
        );

        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        settle(cx);

        assert!(!panel_is_drawn(cx));
        assert_eq!(
            commits.borrow().as_slice(),
            &[edited],
            "a repeated open notify must not suppress the edit's commit"
        );
    }

    #[gpui_kit::test]
    fn clicking_the_lightness_and_alpha_tracks_in_the_open_panel_should_edit_those_channels(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        click_selector(cx, "TEST_COLOR_PICKER_TAB_HSLA");

        changes.borrow_mut().clear();
        let lightness_target = track_point(cx, HslaChannel::Lightness, 0.75);
        cx.simulate_click(lightness_target, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let lightness = state.read_with(cx, |state, _| state.color());
        assert!(
            (lightness.l - 0.75).abs() < 1e-4,
            "lightness is {}, expected 0.75",
            lightness.l
        );
        assert_eq!(lightness.h, DEFAULT_COLOR.h, "hue must not move");
        assert_eq!(lightness.s, DEFAULT_COLOR.s, "saturation must not move");
        assert_eq!(lightness.a, DEFAULT_COLOR.a, "alpha must not move");
        assert_eq!(changes.borrow().as_slice(), &[lightness]);

        changes.borrow_mut().clear();
        let alpha_target = track_point(cx, HslaChannel::Alpha, 0.25);
        cx.simulate_click(alpha_target, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let alpha = state.read_with(cx, |state, _| state.color());
        assert!(
            (alpha.a - 0.25).abs() < 1e-4,
            "alpha is {}, expected 0.25",
            alpha.a
        );
        assert_eq!(alpha.h, lightness.h, "hue must not move");
        assert_eq!(alpha.s, lightness.s, "saturation must not move");
        assert_eq!(alpha.l, lightness.l, "lightness must not move");
        assert_eq!(changes.borrow().as_slice(), &[alpha]);
    }

    #[gpui_kit::test]
    fn typing_a_legal_hex_in_the_open_panel_should_set_the_color_without_rewriting_the_field(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        clear_and_focus_hex(cx, &state);

        cx.simulate_input("#FF0000");
        settle(cx);

        let (color, hex) = state.read_with(cx, |state, cx| {
            (state.color(), state.hex_input.read(cx).value(cx))
        });
        assert_eq!(
            hex_string(color).as_str(),
            "#FF0000",
            "the legal text becomes the color"
        );
        assert_eq!(
            hex.as_str(),
            "#FF0000",
            "the field shows exactly what was typed: a change during typing must not be \
             rewritten"
        );
        assert!(
            !changes.borrow().is_empty(),
            "a legal edit emits at least one ColorChanged"
        );
        assert_eq!(
            changes.borrow().last().copied(),
            Some(color),
            "the last event carries the color the field leaves behind"
        );
        assert!(
            panel_is_drawn(cx),
            "typing in the hex field leaves the panel open"
        );
    }

    #[gpui_kit::test]
    fn an_illegal_hex_in_the_open_panel_should_change_nothing_until_enter_restores_the_field(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        clear_and_focus_hex(cx, &state);
        let before = state.read_with(cx, |state, _| state.color());

        cx.simulate_input("#zz");
        settle(cx);

        assert_eq!(
            state.read_with(cx, |state, _| state.color()),
            before,
            "text that is not hex leaves the color alone"
        );
        assert_eq!(
            changes.borrow().len(),
            0,
            "text that is not hex emits nothing"
        );
        assert_eq!(
            state.read_with(cx, |state, cx| state.hex_input.read(cx).value(cx)),
            "#zz",
            "an illegal edit is left in the field until it is committed"
        );

        press_enter(cx);
        settle(cx);

        assert_eq!(
            state
                .read_with(cx, |state, cx| state.hex_input.read(cx).value(cx))
                .as_str(),
            hex_string(before).as_str(),
            "committing an illegal edit restores the current color's hex"
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.color()),
            before,
            "committing an illegal edit leaves the color alone"
        );
        assert_eq!(
            changes.borrow().len(),
            0,
            "committing an illegal edit emits nothing"
        );
        assert!(
            panel_is_drawn(cx),
            "committing in the hex field leaves the panel open"
        );
    }

    #[gpui_kit::test]
    fn enter_should_apply_the_field_text_and_normalize_it_exactly_once(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        clear_and_focus_hex(cx, &state);
        assert!(
            changes.borrow().is_empty(),
            "opening the panel and focusing the field emit no ColorChanged"
        );
        changes.borrow_mut().clear();

        // `update_value` is the panel's own write-back path: it puts text in the field without a
        // Change event, so the color can only move when Enter delivers the commit.
        set_hex_text_without_change_event(cx, &state, "#fff");
        settle(cx);

        assert!(
            changes.borrow().is_empty(),
            "writing the field without a change event emits nothing"
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.color()),
            DEFAULT_COLOR,
            "the uncommitted text has not been applied as a color"
        );

        press_enter(cx);
        settle(cx);

        let color = state.read_with(cx, |state, _| state.color());
        assert_eq!(
            rgba8(color),
            [255, 255, 255, 255],
            "Enter applies the short form the field holds as white"
        );
        assert_eq!(
            changes.borrow().as_slice(),
            &[color],
            "the commit applies the color exactly once"
        );
        assert_eq!(
            state
                .read_with(cx, |state, cx| state.hex_input.read(cx).value(cx))
                .as_str(),
            "#FFFFFF",
            "committing normalizes the field to the full uppercase hex"
        );
        assert!(
            panel_is_drawn(cx),
            "committing in the hex field leaves the panel open"
        );

        // The commit normalizes even when the color is already current, and emits nothing then.
        set_hex_text_without_change_event(cx, &state, "#ffffff");
        settle(cx);
        assert_eq!(
            changes.borrow().len(),
            1,
            "rewriting the field without a change event emits nothing either"
        );

        press_enter(cx);
        settle(cx);

        assert_eq!(
            state.read_with(cx, |state, _| state.color()),
            color,
            "committing an equivalent hex leaves the color alone"
        );
        assert_eq!(
            changes.borrow().len(),
            1,
            "committing the color already current emits nothing"
        );
        assert_eq!(
            state
                .read_with(cx, |state, cx| state.hex_input.read(cx).value(cx))
                .as_str(),
            "#FFFFFF",
            "even an unchanged commit normalizes the field"
        );
    }

    #[gpui_kit::test]
    fn blurring_the_hex_field_should_apply_the_field_text_and_normalize_it_like_enter(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        // A window has to be active for GPUI to report focus changes: an inactive one blanks
        // the focus paths it hands its listeners, so the blur would never reach the field.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        clear_and_focus_hex(cx, &state);
        assert!(
            changes.borrow().is_empty(),
            "opening the panel and focusing the field emit no ColorChanged"
        );
        changes.borrow_mut().clear();

        // Set the text without a Change event, exactly as the Enter test does: only the blur
        // commit may apply it.
        set_hex_text_without_change_event(cx, &state, "#0f0");
        settle(cx);

        assert!(
            changes.borrow().is_empty(),
            "writing the field without a change event emits nothing"
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.color()),
            DEFAULT_COLOR,
            "the uncommitted text has not been applied as a color"
        );

        // The window is the public test API for taking the caret away from the field; the draw
        // below delivers the blur, which is the same commit path Enter takes.
        cx.update(|window, cx| window.blur(cx));
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        settle(cx);

        let color = state.read_with(cx, |state, _| state.color());
        assert_eq!(
            rgba8(color),
            [0, 255, 0, 255],
            "losing the caret applies the short form the field holds as green"
        );
        assert_eq!(
            changes.borrow().as_slice(),
            &[color],
            "the blur commit applies the color exactly once"
        );
        assert_eq!(
            state
                .read_with(cx, |state, cx| state.hex_input.read(cx).value(cx))
                .as_str(),
            "#00FF00",
            "losing the caret normalizes the field"
        );
        assert!(
            panel_is_drawn(cx),
            "blurring the field does not close the panel"
        );
    }

    #[gpui_kit::test]
    fn setting_the_color_programmatically_should_be_silent_and_should_not_open_a_closed_panel(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        assert!(panel_is_drawn(cx));

        // Leave the default Palette tab with a real click. On Palette the tab assertions below
        // could pass by accident, because Palette is exactly what a reset would produce.
        click_selector(cx, "TEST_COLOR_PICKER_TAB_HSLA");
        assert_eq!(
            state.read_with(cx, |state, _| state.active_tab()),
            ColorPickerTab::Hsla
        );
        assert!(
            changes.borrow().is_empty(),
            "opening the panel and switching tabs emit no ColorChanged"
        );

        let blue = hsla(2.0 / 3.0, 1.0, 0.5, 1.0);
        cx.update(|window, cx| {
            state.update(cx, |state, cx| state.set_color(blue, window, cx));
        });
        settle(cx);

        assert_eq!(
            changes.borrow().len(),
            0,
            "a programmatic change emits no ColorChanged"
        );
        assert_eq!(state.read_with(cx, |state, _| state.color()), blue);
        assert_eq!(
            state
                .read_with(cx, |state, cx| state.hex_input.read(cx).value(cx))
                .as_str(),
            "#0000FF",
            "set_color synchronizes the field to the normalized hex"
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.active_tab()),
            ColorPickerTab::Hsla,
            "set_color does not switch the panel's tab"
        );
        assert_hsla_tab_is_drawn(cx, "set_color on the open panel");
        assert!(panel_is_drawn(cx), "set_color leaves an open panel open");

        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        settle(cx);
        assert!(!panel_is_drawn(cx), "the second press closes the panel");
        assert_eq!(
            state.read_with(cx, |state, _| state.active_tab()),
            ColorPickerTab::Hsla,
            "closing the panel leaves the non-default tab selected"
        );

        let green = hsla(1.0 / 3.0, 1.0, 0.5, 1.0);
        cx.update(|window, cx| {
            state.update(cx, |state, cx| state.set_color(green, window, cx));
        });
        settle(cx);

        assert_eq!(
            changes.borrow().len(),
            0,
            "a programmatic change while closed emits nothing"
        );
        assert_eq!(state.read_with(cx, |state, _| state.color()), green);
        assert_eq!(
            state
                .read_with(cx, |state, cx| state.hex_input.read(cx).value(cx))
                .as_str(),
            "#00FF00",
            "set_color synchronizes the field even while the panel is closed"
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.active_tab()),
            ColorPickerTab::Hsla,
            "a programmatic change while closed must not reset the tab"
        );
        assert!(
            !panel_is_drawn(cx),
            "set_color must not open a closed panel"
        );

        // Reopening makes the closed-panel tab assertion visible: the panel has to draw the
        // HSLA tab it was closed on, not the Palette grid a reset would have selected.
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        settle(cx);
        assert!(panel_is_drawn(cx), "the trigger opens the panel again");
        assert_hsla_tab_is_drawn(cx, "reopening after a programmatic change");
    }

    #[gpui_kit::test]
    fn clicking_the_hue_track_in_the_open_panel_should_set_the_hue_and_emit_once(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        click_selector(cx, "TEST_COLOR_PICKER_TAB_HSLA");
        changes.borrow_mut().clear();

        let target = track_point(cx, HslaChannel::Hue, 0.25);
        cx.simulate_click(target, Modifiers::none());
        settle(cx);

        let color = state.read_with(cx, |state, _| state.color());
        assert!(
            (color.h - 0.25).abs() < 1e-4,
            "hue is {}, expected 0.25",
            color.h
        );
        assert_eq!(color.s, DEFAULT_COLOR.s, "saturation must not move");
        assert_eq!(color.l, DEFAULT_COLOR.l, "lightness must not move");
        assert_eq!(color.a, DEFAULT_COLOR.a, "alpha must not move");
        assert_eq!(
            changes.borrow().as_slice(),
            &[color],
            "one click on the open panel's hue track emits exactly once"
        );
        assert!(
            panel_is_drawn(cx),
            "editing a channel in the open panel leaves it open"
        );
    }

    #[gpui_kit::test]
    fn dragging_the_saturation_track_in_the_open_panel_should_follow_the_pointer(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        click_selector(cx, "TEST_COLOR_PICKER_TAB_HSLA");
        changes.borrow_mut().clear();

        let start = track_point(cx, HslaChannel::Saturation, 0.2);
        let launch = track_point(cx, HslaChannel::Saturation, 0.3);
        let target = track_point(cx, HslaChannel::Saturation, 0.8);
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        // The first move starts the drag; it only updates the color from the second move on.
        cx.simulate_mouse_move(launch, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_move(target, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
        settle(cx);

        let color = state.read_with(cx, |state, _| state.color());
        assert!(
            (color.s - 0.8).abs() < 1e-4,
            "saturation is {}, expected 0.8",
            color.s
        );
        assert_eq!(color.h, DEFAULT_COLOR.h, "hue must not move");
        assert_eq!(color.l, DEFAULT_COLOR.l, "lightness must not move");
        assert_eq!(color.a, DEFAULT_COLOR.a, "alpha must not move");
        assert!(
            changes.borrow().len() >= 2,
            "the press and the drag to 80% both change the color, got {} events",
            changes.borrow().len()
        );
        assert!(
            changes.borrow().iter().all(|change| {
                change.h == DEFAULT_COLOR.h
                    && change.l == DEFAULT_COLOR.l
                    && change.a == DEFAULT_COLOR.a
            }),
            "every drag event only changes saturation"
        );
        assert!(
            panel_is_drawn(cx),
            "dragging a channel in the open panel leaves it open"
        );
    }

    #[gpui_kit::test]
    fn dragging_past_the_track_in_the_open_panel_should_clamp_and_keep_it_open(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        click_selector(cx, "TEST_COLOR_PICKER_TAB_HSLA");
        changes.borrow_mut().clear();

        let start = track_point(cx, HslaChannel::Saturation, 0.2);
        let launch = track_point(cx, HslaChannel::Saturation, 0.3);
        let track = cx
            .debug_bounds(HslaChannel::Saturation.debug_selector())
            .expect("the open panel draws the saturation track");
        let outside = point(
            track.origin.x + track.size.width + px(50.),
            track.origin.y + px(track.size.height.as_f32() / 2.0),
        );

        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(launch, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_move(outside, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_up(outside, MouseButton::Left, Modifiers::none());
        settle(cx);

        let color = state.read_with(cx, |state, _| state.color());
        assert_eq!(
            color.s, 1.0,
            "a pointer beyond the open panel's track clamps saturation to its maximum"
        );
        assert_eq!(color.h, DEFAULT_COLOR.h, "hue must not move");
        assert_eq!(color.l, DEFAULT_COLOR.l, "lightness must not move");
        assert_eq!(color.a, DEFAULT_COLOR.a, "alpha must not move");
        assert!(
            !changes.borrow().is_empty(),
            "the drag that left the track still changed the color"
        );
        assert!(
            panel_is_drawn(cx),
            "dragging out of the track does not close the panel"
        );
    }

    #[gpui_kit::test]
    fn switching_the_theme_should_repaint_the_open_panel_and_keep_its_color(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        click_selector(cx, "TEST_COLOR_PICKER_TAB_HSLA");
        let before = state.read_with(cx, |state, _| state.color());
        assert_eq!(
            cx.read_global(|theme: &Theme, _| theme.appearance),
            Appearance::Light,
            "the harness starts in the light appearance"
        );

        cx.update(|_, cx| theme::apply(Appearance::Dark, cx));
        settle(cx);

        assert_eq!(
            cx.read_global(|theme: &Theme, _| theme.appearance),
            Appearance::Dark,
            "applying the dark appearance installs it in the theme the panel draws with"
        );
        assert!(
            panel_is_drawn(cx),
            "switching to the dark appearance leaves the panel open"
        );
        assert_hsla_tab_is_drawn(cx, "the dark panel");
        assert_eq!(
            state.read_with(cx, |state, _| state.color()),
            before,
            "switching the theme does not change the color"
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.active_tab()),
            ColorPickerTab::Hsla,
            "switching the theme does not change the tab"
        );

        cx.update(|_, cx| theme::apply(Appearance::Light, cx));
        settle(cx);

        assert_eq!(
            cx.read_global(|theme: &Theme, _| theme.appearance),
            Appearance::Light,
            "switching back installs the light appearance again"
        );
        assert!(
            panel_is_drawn(cx),
            "switching back to the light appearance leaves the panel open"
        );
        assert_hsla_tab_is_drawn(cx, "the light panel");
        assert_eq!(
            state.read_with(cx, |state, _| state.color()),
            before,
            "switching back does not change the color either"
        );
    }

    #[test]
    fn labels_should_default_to_the_panel_text() {
        let labels = ColorPickerLabels::default();

        assert_eq!(labels.palette.as_str(), "Palette");
        assert_eq!(labels.hsla.as_str(), "HSLA");
        assert_eq!(labels.hue.as_str(), "Hue");
        assert_eq!(labels.saturation.as_str(), "Saturation");
        assert_eq!(labels.lightness.as_str(), "Lightness");
        assert_eq!(labels.alpha.as_str(), "Alpha");
    }

    #[gpui_kit::test]
    fn set_labels_should_notify_only_when_the_text_changes(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        assert!(
            changes.borrow().is_empty(),
            "building the harness emits no ColorChanged"
        );
        // Only the calls below are under test; nothing in this test sets a color.
        changes.borrow_mut().clear();
        let notifications = Rc::new(Cell::new(0));
        let counter = notifications.clone();
        let _observation = cx.update(|_, cx| {
            cx.observe(&state, move |_, _| {
                counter.set(counter.get() + 1);
            })
        });

        let labels = state.read_with(cx, |state, _| state.labels.clone());
        state.update(cx, |state, cx| state.set_labels(labels.clone(), cx));
        cx.run_until_parked();
        assert_eq!(
            notifications.get(),
            0,
            "setting the labels already in place is a no-op"
        );
        assert!(
            changes.borrow().is_empty(),
            "setting the labels already in place emits no ColorChanged"
        );

        let mut changed = labels;
        changed.palette = SharedString::new_static("Palette!");
        state.update(cx, |state, cx| state.set_labels(changed.clone(), cx));
        cx.run_until_parked();
        assert_eq!(
            notifications.get(),
            1,
            "new labels notify the panel once so they are drawn"
        );
        assert!(
            changes.borrow().is_empty(),
            "new labels repaint the panel without emitting a ColorChanged"
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.labels.clone()),
            changed
        );

        state.update(cx, |state, cx| state.set_labels(changed.clone(), cx));
        cx.run_until_parked();
        assert_eq!(
            notifications.get(),
            1,
            "setting the same new labels again is a no-op"
        );
        assert!(
            changes.borrow().is_empty(),
            "setting the same new labels again emits no ColorChanged"
        );
    }

    #[gpui_kit::test]
    fn set_active_tab_should_notify_only_when_the_tab_changes(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        assert!(
            changes.borrow().is_empty(),
            "building the harness emits no ColorChanged"
        );
        // Only the calls below are under test; nothing in this test sets a color.
        changes.borrow_mut().clear();
        let notifications = Rc::new(Cell::new(0));
        let counter = notifications.clone();
        let _observation = cx.update(|_, cx| {
            cx.observe(&state, move |_, _| {
                counter.set(counter.get() + 1);
            })
        });

        state.update(cx, |state, cx| {
            state.set_active_tab(ColorPickerTab::Palette, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            notifications.get(),
            0,
            "selecting the tab already shown is a no-op"
        );
        assert!(
            changes.borrow().is_empty(),
            "selecting the tab already shown emits no ColorChanged"
        );

        state.update(cx, |state, cx| {
            state.set_active_tab(ColorPickerTab::Hsla, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            notifications.get(),
            1,
            "selecting another tab notifies the panel once"
        );
        assert!(
            changes.borrow().is_empty(),
            "selecting another tab repaints without emitting a ColorChanged"
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.active_tab()),
            ColorPickerTab::Hsla
        );

        state.update(cx, |state, cx| {
            state.set_active_tab(ColorPickerTab::Hsla, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            notifications.get(),
            1,
            "selecting the tab already shown again is a no-op"
        );
        assert!(
            changes.borrow().is_empty(),
            "selecting the tab already shown again emits no ColorChanged"
        );
    }

    #[gpui_kit::test]
    fn the_active_tab_should_persist_across_closing_and_reopening_the_panel(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = harness(cx);
        let state = harness.read_with(cx, |harness, _| harness.picker.clone());
        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        click_selector(cx, "TEST_COLOR_PICKER_TAB_HSLA");
        assert_eq!(
            state.read_with(cx, |state, _| state.active_tab()),
            ColorPickerTab::Hsla
        );

        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        settle(cx);
        assert!(!panel_is_drawn(cx), "the second press closes the panel");

        press(cx, "TEST_COLOR_PICKER_TRIGGER");
        settle(cx);

        assert!(panel_is_drawn(cx), "the panel opens again");
        assert_eq!(
            state.read_with(cx, |state, _| state.active_tab()),
            ColorPickerTab::Hsla,
            "the panel opens on the tab it was closed on"
        );
        assert!(
            cx.debug_bounds(HslaChannel::Hue.debug_selector()).is_some(),
            "the reopened panel draws the HSLA tab the state still selects"
        );
        assert!(
            cx.debug_bounds("TEST_COLOR_PICKER_GRID").is_none(),
            "the reopened panel does not fall back to the palette tab"
        );
    }

    #[gpui_kit::test]
    fn a_track_narrower_than_its_thumb_should_paint_without_panicking(cx: &mut TestAppContext) {
        let (_harness, cx) = harness(cx);

        cx.draw(
            point(px(0.), px(0.)),
            size(px(100.), px(100.)),
            |_window, _cx| {
                canvas(
                    |_, _, _| (),
                    |bounds, (), window, _| {
                        // Ten pixels is narrower than the twelve-pixel thumb, so the clamp's two
                        // bounds would cross; the slices still have to paint.
                        paint_track(
                            window,
                            Bounds::new(bounds.origin, size(px(10.), px(14.))),
                            HslaChannel::Hue,
                            hsla(0.5, 0.5, 0.5, 1.0),
                            0.5,
                            hsla(0., 0., 1., 1.),
                            hsla(0., 0., 0., 0.35),
                        );
                    },
                )
                .size_full()
            },
        );
    }
}

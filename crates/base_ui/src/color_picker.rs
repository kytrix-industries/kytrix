//! A color picker: its state, its events and the panel that presents them.
//!
//! [`ColorPickerState`] owns what the user is editing — the selected color, the active tab, the
//! localized labels and the hex input the panel draws — and emits [`ColorPickerEvent`]. The split
//! is the one [`crate::input::InputState`] uses: the caller holds the entity and subscribes to it,
//! while the entity itself renders the panel, so the picker repaints its own view on a change
//! instead of the caller having to rebuild it.
//!
//! Performance is the reason the state is a view of its own. [`ColorPickerState::use_keyed`] keeps
//! the entity behind a private slot that never notifies, so the observer GPUI's `use_keyed_state`
//! installs on the caller's view does not fire on every color change. That removes one redundant
//! notify; it is not a promise that the caller never renders, because GPUI's dirty propagation
//! still marks the ancestors of a dirty view. The palette — and, in a later phase, the HSLA
//! tracks — is drawn by a canvas rather than by one element per swatch or slice, which is where
//! the frame cost of a picker usually comes from.
//!
//! Both `gpui-base` and `gpui-component` ship a color picker, and this module deliberately uses
//! neither: it keeps base_ui's [`crate::input::Input`], [`crate::button::Button`], theme roles and
//! event naming (`ColorChanged`), reusing only implementation ideas from those components (hex
//! parsing and formatting, a canvas-drawn palette and canvas-drawn tracks). The imports are
//! explicit for the same reason: `gpui_kit::base` exports its own `ColorPicker`, `ColorPickerState`
//! and `ColorPickerEvent`, so a glob import would collide with the types defined here.

use std::{cell::Cell, rc::Rc, sync::LazyLock};

use gpui_kit::{
    AnyElement, App, AppContext, Bounds, Context, ElementId, Entity, EventEmitter, Hsla,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, Point, Render, RenderOnce,
    Rgba, Role, SharedString, StatefulInteractiveElement, Styled, Subscription, Window,
    base::box_shadow, canvas, checkerboard, div, fill, hsla, point, prelude::FluentBuilder, px,
    rems, size,
};
use smol_str::{SmolStr, format_smolstr};

use crate::{
    button::Button,
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    interactive::InteractiveVariantExt,
    join::Join,
    separator::Separator,
    size::UnifiedSizable,
    style::theme::ThemeExt,
};

/// The panel's outer padding; the content width is what decides the panel's width.
const PANEL_PADDING: Pixels = px(12.);
/// The fixed width every row of the panel is laid out in.
const PANEL_CONTENT_WIDTH: Pixels = px(284.);
/// The vertical gap between the tabs, the content, the divider and the hex row.
const PANEL_SECTION_GAP: Pixels = px(12.);

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

/// What a [`ColorPickerState`] reports to its subscribers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColorPickerEvent {
    /// A user action changed the color.
    ///
    /// [`ColorPickerState::set_color`] does not emit this: a programmatic change is silent.
    ColorChanged(Hsla),
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

/// The color picker's business state and the view that renders its panel.
pub struct ColorPickerState {
    color: Hsla,
    active_tab: ColorPickerTab,
    labels: ColorPickerLabels,
    hex_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

/// Holds the picker state inside `use_keyed_state` without ever notifying it.
///
/// The entity is created with `cx.new`, not by `use_keyed_state`, so its own notifies are not
/// forwarded to the view that called `use_keyed_state`. This slot therefore only keeps the
/// entity alive across frames; it publishes nothing by itself.
struct ColorPickerStateSlot {
    state: Entity<ColorPickerState>,
}

impl ColorPickerState {
    /// Creates or reuses the picker state for `id`.
    ///
    /// This is the creation route a view should use, and it must be called from a rendering
    /// context that has a [`Window`] — typically the view's `Render` — because it stores the
    /// state in `window.use_keyed_state`. The state lives in a private
    /// [`ColorPickerStateSlot`] held by that keyed state; the slot never notifies, so the
    /// observer `use_keyed_state` registers on the calling view does not fire when the picker
    /// notifies, and the picker's own view repaints instead. That saves the keyed observer's
    /// extra notify, not the caller's render: GPUI's dirty propagation still marks ancestors of
    /// a dirty view.
    ///
    /// The same `id` reuses the state created first; `default_color` only applies when the state
    /// is created.
    pub fn use_keyed(
        id: impl Into<ElementId>,
        default_color: Hsla,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let id: ElementId = id.into();
        let slot = window.use_keyed_state((id, "color-picker-state-slot"), cx, |window, cx| {
            ColorPickerStateSlot {
                state: cx.new(|cx| ColorPickerState::new(default_color, window, cx)),
            }
        });

        slot.read(cx).state.clone()
    }

    /// Creates the state directly, without the keyed slot.
    ///
    /// Prefer [`ColorPickerState::use_keyed`] in a view. This constructor does not deduplicate
    /// itself, so the caller owns whatever reuse it wants; it needs a window because it builds
    /// the hex input the panel draws.
    ///
    /// Do not call it inside `window.use_keyed_state`: that would make the state itself the
    /// keyed value, so the observer `use_keyed_state` installs on the caller would forward every
    /// notify of this state to the caller as an extra notify. `cx.new(...)` is the owning
    /// context this constructor is for — a test, or a subsystem that already owns the entity.
    pub fn new(default_color: Hsla, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        let subscription = cx.subscribe_in(
            &hex_input,
            window,
            |this, _, event: &InputStateEvent, window, cx| match event {
                InputStateEvent::Change(text) => this.on_hex_change(text, window, cx),
                InputStateEvent::PressEnter(text) | InputStateEvent::Blur(text) => {
                    this.on_hex_commit(text, window, cx)
                }
            },
        );

        Self {
            color: default_color,
            active_tab: ColorPickerTab::default(),
            labels: ColorPickerLabels::default(),
            hex_input,
            _subscriptions: vec![subscription],
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
    /// The hex field is synchronized and the panel repaints, but no
    /// [`ColorPickerEvent::ColorChanged`] is emitted: a programmatic change is not a user action.
    pub fn set_color(&mut self, color: Hsla, window: &mut Window, cx: &mut Context<Self>) {
        self.color = color;
        self.write_hex(color, window, cx);
        cx.notify();
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

        self.color = color;

        if sync_hex {
            self.write_hex(color, window, cx);
        }

        cx.emit(ColorPickerEvent::ColorChanged(color));
        cx.notify();
    }

    /// Applies the palette swatch at `index`.
    ///
    /// This is a user action, so it goes through [`ColorPickerState::set_user_color`]: the event
    /// is emitted only when the swatch differs from the current color, and the hex field is
    /// synchronized. An index outside the palette is ignored rather than panicking; the only
    /// caller passes an index a hit test from the same palette produced.
    fn select_palette(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(swatch) = PALETTE.get(index) else {
            return;
        };

        self.set_user_color(swatch.hsla, true, window, cx);
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

    /// Builds one of the two tab buttons.
    ///
    /// The selected tab is painted with an explicit fill rather than the ghost variant's veil,
    /// because the veil is a hover wash and cannot express a selected segment.
    fn tab_button(
        &self,
        cx: &mut Context<Self>,
        state: Entity<Self>,
        tab: ColorPickerTab,
        active: ColorPickerTab,
        label: SharedString,
    ) -> Button {
        let (fg_default, fg_muted, selected_fill) = {
            let theme = cx.theme();
            (
                theme.roles.fg.default.hsla,
                theme.roles.fg.muted.hsla,
                theme.roles.surface.overlay.hsla,
            )
        };
        let selected = tab == active;
        let id: ElementId = match tab {
            ColorPickerTab::Palette => ("color-picker-tab-palette", cx.entity_id()).into(),
            ColorPickerTab::Hsla => ("color-picker-tab-hsla", cx.entity_id()).into(),
        };

        Button::new(id)
            .ghost()
            .small()
            .flex_1()
            .justify_center()
            .aria_selected(selected)
            .text_color(if selected { fg_default } else { fg_muted })
            .when(selected, |this| this.bg(selected_fill))
            .debug_selector(move || match tab {
                ColorPickerTab::Palette => "TEST_COLOR_PICKER_TAB_PALETTE".into(),
                ColorPickerTab::Hsla => "TEST_COLOR_PICKER_TAB_HSLA".into(),
            })
            .child(label)
            .on_click(move |_, _, cx| {
                state.update(cx, |state, cx| state.set_active_tab(tab, cx));
            })
    }
}

impl EventEmitter<ColorPickerEvent> for ColorPickerState {}

impl Render for ColorPickerState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Read the theme through Copy values before anything borrows the context mutably; the
        // panel must paint from the values of this frame, not from a cached theme.
        let (panel_background, panel_border, panel_shadow, tab_track, fg_muted) = {
            let theme = cx.theme();
            (
                theme.components.menu.background.hsla,
                theme.roles.line.divider.hsla,
                theme.components.menu.shadow.hsla,
                theme.roles.surface.sunken.hsla,
                theme.roles.fg.muted.hsla,
            )
        };

        let active = self.active_tab;
        let state = cx.entity();
        let content: AnyElement = match active {
            ColorPickerTab::Palette => {
                PaletteGrid::new(state.clone(), self.color, self.labels.palette.clone())
                    .into_any_element()
            }
            // The HSLA sliders replace this placeholder in the next phase; until then the tab
            // still exercises the panel's shell and its tab switch.
            ColorPickerTab::Hsla => div()
                .h(px(120.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .bg(tab_track)
                .text_size(rems(0.75))
                .text_color(fg_muted)
                .child(self.labels.hsla.clone())
                .into_any_element(),
        };

        div()
            .p(PANEL_PADDING)
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
                    .gap(PANEL_SECTION_GAP)
                    .child(
                        Join::default()
                            .w_full()
                            .p_1()
                            .bg(tab_track)
                            .rounded_sm()
                            .button(self.tab_button(
                                cx,
                                state.clone(),
                                ColorPickerTab::Palette,
                                active,
                                self.labels.palette.clone(),
                            ))
                            .button(self.tab_button(
                                cx,
                                state,
                                ColorPickerTab::Hsla,
                                active,
                                self.labels.hsla.clone(),
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
                            .child(ColorWell::new(self.color).size(px(30.)).radius(px(6.)))
                            .child(div().flex_1().child(Input::new(&self.hex_input).medium())),
                    ),
            )
    }
}

/// A small color block: the picker's trigger and its footer preview.
///
/// It paints everything in one canvas so a swatch costs one element, and a translucent color is
/// shown over a checkerboard rather than over whatever happens to be behind the block.
#[derive(IntoElement)]
struct ColorWell {
    color: Hsla,
    size: Pixels,
    radius: Pixels,
}

impl ColorWell {
    /// A well showing `color`, at the size the trigger uses.
    fn new(color: Hsla) -> Self {
        Self {
            color,
            size: px(18.),
            radius: px(4.),
        }
    }

    /// Sets the size of the block.
    fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }

    /// Sets the corner radius of the block.
    fn radius(mut self, radius: Pixels) -> Self {
        self.radius = radius;
        self
    }
}

impl RenderOnce for ColorWell {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            color,
            size,
            radius,
        } = self;
        let (base, checker, border) = {
            let theme = cx.theme();
            (
                theme.roles.surface.overlay.hsla,
                theme.roles.surface.sunken.hsla,
                theme.roles.line.divider.hsla,
            )
        };
        // An alpha that quantizes to opaque displays as opaque in the hex field, so it must not
        // show a checkerboard here either.
        let translucent = rgba8(color)[3] < 255;

        div().relative().w(size).h(size).child(
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
                    window.paint_quad(
                        fill(bounds, hsla(0., 0., 0., 0.))
                            .border_widths(px(1.))
                            .border_color(border)
                            .corner_radii(radius),
                    );
                },
            )
            .absolute()
            .size_full(),
        )
    }
}

/// The 12×12 palette of swatches, drawn by a single canvas.
///
/// The state is only used to route a click back to it; the color it draws and the label it
/// announces are handed in by the panel, so this element never reads the state while rendering.
#[derive(IntoElement)]
struct PaletteGrid {
    state: Entity<ColorPickerState>,
    color: Hsla,
    label: SharedString,
}

impl PaletteGrid {
    /// Builds the grid for `state`, highlighting `color` if it is one of the swatches.
    fn new(state: Entity<ColorPickerState>, color: Hsla, label: SharedString) -> Self {
        Self {
            state,
            color,
            label,
        }
    }
}

impl RenderOnce for PaletteGrid {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let Self {
            state,
            color,
            label,
        } = self;
        let bounds = Rc::new(Cell::new(Bounds::default()));
        let prepaint_bounds = bounds.clone();
        let selected = palette_index(color);

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
                        for (index, swatch) in PALETTE.iter().enumerate() {
                            let row = index / PALETTE_COLS;
                            let col = index % PALETTE_COLS;
                            let origin = point(
                                bounds.left() + px(PALETTE_STEP * col as f32),
                                bounds.top() + px(PALETTE_STEP * row as f32),
                            );
                            let cell =
                                Bounds::new(origin, size(px(PALETTE_CELL), px(PALETTE_CELL)));

                            window.paint_quad(fill(cell, swatch.hsla).corner_radii(px(4.)));

                            if selected == Some(index) {
                                let inner = Bounds::new(
                                    point(origin.x + px(1.), origin.y + px(1.)),
                                    size(px(PALETTE_CELL - 2.0), px(PALETTE_CELL - 2.0)),
                                );
                                window.paint_quad(
                                    fill(inner, hsla(0., 0., 0., 0.))
                                        .border_widths(px(2.))
                                        .border_color(selection_ring_color(swatch.hsla))
                                        .corner_radii(px(3.)),
                                );
                            }
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                let Some(index) = palette_index_at(bounds.get(), event.position) else {
                    return;
                };

                state.update(cx, |state, cx| state.select_palette(index, window, cx));
            })
    }
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

/// One palette swatch: the color it draws and its eight-bit sRGB channels.
struct PaletteSwatch {
    hsla: Hsla,
    rgb: [u8; 3],
}

/// The hue of a palette row, or `None` for the neutral row.
fn palette_hue(row: usize) -> Option<f32> {
    (row > 0).then(|| (row - 1) as f32 / (PALETTE_ROWS as f32 - 1.0))
}

/// The twelve by twelve palette, generated once.
///
/// Row 0 is the neutral ramp and each following row is one hue family; each column is one
/// lightness step. The table is a process-wide static because generating it costs 144 HSL to
/// RGB conversions, which every picker would otherwise repeat when its palette tab opens.
static PALETTE: LazyLock<[PaletteSwatch; PALETTE_ROWS * PALETTE_COLS]> = LazyLock::new(|| {
    std::array::from_fn(|index| {
        let row = index / PALETTE_COLS;
        let col = index % PALETTE_COLS;
        let color = match palette_hue(row) {
            Some(hue) => hsla(hue, PALETTE_SATURATION, PALETTE_LIGHTNESS[col], 1.0),
            None => hsla(0., 0., PALETTE_LIGHTNESS[col], 1.0),
        };
        let [r, g, b, _] = rgba8(color);

        PaletteSwatch {
            hsla: color,
            rgb: [r, g, b],
        }
    })
});

/// The index of the swatch `color` displays as, if any.
///
/// The comparison is on the eight-bit sRGB channels rather than on the float [`Hsla`]: a color
/// that came from the hex field is eight-bit by construction, and selecting a swatch writes the
/// very `Hsla` whose `rgb` entry was generated here. Alpha has to quantize to opaque, because
/// every swatch is opaque and a noticeably translucent color is not the swatch it resembles.
fn palette_index(color: Hsla) -> Option<usize> {
    let [r, g, b, a] = rgba8(color);

    if a != 255 {
        return None;
    }

    PALETTE.iter().position(|swatch| swatch.rgb == [r, g, b])
}

/// The color of the ring around the selected swatch.
///
/// The ring has to read at both ends of the ramp, so it flips with the swatch's luma rather than
/// with the theme: a light swatch gets a dark ring and a dark swatch a light one.
fn selection_ring_color(color: Hsla) -> Hsla {
    let [r, g, b, _] = rgba8(color);
    let luma = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;

    if luma > 140.0 {
        hsla(0., 0., 0., 0.85)
    } else {
        hsla(0., 0., 1., 0.95)
    }
}

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
    use std::{cell::RefCell, rc::Rc};

    use gpui_kit::{
        AppContext, Bounds, Context, Entity, Hsla, IntoElement, Modifiers, ParentElement, Pixels,
        Point, Render, Styled, Subscription, TestAppContext, Window, div, hsla, point, px, size,
    };

    use super::{
        ColorPickerEvent, ColorPickerState, PALETTE, PALETTE_COLS, PALETTE_LIGHTNESS, PALETTE_ROWS,
        PALETTE_SATURATION, PALETTE_SIZE, PALETTE_STEP, PANEL_CONTENT_WIDTH, hex_string,
        palette_hue, palette_index, palette_index_at, parse_hex, rgba8, selection_ring_color,
    };
    use crate::style::theme;

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
            let swatch = &PALETTE[col];

            assert_eq!(swatch.hsla.s, 0.0, "row 0 is the neutral ramp");
            assert_eq!(swatch.hsla.l, PALETTE_LIGHTNESS[col]);
            assert_eq!(swatch.hsla.a, 1.0);
            assert_eq!(swatch.rgb[0], swatch.rgb[1]);
            assert_eq!(swatch.rgb[1], swatch.rgb[2]);
        }

        for row in 1..PALETTE_ROWS {
            let hue = palette_hue(row).expect("every row after the neutral one has a hue");
            assert!((hue - (row - 1) as f32 / (PALETTE_ROWS - 1) as f32).abs() < 1e-6);

            for col in 0..PALETTE_COLS {
                let swatch = &PALETTE[row * PALETTE_COLS + col];

                assert_eq!(swatch.hsla.h, hue);
                assert_eq!(swatch.hsla.s, PALETTE_SATURATION);
                assert_eq!(swatch.hsla.l, PALETTE_LIGHTNESS[col]);
                assert_eq!(swatch.hsla.a, 1.0);
            }
        }

        assert_eq!(palette_hue(0), None);
    }

    #[test]
    fn palette_index_should_find_every_swatch() {
        for (index, swatch) in PALETTE.iter().enumerate() {
            assert_eq!(palette_index(swatch.hsla), Some(index), "swatch {index}");
        }
    }

    #[test]
    fn palette_index_should_reject_colors_that_are_not_swatches() {
        assert_eq!(
            palette_index(hsla(0.5, 0.1, 0.5, 1.0)),
            None,
            "a desaturated mid-tone is not one of the saturated or neutral swatches"
        );

        let mut translucent = PALETTE[28].hsla;
        translucent.a = 0.5;
        assert_eq!(
            palette_index(translucent),
            None,
            "a translucent color does not display as the opaque swatch it resembles"
        );

        let mut almost_opaque = PALETTE[28].hsla;
        almost_opaque.a = 254.5 / 255.0;
        assert_eq!(
            palette_index(almost_opaque),
            Some(28),
            "an alpha that quantizes to opaque displays as the swatch"
        );
    }

    #[test]
    fn selection_ring_should_contrast_with_the_swatch() {
        assert_eq!(
            selection_ring_color(hsla(0., 0., 1., 1.)),
            hsla(0., 0., 0., 0.85),
            "a white swatch gets a dark ring"
        );
        assert_eq!(
            selection_ring_color(hsla(0., 0., 0., 1.)),
            hsla(0., 0., 1., 0.95),
            "a black swatch gets a light ring"
        );
        assert_eq!(
            selection_ring_color(PALETTE[PALETTE_COLS - 1].hsla),
            hsla(0., 0., 0., 0.85),
            "the light end of the ramp gets a dark ring"
        );
        assert_eq!(
            selection_ring_color(PALETTE[0].hsla),
            hsla(0., 0., 1., 0.95),
            "the dark end of the ramp gets a light ring"
        );
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

    struct PaletteHarness {
        state: Entity<ColorPickerState>,
        changes: Rc<RefCell<Vec<Hsla>>>,
        _subscription: Subscription,
    }

    impl PaletteHarness {
        fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
            let state = cx.new(|cx| ColorPickerState::new(DEFAULT_COLOR, window, cx));
            let changes = Rc::new(RefCell::new(Vec::new()));
            let sink = changes.clone();
            let subscription = cx.subscribe(&state, move |_, _, event: &ColorPickerEvent, _| {
                let ColorPickerEvent::ColorChanged(color) = event;
                sink.borrow_mut().push(*color);
            });

            Self {
                state,
                changes,
                _subscription: subscription,
            }
        }
    }

    impl Render for PaletteHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.state.clone())
        }
    }

    #[gpui_kit::test]
    fn select_palette_should_apply_the_swatch_emit_once_and_sync_the_hex_field(
        cx: &mut TestAppContext,
    ) {
        cx.update(theme::install);
        let (harness, cx) = cx.add_window_view(PaletteHarness::new);
        let state = harness.read_with(cx, |harness, _| harness.state.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        let swatch = PALETTE[28].hsla;

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
    fn the_palette_grid_should_draw_at_284_pixels_and_select_the_clicked_swatch(
        cx: &mut TestAppContext,
    ) {
        cx.update(theme::install);
        let (harness, cx) = cx.add_window_view(PaletteHarness::new);
        let state = harness.read_with(cx, |harness, _| harness.state.clone());
        let changes = harness.read_with(cx, |harness, _| harness.changes.clone());
        cx.simulate_resize(size(px(900.), px(800.)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let grid = cx
            .debug_bounds("TEST_COLOR_PICKER_GRID")
            .expect("the palette tab draws the grid");
        assert_eq!(grid.size, size(px(PALETTE_SIZE), px(PALETTE_SIZE)));

        let target = point(
            grid.origin.x + px(PALETTE_STEP * 4.0 + 10.0),
            grid.origin.y + px(PALETTE_STEP * 2.0 + 10.0),
        );
        cx.simulate_click(target, Modifiers::none());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let swatch = PALETTE[28].hsla;
        assert_eq!(state.read_with(cx, |state, _| state.color()), swatch);
        assert_eq!(changes.borrow().as_slice(), &[swatch]);
    }
}

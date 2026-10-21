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
//! still marks the ancestors of a dirty view. The palette and the HSLA tracks, once they land, are
//! drawn by a canvas rather than by one element per swatch or slice, which is where the frame cost
//! of a picker usually comes from.
//!
//! Both `gpui-base` and `gpui-component` ship a color picker, and this module deliberately uses
//! neither: it keeps base_ui's [`crate::input::Input`], [`crate::button::Button`], theme roles and
//! event naming (`ColorChanged`), reusing only implementation ideas from those components (hex
//! parsing and formatting, a canvas-drawn palette and canvas-drawn tracks). The imports are
//! explicit for the same reason: `gpui_kit::base` exports its own `ColorPicker`, `ColorPickerState`
//! and `ColorPickerEvent`, so a glob import would collide with the types defined here.

use gpui_kit::{
    App, AppContext, Context, ElementId, Entity, EventEmitter, Hsla, InteractiveElement,
    IntoElement, ParentElement, Pixels, Render, Rgba, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Window, base::box_shadow, div, prelude::FluentBuilder, px, rems,
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
    /// This is the creation route a view should use. The state lives in a private
    /// [`ColorPickerStateSlot`] held by `window.use_keyed_state`; the slot never notifies, so the
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
    /// Prefer [`ColorPickerState::use_keyed`] in a view: this constructor does not deduplicate
    /// itself, so the caller owns whatever reuse it wants. It needs a window because it builds
    /// the hex input the panel draws.
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
                    // The palette grid and the HSLA sliders replace this placeholder in later
                    // phases; until then the panel still exercises its shell, tabs and hex row.
                    .child(
                        div()
                            .h(px(120.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .bg(tab_track)
                            .text_size(rems(0.75))
                            .text_color(fg_muted)
                            .child(match active {
                                ColorPickerTab::Palette => self.labels.palette.clone(),
                                ColorPickerTab::Hsla => self.labels.hsla.clone(),
                            }),
                    )
                    .child(Separator::horizontal())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .debug_selector(|| "TEST_COLOR_PICKER_HEX_INPUT".into())
                            .child(div().flex_1().child(Input::new(&self.hex_input).medium())),
                    ),
            )
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

#[cfg(test)]
mod tests {
    use gpui_kit::hsla;

    use super::{hex_string, parse_hex, rgba8};

    #[test]
    fn parse_hex_should_accept_every_supported_length() {
        assert_eq!(
            parse_hex("#fff"),
            parse_hex("#ffffff"),
            "the three-digit form names the same color as the six-digit one"
        );
        assert_eq!(
            parse_hex("#ffff"),
            parse_hex("#ffffffff"),
            "the four-digit form names the same color as the eight-digit one"
        );
        assert_eq!(
            parse_hex("#f00"),
            parse_hex("#ff0000"),
            "a short digit is duplicated, not scaled"
        );
        assert_eq!(parse_hex("#abcd"), parse_hex("#aabbccdd"));
        assert_eq!(
            rgba8(parse_hex("#ff0000").expect("red parses")),
            [255, 0, 0, 255]
        );
        assert_eq!(
            rgba8(parse_hex("#00ff0080").expect("transparent green parses")),
            [0, 255, 0, 128]
        );
    }

    #[test]
    fn parse_hex_should_ignore_the_hash_surrounding_whitespace_and_case() {
        assert_eq!(parse_hex("ff0000"), parse_hex("#FF0000"));
        assert_eq!(parse_hex("  #Ff0000  "), parse_hex("#ff0000"));
        assert_eq!(parse_hex("FfFf"), parse_hex("#ffff"));
    }

    #[test]
    fn parse_hex_should_reject_anything_that_is_not_hex() {
        for text in [
            "", "#", "#12", "#12345", "#1234567", "#ggg", "#+f0000", "#-fffff",
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
}

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{Arc, LazyLock},
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, ActionSource, Event},
    interaction::{ActionCtx, DragPhase},
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeKind,
    },
    paint::{
        canvas::Canvas,
        color::{Color, ColorResolver},
        primitive::{Border, Quad, QuadPaint},
        style::{Cursor, StylePatch},
        style_builder::{PaintBuilder, StyleBuilder},
    },
    text::{TextAlign, TextStyle},
    window::WindowId,
};

use crate::{
    kit::{
        input::{self as kit_input, InputNode, InputSpec, InputState},
        layer::{Band, Layer},
        overlay::{
            Alignment, Anchor, AnimationRouter, DismissPolicy, Overlay, OverlayA11y,
            OverlayOptions, OverlaySurface, Placement, Side,
        },
        size::UnifiedSize,
    },
    theme::{
        appearance::Appearance,
        components::{ControlSize, InteractiveElementColors, InteractiveStateColors},
        handle::ThemeHandle,
        role::Role,
    },
};

const PANEL_CONTENT_WIDTH: f32 = 284.0;
const SLIDER_ROW_HEIGHT: f32 = 24.0;
const SLIDER_LABEL_WIDTH: f32 = 72.0;
const SLIDER_VALUE_WIDTH: f32 = 40.0;
const SLIDER_ROW_INNER_GAP: f32 = 8.0;
const TRACK_HEIGHT: f32 = 14.0;
const TRACK_RADIUS: f32 = 7.0;
const TRACK_SLICES: usize = 256;
const THUMB_DIAMETER: f32 = 12.0;
const PALETTE_COLS: usize = 12;
const PALETTE_ROWS: usize = 12;
const PALETTE_CELL: f32 = 20.0;
const PALETTE_GAP: f32 = 4.0;
const PALETTE_STEP: f32 = 24.0;
const PALETTE_SIZE: f32 =
    PALETTE_CELL * PALETTE_COLS as f32 + PALETTE_GAP * (PALETTE_COLS as f32 - 1.0);
const PALETTE_SATURATION: f32 = 0.72;
const PALETTE_LIGHTNESS: [f32; PALETTE_COLS] = [
    0.06, 0.12, 0.18, 0.26, 0.34, 0.42, 0.50, 0.60, 0.70, 0.80, 0.90, 0.96,
];

const TRIGGER_RADIUS: f32 = 4.0;
const TRIGGER_PADDING: f32 = 2.0;
const TRIGGER_WELL_RADIUS: LogicalPx = LogicalPx::new(4.0);
const FOOTER_WELL_SIZE: f32 = 30.0;
const FOOTER_WELL_RADIUS: LogicalPx = LogicalPx::new(6.0);
const PANEL_PADDING: LogicalPx = LogicalPx::new(8.0);
const CONTROL_RADIUS: f32 = 4.0;
const RING_WIDTH: f32 = 2.0;
const RING_INSET: f32 = 1.0;
const CHECKER_CELL: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Hsla {
    h: f32,
    s: f32,
    l: f32,
    a: f32,
}

const fn hsla(h: f32, s: f32, l: f32, a: f32) -> Hsla {
    Hsla { h, s, l, a }
}

fn hsla_to_color(color: Hsla) -> Color {
    let h = color.h.rem_euclid(1.0) * 6.0;
    let s = color.s.clamp(0.0, 1.0);
    let l = color.l.clamp(0.0, 1.0);
    let a = color.a.clamp(0.0, 1.0);
    let chroma = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = chroma * (1.0 - (h % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match h as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = l - chroma / 2.0;

    Color::srgb(r1 + m, g1 + m, b1 + m, a)
}

fn color_to_hsla(color: Color) -> Hsla {
    let [r, g, b, a] = srgb_components(color);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;

    if (max - min).abs() <= f32::EPSILON {
        return hsla(0.0, 0.0, l, a);
    }

    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() <= f32::EPSILON {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if (max - g).abs() <= f32::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };

    hsla(h / 6.0, s, l, a)
}

fn srgb_components(color: Color) -> [f32; 4] {
    if let Color::Srgb { r, g, b, alpha } = color {
        return [r, g, b, alpha];
    }

    let linear = ColorResolver.resolve(color);
    let [r, g, b, a] = linear.components;
    if a <= 0.0 {
        return [0.0, 0.0, 0.0, 0.0];
    }

    [
        linear_to_srgb(r / a),
        linear_to_srgb(g / a),
        linear_to_srgb(b / a),
        a,
    ]
}

fn linear_to_srgb(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);

    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

fn rgba8(color: Color) -> [u8; 4] {
    let [r, g, b, a] = srgb_components(color);
    let quantize = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;

    [quantize(r), quantize(g), quantize(b), quantize(a)]
}

fn hex_string(color: Color) -> SmolStr {
    let [r, g, b, a] = rgba8(color);

    if a < 255 {
        format_smolstr!("#{r:02X}{g:02X}{b:02X}{a:02X}")
    } else {
        format_smolstr!("#{r:02X}{g:02X}{b:02X}")
    }
}

fn parse_hex(text: &str) -> Option<Color> {
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

    Some(Color::srgb(
        component(0)?,
        component(1)?,
        component(2)?,
        if has_alpha { component(3)? } else { 1.0 },
    ))
}

fn hover_ring_color(color: Color) -> Color {
    let [r, g, b, _] = rgba8(color);
    let luma = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;

    if luma > 140.0 {
        Color::srgb(0.0, 0.0, 0.0, 0.85)
    } else {
        Color::srgb(1.0, 1.0, 1.0, 0.95)
    }
}

fn palette_hue(row: usize) -> Option<f32> {
    (row > 0).then(|| (row - 1) as f32 / (PALETTE_ROWS as f32 - 1.0))
}

static PALETTE: LazyLock<[Hsla; PALETTE_ROWS * PALETTE_COLS]> = LazyLock::new(|| {
    std::array::from_fn(|index| {
        let row = index / PALETTE_COLS;
        let col = index % PALETTE_COLS;

        match palette_hue(row) {
            Some(hue) => hsla(hue, PALETTE_SATURATION, PALETTE_LIGHTNESS[col], 1.0),
            None => hsla(0.0, 0.0, PALETTE_LIGHTNESS[col], 1.0),
        }
    })
});

fn palette_cell_bounds(bounds: LogicalRect, row: usize, col: usize) -> LogicalRect {
    let scale_x = bounds.size.width / PALETTE_SIZE;
    let scale_y = bounds.size.height / PALETTE_SIZE;
    LogicalRect::new(
        bounds.origin.x + scale_x * (PALETTE_STEP * col as f32),
        bounds.origin.y + scale_y * (PALETTE_STEP * row as f32),
        scale_x * PALETTE_CELL,
        scale_y * PALETTE_CELL,
    )
}

fn palette_index_at(size: LogicalSize, position: LogicalPoint) -> Option<usize> {
    let (width, height) = (size.width.value(), size.height.value());
    let (x, y) = (position.x.value(), position.y.value());

    if !(0.0..width).contains(&x) || !(0.0..height).contains(&y) {
        return None;
    }

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

fn well_canvas(
    bounds: LogicalRect,
    color: Color,
    base: Color,
    checker: Color,
    radius: LogicalPx,
) -> Canvas {
    let mut canvas = Canvas::new(bounds);
    canvas.push_quad(solid_quad(bounds, radius, base));

    if rgba8(color)[3] < 255 {
        let right = bounds.origin.x + bounds.size.width;
        let bottom = bounds.origin.y + bounds.size.height;
        let cols = (bounds.size.width.value() / CHECKER_CELL).ceil() as usize;
        let rows = (bounds.size.height.value() / CHECKER_CELL).ceil() as usize;
        for row in 0..rows {
            for col in 0..cols {
                if (row + col) % 2 == 0 {
                    continue;
                }
                let x = bounds.origin.x + col as f32 * CHECKER_CELL;
                let y = bounds.origin.y + row as f32 * CHECKER_CELL;
                let width = CHECKER_CELL.min(f32::from(right - x));
                let height = CHECKER_CELL.min(f32::from(bottom - y));
                if width > 0.0 && height > 0.0 {
                    canvas.push_quad(solid_quad(
                        LogicalRect::new(x, y, px(width), px(height)),
                        px(0.0),
                        checker,
                    ));
                }
            }
        }
    }

    canvas.push_quad(solid_quad(bounds, radius, color));

    canvas
}

fn palette_canvas(bounds: LogicalRect, hovered: Option<usize>) -> Canvas {
    let mut canvas = Canvas::new(bounds);
    let scale = (bounds.size.width / PALETTE_SIZE).min(bounds.size.height / PALETTE_SIZE);
    let cell_radius = scale * 4.0;
    let ring_inset = scale * RING_INSET;
    let ring_width = scale * RING_WIDTH;
    let ring_radius = scale * 3.0;

    for (index, swatch) in PALETTE.iter().enumerate() {
        let color = hsla_to_color(*swatch);
        let cell = palette_cell_bounds(bounds, index / PALETTE_COLS, index % PALETTE_COLS);
        canvas.push_quad(solid_quad(cell, cell_radius, color));

        if hovered == Some(index) {
            let inner = LogicalRect::new(
                cell.origin.x + ring_inset,
                cell.origin.y + ring_inset,
                cell.size.width - ring_inset * 2.0,
                cell.size.height - ring_inset * 2.0,
            );
            canvas.push_quad(Quad {
                bounds: inner,
                corner_radius: [ring_radius; 4],
                paint: QuadPaint::Solid {
                    fill: Color::TRANSPARENT,
                    border: Some(Border {
                        width: ring_width,
                        color: hover_ring_color(color),
                    }),
                },
            });
        }
    }

    canvas
}

fn track_canvas(
    bounds: LogicalRect,
    channel: HslaChannel,
    base: Hsla,
    value: f32,
    thumb_fill: Color,
    thumb_border: Color,
) -> Canvas {
    let mut canvas = Canvas::new(bounds);
    let left = bounds.origin.x;
    let top = bounds.origin.y;
    let width = bounds.size.width;
    let height = bounds.size.height;

    if width.value() <= 0.0 || height.value() <= 0.0 {
        return canvas;
    }

    for index in 0..TRACK_SLICES {
        let t0 = index as f32 / TRACK_SLICES as f32;
        let t1 = (index + 1) as f32 / TRACK_SLICES as f32;
        let t = (t0 + t1) / 2.0;
        let color = match channel {
            HslaChannel::Hue => hsla_to_color(hsla(t, 1.0, 0.5, 1.0)),
            HslaChannel::Saturation => hsla_to_color(hsla(base.h, t, base.l, 1.0)),
            HslaChannel::Lightness => hsla_to_color(hsla(base.h, base.s, t, 1.0)),
            HslaChannel::Alpha => hsla_to_color(hsla(base.h, base.s, base.l, t)),
        };
        let x0 = left + width * t0;
        let x1 = left + width * t1;
        let center = (x0 + x1) / 2.0;
        let distance = (center - left).min((left + width) - center).value();
        let inset = if distance < TRACK_RADIUS {
            TRACK_RADIUS
                - (TRACK_RADIUS * TRACK_RADIUS
                    - (TRACK_RADIUS - distance) * (TRACK_RADIUS - distance))
                    .sqrt()
        } else {
            0.0
        };
        let slice_height = height - inset * 2.0;

        if slice_height.value() <= 0.0 {
            continue;
        }

        canvas.push_quad(Quad {
            bounds: LogicalRect::new(x0, top + inset, x1 - x0, slice_height),
            corner_radius: [px(0.0); 4],
            paint: QuadPaint::Solid {
                fill: color,
                border: None,
            },
        });
    }

    if width.value() < THUMB_DIAMETER {
        return canvas;
    }

    let half = THUMB_DIAMETER / 2.0;
    let x = (left + width * value)
        .value()
        .clamp((left + half).value(), (left + width - half).value());
    let y = top + height / 2.0;
    canvas.push_quad(Quad {
        bounds: LogicalRect::new(
            px(x - half),
            y - half,
            px(THUMB_DIAMETER),
            px(THUMB_DIAMETER),
        ),
        corner_radius: [px(half); 4],
        paint: QuadPaint::Solid {
            fill: thumb_fill,
            border: Some(Border {
                width: px(1.0),
                color: thumb_border,
            }),
        },
    });

    canvas
}

fn solid_quad(bounds: LogicalRect, radius: LogicalPx, color: Color) -> Quad {
    Quad {
        bounds,
        corner_radius: [radius.max(px(0.0)); 4],
        paint: QuadPaint::Solid {
            fill: color,
            border: None,
        },
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ColorPickerTab {
    #[default]
    Palette,
    Hsla,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColorPickerLabels {
    pub panel: SmolStr,
    pub palette: SmolStr,
    pub hsla: SmolStr,
    pub hue: SmolStr,
    pub saturation: SmolStr,
    pub lightness: SmolStr,
    pub alpha: SmolStr,
    pub confirm: SmolStr,
}

impl Default for ColorPickerLabels {
    fn default() -> Self {
        Self {
            panel: SmolStr::new_static("Color"),
            palette: SmolStr::new_static("Palette"),
            hsla: SmolStr::new_static("HSLA"),
            hue: SmolStr::new_static("Hue"),
            saturation: SmolStr::new_static("Saturation"),
            lightness: SmolStr::new_static("Lightness"),
            alpha: SmolStr::new_static("Alpha"),
            confirm: SmolStr::new_static("OK"),
        }
    }
}

#[derive(Clone)]
pub struct ColorPicker {
    inner: Rc<Inner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TriggerView {
    node: NodeId,
    well: NodeId,
}

struct Panel {
    window: WindowId,
    body: NodeId,
    body_view: BodyView,
    tab_active: ColorPickerTab,
    appearance: Appearance,
    tabs: [NodeId; 2],
    confirm: NodeId,
    well: NodeId,
    input: InputNode,
    well_last: Option<(LogicalRect, Color)>,
}

enum BodyView {
    Palette(PaletteView),
    Sliders(SliderView),
}

struct PaletteView {
    root: NodeId,
    canvas: NodeId,
    cells: Vec<NodeId>,
    last: Option<(LogicalRect, Option<usize>)>,
}

struct SliderView {
    root: NodeId,
    tracks: [TrackView; 4],
    thumb_fill: Color,
    thumb_border: Color,
}

#[derive(Debug)]
struct TrackView {
    track: NodeId,
    canvas: NodeId,
    last: Option<(LogicalRect, Hsla, f32)>,
}

struct Actions {
    trigger: Action,
    confirm: Action,
    hex_submit: Action,
    palette: Action,
    tab: [Action; 2],
    track: [Action; 4],
    drag: [Action; 4],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HslaChannel {
    Hue,
    Saturation,
    Lightness,
    Alpha,
}

impl HslaChannel {
    const ALL: [Self; 4] = [Self::Hue, Self::Saturation, Self::Lightness, Self::Alpha];

    const fn index(self) -> usize {
        match self {
            Self::Hue => 0,
            Self::Saturation => 1,
            Self::Lightness => 2,
            Self::Alpha => 3,
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::Hue => "hue",
            Self::Saturation => "saturation",
            Self::Lightness => "lightness",
            Self::Alpha => "alpha",
        }
    }

    fn label(self, labels: &ColorPickerLabels) -> SmolStr {
        match self {
            Self::Hue => labels.hue.clone(),
            Self::Saturation => labels.saturation.clone(),
            Self::Lightness => labels.lightness.clone(),
            Self::Alpha => labels.alpha.clone(),
        }
    }

    const fn value(self, color: Hsla) -> f32 {
        match self {
            Self::Hue => color.h,
            Self::Saturation => color.s,
            Self::Lightness => color.l,
            Self::Alpha => color.a,
        }
    }

    const fn display_value(self, color: Hsla) -> i32 {
        match self {
            Self::Hue => (self.value(color) * 360.0).round() as i32,
            Self::Saturation | Self::Lightness | Self::Alpha => {
                (self.value(color) * 100.0).round() as i32
            }
        }
    }

    const fn max(self) -> f32 {
        match self {
            Self::Hue => 360.0,
            Self::Saturation | Self::Lightness | Self::Alpha => 100.0,
        }
    }

    fn value_text(self, color: Hsla) -> SmolStr {
        format_smolstr!("{}", self.display_value(color))
    }
}

struct Inner {
    key: SmolStr,
    theme: ThemeHandle,
    scope: SignalScope,
    overlay: Overlay,
    actions: Actions,
    size: UnifiedSize,
    color: Signal<Color>,
    draft: Signal<Option<Color>>,
    hovered: Signal<Option<usize>>,
    active_tab: Signal<ColorPickerTab>,
    labels: Signal<ColorPickerLabels>,
    hex_state: Signal<InputState>,
    hex_placeholder: Signal<SmolStr>,
    trigger_hovered: Signal<bool>,
    trigger_focused: Signal<bool>,
    tab_hover: Signal<Option<usize>>,
    confirm_hovered: Signal<bool>,
    change: RefCell<Option<Rc<dyn Fn(&mut App, Color)>>>,
    open_callback: RefCell<Option<Rc<dyn Fn(&mut App)>>>,
    trigger: RefCell<Option<TriggerView>>,
    panel: RefCell<Option<Panel>>,
    was_open: Cell<bool>,
    last_hex_text: RefCell<SmolStr>,
    last_hex_focus: Cell<bool>,
    last_trigger_color: Cell<Option<Color>>,
    last_trigger_bounds: Cell<Option<LogicalRect>>,
    last_color: Cell<Color>,
    appearance: Rc<Cell<Appearance>>,
    last_appearance: Cell<Appearance>,
}

impl ColorPicker {
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
        key: impl Into<SmolStr>,
        initial_color: Color,
        size: UnifiedSize,
    ) -> Self {
        let key = key.into();
        let scope = app.signal_scope();
        let actions = Actions {
            trigger: Action::Custom(format_smolstr!("{key}.trigger")),
            confirm: Action::Custom(format_smolstr!("{key}.confirm")),
            hex_submit: Action::Custom(format_smolstr!("{key}.hex.submit")),
            palette: Action::Custom(format_smolstr!("{key}.palette")),
            tab: [
                Action::Custom(format_smolstr!("{key}.tab.palette")),
                Action::Custom(format_smolstr!("{key}.tab.hsla")),
            ],
            track: std::array::from_fn(|index| {
                Action::Custom(format_smolstr!(
                    "{key}.track.{}",
                    HslaChannel::ALL[index].id()
                ))
            }),
            drag: std::array::from_fn(|index| {
                Action::Custom(format_smolstr!(
                    "{key}.drag.{}",
                    HslaChannel::ALL[index].id()
                ))
            }),
        };
        let labels = ColorPickerLabels::default();
        let overlay = Overlay::new(
            app,
            theme.clone(),
            layer,
            router,
            format_smolstr!("{key}:overlay"),
            OverlayOptions {
                placement: Placement::new(Side::Bottom, Alignment::Start),
                gap: px(4.0),
                margin: px(4.0),
                flip: true,
                dismiss: DismissPolicy::OutsideClick,
                modal: false,
                restore_focus: true,
                band: Band::Popup,
                slot: crate::kit::layer::LayerSlot::Exclusive,
                surface: OverlaySurface {
                    padding: PANEL_PADDING,
                    ..OverlaySurface::menu()
                },
                scrim: None,
                a11y: OverlayA11y::dialog(labels.panel.clone()),
                fade: true,
            },
        );
        let inner = Rc::new(Inner {
            key,
            theme,
            scope,
            overlay,
            actions,
            size,
            color: app.signal_scoped(scope, initial_color),
            draft: app.signal_scoped(scope, None),
            hovered: app.signal_scoped(scope, None),
            active_tab: app.signal_scoped(scope, ColorPickerTab::default()),
            labels: app.signal_scoped(scope, labels),
            hex_state: app
                .signal_scoped(scope, InputState::new(hex_string(initial_color).as_str())),
            hex_placeholder: app.signal_scoped(scope, SmolStr::new_static("#RRGGBB")),
            trigger_hovered: app.signal_scoped(scope, false),
            trigger_focused: app.signal_scoped(scope, false),
            tab_hover: app.signal_scoped(scope, None),
            confirm_hovered: app.signal_scoped(scope, false),
            change: RefCell::new(None),
            open_callback: RefCell::new(None),
            trigger: RefCell::new(None),
            panel: RefCell::new(None),
            was_open: Cell::new(false),
            last_hex_text: RefCell::new(hex_string(initial_color)),
            last_hex_focus: Cell::new(false),
            last_trigger_color: Cell::new(None),
            last_trigger_bounds: Cell::new(None),
            last_color: Cell::new(initial_color),
            appearance: Rc::new(Cell::new(Appearance::Light)),
            last_appearance: Cell::new(Appearance::Light),
        });
        Self { inner }
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    #[must_use]
    pub fn unified_size(&self) -> UnifiedSize {
        self.inner.size
    }

    #[must_use]
    pub fn color(&self, app: &App) -> Color {
        *app.read(&self.inner.color)
    }

    #[must_use]
    pub fn color_signal(&self) -> Signal<Color> {
        self.inner.color
    }

    #[must_use]
    pub fn draft(&self, app: &App) -> Option<Color> {
        *app.read(&self.inner.draft)
    }

    #[must_use]
    pub fn hovered_swatch(&self, app: &App) -> Option<usize> {
        *app.read(&self.inner.hovered)
    }

    #[must_use]
    pub fn active_tab(&self, app: &App) -> ColorPickerTab {
        *app.read(&self.inner.active_tab)
    }

    #[must_use]
    pub fn labels(&self, app: &App) -> ColorPickerLabels {
        app.read(&self.inner.labels).clone()
    }

    #[must_use]
    pub fn hex_text(&self, app: &App) -> SmolStr {
        SmolStr::new(app.read(&self.inner.hex_state).text())
    }

    #[must_use]
    pub fn is_open(&self, app: &App) -> bool {
        self.inner.overlay.is_open(app)
    }

    #[must_use]
    pub fn open_state(&self) -> Signal<bool> {
        self.inner.overlay.open_state()
    }

    pub fn open(&self, app: &mut App, window: WindowId) -> bool {
        if self.inner.overlay.is_open(app) {
            return false;
        }
        self.open_panel(app, window);
        self.inner.overlay.is_open(app)
    }

    pub fn close(&self, app: &mut App) -> bool {
        self.inner.overlay.close(app)
    }

    #[must_use]
    pub fn trigger_node(&self) -> Option<NodeId> {
        self.inner.trigger.borrow().map(|view| view.node)
    }

    #[must_use]
    pub fn overlay(&self) -> &Overlay {
        &self.inner.overlay
    }

    pub fn on_change(&self, callback: impl Fn(&mut App, Color) + 'static) {
        *self.inner.change.borrow_mut() = Some(Rc::new(callback));
    }

    pub fn on_open(&self, callback: impl Fn(&mut App) + 'static) {
        *self.inner.open_callback.borrow_mut() = Some(Rc::new(callback));
    }

    pub fn set_labels(&self, app: &mut App, labels: ColorPickerLabels) -> bool {
        app.write(&self.inner.labels, labels)
    }

    pub fn set_active_tab(&self, app: &mut App, tab: ColorPickerTab) -> bool {
        app.write(&self.inner.active_tab, tab)
    }

    pub fn set_color(&self, app: &mut App, color: Color) -> bool {
        let inner = &self.inner;
        let changed = *app.read(&inner.color) != color
            || app.read(&inner.draft).is_some()
            || app.read(&inner.hovered).is_some();
        app.write(&inner.color, color);
        app.write(&inner.draft, None);
        app.write(&inner.hovered, None);
        self.with_input(|input| self.store_hex(app, color, input));
        inner.last_trigger_color.set(None);
        inner.last_color.set(color);

        changed
    }

    pub fn build(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        if let Some(view) = *self.inner.trigger.borrow() {
            return Ok(view.node);
        }
        if app.window(window).is_none() {
            return Err(EngineError::UnknownWindow { window });
        }
        self.register_actions(app);

        let inner = &self.inner;
        let side = inner.size.h();
        let style = StyleBuilder::new()
            .flex_row()
            .items_center()
            .justify_center()
            .size(side, side)
            .padding(px(TRIGGER_PADDING))
            .border(inner.theme.tokens().border_width)
            .relative()
            .build();
        let mut paint = PaintBuilder::new().build();
        paint.corner_radius = [px(TRIGGER_RADIUS); 4];

        let theme = inner.theme.clone();
        let open = inner.overlay.open_state();
        let hovered = inner.trigger_hovered;
        let focused = inner.trigger_focused;
        let appearance = inner.appearance.clone();
        let color = inner.color;
        let initial_hex = hex_string(*app.read(&color));
        let trigger = NodeBuilder::boxed()
            .id(format_smolstr!("{}:trigger", inner.key))
            .style(style)
            .paint(paint)
            .tab_stop(true)
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(initial_hex),
                ..A11yOverride::default()
            })
            .bind_a11y(move |cx| A11yPatch {
                label: Some(hex_string(*cx.read(&color))),
                ..A11yPatch::default()
            })
            .bind_style(move |cx| {
                appearance.set(theme.appearance(cx));
                let is_hovered = *cx.read(&hovered);
                let is_open = *cx.read(&open);
                let is_focused = *cx.read(&focused);
                let tonal = theme.components(cx).interactive.tonal;
                let recipe = recipe_for(&tonal, is_hovered, is_open);
                StylePatch {
                    background: Some(recipe.background),
                    foreground: Some(recipe.foreground),
                    border: Some(Border {
                        width: if is_focused {
                            theme.tokens().border_width * 2.0
                        } else {
                            theme.tokens().border_width
                        },
                        color: if is_focused {
                            theme.role(cx, Role::LineFocus)
                        } else {
                            recipe.border
                        },
                    }),
                    interactive: Some(true),
                    cursor: Some(Cursor::Pointer),
                    ..StylePatch::default()
                }
            })
            .on_click(inner.actions.trigger.clone())
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                parent,
            )?;
        let well_side = side - TRIGGER_PADDING * 2.0 - inner.theme.tokens().border_width * 2.0;
        let well = NodeBuilder::canvas(Canvas::new(LogicalRect::default()))
            .id(format_smolstr!("{}:trigger.well", inner.key))
            .style(StyleBuilder::new().size(well_side, well_side).build())
            .tab_stop(false)
            .a11y(A11yOverride {
                hidden: true,
                ..A11yOverride::default()
            })
            .bind_style(|_cx| StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            })
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                trigger,
            )?;
        *inner.trigger.borrow_mut() = Some(TriggerView {
            node: trigger,
            well,
        });
        Ok(trigger)
    }

    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let inner = &self.inner;
        let mut changed = inner.overlay.sync(app);
        let open = inner.overlay.is_open(app);

        let current = *app.read(&inner.color);
        if current != inner.last_color.get() {
            inner.last_color.set(current);
            app.write(&inner.draft, None);
            app.write(&inner.hovered, None);
            self.with_input(|input| self.store_hex(app, current, input));
            inner.last_trigger_color.set(None);
            changed = true;
        }

        if inner.was_open.get() != open {
            inner.was_open.set(open);
            changed = true;
            if !open {
                self.reset_after_close(app);
            }
        }

        if open && inner.panel.borrow().is_some() {
            let mut panel = inner.panel.take().expect("上面刚检查过面板存在");
            let panel_changed = self.sync_panel(app, &mut panel)?;
            *inner.panel.borrow_mut() = Some(panel);
            changed |= panel_changed;
        }

        changed |= self.sync_trigger(app)?;

        Ok(changed)
    }

    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        let panel = self.inner.panel.borrow();
        let Some(panel) = panel.as_ref() else {
            return false;
        };
        panel.input.dispatch(app, event)
    }

    pub fn release(&self, app: &mut App) -> usize {
        let inner = &self.inner;
        for action in self.action_list() {
            app.off_action(&action);
        }
        inner.overlay.release(app);
        inner.panel.borrow_mut().take();
        *inner.trigger.borrow_mut() = None;
        app.release_scope(inner.scope)
    }

    fn action_list(&self) -> Vec<Action> {
        let actions = &self.inner.actions;
        let mut list = Vec::with_capacity(12);
        list.push(actions.trigger.clone());
        list.push(actions.confirm.clone());
        list.push(actions.hex_submit.clone());
        list.push(actions.palette.clone());
        list.extend(actions.tab.iter().cloned());
        list.extend(actions.track.iter().cloned());
        list.extend(actions.drag.iter().cloned());

        list
    }

    fn register_actions(&self, app: &mut App) {
        let trigger = self.clone();
        app.on_action(self.inner.actions.trigger.clone(), move |ctx| {
            trigger.on_trigger(ctx);
        });
        let confirm = self.clone();
        app.on_action(self.inner.actions.confirm.clone(), move |ctx| {
            confirm.on_confirm(ctx.app_mut());
        });
        let submit = self.clone();
        app.on_action(self.inner.actions.hex_submit.clone(), move |ctx| {
            submit.on_hex_submit(ctx.app_mut());
        });
        let palette = self.clone();
        app.on_action(self.inner.actions.palette.clone(), move |ctx| {
            palette.on_palette(ctx);
        });
        for (index, action) in self.inner.actions.tab.clone().into_iter().enumerate() {
            let picker = self.clone();
            app.on_action(action, move |ctx| {
                picker.clear_preview(ctx.app_mut());
                let tab = tab_of(index);
                picker.set_active_tab(ctx.app_mut(), tab);
            });
        }
        for (index, action) in self.inner.actions.track.clone().into_iter().enumerate() {
            let picker = self.clone();
            app.on_action(action, move |ctx| {
                picker.on_track_click(ctx, HslaChannel::ALL[index]);
            });
        }
        for (index, action) in self.inner.actions.drag.clone().into_iter().enumerate() {
            let picker = self.clone();
            app.on_action(action, move |ctx| {
                picker.on_track_drag(ctx, HslaChannel::ALL[index]);
            });
        }
    }

    fn on_confirm(&self, app: &mut App) {
        let inner = &self.inner;
        let draft = *app.read(&inner.draft);
        app.write(&inner.hovered, None);

        if let Some(color) = draft {
            app.write(&inner.color, color);
            app.write(&inner.draft, None);
            self.with_input(|input| self.store_hex(app, color, input));
            inner.last_color.set(color);
            let callback = inner.change.borrow().clone();
            if let Some(callback) = callback {
                callback(app, color);
            }
        } else {
            let display = self.display_color(app);
            self.with_input(|input| self.store_hex(app, display, input));
        }

        inner.overlay.close(app);
    }

    fn on_trigger(&self, ctx: &mut ActionCtx<'_>) {
        let window = ctx.window();
        let app = ctx.app_mut();
        if self.inner.overlay.is_open(app) {
            self.inner.overlay.close(app);
            return;
        }

        self.open_panel(app, window);
    }

    fn on_palette(&self, ctx: &mut ActionCtx<'_>) {
        let Some(hit) = ctx.hit() else {
            return;
        };
        let index = {
            let panel = self.inner.panel.borrow();
            let Some(panel) = panel.as_ref() else {
                return;
            };
            let BodyView::Palette(view) = &panel.body_view else {
                return;
            };
            match view.cells.iter().position(|cell| *cell == hit.node) {
                Some(index) => Some(index),
                None => ctx
                    .app_mut()
                    .window(panel.window)
                    .and_then(|target| target.tree().get(view.root).ok())
                    .map(|node| node.layout().bounds.size)
                    .and_then(|size| palette_index_at(size, hit.local_point)),
            }
        };
        if let Some(index) = index {
            self.with_input(|input| {
                let Some(swatch) = PALETTE.get(index) else {
                    return;
                };
                let color = hsla_to_color(*swatch);
                self.set_user_color(ctx.app_mut(), color, true, input);
            });
        }
    }

    fn on_track_click(&self, ctx: &mut ActionCtx<'_>, channel: HslaChannel) {
        let Some(hit) = ctx.hit() else {
            return;
        };
        let width = {
            let panel = self.inner.panel.borrow();
            let Some(panel) = panel.as_ref() else {
                return;
            };
            let BodyView::Sliders(view) = &panel.body_view else {
                return;
            };
            let track = view.tracks[channel.index()].track;
            ctx.app_mut()
                .window(panel.window)
                .and_then(|target| target.tree().get(track).ok())
                .map(|node| node.layout().bounds.size.width)
                .unwrap_or(px(0.0))
        };
        if width.value() <= 0.0 {
            return;
        }
        self.set_channel(ctx.app_mut(), channel, hit.local_point.x / width);
    }

    fn on_track_drag(&self, ctx: &mut ActionCtx<'_>, channel: HslaChannel) {
        let ActionSource::Drag { phase, .. } = ctx.source() else {
            return;
        };
        let window = ctx.window();
        let (track, width) = {
            let panel = self.inner.panel.borrow();
            let Some(panel) = panel.as_ref() else {
                return;
            };
            let BodyView::Sliders(view) = &panel.body_view else {
                return;
            };
            let track = view.tracks[channel.index()].track;
            let width = ctx
                .app_mut()
                .window(panel.window)
                .and_then(|target| target.tree().get(track).ok())
                .map(|node| node.layout().bounds.size.width)
                .unwrap_or(px(0.0));
            (track, width)
        };

        match phase {
            DragPhase::Start => {
                let _ = ctx.app_mut().capture_pointer(window, track);
                self.drag_to(ctx, channel, track, f32::from(width));
            }
            DragPhase::Move => self.drag_to(ctx, channel, track, f32::from(width)),
            DragPhase::End | DragPhase::Cancel => {
                let _ = ctx.app_mut().release_pointer(window);
            }
        }
    }

    fn drag_to(&self, ctx: &mut ActionCtx<'_>, channel: HslaChannel, track: NodeId, width: f32) {
        let window = ctx.window();
        if width <= 0.0 || track.window() != Some(window) {
            return;
        }
        let app = ctx.app_mut();
        let Some(state) = app.drag_state(window) else {
            return;
        };
        let Some(info) = app.drag(&state) else {
            return;
        };
        let Some(origin) = app
            .window(window)
            .and_then(|target| target.tree().get(track).ok())
            .map(|node| node.layout().bounds.origin.x)
        else {
            return;
        };
        let t = (info.current.x - origin) / width;
        self.set_channel(app, channel, f32::from(t));
    }

    fn set_preview_swatch(&self, app: &mut App, index: Option<usize>, input: Option<&InputNode>) {
        let inner = &self.inner;
        let index = index.filter(|index| *index < PALETTE.len());
        if *app.read(&inner.hovered) == index {
            return;
        }
        app.write(&inner.hovered, index);
        let preview = index
            .map(|index| hsla_to_color(PALETTE[index]))
            .unwrap_or_else(|| self.display_color(app));
        self.store_hex(app, preview, input);
    }

    fn clear_preview(&self, app: &mut App) {
        if app.read(&self.inner.hovered).is_none() {
            return;
        }
        self.with_input(|input| self.set_preview_swatch(app, None, input));
    }

    fn set_channel(&self, app: &mut App, channel: HslaChannel, t: f32) {
        let base = color_to_hsla(self.display_color(app));
        let mut color = base;
        match channel {
            HslaChannel::Hue => color.h = t.clamp(0.0, 1.0),
            HslaChannel::Saturation => color.s = t.clamp(0.0, 1.0),
            HslaChannel::Lightness => color.l = t.clamp(0.0, 1.0),
            HslaChannel::Alpha => color.a = t.clamp(0.0, 1.0),
        }
        let candidate = hsla_to_color(color);
        self.with_input(|input| self.set_user_color(app, candidate, true, input));
    }

    fn set_user_color(
        &self,
        app: &mut App,
        color: Color,
        sync_hex: bool,
        input: Option<&InputNode>,
    ) -> bool {
        let inner = &self.inner;
        let display = self.display_color(app);
        if rgba8(color) == rgba8(display) {
            return false;
        }

        let confirmed = *app.read(&inner.color);
        app.write(&inner.hovered, None);
        app.write(
            &inner.draft,
            (rgba8(color) != rgba8(confirmed)).then_some(color),
        );

        if sync_hex {
            self.store_hex(app, color, input);
        }

        true
    }

    fn commit_hex_text(&self, app: &mut App, input: Option<&InputNode>) {
        let text = SmolStr::new(app.read(&self.inner.hex_state).text());
        if let Some(color) = parse_hex(text.as_str()) {
            self.set_user_color(app, color, false, input);
        }
        let display = self.display_color(app);
        self.store_hex(app, display, input);
    }

    fn on_hex_submit(&self, app: &mut App) {
        self.with_input(|input| self.commit_hex_text(app, input));
    }

    fn display_color(&self, app: &App) -> Color {
        (*app.read(&self.inner.draft)).unwrap_or_else(|| *app.read(&self.inner.color))
    }

    fn store_hex(&self, app: &mut App, color: Color, input: Option<&InputNode>) -> bool {
        let hex = hex_string(color);
        let changed = match input {
            Some(input) => input.update_value(app, hex.as_str()),
            None => {
                let focused = app.read(&self.inner.hex_state).is_focused();
                let mut state = InputState::new(hex.as_str());
                state.set_focused(focused);
                app.write(&self.inner.hex_state, state)
            }
        };
        *self.inner.last_hex_text.borrow_mut() = hex;

        changed
    }

    fn with_input<T>(&self, body: impl FnOnce(Option<&InputNode>) -> T) -> T {
        let panel = self.inner.panel.borrow();
        body(panel.as_ref().map(|panel| &panel.input))
    }

    fn open_panel(&self, app: &mut App, window: WindowId) {
        let inner = &self.inner;
        if inner.overlay.is_open(app) {
            return;
        }
        let Some(trigger) = inner.trigger.borrow().map(|view| view.node) else {
            return;
        };
        let panel_label = app.read(&inner.labels).panel.clone();
        inner.overlay.set_a11y(OverlayA11y::dialog(panel_label));
        let opened = inner
            .overlay
            .open(app, window, Anchor::node(trigger), |_, _| Ok(()))
            .unwrap_or(false);
        if !opened {
            return;
        }
        let Some(panel) = inner.overlay.panel_node() else {
            return;
        };
        let built = self.build_panel(app, window, panel).is_ok();
        if !built {
            inner.overlay.close_now(app);
            return;
        }
        let callback = inner.open_callback.borrow().clone();
        if let Some(callback) = callback {
            callback(app);
        }
    }

    fn build_panel(
        &self,
        app: &mut App,
        window: WindowId,
        panel_node: NodeId,
    ) -> Result<(), EngineError> {
        let inner = &self.inner;
        let labels = app.read(&inner.labels).clone();
        let active = *app.read(&inner.active_tab);
        let appearance = inner.appearance.get();
        let labels_signal = inner.labels;
        let active_signal = inner.active_tab;
        let tab_hover = inner.tab_hover;
        let confirm_hovered = inner.confirm_hovered;
        let theme_handle = inner.theme.clone();
        let border = inner.theme.tokens().border_width;
        let small = UnifiedSize::Small;
        let medium = UnifiedSize::Medium;
        let hex_state = inner.hex_state;
        let hex_placeholder = inner.hex_placeholder;
        let hex_submit = inner.actions.hex_submit.clone();

        let tree = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut();
        let content = NodeBuilder::boxed()
            .id(format_smolstr!("{}:panel.content", inner.key))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .gap(px(8.0))
                    .width(px(PANEL_CONTENT_WIDTH))
                    .build(),
            )
            .build(tree, panel_node)?;

        let tabs = NodeBuilder::boxed()
            .id(format_smolstr!("{}:panel.tabs", inner.key))
            .style(StyleBuilder::new().flex_row().width_full().build())
            .a11y(A11yOverride {
                role: Some(A11yRole::TabList),
                ..A11yOverride::default()
            })
            .build(tree, content)?;
        let mut tab_nodes = [tabs; 2];
        let label_fns: [fn(&ColorPickerLabels) -> SmolStr; 2] = [
            |labels| labels.palette.clone(),
            |labels| labels.hsla.clone(),
        ];
        for (index, label_fn) in label_fns.into_iter().enumerate() {
            let owner = theme_handle.clone();
            let owner_for_text = theme_handle.clone();
            let hover = tab_hover;
            let selected = active_signal;
            let selected_for_text = active_signal;
            let labels_for_text = labels_signal;
            let mut paint = PaintBuilder::new().build();
            paint.corner_radius = if index == 0 {
                [px(CONTROL_RADIUS), px(0.0), px(0.0), px(CONTROL_RADIUS)]
            } else {
                [px(0.0), px(CONTROL_RADIUS), px(CONTROL_RADIUS), px(0.0)]
            };
            let tab = NodeBuilder::boxed()
                .id(format_smolstr!("{}:panel.tab.{}", inner.key, index))
                .style(
                    StyleBuilder::new()
                        .flex_1()
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .height(small.h())
                        .border(border)
                        .build(),
                )
                .paint(paint)
                .tab_stop(true)
                .a11y(A11yOverride {
                    role: Some(A11yRole::Tab),
                    label: Some(label_fn(&labels)),
                    ..A11yOverride::default()
                })
                .bind_a11y(move |cx| A11yPatch {
                    label: Some(label_fn(&cx.read(&labels_signal))),
                    selected: Some(*cx.read(&selected) == tab_of(index)),
                    pos_in_set: Some(index),
                    size_of_set: Some(2),
                    ..A11yPatch::default()
                })
                .bind_style(move |cx| {
                    let tonal = owner.components(cx).interactive.tonal;
                    let is_selected = *cx.read(&selected) == tab_of(index);
                    let is_hovered = *cx.read(&hover) == Some(index);
                    let recipe = recipe_for(&tonal, is_hovered, is_selected);
                    StylePatch {
                        background: Some(recipe.background),
                        foreground: Some(recipe.foreground),
                        border: Some(Border {
                            width: owner.tokens().border_width,
                            color: recipe.border,
                        }),
                        cursor: Some(Cursor::Pointer),
                        ..StylePatch::default()
                    }
                })
                .on_click(inner.actions.tab[index].clone())
                .build(tree, tabs)?;
            NodeBuilder::text(label_fn(&labels), TextStyle::new(small.text_size()))
                .id(format_smolstr!("{}:panel.tab.{index}.text", inner.key))
                .style(StyleBuilder::new().flex_1().build())
                .bind_text(move |cx| label_fn(&cx.read(&labels_for_text)))
                .bind_style(move |cx| {
                    let tonal = owner_for_text.components(cx).interactive.tonal;
                    let is_selected = *cx.read(&selected_for_text) == tab_of(index);
                    let is_hovered = *cx.read(&hover) == Some(index);
                    StylePatch {
                        foreground: Some(recipe_for(&tonal, is_hovered, is_selected).foreground),
                        ..StylePatch::default()
                    }
                })
                .build(tree, tab)?;
            tab_nodes[index] = tab;
        }

        let body = NodeBuilder::boxed()
            .id(format_smolstr!("{}:panel.body", inner.key))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .width_full()
                    .gap(px(8.0))
                    .build(),
            )
            .build(tree, content)?;

        let owner = theme_handle.clone();
        let _separator = NodeBuilder::boxed()
            .id(format_smolstr!("{}:panel.separator", inner.key))
            .style(StyleBuilder::new().width_full().height(px(1.0)).build())
            .bind_style(move |cx| StylePatch {
                background: Some(owner.role(cx, Role::LineDivider)),
                interactive: Some(false),
                ..StylePatch::default()
            })
            .a11y(A11yOverride {
                role: Some(A11yRole::Separator),
                hidden: true,
                ..A11yOverride::default()
            })
            .build(tree, content)?;

        let footer = NodeBuilder::boxed()
            .id(format_smolstr!("{}:panel.footer", inner.key))
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .width_full()
                    .build(),
            )
            .build(tree, content)?;

        let well = NodeBuilder::canvas(Canvas::new(LogicalRect::default()))
            .id(format_smolstr!("{}:panel.well", inner.key))
            .style(
                StyleBuilder::new()
                    .size(px(FOOTER_WELL_SIZE), px(FOOTER_WELL_SIZE))
                    .build(),
            )
            .tab_stop(false)
            .a11y(A11yOverride {
                hidden: true,
                ..A11yOverride::default()
            })
            .bind_style(|_cx| StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            })
            .build(tree, footer)?;

        let input_slot = NodeBuilder::boxed()
            .id(format_smolstr!("{}:panel.hex", inner.key))
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .flex_row()
                    .items_center()
                    .build(),
            )
            .build(tree, footer)?;
        let input = kit_input::build(
            tree,
            input_slot,
            Arc::new(InputSpec {
                name: format_smolstr!("{}.hex", inner.key),
                placeholder: SmolStr::new_static("#RRGGBB"),
                masked: false,
                multiline: false,
                readonly: false,
                disabled: false,
                max_length: None,
                size: ControlSize::Medium,
                submit: Some(hex_submit),
            }),
            &inner.theme,
            &hex_state,
            &hex_placeholder,
        );

        let owner = theme_handle.clone();
        let owner_for_text = theme_handle;
        let confirm = NodeBuilder::boxed()
            .id(format_smolstr!("{}:panel.confirm", inner.key))
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .height(medium.h())
                    .padding_x(medium.px())
                    .border(border)
                    .build(),
            )
            .paint({
                let mut paint = PaintBuilder::new().build();
                paint.corner_radius = [px(CONTROL_RADIUS); 4];
                paint
            })
            .tab_stop(true)
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(labels.confirm.clone()),
                ..A11yOverride::default()
            })
            .bind_a11y(move |cx| A11yPatch {
                label: Some(cx.read(&labels_signal).confirm.clone()),
                ..A11yPatch::default()
            })
            .bind_style(move |cx| {
                let filled = owner.components(cx).interactive.filled;
                let recipe = recipe_for(&filled, *cx.read(&confirm_hovered), false);
                StylePatch {
                    background: Some(recipe.background),
                    foreground: Some(recipe.foreground),
                    border: Some(Border {
                        width: owner.tokens().border_width,
                        color: recipe.border,
                    }),
                    cursor: Some(Cursor::Pointer),
                    ..StylePatch::default()
                }
            })
            .on_click(inner.actions.confirm.clone())
            .build(tree, footer)?;
        NodeBuilder::text(labels.confirm, TextStyle::new(medium.text_size()))
            .id(format_smolstr!("{}:panel.confirm.text", inner.key))
            .bind_text(move |cx| cx.read(&labels_signal).confirm.clone())
            .bind_style(move |cx| {
                let filled = owner_for_text.components(cx).interactive.filled;
                StylePatch {
                    foreground: Some(
                        recipe_for(&filled, *cx.read(&confirm_hovered), false).foreground,
                    ),
                    ..StylePatch::default()
                }
            })
            .build(tree, confirm)?;

        input.install(app);

        let body_view = self.build_body(app, window, body, active)?;
        let panel = Panel {
            window,
            body,
            body_view,
            tab_active: active,
            appearance,
            tabs: tab_nodes,
            confirm,
            well,
            input,
            well_last: None,
        };
        *inner.panel.borrow_mut() = Some(panel);

        let color = self.display_color(app);
        let hex = hex_string(color);
        {
            let panel = inner.panel.borrow();
            if let Some(panel) = panel.as_ref() {
                panel.input.update_value(app, hex.as_str());
            }
        }
        *inner.last_hex_text.borrow_mut() = hex;
        inner.last_hex_focus.set(false);
        inner.was_open.set(true);

        Ok(())
    }

    fn build_body(
        &self,
        app: &mut App,
        window: WindowId,
        body: NodeId,
        tab: ColorPickerTab,
    ) -> Result<BodyView, EngineError> {
        match tab {
            ColorPickerTab::Palette => self.build_palette(app, window, body).map(BodyView::Palette),
            ColorPickerTab::Hsla => self.build_sliders(app, window, body).map(BodyView::Sliders),
        }
    }

    fn build_palette(
        &self,
        app: &mut App,
        window: WindowId,
        body: NodeId,
    ) -> Result<PaletteView, EngineError> {
        let inner = &self.inner;
        let labels = inner.labels;
        let initial_label = self.labels_text(app).palette;
        let tree = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut();
        let root = NodeBuilder::boxed()
            .id(format_smolstr!("{}:palette", inner.key))
            .style(
                StyleBuilder::new()
                    .relative()
                    .size(px(PALETTE_SIZE), px(PALETTE_SIZE))
                    .build(),
            )
            .tab_stop(false)
            .a11y(A11yOverride {
                role: Some(A11yRole::Group),
                label: Some(initial_label),
                ..A11yOverride::default()
            })
            .bind_a11y(move |cx| A11yPatch {
                label: Some(cx.read(&labels).palette.clone()),
                ..A11yPatch::default()
            })
            .bind_style(|_cx| StylePatch {
                cursor: Some(Cursor::Pointer),
                ..StylePatch::default()
            })
            .on_click(inner.actions.palette.clone())
            .build(tree, body)?;
        let canvas = NodeBuilder::canvas(Canvas::new(LogicalRect::default()))
            .id(format_smolstr!("{}:palette.canvas", inner.key))
            .style(StyleBuilder::new().absolute().size_full().build())
            .tab_stop(false)
            .a11y(A11yOverride {
                hidden: true,
                ..A11yOverride::default()
            })
            .bind_style(|_cx| StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            })
            .build(tree, root)?;
        let mut cells = Vec::with_capacity(PALETTE.len());
        for index in 0..PALETTE.len() {
            let row = index / PALETTE_COLS;
            let col = index % PALETTE_COLS;
            let cell = NodeBuilder::boxed()
                .id(format_smolstr!("{}:palette.cell.{index}", inner.key))
                .style(
                    StyleBuilder::new()
                        .absolute()
                        .left(px(PALETTE_STEP * col as f32))
                        .top(px(PALETTE_STEP * row as f32))
                        .size(px(PALETTE_CELL), px(PALETTE_CELL))
                        .build(),
                )
                .tab_stop(false)
                .a11y(A11yOverride {
                    hidden: true,
                    ..A11yOverride::default()
                })
                .build(tree, root)?;
            cells.push(cell);
        }

        Ok(PaletteView {
            root,
            canvas,
            cells,
            last: None,
        })
    }

    fn build_sliders(
        &self,
        app: &mut App,
        window: WindowId,
        body: NodeId,
    ) -> Result<SliderView, EngineError> {
        let inner = &self.inner;
        let appearance = inner.appearance.get();
        let theme = inner.theme.theme_for(appearance);
        let thumb_fill = theme.role(Role::SurfaceOverlay);
        let theme_handle = inner.theme.clone();
        let labels_signal = inner.labels;
        let color_signal = inner.color;
        let draft_signal = inner.draft;
        let labels = self.labels_text(app);
        let display = color_to_hsla(self.display_color(app));

        let tree = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut();
        let root = NodeBuilder::boxed()
            .id(format_smolstr!("{}:sliders", inner.key))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .gap(px(8.0))
                    .width_full()
                    .build(),
            )
            .build(tree, body)?;
        let mut tracks: Vec<TrackView> = Vec::with_capacity(4);
        for channel in HslaChannel::ALL {
            let index = channel.index();
            let owner = theme_handle.clone();
            let owner_for_value = theme_handle.clone();
            let labels_for_row = labels_signal;
            let labels_for_a11y = labels_signal;
            let color_for_value = color_signal;
            let draft_for_value = draft_signal;
            let color_for_a11y = color_signal;
            let draft_for_a11y = draft_signal;
            let row = NodeBuilder::boxed()
                .id(format_smolstr!("{}:slider.{}.row", inner.key, channel.id()))
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .gap(px(SLIDER_ROW_INNER_GAP))
                        .height(px(SLIDER_ROW_HEIGHT))
                        .width_full()
                        .build(),
                )
                .build(tree, root)?;
            NodeBuilder::text(channel.label(&labels), TextStyle::new(px(12.0)))
                .id(format_smolstr!(
                    "{}:slider.{}.label",
                    inner.key,
                    channel.id()
                ))
                .style(StyleBuilder::new().width(px(SLIDER_LABEL_WIDTH)).build())
                .bind_text(move |cx| channel.label(&cx.read(&labels_for_row)))
                .bind_style(move |cx| StylePatch {
                    foreground: Some(owner.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .build(tree, row)?;
            let track = NodeBuilder::boxed()
                .id(format_smolstr!(
                    "{}:slider.{}.track",
                    inner.key,
                    channel.id()
                ))
                .style(
                    StyleBuilder::new()
                        .relative()
                        .flex_1()
                        .height(px(TRACK_HEIGHT))
                        .build(),
                )
                .tab_stop(false)
                .a11y(A11yOverride {
                    role: Some(A11yRole::Slider),
                    label: Some(channel.label(&labels)),
                    numeric_value: Some(f64::from(channel.display_value(display))),
                    min_numeric_value: Some(0.0),
                    max_numeric_value: Some(f64::from(channel.max())),
                    ..A11yOverride::default()
                })
                .bind_a11y(move |cx| {
                    let draft = *cx.read(&draft_for_a11y);
                    let confirmed = *cx.read(&color_for_a11y);
                    let hsla_value = color_to_hsla(draft.unwrap_or(confirmed));
                    A11yPatch {
                        label: Some(channel.label(&cx.read(&labels_for_a11y))),
                        numeric_value: Some(f64::from(channel.display_value(hsla_value))),
                        min_numeric_value: Some(0.0),
                        max_numeric_value: Some(f64::from(channel.max())),
                        ..A11yPatch::default()
                    }
                })
                .bind_style(|_cx| StylePatch {
                    cursor: Some(Cursor::Pointer),
                    ..StylePatch::default()
                })
                .on_click(inner.actions.track[index].clone())
                .on_drag(inner.actions.drag[index].clone())
                .build(tree, row)?;
            let canvas = NodeBuilder::canvas(Canvas::new(LogicalRect::default()))
                .id(format_smolstr!(
                    "{}:slider.{}.canvas",
                    inner.key,
                    channel.id()
                ))
                .style(StyleBuilder::new().absolute().size_full().build())
                .tab_stop(false)
                .a11y(A11yOverride {
                    hidden: true,
                    ..A11yOverride::default()
                })
                .bind_style(|_cx| StylePatch {
                    interactive: Some(false),
                    ..StylePatch::default()
                })
                .build(tree, track)?;
            NodeBuilder::text(
                channel.value_text(display),
                TextStyle::new(px(12.0)).with_alignment(TextAlign::End),
            )
            .id(format_smolstr!(
                "{}:slider.{}.value",
                inner.key,
                channel.id()
            ))
            .style(StyleBuilder::new().width(px(SLIDER_VALUE_WIDTH)).build())
            .a11y(A11yOverride {
                hidden: true,
                ..A11yOverride::default()
            })
            .bind_text(move |cx| {
                let draft = *cx.read(&draft_for_value);
                let confirmed = *cx.read(&color_for_value);
                channel.value_text(color_to_hsla(draft.unwrap_or(confirmed)))
            })
            .bind_style(move |cx| StylePatch {
                foreground: Some(owner_for_value.role(cx, Role::FgMuted)),
                ..StylePatch::default()
            })
            .build(tree, row)?;
            tracks.push(TrackView {
                track,
                canvas,
                last: None,
            });
        }

        let array: [TrackView; 4] = tracks.try_into().expect("四条 HSL 通道各建一条滑条");

        Ok(SliderView {
            root,
            tracks: array,
            thumb_fill,
            thumb_border: Color::srgb(0.0, 0.0, 0.0, 0.35),
        })
    }

    fn rebuild_body(&self, app: &mut App, panel: &mut Panel) {
        let active = *app.read(&self.inner.active_tab);
        if panel.tab_active == active {
            return;
        }
        let old_root = match &panel.body_view {
            BodyView::Palette(view) => view.root,
            BodyView::Sliders(view) => view.root,
        };
        if let Some(window) = app.window_mut(panel.window) {
            let _ = window.tree_mut().remove(old_root);
        }
        if let Ok(view) = self.build_body(app, panel.window, panel.body, active) {
            panel.body_view = view;
            panel.tab_active = active;
        }
    }

    fn sync_trigger(&self, app: &mut App) -> Result<bool, EngineError> {
        let inner = &self.inner;
        let Some(view) = *inner.trigger.borrow() else {
            return Ok(false);
        };
        let Some(window) = view.node.window() else {
            return Ok(false);
        };
        let mut changed = false;
        let hovered = node_in_subtree(app, window, view.node, app.hovered(window));
        changed |= app.write(&inner.trigger_hovered, hovered);
        let focused = node_in_subtree(app, window, view.node, app.focused(window));
        changed |= app.write(&inner.trigger_focused, focused);

        let appearance = inner.appearance.get();
        if appearance != inner.last_appearance.get() {
            inner.last_appearance.set(appearance);
            inner.last_trigger_color.set(None);
        }

        let color = *app.read(&inner.color);
        let Some(bounds) = node_bounds(app, window, view.well) else {
            return Ok(changed);
        };
        if inner.last_trigger_color.get() == Some(color)
            && inner.last_trigger_bounds.get() == Some(bounds)
        {
            return Ok(changed);
        }
        let theme = inner.theme.theme_for(appearance);
        let canvas = well_canvas(
            bounds,
            color,
            theme.role(Role::SurfaceOverlay),
            theme.role(Role::SurfaceSunken),
            TRIGGER_WELL_RADIUS.min(bounds.size.width.min(bounds.size.height) / 2.0),
        );
        changed |= apply_canvas(app, window, view.well, canvas)?;
        inner.last_trigger_color.set(Some(color));
        inner.last_trigger_bounds.set(Some(bounds));

        Ok(changed)
    }

    fn sync_panel(&self, app: &mut App, panel: &mut Panel) -> Result<bool, EngineError> {
        let inner = &self.inner;
        let window = panel.window;
        let mut changed = panel.input.sync(app);

        changed |= self.sync_hex(app, panel);
        changed |= self.rebuild_body_if_needed(app, panel);

        let appearance = inner.appearance.get();
        if appearance != panel.appearance {
            panel.appearance = appearance;
            panel.well_last = None;
            let theme = inner.theme.theme_for(appearance);
            let thumb_fill = theme.role(Role::SurfaceOverlay);
            if let BodyView::Sliders(view) = &mut panel.body_view {
                view.thumb_fill = thumb_fill;
                for track in &mut view.tracks {
                    track.last = None;
                }
            }
        }

        let hovered = app.hovered(window);
        let tab_hover = if node_in_subtree(app, window, panel.tabs[0], hovered) {
            Some(0)
        } else if node_in_subtree(app, window, panel.tabs[1], hovered) {
            Some(1)
        } else {
            None
        };
        changed |= app.write(&inner.tab_hover, tab_hover);
        changed |= app.write(
            &inner.confirm_hovered,
            node_in_subtree(app, window, panel.confirm, hovered),
        );

        changed |= self.sync_palette_hover(app, panel, hovered);
        changed |= self.sync_body_canvases(app, panel)?;
        changed |= self.sync_footer_well(app, panel)?;

        Ok(changed)
    }

    fn sync_hex(&self, app: &mut App, panel: &mut Panel) -> bool {
        let state = app.read(&self.inner.hex_state).clone();
        let text = SmolStr::new(state.text());
        let focused = state.is_focused();
        let mut changed = false;

        if text != *self.inner.last_hex_text.borrow() {
            *self.inner.last_hex_text.borrow_mut() = text.clone();
            if let Some(color) = parse_hex(text.as_str()) {
                changed |= self.set_user_color(app, color, false, Some(&panel.input));
            }
        }
        if self.inner.last_hex_focus.get() && !focused {
            let display = self.display_color(app);
            if let Some(color) = parse_hex(text.as_str()) {
                changed |= self.set_user_color(app, color, false, Some(&panel.input));
            }
            changed |= self.store_hex(app, display, Some(&panel.input));
        }
        self.inner.last_hex_focus.set(focused);

        changed
    }

    fn rebuild_body_if_needed(&self, app: &mut App, panel: &mut Panel) -> bool {
        let active = *app.read(&self.inner.active_tab);
        if panel.tab_active == active {
            return false;
        }
        self.rebuild_body(app, panel);

        true
    }

    fn sync_palette_hover(
        &self,
        app: &mut App,
        panel: &mut Panel,
        hovered: Option<NodeId>,
    ) -> bool {
        let BodyView::Palette(view) = &panel.body_view else {
            return false;
        };
        let current = *app.read(&self.inner.hovered);
        let next = match hovered {
            Some(node) => {
                if let Some(index) = view.cells.iter().position(|cell| *cell == node) {
                    Some(index)
                } else if node_in_subtree(app, panel.window, view.root, hovered) {
                    current
                } else {
                    None
                }
            }
            None => None,
        };
        if next == current {
            return false;
        }
        self.set_preview_swatch(app, next, Some(&panel.input));

        true
    }

    fn sync_body_canvases(&self, app: &mut App, panel: &mut Panel) -> Result<bool, EngineError> {
        let window = panel.window;
        let mut changed = false;
        match &mut panel.body_view {
            BodyView::Palette(view) => {
                let hovered = *app.read(&self.inner.hovered);
                let Some(bounds) = node_bounds(app, window, view.canvas) else {
                    return Ok(changed);
                };
                if view.last == Some((bounds, hovered)) {
                    return Ok(changed);
                }
                let canvas = palette_canvas(bounds, hovered);
                changed |= apply_canvas(app, window, view.canvas, canvas)?;
                view.last = Some((bounds, hovered));
            }
            BodyView::Sliders(view) => {
                let base = color_to_hsla(self.display_color(app));
                for channel in HslaChannel::ALL {
                    let track = &mut view.tracks[channel.index()];
                    let value = channel.value(base);
                    let Some(bounds) = node_bounds(app, window, track.canvas) else {
                        continue;
                    };
                    if track.last == Some((bounds, base, value)) {
                        continue;
                    }
                    let canvas = track_canvas(
                        bounds,
                        channel,
                        base,
                        value,
                        view.thumb_fill,
                        view.thumb_border,
                    );
                    changed |= apply_canvas(app, window, track.canvas, canvas)?;
                    track.last = Some((bounds, base, value));
                }
            }
        }

        Ok(changed)
    }

    fn sync_footer_well(&self, app: &mut App, panel: &mut Panel) -> Result<bool, EngineError> {
        let preview = app
            .read(&self.inner.hovered)
            .map(|index| hsla_to_color(PALETTE[index]));
        let color = preview.unwrap_or_else(|| self.display_color(app));
        let window = panel.window;
        let appearance = panel.appearance;
        let Some(bounds) = node_bounds(app, window, panel.well) else {
            return Ok(false);
        };
        if panel.well_last == Some((bounds, color)) {
            return Ok(false);
        }
        let theme = self.inner.theme.theme_for(appearance);
        let canvas = well_canvas(
            bounds,
            color,
            theme.role(Role::SurfaceOverlay),
            theme.role(Role::SurfaceSunken),
            FOOTER_WELL_RADIUS.min(bounds.size.width.min(bounds.size.height) / 2.0),
        );
        let changed = apply_canvas(app, window, panel.well, canvas)?;
        panel.well_last = Some((bounds, color));

        Ok(changed)
    }

    fn reset_after_close(&self, app: &mut App) {
        let inner = &self.inner;
        inner.panel.borrow_mut().take();
        let hovered = *app.read(&inner.hovered);
        let draft = *app.read(&inner.draft);
        app.write(&inner.hovered, None);
        app.write(&inner.draft, None);
        if hovered.is_some() || draft.is_some() {
            let color = *app.read(&inner.color);
            self.store_hex(app, color, None);
        }
        inner.last_hex_focus.set(false);
        inner.last_trigger_color.set(None);
    }

    fn labels_text(&self, app: &App) -> ColorPickerLabels {
        app.read(&self.inner.labels).clone()
    }
}

const fn tab_of(index: usize) -> ColorPickerTab {
    if index == 0 {
        ColorPickerTab::Palette
    } else {
        ColorPickerTab::Hsla
    }
}

fn recipe_for(
    colors: &InteractiveStateColors,
    hovered: bool,
    selected: bool,
) -> InteractiveElementColors {
    if selected {
        colors.active
    } else if hovered {
        colors.hover
    } else {
        colors.normal
    }
}

fn node_bounds(app: &App, window: WindowId, node: NodeId) -> Option<LogicalRect> {
    app.window(window)
        .and_then(|target| target.tree().get(node).ok())
        .map(|node| node.layout().bounds)
}

fn apply_canvas(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    canvas: Canvas,
) -> Result<bool, EngineError> {
    let unchanged = app
        .window(window)
        .is_some_and(|target| match target.tree().get(node) {
            Ok(node) => matches!(
                node.kind(),
                NodeKind::Canvas(existing) if existing.as_ref() == &canvas
            ),
            Err(_) => false,
        });
    if unchanged {
        return Ok(false);
    }

    app.window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .set_canvas(node, canvas)
}

fn node_in_subtree(app: &App, window: WindowId, root: NodeId, candidate: Option<NodeId>) -> bool {
    let Some(candidate) = candidate else {
        return false;
    };
    let Some(target) = app.window(window) else {
        return false;
    };
    let tree = target.tree();
    let mut cursor = Some(candidate);
    while let Some(node) = cursor {
        if node == root {
            return true;
        }
        cursor = tree.get(node).ok().and_then(|target| target.parent());
    }

    false
}

#[cfg(test)]
mod tests {
    use ui_core::{
        math::{LogicalPoint, LogicalRect, LogicalSize, px},
        paint::{
            canvas::Canvas,
            color::ColorResolver,
            primitive::{Primitive, Quad},
        },
    };

    use super::{
        Color, ColorPickerLabels, HslaChannel, PALETTE, PALETTE_COLS, PALETTE_LIGHTNESS,
        PALETTE_ROWS, PALETTE_SATURATION, PALETTE_SIZE, PALETTE_STEP, PANEL_CONTENT_WIDTH,
        SLIDER_LABEL_WIDTH, SLIDER_ROW_HEIGHT, SLIDER_ROW_INNER_GAP, SLIDER_VALUE_WIDTH,
        THUMB_DIAMETER, TRACK_HEIGHT, TRACK_RADIUS, TRACK_SLICES, color_to_hsla, hex_string,
        hover_ring_color, hsla, hsla_to_color, linear_to_srgb, palette_canvas, palette_cell_bounds,
        palette_hue, palette_index_at, parse_hex, rgba8, srgb_components, track_canvas,
        well_canvas,
    };

    const EPS: f32 = 1.0e-4;

    fn canvas_quads(canvas: &Canvas) -> Vec<&Quad> {
        canvas
            .primitives()
            .iter()
            .filter_map(|primitive| match primitive {
                Primitive::Quad(quad) => Some(quad),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_hover_ring_should_contrast_with_the_swatch() {
        assert_eq!(
            hover_ring_color(Color::WHITE),
            Color::srgb(0.0, 0.0, 0.0, 0.85),
            "亮块得到深色环"
        );
        assert_eq!(
            hover_ring_color(Color::BLACK),
            Color::srgb(1.0, 1.0, 1.0, 0.95),
            "暗块得到亮色环"
        );
        assert_eq!(
            hover_ring_color(hsla_to_color(PALETTE[PALETTE_COLS - 1])),
            Color::srgb(0.0, 0.0, 0.0, 0.85),
            "明度阶梯亮端得到深色环"
        );
        assert_eq!(
            hover_ring_color(hsla_to_color(PALETTE[0])),
            Color::srgb(1.0, 1.0, 1.0, 0.95),
            "明度阶梯暗端得到亮色环"
        );
    }

    #[test]
    fn the_palette_should_be_twelve_lightness_steps_by_twelve_rows() {
        assert_eq!(PALETTE.len(), 144);
        assert_eq!(PALETTE.len(), PALETTE_ROWS * PALETTE_COLS);
        assert_eq!(PALETTE_SIZE, 284.0, "十二个 20px 格 + 十一个 4px 间隙");
        assert_eq!(
            PANEL_CONTENT_WIDTH, PALETTE_SIZE,
            "色板恰好填满面板内容宽度"
        );
        assert!(
            PALETTE_LIGHTNESS.windows(2).all(|pair| pair[0] < pair[1]),
            "明度列从暗到亮"
        );

        for col in 0..PALETTE_COLS {
            let color = PALETTE[col];
            assert_eq!(color.s, 0.0, "第 0 行是中性色阶梯");
            assert_eq!(color.l, PALETTE_LIGHTNESS[col]);
            assert_eq!(color.a, 1.0);
        }

        for row in 1..PALETTE_ROWS {
            let hue = palette_hue(row).expect("第 0 行之后每行都有色相");
            assert!((hue - (row - 1) as f32 / (PALETTE_ROWS - 1) as f32).abs() < 1.0e-6);

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

    fn cell_center(row: usize, col: usize) -> LogicalPoint {
        LogicalPoint::new(
            px(PALETTE_STEP * col as f32 + 10.0),
            px(PALETTE_STEP * row as f32 + 10.0),
        )
    }

    #[test]
    fn palette_index_at_should_map_a_cell_center_to_its_index() {
        let grid = LogicalSize::new(px(PALETTE_SIZE), px(PALETTE_SIZE));
        assert_eq!(palette_index_at(grid, cell_center(2, 4)), Some(28));
        assert_eq!(palette_index_at(grid, cell_center(0, 0)), Some(0));
    }

    #[test]
    fn palette_index_at_should_hit_the_last_cell_and_reject_its_trailing_edge() {
        let grid = LogicalSize::new(px(PALETTE_SIZE), px(PALETTE_SIZE));
        assert_eq!(
            palette_index_at(grid, cell_center(PALETTE_ROWS - 1, PALETTE_COLS - 1)),
            Some(143)
        );
        assert_eq!(
            palette_index_at(grid, LogicalPoint::new(px(PALETTE_SIZE), px(PALETTE_SIZE))),
            None,
            "右下角在网格的半开边界之外"
        );
    }

    #[test]
    fn palette_index_at_should_reject_the_gap_between_cells() {
        let grid = LogicalSize::new(px(PALETTE_SIZE), px(PALETTE_SIZE));
        assert_eq!(
            palette_index_at(grid, LogicalPoint::new(px(22.0), px(10.0))),
            None
        );
        assert_eq!(
            palette_index_at(grid, LogicalPoint::new(px(10.0), px(22.0))),
            None
        );
    }

    #[test]
    fn palette_index_at_should_reject_positions_outside_the_grid() {
        let grid = LogicalSize::new(px(PALETTE_SIZE), px(PALETTE_SIZE));
        for position in [
            LogicalPoint::new(px(PALETTE_SIZE), px(10.0)),
            LogicalPoint::new(px(10.0), px(PALETTE_SIZE)),
            LogicalPoint::new(px(-1.0), px(10.0)),
            LogicalPoint::new(px(10.0), px(-1.0)),
        ] {
            assert_eq!(palette_index_at(grid, position), None, "{position:?}");
        }
    }

    #[test]
    fn palette_index_at_should_follow_a_scaled_grid() {
        let scaled = PALETTE_SIZE / 2.0;
        let pitch = PALETTE_STEP / 2.0;
        let half_cell = 10.0 / 2.0;
        assert_eq!(
            palette_index_at(
                LogicalSize::new(px(scaled), px(scaled)),
                LogicalPoint::new(px(4.0 * pitch + half_cell), px(2.0 * pitch + half_cell)),
            ),
            Some(28),
            "缩放后的同一色块映射回同一下标"
        );
    }

    #[test]
    fn palette_index_at_should_pin_the_24_pixel_pitch_to_the_right_edge() {
        let grid = LogicalSize::new(px(PALETTE_SIZE), px(PALETTE_SIZE));
        let y = PALETTE_STEP * (PALETTE_ROWS - 1) as f32 + 10.0;
        assert_eq!(y, 274.0);
        assert_eq!(
            palette_index_at(grid, LogicalPoint::new(px(263.999), px(y))),
            None
        );
        assert_eq!(
            palette_index_at(grid, LogicalPoint::new(px(264.0), px(y))),
            Some(143)
        );
        assert_eq!(
            palette_index_at(grid, LogicalPoint::new(px(283.999), px(y))),
            Some(143)
        );
        assert_eq!(
            palette_index_at(grid, LogicalPoint::new(px(284.0), px(y))),
            None
        );
        assert_eq!(
            palette_index_at(grid, LogicalPoint::new(px(239.999), px(y))),
            None
        );
        assert_eq!(
            palette_index_at(grid, LogicalPoint::new(px(240.0), px(y))),
            Some(142)
        );
    }

    #[test]
    fn drawn_cells_should_map_back_to_their_own_index_at_any_scale() {
        for edge in [PALETTE_SIZE, PALETTE_SIZE / 2.0] {
            let bounds = LogicalRect::new(px(100.0), px(50.0), px(edge), px(edge));
            let size = LogicalSize::new(px(edge), px(edge));
            for (index, _) in PALETTE.iter().enumerate() {
                let cell = palette_cell_bounds(bounds, index / PALETTE_COLS, index % PALETTE_COLS);
                let local = LogicalPoint::new(
                    cell.origin.x + cell.size.width / 2.0 - bounds.origin.x,
                    cell.origin.y + cell.size.height / 2.0 - bounds.origin.y,
                );
                assert_eq!(
                    palette_index_at(size, local),
                    Some(index),
                    "画在 {edge}px 网格上的色块 {index} 的圆心必须命中自己"
                );
            }
        }
    }

    #[test]
    fn hsla_channel_should_name_its_track() {
        assert_eq!(HslaChannel::Hue.id(), "hue");
        assert_eq!(HslaChannel::Saturation.id(), "saturation");
        assert_eq!(HslaChannel::Lightness.id(), "lightness");
        assert_eq!(HslaChannel::Alpha.id(), "alpha");
        assert_eq!(HslaChannel::ALL.map(HslaChannel::index), [0, 1, 2, 3]);
    }

    #[test]
    fn hsla_channel_should_read_its_label_from_the_panel_labels() {
        let labels = ColorPickerLabels {
            panel: smol_str::SmolStr::new_static("panel!"),
            palette: smol_str::SmolStr::new_static("palette!"),
            hsla: smol_str::SmolStr::new_static("hsla!"),
            hue: smol_str::SmolStr::new_static("hue!"),
            saturation: smol_str::SmolStr::new_static("saturation!"),
            lightness: smol_str::SmolStr::new_static("lightness!"),
            alpha: smol_str::SmolStr::new_static("alpha!"),
            confirm: smol_str::SmolStr::new_static("confirm!"),
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
        let zero = hsla(0.0, 0.0, 0.0, 0.0);
        let half = hsla(0.5, 0.5, 0.5, 0.5);
        let full = hsla(1.0, 1.0, 1.0, 1.0);
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
        assert_eq!(HslaChannel::Hue.value_text(almost_full).as_str(), "360");
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
                .value_text(hsla(0.0001, 0.0, 0.0, 0.0))
                .as_str(),
            "0"
        );
        assert_eq!(
            HslaChannel::Hue
                .value_text(hsla(0.5555, 0.0, 0.0, 0.0))
                .as_str(),
            "200"
        );
    }

    #[test]
    fn the_slider_rows_should_fill_the_panel_content_width() {
        assert_eq!(SLIDER_ROW_HEIGHT, 24.0);
        assert_eq!(SLIDER_LABEL_WIDTH, 72.0);
        assert_eq!(SLIDER_VALUE_WIDTH, 40.0);
        assert_eq!(SLIDER_ROW_INNER_GAP, 8.0);
        assert_eq!(TRACK_HEIGHT, 14.0);
        assert_eq!(TRACK_RADIUS * 2.0, TRACK_HEIGHT);
        assert_eq!(TRACK_SLICES, 256);
        assert_eq!(THUMB_DIAMETER, 12.0);
        assert_eq!(
            SLIDER_LABEL_WIDTH + SLIDER_ROW_INNER_GAP * 2.0 + SLIDER_VALUE_WIDTH + 156.0,
            PANEL_CONTENT_WIDTH,
            "72（标签）+ 8 + 156（轨道）+ 8 + 40（数值）= 284"
        );
        assert_eq!(PANEL_CONTENT_WIDTH, 284.0);
    }

    #[test]
    fn parse_hex_should_accept_every_supported_length() {
        assert_eq!(rgba8(parse_hex("#f00").expect("#rgb")), [255, 0, 0, 255]);
        assert_eq!(rgba8(parse_hex("#f008").expect("#rgba")), [255, 0, 0, 136]);
        assert_eq!(
            rgba8(parse_hex("#00ff00").expect("#rrggbb")),
            [0, 255, 0, 255]
        );
        assert_eq!(
            rgba8(parse_hex("#00ff0080").expect("#rrggbbaa")),
            [0, 255, 0, 128]
        );
        assert_eq!(
            rgba8(parse_hex("#a1b2c3").expect("#rrggbb")),
            [161, 178, 195, 255]
        );
        assert_eq!(
            rgba8(parse_hex("#a1b2c3d4").expect("#rrggbbaa")),
            [161, 178, 195, 212]
        );
        assert_eq!(
            rgba8(parse_hex("#f00").expect("#rgb")),
            rgba8(parse_hex("#ff0000").expect("#rrggbb")),
            "短格式逐 nibble 复制，不缩放"
        );
    }

    #[test]
    fn parse_hex_should_ignore_the_hash_surrounding_whitespace_and_case() {
        for text in ["#ff0000", "ff0000", "#FF0000", "  #Ff0000  "] {
            assert_eq!(
                rgba8(parse_hex(text).expect("红色")),
                [255, 0, 0, 255],
                "{text:?}"
            );
        }
        for text in ["#abcdef", "abcdef", "#ABCDEF", "  #AbCdEf  "] {
            assert_eq!(
                rgba8(parse_hex(text).expect("蓝灰")),
                [171, 205, 239, 255],
                "{text:?}"
            );
        }
        for text in ["#abcd", "abcd", "#ABCD"] {
            assert_eq!(
                rgba8(parse_hex(text).expect("半透明白")),
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
            assert!(parse_hex(text).is_none(), "{text:?} 不得解析");
        }
    }

    #[test]
    fn hex_string_should_be_uppercase_and_omit_an_opaque_alpha() {
        assert_eq!(
            hex_string(hsla_to_color(hsla(0.0, 1.0, 0.5, 1.0))).as_str(),
            "#FF0000"
        );
        assert_eq!(hex_string(Color::BLACK).as_str(), "#000000");
    }

    #[test]
    fn hex_string_should_include_the_alpha_when_it_is_not_opaque() {
        assert_eq!(
            hex_string(hsla_to_color(hsla(0.0, 1.0, 0.5, 0.5))).as_str(),
            "#FF000080"
        );
        assert_eq!(hex_string(Color::TRANSPARENT).as_str(), "#00000000");
    }

    #[test]
    fn hex_string_should_round_rather_than_truncate() {
        assert_eq!(
            hex_string(hsla_to_color(hsla(0.0, 1.0, 0.5, 0.5))).as_str(),
            "#FF000080",
            "0.5 × 255 = 127.5：四舍五入写出 80"
        );
        assert_eq!(
            hex_string(hsla_to_color(hsla(0.0, 0.0, 0.0, 254.5 / 255.0))).as_str(),
            "#000000",
            "量化后不透明的 alpha 被省略"
        );
    }

    #[test]
    fn hex_should_round_trip_through_eight_bit_quantization() {
        let colors = [
            hsla_to_color(hsla(0.0, 1.0, 0.5, 1.0)),
            hsla_to_color(hsla(0.55, 0.8, 0.53, 1.0)),
            hsla_to_color(hsla(0.33, 0.42, 0.61, 0.5)),
            Color::TRANSPARENT,
        ];
        for color in colors {
            let hex = hex_string(color);
            let parsed = parse_hex(hex.as_str()).expect("picker 自己写出的 HEX 必须能解析");
            assert_eq!(rgba8(parsed), rgba8(color), "{hex} 必须八位往返一致");
        }
    }

    #[test]
    fn hsl_and_srgb_should_round_trip_through_more_than_thirty_samples() {
        let mut samples = 0usize;
        for hue_index in 0..6 {
            for saturation in [0.25_f32, 0.5, 0.75, 1.0] {
                for lightness in [0.05_f32, 0.25, 0.5, 0.75, 0.95] {
                    let h = hue_index as f32 / 6.0;
                    let color = hsla_to_color(hsla(h, saturation, lightness, 0.5));
                    let back = color_to_hsla(color);
                    assert!(
                        (back.h - h).abs() < EPS,
                        "hue {h} 往返成 {}（s={saturation}, l={lightness}）",
                        back.h
                    );
                    assert!((back.s - saturation).abs() < EPS);
                    assert!((back.l - lightness).abs() < EPS);
                    assert!((back.a - 0.5).abs() < EPS);
                    samples += 1;
                }
            }
        }
        for lightness in [0.0_f32, 0.25, 0.5, 0.75, 1.0] {
            let back = color_to_hsla(hsla_to_color(hsla(0.0, 0.0, lightness, 1.0)));
            assert_eq!(back.s, 0.0, "灰色没有饱和度");
            assert!((back.l - lightness).abs() < EPS);
            samples += 1;
        }
        for hue in [0.1_f32, 0.37] {
            for saturation in [0.3_f32, 1.0] {
                for lightness in [0.2_f32, 0.8] {
                    for alpha in [0.0_f32, 1.0] {
                        let color = hsla_to_color(hsla(hue, saturation, lightness, alpha));
                        let back = color_to_hsla(color);
                        assert!(
                            (back.h - hue).abs() < EPS,
                            "任意色相 {hue} 往返成 {}（s={saturation}, l={lightness}, a={alpha}）",
                            back.h
                        );
                        assert!((back.s - saturation).abs() < EPS);
                        assert!((back.l - lightness).abs() < EPS);
                        assert!((back.a - alpha).abs() < EPS);
                        samples += 1;
                    }
                }
            }
        }
        assert!(samples >= 30, "抽样数 {samples} 不足 30");
    }

    #[test]
    fn colors_in_other_spaces_should_resolve_through_linear_srgb() {
        let gray = color_to_hsla(Color::linear_srgb(0.5, 0.5, 0.5, 1.0));
        assert!(
            (gray.l - 0.735_356_9).abs() < 2.0e-5,
            "线性 0.5 灰 → sRGB {}（应 ≈ 0.73536）",
            gray.l
        );
        assert_eq!((gray.h, gray.s), (0.0, 0.0), "灰阶没有色相/饱和度");

        for color in [
            Color::display_p3(0.6, 0.2, 0.1, 1.0),
            Color::oklab(0.5, 0.1, 0.05, 0.8),
            Color::oklch(0.5, 0.1, 0.2, 0.5),
        ] {
            let linear = ColorResolver.resolve(color);
            let [r, g, b, a] = srgb_components(color);
            let resolved_alpha = linear.components[3];
            assert!((a - resolved_alpha).abs() < 1.0e-5);
            for (encoded, channel) in [r, g, b].into_iter().zip(&linear.components[..3]) {
                assert!(
                    (encoded - linear_to_srgb(*channel / resolved_alpha)).abs() < 1.0e-5,
                    "非 Srgb 分量必须走线性（去预乘）→ sRGB 编码"
                );
            }
            let back = hsla_to_color(color_to_hsla(color));
            for (left, right) in srgb_components(back)
                .into_iter()
                .zip(srgb_components(color))
            {
                assert!((left - right).abs() < EPS);
            }
        }

        assert_eq!(
            srgb_components(Color::display_p3(0.5, 0.5, 0.5, 0.0)),
            [0.0; 4],
            "alpha 0 短路（预乘后不可反解）"
        );
    }

    #[test]
    fn linear_and_srgb_should_use_the_iec_transfer_curve() {
        let linear = srgb_components(Color::linear_srgb(0.5, 0.5, 0.5, 1.0));
        for value in &linear[..3] {
            assert!(
                (value - 0.735_356_9).abs() < 1.0e-5,
                "线性 0.5 → sRGB {}",
                value
            );
        }
        let half = srgb_components(Color::linear_srgb(
            0.214_041_1,
            0.214_041_1,
            0.214_041_1,
            1.0,
        ));
        assert!((half[0] - 0.5).abs() < 1.0e-4, "{}", half[0]);
    }

    #[test]
    fn a_translucent_well_draws_a_checkerboard_and_an_opaque_one_does_not() {
        let bounds = LogicalRect::new(px(10.0), px(20.0), px(24.0), px(24.0));
        let base = Color::srgb(0.1, 0.1, 0.1, 1.0);
        let checker = Color::srgb(0.8, 0.8, 0.8, 1.0);
        let opaque = well_canvas(bounds, Color::WHITE, base, checker, px(4.0));
        assert_eq!(canvas_quads(&opaque).len(), 2, "底色 + 颜色");

        let translucent = well_canvas(
            bounds,
            Color::srgb(1.0, 0.0, 0.0, 0.5),
            base,
            checker,
            px(4.0),
        );
        assert!(
            canvas_quads(&translucent).len() > 2,
            "半透明颜色必须露出棋盘格"
        );
        assert_eq!(translucent.bounds(), bounds, "画布 AABB = 色块盒");
    }

    #[test]
    fn a_palette_canvas_draws_every_cell_and_the_hover_ring() {
        let bounds = LogicalRect::new(px(0.0), px(0.0), px(PALETTE_SIZE), px(PALETTE_SIZE));
        let canvas = palette_canvas(bounds, None);
        assert_eq!(canvas_quads(&canvas).len(), PALETTE.len());
        let hovered = palette_canvas(bounds, Some(28));
        assert_eq!(canvas_quads(&hovered).len(), PALETTE.len() + 1);
        let ring = canvas_quads(&hovered)
            .into_iter()
            .find(|quad| {
                matches!(
                    quad.paint,
                    ui_core::paint::primitive::QuadPaint::Solid {
                        border: Some(_),
                        ..
                    }
                )
            })
            .expect("悬停环图元使用带边框的绘制模式");
        assert_eq!(ring.corner_radius, [px(3.0); 4]);
    }

    #[test]
    fn a_track_narrower_than_its_thumb_should_paint_without_panicking() {
        let bounds = LogicalRect::new(px(0.0), px(0.0), px(10.0), px(14.0));
        let canvas = track_canvas(
            bounds,
            HslaChannel::Hue,
            hsla(0.5, 0.5, 0.5, 1.0),
            0.5,
            Color::WHITE,
            Color::BLACK,
        );
        let quads = canvas_quads(&canvas);
        assert_eq!(quads.len(), TRACK_SLICES, "窄轨道只画切片，没有拇指");
        for quad in quads {
            assert!(quad.bounds.origin.x >= bounds.origin.x - EPS);
            assert!((quad.bounds.origin.x + quad.bounds.size.width).value() <= 10.0 + EPS);
        }
    }

    #[test]
    fn a_full_track_draws_the_thumb_at_the_value() {
        let bounds = LogicalRect::new(px(100.0), px(5.0), px(156.0), px(14.0));
        let canvas = track_canvas(
            bounds,
            HslaChannel::Saturation,
            hsla(0.2, 0.5, 0.4, 1.0),
            0.25,
            Color::WHITE,
            Color::BLACK,
        );
        let quads = canvas_quads(&canvas);
        assert_eq!(quads.len(), TRACK_SLICES + 1);
        let thumb = quads.last().expect("拇指");
        assert!((thumb.bounds.size.width.value() - THUMB_DIAMETER).abs() < EPS);
        let center = thumb.bounds.origin.x + thumb.bounds.size.width / 2.0;
        assert!((center.value() - (100.0 + 156.0 * 0.25)).abs() < EPS);
    }

    #[test]
    fn labels_should_default_to_the_panel_text() {
        let labels = ColorPickerLabels::default();
        assert_eq!(labels.panel.as_str(), "Color");
        assert_eq!(labels.palette.as_str(), "Palette");
        assert_eq!(labels.hsla.as_str(), "HSLA");
        assert_eq!(labels.hue.as_str(), "Hue");
        assert_eq!(labels.saturation.as_str(), "Saturation");
        assert_eq!(labels.lightness.as_str(), "Lightness");
        assert_eq!(labels.alpha.as_str(), "Alpha");
        assert_eq!(labels.confirm.as_str(), "OK");
    }
}

#[cfg(test)]
mod ui_tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        sync::Arc,
        time::Duration,
    };

    use smol_str::SmolStr;
    use ui_core::{
        a11y::A11yRole,
        event::{Event, InputEvent, KeyCode, KeyState, Modifiers, PointerButton},
        math::{LogicalPoint, LogicalRect, px},
        node::{
            id::NodeId,
            tree::{NodeKind, NodeTree},
        },
        paint::canvas::Canvas,
        test_support::app::TestApp,
    };

    use super::{
        BodyView, Color, ColorPicker, ColorPickerTab, Hsla, UnifiedSize, hex_string, hsla,
        hsla_to_color, palette_index_at, rgba8,
    };
    use crate::{
        kit::{layer::Layer, overlay::AnimationRouter, test_support::full_a11y},
        theme::{appearance::Appearance, handle::ThemeHandle, role::Role},
    };

    const DEFAULT: Hsla = hsla(0.2, 0.5, 0.4, 1.0);

    fn default_color() -> Color {
        hsla_to_color(DEFAULT)
    }

    struct Harness {
        app: TestApp,
        picker: ColorPicker,
        theme: ThemeHandle,
        changes: Rc<RefCell<Vec<Color>>>,
        window: ui_core::window::WindowId,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_size(UnifiedSize::Medium)
        }

        fn with_size(size: UnifiedSize) -> Self {
            let mut app = TestApp::new();
            let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
            let layer = Layer::new();
            let router = AnimationRouter::new();
            let picker = ColorPicker::new(
                app.app_mut(),
                theme.clone(),
                layer,
                router,
                "picker",
                default_color(),
                size,
            );
            let window = app.window_id();
            let root = app.app().window(window).expect("窗口").tree().root();
            picker.build(app.app_mut(), window, root).expect("触发器");
            let changes = Rc::new(RefCell::new(Vec::new()));
            let sink = changes.clone();
            picker.on_change(move |_, color| sink.borrow_mut().push(color));
            let mut harness = Self {
                app,
                picker,
                theme,
                changes,
                window,
            };
            harness.pump();
            harness
        }

        fn pump(&mut self) -> bool {
            let _ = self.app.drain().expect("帧");
            let changed = self.picker.sync(self.app.app_mut()).expect("sync");
            let _ = self.app.drain().expect("帧");
            changed
        }

        fn pump_times(&mut self, times: usize) {
            for _ in 0..times {
                self.pump();
            }
        }

        fn settle(&mut self) {
            for _ in 0..4 {
                self.app.advance(Duration::from_millis(60));
                self.pump();
            }
        }

        fn settled(&mut self) -> bool {
            for _ in 0..6 {
                let changed = self.pump();
                if !changed && !self.app.frame_record().rendered {
                    return true;
                }
            }
            false
        }

        fn bounds(&self, node: ui_core::node::id::NodeId) -> LogicalRect {
            self.app.probe().bounds(node).expect("节点已布局")
        }

        fn center(rect: LogicalRect) -> LogicalPoint {
            LogicalPoint::new(
                rect.origin.x + rect.size.width / 2.0,
                rect.origin.y + rect.size.height / 2.0,
            )
        }

        fn click_at(&mut self, point: LogicalPoint) {
            let _ = self.app.inject(InputEvent::pointer_down(
                point,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let _ = self.app.inject(InputEvent::pointer_up(
                point,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            self.pump_times(2);
        }

        fn click_node(&mut self, node: ui_core::node::id::NodeId) {
            let point = Self::center(self.bounds(node));
            self.click_at(point);
        }

        fn trigger(&self) -> ui_core::node::id::NodeId {
            self.picker.trigger_node().expect("触发器已挂载")
        }

        fn open(&mut self) {
            self.click_node(self.trigger());
        }

        fn panel_open(&self) -> bool {
            self.picker.inner.panel.borrow().is_some()
        }

        fn palette(&self) -> Option<(LogicalRect, Vec<ui_core::node::id::NodeId>)> {
            let panel = self.picker.inner.panel.borrow();
            let panel = panel.as_ref()?;
            let BodyView::Palette(view) = &panel.body_view else {
                return None;
            };
            Some((self.bounds(view.root), view.cells.clone()))
        }

        fn tracks(&self) -> Option<[ui_core::node::id::NodeId; 4]> {
            let panel = self.picker.inner.panel.borrow();
            let panel = panel.as_ref()?;
            let BodyView::Sliders(view) = &panel.body_view else {
                return None;
            };
            Some(std::array::from_fn(|index| view.tracks[index].track))
        }

        fn track_canvas(&self, index: usize) -> Option<ui_core::node::id::NodeId> {
            let panel = self.picker.inner.panel.borrow();
            let panel = panel.as_ref()?;
            let BodyView::Sliders(view) = &panel.body_view else {
                return None;
            };
            Some(view.tracks[index].canvas)
        }

        fn palette_canvas(&self) -> Option<ui_core::node::id::NodeId> {
            let panel = self.picker.inner.panel.borrow();
            let panel = panel.as_ref()?;
            let BodyView::Palette(view) = &panel.body_view else {
                return None;
            };
            Some(view.canvas)
        }

        fn tabs(&self) -> [ui_core::node::id::NodeId; 2] {
            let panel = self.picker.inner.panel.borrow();
            panel.as_ref().expect("面板已打开").tabs
        }

        fn confirm(&self) -> ui_core::node::id::NodeId {
            let panel = self.picker.inner.panel.borrow();
            panel.as_ref().expect("面板已打开").confirm
        }

        fn input_root(&self) -> ui_core::node::id::NodeId {
            let panel = self.picker.inner.panel.borrow();
            panel.as_ref().expect("面板已打开").input.root
        }

        fn hex_focus(&mut self) {
            let root = self.input_root();
            assert!(
                self.app.app_mut().set_focus(self.window, Some(root)),
                "HEX 输入框可聚焦"
            );
            self.pump_times(2);
        }

        fn hex_blur(&mut self) {
            self.app.app_mut().set_focus(self.window, None);
            self.pump_times(2);
        }

        fn clear_hex(&mut self) {
            let panel = self.picker.inner.panel.borrow();
            let input = &panel.as_ref().expect("面板已打开").input;
            input.update_value(self.app.app_mut(), "");
            drop(panel);
            self.pump_times(1);
        }

        fn pump_event(&mut self, event: Event) {
            self.app.dispatch_event(event);
            let events = self.app.app_mut().drain_events();
            for (_window, event) in events {
                self.picker.dispatch(self.app.app_mut(), &event);
            }
            self.pump_times(2);
        }

        fn type_text(&mut self, text: &str) {
            self.pump_event(Event::Input(InputEvent::Text {
                text: SmolStr::new(text),
                timestamp: std::time::Instant::now(),
            }));
        }

        fn key(&mut self, code: KeyCode) {
            self.pump_event(Event::Input(InputEvent::Key {
                code,
                state: KeyState::Down,
                repeat: false,
                modifiers: Modifiers::empty(),
                timestamp: std::time::Instant::now(),
            }));
        }

        fn click_cell(&mut self, row: usize, col: usize) {
            let (grid, _) = self.palette().expect("色板已显示");
            let point = LogicalPoint::new(
                grid.origin.x + super::PALETTE_STEP * col as f32 + 10.0,
                grid.origin.y + super::PALETTE_STEP * row as f32 + 10.0,
            );
            self.click_at(point);
        }

        fn hover_at(&mut self, point: LogicalPoint) {
            let _ = self
                .app
                .inject(InputEvent::pointer_move(point, Modifiers::empty()));
            self.pump_times(2);
        }

        fn drag_track(&mut self, channel: usize, start_fraction: f32, target: LogicalPoint) {
            let start = self.track_point(channel, start_fraction);
            let launch_fraction = if target.x >= start.x {
                (start_fraction + 0.1).min(1.0)
            } else {
                (start_fraction - 0.1).max(0.0)
            };
            let launch = self.track_point(channel, launch_fraction);
            let _ = self.app.inject(InputEvent::pointer_down(
                start,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let _ = self
                .app
                .inject(InputEvent::pointer_move(launch, Modifiers::empty()));
            let _ = self
                .app
                .inject(InputEvent::pointer_move(target, Modifiers::empty()));
            let _ = self.app.inject(InputEvent::pointer_up(
                target,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            self.pump_times(2);
        }

        fn focus_node(&mut self, node: NodeId) {
            assert!(
                self.app.app_mut().set_focus(self.window, Some(node)),
                "节点可聚焦"
            );
            self.pump_times(2);
        }

        fn key_press(&mut self, code: KeyCode) {
            for state in [KeyState::Down, KeyState::Up] {
                self.pump_event(Event::Input(InputEvent::Key {
                    code,
                    state,
                    repeat: false,
                    modifiers: Modifiers::empty(),
                    timestamp: std::time::Instant::now(),
                }));
            }
        }

        fn track_point(&self, channel: usize, fraction: f32) -> LogicalPoint {
            let track = self.tracks().expect("HSLA 页")[channel];
            let bounds = self.bounds(track);
            LogicalPoint::new(
                bounds.origin.x + bounds.size.width * fraction,
                bounds.origin.y + bounds.size.height / 2.0,
            )
        }

        fn switch_to_hsla(&mut self) {
            let tabs = self.tabs();
            self.click_node(tabs[1]);
        }

        fn canvas_content(&self, node: NodeId) -> Arc<Canvas> {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            match tree.get(node).expect("节点").kind() {
                NodeKind::Canvas(canvas) => canvas.clone(),
                other => panic!("期望 Canvas，实际 {other:?}"),
            }
        }

        fn text_node(&self, root: NodeId) -> NodeId {
            fn walk(tree: &NodeTree, node: NodeId) -> Option<NodeId> {
                let target = tree.get(node).ok()?;
                if matches!(target.kind(), NodeKind::Text(_)) {
                    return Some(node);
                }
                target
                    .children()
                    .iter()
                    .find_map(|child| walk(tree, *child))
            }

            let tree = self.app.app().window(self.window).expect("窗口").tree();
            walk(tree, root).expect("子树里有文本")
        }

        fn text_foreground(&self, node: NodeId) -> Option<Color> {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            tree.get(node).expect("文本节点").style_patch().foreground
        }

        fn trigger_well(&self) -> NodeId {
            self.picker
                .inner
                .trigger
                .borrow()
                .as_ref()
                .expect("触发器已挂载")
                .well
        }

        fn panel_well(&self) -> NodeId {
            self.picker
                .inner
                .panel
                .borrow()
                .as_ref()
                .expect("面板已打开")
                .well
        }

        fn panel_painter(&self, node: NodeId) -> Vec<LogicalRect> {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            match tree.get(node).expect("节点").kind() {
                NodeKind::Canvas(canvas) => canvas
                    .primitives()
                    .iter()
                    .filter_map(|primitive| match primitive {
                        ui_core::paint::primitive::Primitive::Quad(quad) => Some(quad.bounds),
                        _ => None,
                    })
                    .collect(),
                other => panic!("期望 Canvas，实际 {other:?}"),
            }
        }
    }

    #[test]
    fn the_picker_should_start_closed_on_the_default_color_and_palette_tab() {
        let harness = Harness::new();
        assert_eq!(harness.picker.color(harness.app.app()), default_color());
        assert_eq!(
            harness.picker.active_tab(harness.app.app()),
            ColorPickerTab::Palette
        );
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            hex_string(default_color())
        );
        assert!(harness.app.probe().bounds(harness.trigger()).is_some());
        assert!(!harness.panel_open(), "面板初始关闭");
    }

    #[test]
    fn the_panel_surface_should_use_the_old_double_layer_menu_recipe() {
        let harness = Harness::new();
        let surface = &harness.picker.overlay().options().surface;
        assert_eq!(surface.background, Role::SurfaceOverlay);
        assert_eq!(surface.corner_radius, px(4.0), "旧 rounded_sm");
        assert_eq!(surface.padding, super::PANEL_PADDING, "旧 p_2");
        let layers: Vec<_> = surface.shadows.iter().collect();
        assert_eq!(layers.len(), 2, "旧面板是双层菜单阴影");
        assert_eq!(layers[0].offset, LogicalPoint::new(px(0.0), px(1.0)));
        assert_eq!(layers[0].blur, px(2.0));
        assert_eq!(layers[0].spread, px(0.0));
        assert_eq!(layers[1].offset, LogicalPoint::new(px(0.0), px(3.0)));
        assert_eq!(layers[1].blur, px(6.0));
        assert_eq!(
            layers[1].spread,
            px(-2.0),
            "第二层负 spread 是旧菜单的紧凑接触阴影"
        );
        assert_eq!(layers[0].color, layers[1].color, "两层同色（菜单阴影色）");
    }

    #[test]
    fn the_trigger_should_be_square_at_every_unified_size() {
        for (size, edge) in [
            (UnifiedSize::Small, 20.0),
            (UnifiedSize::Medium, 30.0),
            (UnifiedSize::Large, 40.0),
        ] {
            let mut harness = Harness::with_size(size);
            let bounds = harness.bounds(harness.trigger());
            assert_eq!(bounds.size.width, bounds.size.height, "{size:?}");
            assert_eq!(bounds.size.width, px(edge), "{size:?} 触发器边长");
            harness.open();
            assert!(harness.panel_open(), "{size:?} 方形触发器仍能打开面板");
        }
    }

    #[test]
    fn pressing_the_trigger_should_open_the_panel() {
        let mut harness = Harness::new();
        assert!(!harness.panel_open());
        harness.open();
        assert!(harness.panel_open(), "按下触发器挂载面板");
        assert!(harness.palette().is_some(), "默认显示色板页");
        let visual = harness
            .picker
            .overlay()
            .overlay_node()
            .map(|node| harness.app.probe().bounds(node));
        let _ = visual;
    }

    #[test]
    fn the_open_state_signal_and_on_open_callback_track_the_panel() {
        let mut harness = Harness::new();
        let opened = Rc::new(Cell::new(0usize));
        let sink = opened.clone();
        harness.picker.on_open(move |_| sink.set(sink.get() + 1));
        let state = harness.picker.open_state();
        assert!(
            !*harness.app.app().read(&state),
            "初始关闭：open_state 信号必须是 false"
        );
        assert!(!harness.picker.is_open(harness.app.app()));

        harness.open();
        assert!(
            harness.picker.is_open(harness.app.app()),
            "点击触发器后 is_open 必须可观测为 true"
        );
        assert!(
            *harness.app.app().read(&state),
            "点击触发器后 open_state 信号必须为 true"
        );
        assert_eq!(opened.get(), 1, "关闭 → 打开的边沿只回调一次");

        assert!(harness.picker.close(harness.app.app_mut()));
        harness.pump_times(2);
        assert!(!harness.picker.is_open(harness.app.app()));
        assert!(!*harness.app.app().read(&state));
        assert_eq!(opened.get(), 1, "关闭不触发 on_open");

        assert!(
            harness.picker.open(harness.app.app_mut(), harness.window),
            "程序化打开返回 true"
        );
        harness.pump_times(1);
        assert!(harness.picker.is_open(harness.app.app()));
        assert_eq!(opened.get(), 2, "程序化打开同样触发 on_open");
        assert!(
            !harness.picker.open(harness.app.app_mut(), harness.window),
            "已打开时程序化 open 是 no-op"
        );
        harness.pump_times(1);
        assert_eq!(opened.get(), 2, "重复打开不再回调");
    }

    #[test]
    fn pressing_outside_the_panel_should_close_it() {
        let mut harness = Harness::new();
        harness.open();
        assert!(harness.panel_open());
        harness.click_at(LogicalPoint::new(px(760.0), px(560.0)));
        assert!(!harness.panel_open(), "面板外部按下关闭面板");
    }

    #[test]
    fn pressing_the_trigger_of_an_open_panel_should_close_it_without_reopening_it() {
        let mut harness = Harness::new();
        harness.open();
        assert!(harness.panel_open());
        harness.open();
        harness.settle();
        assert!(!harness.panel_open(), "闭合按下的抬起不得重新打开面板");
        harness.settle();
        assert!(!harness.panel_open(), "后续帧保持关闭");
    }

    #[test]
    fn the_open_panel_should_switch_between_the_palette_and_the_hsla_tracks() {
        let mut harness = Harness::new();
        harness.open();
        assert!(harness.palette().is_some());
        let (grid, _) = harness.palette().expect("色板");
        assert_eq!(grid.size.width, px(super::PALETTE_SIZE));
        assert_eq!(grid.size.height, px(super::PALETTE_SIZE));

        harness.switch_to_hsla();
        assert_eq!(
            harness.picker.active_tab(harness.app.app()),
            ColorPickerTab::Hsla
        );
        assert!(harness.palette().is_none());
        let tabs = harness.tabs();
        let _ = tabs;
        let tracks = harness.tracks().expect("HSLA 页画出四条轨道");
        let height = harness.bounds(tracks[0]).size.height;
        assert_eq!(height, px(super::TRACK_HEIGHT));
        let width = harness.bounds(tracks[0]).size.width;
        assert_eq!(width, px(156.0), "72 + 8 + 156 + 8 + 40 = 284");

        let tabs = harness.tabs();
        harness.click_node(tabs[0]);
        assert!(harness.palette().is_some(), "切回色板");
        assert!(harness.panel_open(), "切页保持面板打开");
    }

    #[test]
    fn clicking_a_palette_cell_should_edit_the_draft_and_keep_the_panel_open() {
        let mut harness = Harness::new();
        harness.open();
        harness.click_cell(2, 4);
        let swatch = super::hsla_to_color(super::PALETTE[28]);
        let draft = harness.picker.draft(harness.app.app()).expect("草稿");
        assert_eq!(rgba8(draft), rgba8(swatch));
        assert_eq!(
            harness.picker.color(harness.app.app()),
            default_color(),
            "确认色保持不变"
        );
        assert!(
            harness.changes.borrow().is_empty(),
            "选色不是事件，确认按钮才报告"
        );
        assert!(harness.panel_open(), "选色保持面板打开");
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            hex_string(swatch),
            "HEX 字段跟随草稿"
        );

        harness.click_node(harness.confirm());
        harness.settle();
        assert_eq!(harness.picker.color(harness.app.app()), swatch);
        assert_eq!(harness.changes.borrow().as_slice(), &[swatch]);
        assert!(!harness.panel_open(), "确认关闭面板");

        harness.open();
        harness.click_cell(2, 4);
        harness.click_node(harness.confirm());
        harness.settle();
        assert_eq!(
            harness.changes.borrow().len(),
            1,
            "确认已选中的颜色不再发出事件"
        );
        assert!(!harness.panel_open(), "确认仍在关闭面板");
    }

    #[test]
    fn hovering_a_palette_cell_should_preview_the_footer_and_hold_across_gaps() {
        let mut harness = Harness::new();
        harness.open();
        let (grid, _) = harness.palette().expect("色板");
        let cell = LogicalPoint::new(
            grid.origin.x + super::PALETTE_STEP * 4.0 + 10.0,
            grid.origin.y + super::PALETTE_STEP * 2.0 + 10.0,
        );
        harness.hover_at(cell);
        let hovered = super::hsla_to_color(super::PALETTE[28]);
        assert_eq!(harness.picker.hovered_swatch(harness.app.app()), Some(28));
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            hex_string(hovered),
            "悬停把色块预览写进 HEX 字段"
        );
        assert_eq!(
            harness.picker.draft(harness.app.app()),
            None,
            "悬停只预览，不得写草稿"
        );
        assert_eq!(
            harness.picker.color(harness.app.app()),
            default_color(),
            "悬停不改确认色"
        );
        assert!(harness.changes.borrow().is_empty());
        assert!(harness.panel_open());

        let gap = LogicalPoint::new(cell.x + 12.0, cell.y);
        harness.hover_at(gap);
        assert_eq!(
            harness.picker.hovered_swatch(harness.app.app()),
            Some(28),
            "跨色板间隙保留上一个预览"
        );
        assert_eq!(harness.picker.draft(harness.app.app()), None);

        let next = LogicalPoint::new(cell.x + super::PALETTE_STEP, cell.y);
        harness.hover_at(next);
        assert_eq!(harness.picker.hovered_swatch(harness.app.app()), Some(29));
        assert_eq!(harness.picker.draft(harness.app.app()), None);

        harness.hover_at(LogicalPoint::new(px(760.0), px(560.0)));
        assert_eq!(
            harness.picker.hovered_swatch(harness.app.app()),
            None,
            "离开色板清掉预览"
        );
        assert_eq!(harness.picker.draft(harness.app.app()), None);
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            hex_string(default_color()),
            "离开色板恢复确认色的 HEX"
        );
        assert!(harness.changes.borrow().is_empty());
    }

    #[test]
    fn keyboard_confirm_after_hovering_should_report_nothing() {
        let mut harness = Harness::new();
        harness.open();
        let (grid, _) = harness.palette().expect("色板");
        let cell = LogicalPoint::new(
            grid.origin.x + super::PALETTE_STEP * 4.0 + 10.0,
            grid.origin.y + super::PALETTE_STEP * 2.0 + 10.0,
        );
        harness.hover_at(cell);
        assert_eq!(harness.picker.hovered_swatch(harness.app.app()), Some(28));
        assert_eq!(
            harness.picker.draft(harness.app.app()),
            None,
            "悬停不写草稿"
        );

        harness.focus_node(harness.confirm());
        harness.key_press(KeyCode::Enter);
        harness.settle();
        assert!(!harness.panel_open(), "Enter 激活确认并关闭面板");
        assert!(
            harness.changes.borrow().is_empty(),
            "悬停预览不是编辑，确认不得报色"
        );
        assert_eq!(harness.picker.color(harness.app.app()), default_color());
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            hex_string(default_color())
        );
    }

    #[test]
    fn closing_the_panel_should_clear_a_palette_hover_preview() {
        let mut harness = Harness::new();
        harness.open();
        let (grid, _) = harness.palette().expect("色板");
        let cell = LogicalPoint::new(
            grid.origin.x + super::PALETTE_STEP * 4.0 + 10.0,
            grid.origin.y + super::PALETTE_STEP * 2.0 + 10.0,
        );
        harness.hover_at(cell);
        assert_eq!(harness.picker.hovered_swatch(harness.app.app()), Some(28));
        harness.open();
        harness.settle();
        assert!(!harness.panel_open());
        assert_eq!(harness.picker.hovered_swatch(harness.app.app()), None);
        assert_eq!(harness.picker.draft(harness.app.app()), None);
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            hex_string(default_color())
        );
    }

    #[test]
    fn dismissing_the_panel_after_an_edit_should_report_nothing() {
        let mut harness = Harness::new();
        harness.open();
        harness.click_cell(2, 4);
        assert!(harness.picker.draft(harness.app.app()).is_some());
        harness.click_at(LogicalPoint::new(px(760.0), px(560.0)));
        harness.settle();
        assert!(!harness.panel_open(), "外部按压关闭面板");
        assert_eq!(
            harness.picker.color(harness.app.app()),
            default_color(),
            "被丢弃的草稿不成为确认色"
        );
        assert_eq!(harness.picker.draft(harness.app.app()), None);
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            hex_string(default_color())
        );
        assert!(harness.changes.borrow().is_empty(), "关闭不是确认");
    }

    #[test]
    fn confirming_a_palette_swatch_should_report_the_final_color_once() {
        let mut harness = Harness::new();
        harness.open();
        harness.click_cell(2, 4);
        assert!(harness.changes.borrow().is_empty());
        harness.click_node(harness.confirm());
        harness.settle();
        let swatch = super::hsla_to_color(super::PALETTE[28]);
        assert_eq!(harness.picker.color(harness.app.app()), swatch);
        assert_eq!(
            harness.changes.borrow().as_slice(),
            &[swatch],
            "确认只发一次事件，携带最终色"
        );
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            hex_string(swatch)
        );
        assert!(!harness.panel_open());
    }

    #[test]
    fn confirming_after_a_slider_change_should_report_the_final_color_once() {
        let mut harness = Harness::new();
        harness.open();
        harness.switch_to_hsla();
        let target = harness.track_point(0, 0.25);
        harness.click_at(target);
        let edited = harness
            .picker
            .draft(harness.app.app())
            .expect("轨道编辑产生草稿");
        assert!((super::color_to_hsla(edited).h - 0.25).abs() < 1.0e-3);
        assert!(harness.changes.borrow().is_empty());
        assert!(harness.panel_open());

        harness.click_node(harness.confirm());
        harness.settle();
        assert!(!harness.panel_open());
        assert_eq!(harness.picker.color(harness.app.app()), edited);
        assert_eq!(harness.changes.borrow().as_slice(), &[edited]);
    }

    #[test]
    fn confirming_without_an_edit_should_report_nothing() {
        let mut harness = Harness::new();
        harness.open();
        harness.click_node(harness.confirm());
        harness.settle();
        assert!(!harness.panel_open());
        assert!(harness.changes.borrow().is_empty());
    }

    #[test]
    fn a_programmatic_change_should_not_be_reported_by_a_confirmation() {
        let mut harness = Harness::new();
        let blue = super::hsla_to_color(super::hsla(2.0 / 3.0, 1.0, 0.5, 1.0));
        harness.picker.set_color(harness.app.app_mut(), blue);
        harness.pump_times(1);
        harness.open();
        let green = super::hsla_to_color(super::hsla(1.0 / 3.0, 1.0, 0.5, 1.0));
        harness.picker.set_color(harness.app.app_mut(), green);
        harness.pump_times(1);
        harness.click_node(harness.confirm());
        harness.settle();
        assert!(!harness.panel_open());
        assert_eq!(harness.picker.color(harness.app.app()), green);
        assert!(
            harness.changes.borrow().is_empty(),
            "set_color 是静默的，无论面板开或关"
        );
    }

    #[test]
    fn set_color_should_clear_the_edit_the_panel_is_holding() {
        let mut harness = Harness::new();
        harness.open();
        harness.click_cell(2, 4);
        assert!(harness.picker.draft(harness.app.app()).is_some());
        let blue = super::hsla_to_color(super::hsla(2.0 / 3.0, 1.0, 0.5, 1.0));
        harness.picker.set_color(harness.app.app_mut(), blue);
        harness.pump_times(1);
        assert_eq!(harness.picker.draft(harness.app.app()), None);
        harness.click_node(harness.confirm());
        harness.settle();
        assert_eq!(harness.picker.color(harness.app.app()), blue);
        assert!(
            harness.changes.borrow().is_empty(),
            "程序化改色后确认没有用户编辑可报告"
        );
    }

    #[test]
    fn restoring_the_confirmed_color_should_leave_nothing_to_confirm() {
        let mut harness = Harness::new();
        harness.open();
        harness.switch_to_hsla();
        let target = harness.track_point(0, 0.75);
        harness.click_at(target);
        assert!(harness.picker.draft(harness.app.app()).is_some());
        harness
            .picker
            .set_channel(harness.app.app_mut(), super::HslaChannel::Hue, DEFAULT.h);
        harness.pump_times(1);
        assert_eq!(
            harness.picker.draft(harness.app.app()),
            None,
            "编辑回到确认色时不留下草稿"
        );
        harness.click_node(harness.confirm());
        harness.settle();
        assert!(!harness.panel_open());
        assert!(harness.changes.borrow().is_empty());
    }

    #[test]
    fn a_repeated_open_signal_write_should_be_a_no_op_and_keep_the_edit() {
        let mut harness = Harness::new();
        harness.open();
        harness.switch_to_hsla();
        harness.click_at(harness.track_point(0, 0.75));
        let edited = harness.picker.draft(harness.app.app()).expect("轨道编辑");
        let open = harness.picker.overlay().open_state();
        assert!(
            !harness.app.app_mut().write(&open, true),
            "等值写短路（I3：不产生帧）"
        );
        harness.pump_times(2);
        assert!(harness.panel_open(), "重复写入保持面板打开");
        assert_eq!(
            harness.picker.draft(harness.app.app()),
            Some(edited),
            "重复打开通知不得丢草稿"
        );
        assert!(harness.changes.borrow().is_empty());

        harness.click_node(harness.confirm());
        harness.settle();
        assert_eq!(harness.changes.borrow().as_slice(), &[edited]);
    }

    #[test]
    fn a_second_interaction_should_report_on_its_own_after_the_first() {
        let mut harness = Harness::new();
        harness.open();
        harness.click_cell(2, 4);
        harness.click_node(harness.confirm());
        harness.settle();
        let first = super::hsla_to_color(super::PALETTE[28]);
        assert_eq!(harness.changes.borrow().as_slice(), &[first]);

        harness.open();
        harness.open();
        harness.settle();
        assert_eq!(
            harness.changes.borrow().as_slice(),
            &[first],
            "无编辑的关闭不重复报告"
        );

        harness.open();
        harness.switch_to_hsla();
        let target = harness.track_point(0, 0.75);
        harness.click_at(target);
        let second = harness.picker.draft(harness.app.app()).expect("第二段编辑");
        assert_ne!(second, first);
        harness.click_node(harness.confirm());
        harness.settle();
        assert_eq!(harness.changes.borrow().as_slice(), &[first, second]);
    }

    #[test]
    fn clicking_the_lightness_and_alpha_tracks_in_the_open_panel_should_edit_those_channels() {
        let mut harness = Harness::new();
        harness.open();
        harness.switch_to_hsla();

        harness.click_at(harness.track_point(2, 0.75));
        let lightness = harness.picker.draft(harness.app.app()).expect("明度编辑");
        assert!((super::color_to_hsla(lightness).l - 0.75).abs() < 1.0e-3);
        let lightness_h = super::color_to_hsla(lightness).h;

        harness.click_at(harness.track_point(3, 0.25));
        let alpha = harness.picker.draft(harness.app.app()).expect("透明度编辑");
        let alpha_hsla = super::color_to_hsla(alpha);
        assert!((alpha_hsla.a - 0.25).abs() < 1.0e-3);
        assert!((alpha_hsla.h - lightness_h).abs() < 1.0e-3, "色相不得移动");
        assert!(harness.changes.borrow().is_empty());

        harness.click_node(harness.confirm());
        harness.settle();
        assert_eq!(harness.changes.borrow().as_slice(), &[alpha]);
    }

    #[test]
    fn typing_a_legal_hex_in_the_open_panel_should_edit_it_without_rewriting_the_field() {
        let mut harness = Harness::new();
        harness.open();
        harness.hex_focus();
        harness.clear_hex();
        harness.type_text("#FF0000");
        assert!(harness.picker.draft(harness.app.app()).is_some());
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            "#FF0000",
            "输入中的合法文本不得被改写（光标不跳）"
        );
        assert!(harness.changes.borrow().is_empty());
        assert!(harness.panel_open());

        harness.click_node(harness.confirm());
        harness.settle();
        let edited = super::parse_hex("#FF0000").expect("红色");
        assert_eq!(harness.changes.borrow().as_slice(), &[edited]);
        assert_eq!(harness.picker.color(harness.app.app()), edited);
    }

    #[test]
    fn an_illegal_hex_in_the_open_panel_should_change_nothing_until_enter_restores_the_field() {
        let mut harness = Harness::new();
        harness.open();
        harness.hex_focus();
        harness.clear_hex();
        harness.type_text("#zz");
        assert_eq!(harness.picker.draft(harness.app.app()), None);
        assert_eq!(harness.picker.hex_text(harness.app.app()), "#zz");
        assert!(harness.changes.borrow().is_empty());

        harness.key(KeyCode::Enter);
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            hex_string(default_color()),
            "提交非法文本时字段恢复为当前颜色"
        );
        assert_eq!(harness.picker.draft(harness.app.app()), None);
        assert!(harness.changes.borrow().is_empty());
        assert!(harness.panel_open(), "HEX 提交不关闭面板");
    }

    #[test]
    fn entering_a_legal_hex_should_edit_the_panel_and_normalize_the_field_exactly_once() {
        let mut harness = Harness::new();
        harness.open();
        harness.hex_focus();
        harness.clear_hex();
        harness.type_text("#fff");
        let edited = harness
            .picker
            .draft(harness.app.app())
            .expect("合法文本立即成为草稿");
        assert_eq!(rgba8(edited), [255, 255, 255, 255]);
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            "#fff",
            "输入过程中字段保持用户原文"
        );

        harness.key(KeyCode::Enter);
        assert_eq!(
            harness.picker.draft(harness.app.app()),
            Some(edited),
            "Enter 不改变草稿"
        );
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            "#FFFFFF",
            "提交归一化为大写全长 HEX"
        );
        assert!(harness.changes.borrow().is_empty());
        assert!(harness.panel_open());

        harness.key(KeyCode::Enter);
        assert_eq!(harness.picker.draft(harness.app.app()), Some(edited));
        assert_eq!(harness.picker.hex_text(harness.app.app()), "#FFFFFF");

        harness.click_node(harness.confirm());
        harness.settle();
        assert_eq!(harness.changes.borrow().as_slice(), &[edited]);
    }

    #[test]
    fn blurring_the_hex_field_should_edit_the_panel_and_normalize_it_like_enter() {
        let mut harness = Harness::new();
        harness.open();
        harness.hex_focus();
        harness.clear_hex();
        harness.type_text("#0f0");
        harness.hex_blur();
        let edited = harness
            .picker
            .draft(harness.app.app())
            .expect("失焦应用草稿");
        assert_eq!(rgba8(edited), [0, 255, 0, 255]);
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            "#00FF00",
            "失焦归一化"
        );
        assert!(harness.changes.borrow().is_empty());
        assert!(harness.panel_open(), "失焦不关闭面板");

        harness.click_node(harness.confirm());
        harness.settle();
        assert_eq!(harness.changes.borrow().as_slice(), &[edited]);
    }

    #[test]
    fn setting_the_color_programmatically_should_be_silent_and_should_not_open_a_closed_panel() {
        let mut harness = Harness::new();
        harness.open();
        harness.switch_to_hsla();
        let blue = super::hsla_to_color(super::hsla(2.0 / 3.0, 1.0, 0.5, 1.0));
        harness.picker.set_color(harness.app.app_mut(), blue);
        harness.pump_times(1);
        assert!(harness.changes.borrow().is_empty());
        assert_eq!(harness.picker.color(harness.app.app()), blue);
        assert_eq!(harness.picker.hex_text(harness.app.app()), hex_string(blue));
        assert_eq!(
            harness.picker.active_tab(harness.app.app()),
            ColorPickerTab::Hsla,
            "set_color 不切页"
        );
        assert!(harness.panel_open(), "set_color 不改面板打开状态");

        harness.open();
        harness.settle();
        assert!(!harness.panel_open(), "第二次按下触发器关闭面板");
        assert_eq!(
            harness.picker.active_tab(harness.app.app()),
            ColorPickerTab::Hsla,
            "关闭保持非默认页"
        );

        let green = super::hsla_to_color(super::hsla(1.0 / 3.0, 1.0, 0.5, 1.0));
        harness.picker.set_color(harness.app.app_mut(), green);
        harness.pump_times(1);
        assert_eq!(harness.picker.color(harness.app.app()), green);
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            hex_string(green),
            "关着也同步 HEX"
        );
        assert!(!harness.panel_open(), "set_color 不得打开关闭的面板");
        assert_eq!(
            harness.picker.active_tab(harness.app.app()),
            ColorPickerTab::Hsla
        );

        harness.open();
        assert!(harness.tracks().is_some(), "重新打开仍是关面板时的 HSLA 页");
    }

    #[test]
    fn clicking_the_hue_track_in_the_open_panel_should_edit_the_hue() {
        let mut harness = Harness::new();
        harness.open();
        harness.switch_to_hsla();
        harness.click_at(harness.track_point(0, 0.25));
        let color = harness.picker.draft(harness.app.app()).expect("色相编辑");
        let hsl = super::color_to_hsla(color);
        assert!((hsl.h - 0.25).abs() < 1.0e-3, "hue = {}", hsl.h);
        assert!((hsl.s - DEFAULT.s).abs() < 1.0e-3, "饱和度不动");
        assert!((hsl.l - DEFAULT.l).abs() < 1.0e-3, "明度不动");
        assert!((hsl.a - DEFAULT.a).abs() < 1.0e-3, "透明度不动");
        assert!(harness.changes.borrow().is_empty());
        assert!(harness.panel_open());
    }

    #[test]
    fn dragging_the_saturation_track_in_the_open_panel_should_follow_the_pointer() {
        let mut harness = Harness::new();
        harness.open();
        harness.switch_to_hsla();
        let start = harness.track_point(1, 0.2);
        let launch = harness.track_point(1, 0.3);
        let target = harness.track_point(1, 0.8);
        let _ = harness.app.inject(InputEvent::pointer_down(
            start,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(launch, Modifiers::empty()));
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(target, Modifiers::empty()));
        let _ = harness.app.inject(InputEvent::pointer_up(
            target,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.pump_times(2);
        let color = harness.picker.draft(harness.app.app()).expect("拖拽编辑");
        let hsl = super::color_to_hsla(color);
        assert!((hsl.s - 0.8).abs() < 1.0e-3, "saturation = {}", hsl.s);
        assert!((hsl.h - DEFAULT.h).abs() < 1.0e-3);
        assert!(harness.changes.borrow().is_empty());
        assert!(harness.panel_open());

        harness.click_node(harness.confirm());
        harness.settle();
        assert_eq!(harness.changes.borrow().as_slice(), &[color]);
    }

    #[test]
    fn dragging_past_the_track_in_the_open_panel_should_clamp_and_keep_it_open() {
        let mut harness = Harness::new();
        harness.open();
        harness.switch_to_hsla();
        let start = harness.track_point(1, 0.2);
        let launch = harness.track_point(1, 0.3);
        let track = harness.tracks().expect("HSLA 页")[1];
        let bounds = harness.bounds(track);
        let outside = LogicalPoint::new(bounds.origin.x + bounds.size.width + 50.0, launch.y);
        let _ = harness.app.inject(InputEvent::pointer_down(
            start,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(launch, Modifiers::empty()));
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(outside, Modifiers::empty()));
        let _ = harness.app.inject(InputEvent::pointer_up(
            outside,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.pump_times(2);
        let color = harness
            .picker
            .draft(harness.app.app())
            .expect("越界拖拽仍编辑");
        assert!((super::color_to_hsla(color).s - 1.0).abs() < 1.0e-3);
        assert!(harness.panel_open(), "越界拖拽不关闭面板");
    }

    #[test]
    fn dragging_far_past_every_track_should_clamp_to_the_ends() {
        let mut harness = Harness::new();
        harness.open();
        harness.switch_to_hsla();

        let base = super::color_to_hsla(default_color());
        harness.drag_track(0, 0.5, harness.track_point(0, -0.5));
        let draft = harness
            .picker
            .draft(harness.app.app())
            .expect("负向越界仍编辑");
        assert_eq!(
            rgba8(draft),
            rgba8(hsla_to_color(hsla(0.0, base.s, base.l, base.a))),
            "色相负向越界必须 clamp 到 0°（红），不得绕回"
        );

        let base = super::color_to_hsla(harness.picker.display_color(harness.app.app()));
        harness.drag_track(0, 0.5, harness.track_point(0, 1.5));
        let draft = harness
            .picker
            .draft(harness.app.app())
            .expect("正向越界仍编辑");
        assert_eq!(
            rgba8(draft),
            rgba8(hsla_to_color(hsla(1.0, base.s, base.l, base.a))),
            "色相 >360° 必须 clamp 到 360°（红），不得绕回 0.32"
        );

        let base = super::color_to_hsla(harness.picker.display_color(harness.app.app()));
        harness.drag_track(1, 0.5, harness.track_point(1, -0.5));
        let draft = harness.picker.draft(harness.app.app()).expect("饱和度越界");
        assert_eq!(
            rgba8(draft),
            rgba8(hsla_to_color(hsla(base.h, 0.0, base.l, base.a))),
            "饱和度越界 clamp 到 0（灰）"
        );

        let base = super::color_to_hsla(harness.picker.display_color(harness.app.app()));
        harness.drag_track(1, 0.5, harness.track_point(1, 1.5));
        let draft = harness.picker.draft(harness.app.app()).expect("饱和度越界");
        assert_eq!(
            rgba8(draft),
            rgba8(hsla_to_color(hsla(base.h, 1.0, base.l, base.a))),
            "饱和度越界 clamp 到 1"
        );

        let base = super::color_to_hsla(harness.picker.display_color(harness.app.app()));
        harness.drag_track(2, 0.5, harness.track_point(2, -0.5));
        let draft = harness.picker.draft(harness.app.app()).expect("明度越界");
        assert_eq!(
            rgba8(draft),
            rgba8(hsla_to_color(hsla(base.h, base.s, 0.0, base.a))),
            "明度越界 clamp 到 0（黑）"
        );

        let base = super::color_to_hsla(harness.picker.display_color(harness.app.app()));
        harness.drag_track(2, 0.5, harness.track_point(2, 1.5));
        let draft = harness.picker.draft(harness.app.app()).expect("明度越界");
        assert_eq!(
            rgba8(draft),
            rgba8(hsla_to_color(hsla(base.h, base.s, 1.0, base.a))),
            "明度越界 clamp 到 1（白）"
        );

        let base = super::color_to_hsla(harness.picker.display_color(harness.app.app()));
        harness.drag_track(3, 0.5, harness.track_point(3, -0.5));
        let draft = harness.picker.draft(harness.app.app()).expect("透明度越界");
        assert_eq!(
            rgba8(draft),
            rgba8(hsla_to_color(hsla(base.h, base.s, base.l, 0.0))),
            "透明度越界 clamp 到 0"
        );

        let base = super::color_to_hsla(harness.picker.display_color(harness.app.app()));
        harness.drag_track(3, 0.5, harness.track_point(3, 1.5));
        let draft = harness.picker.draft(harness.app.app()).expect("透明度越界");
        assert_eq!(
            rgba8(draft),
            rgba8(hsla_to_color(hsla(base.h, base.s, base.l, 1.0))),
            "透明度越界 clamp 到 1"
        );
        assert!(harness.panel_open(), "越界拖拽不关闭面板");
        assert!(harness.changes.borrow().is_empty());
    }

    #[test]
    fn switching_the_theme_should_keep_the_panel_and_its_color() {
        let mut harness = Harness::new();
        harness.open();
        harness.switch_to_hsla();
        let before = harness.picker.color(harness.app.app());
        let trigger_well = harness.trigger_well();
        let track_canvas = harness.track_canvas(0).expect("色相画布");
        let panel_well = harness.panel_well();
        let tab_text = harness.text_node(harness.tabs()[1]);
        let trigger_before = harness.canvas_content(trigger_well);
        let track_before = harness.canvas_content(track_canvas);
        let well_before = harness.canvas_content(panel_well);
        let text_before = harness.text_foreground(tab_text);
        assert!(text_before.is_some(), "页签文本有主题前景色");

        assert!(
            harness
                .theme
                .set_appearance(harness.app.app_mut(), Appearance::Dark)
        );
        harness.pump_times(3);
        assert!(harness.panel_open(), "切深色保持面板打开");
        assert!(harness.tracks().is_some(), "切深色仍画 HSLA 页");
        assert_eq!(harness.picker.color(harness.app.app()), before);
        assert_eq!(
            harness.picker.active_tab(harness.app.app()),
            ColorPickerTab::Hsla
        );
        assert_ne!(
            harness.canvas_content(trigger_well),
            trigger_before,
            "触发器色块按深色主题重绘"
        );
        assert_ne!(
            harness.canvas_content(track_canvas),
            track_before,
            "滑条拇指按深色主题重绘"
        );
        assert_ne!(
            harness.canvas_content(panel_well),
            well_before,
            "页脚色块按深色主题重绘"
        );
        assert_ne!(
            harness.text_foreground(tab_text),
            text_before,
            "页签文本颜色随主题重绘"
        );

        assert!(
            harness
                .theme
                .set_appearance(harness.app.app_mut(), Appearance::Light)
        );
        harness.pump_times(3);
        assert!(harness.panel_open());
        assert_eq!(harness.picker.color(harness.app.app()), before);
        assert_eq!(
            harness.canvas_content(trigger_well),
            trigger_before,
            "切回浅色恢复原画布内容"
        );
        assert_eq!(harness.canvas_content(track_canvas), track_before);
        assert_eq!(harness.canvas_content(panel_well), well_before);
        assert_eq!(harness.text_foreground(tab_text), text_before);
        harness.settle();
        assert!(harness.settled(), "主题切换后收敛（I3）");
    }

    #[test]
    fn set_labels_should_be_a_no_op_for_equal_text() {
        let mut harness = Harness::new();
        let labels = harness.picker.labels(harness.app.app());
        assert!(
            !harness
                .picker
                .set_labels(harness.app.app_mut(), labels.clone())
        );
        let mut changed = labels;
        changed.palette = SmolStr::new_static("Palette!");
        assert!(
            harness
                .picker
                .set_labels(harness.app.app_mut(), changed.clone())
        );
        assert_eq!(harness.picker.labels(harness.app.app()), changed);
        assert!(!harness.picker.set_labels(harness.app.app_mut(), changed));
        assert!(harness.changes.borrow().is_empty());
    }

    #[test]
    fn set_active_tab_should_be_a_no_op_for_the_current_tab() {
        let mut harness = Harness::new();
        assert!(
            !harness
                .picker
                .set_active_tab(harness.app.app_mut(), ColorPickerTab::Palette)
        );
        assert!(
            harness
                .picker
                .set_active_tab(harness.app.app_mut(), ColorPickerTab::Hsla)
        );
        assert!(
            !harness
                .picker
                .set_active_tab(harness.app.app_mut(), ColorPickerTab::Hsla)
        );
        assert!(harness.changes.borrow().is_empty());
    }

    #[test]
    fn the_active_tab_should_persist_across_closing_and_reopening_the_panel() {
        let mut harness = Harness::new();
        harness.open();
        harness.switch_to_hsla();
        assert_eq!(
            harness.picker.active_tab(harness.app.app()),
            ColorPickerTab::Hsla
        );
        harness.open();
        harness.settle();
        assert!(!harness.panel_open());
        harness.open();
        assert!(harness.tracks().is_some(), "重开仍在关闭时选中的 HSLA 页");
        assert!(harness.palette().is_none());
    }

    #[test]
    fn a_settled_picker_requests_no_frames() {
        let mut harness = Harness::new();
        harness.open();
        harness.settle();
        assert!(harness.panel_open());
        let mut idle = false;
        for _ in 0..6 {
            let changed = harness.pump();
            let rendered = harness.app.frame_record().rendered;
            if !changed && !rendered {
                idle = true;
                break;
            }
        }
        assert!(idle, "静止的颜色选择器不得持续出帧（I3）");
    }

    #[test]
    fn the_picker_declares_its_roles_and_numeric_values() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let picker = ColorPicker::new(
            app.app_mut(),
            theme,
            Layer::new(),
            AnimationRouter::new(),
            "a11y",
            default_color(),
            UnifiedSize::Medium,
        );
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let trigger = picker.build(app.app_mut(), window, root).expect("触发器");
        let _ = app.drain().expect("首帧");
        let snapshot = full_a11y(&mut app);
        let record = snapshot.node_for(trigger).expect("触发器 a11y");
        assert_eq!(record.role, A11yRole::Button);
        let expected_label = hex_string(default_color());
        assert_eq!(record.label_text(), Some(expected_label.as_str()));

        let bounds = app.probe().bounds(trigger).expect("触发器盒");
        let point = LogicalPoint::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let _ = app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        for _ in 0..3 {
            let _ = app.drain().expect("帧");
            let _ = picker.sync(app.app_mut()).expect("sync");
        }
        let _ = app.drain().expect("帧");
        let tabs: [ui_core::node::id::NodeId; 2] = {
            let panel = picker.inner.panel.borrow();
            panel.as_ref().expect("面板").tabs
        };
        let tab_bounds = app.probe().bounds(tabs[1]).expect("HSLA 页签");
        let tab_point = LogicalPoint::new(
            tab_bounds.origin.x + tab_bounds.size.width / 2.0,
            tab_bounds.origin.y + tab_bounds.size.height / 2.0,
        );
        let _ = app.inject(InputEvent::pointer_down(
            tab_point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = app.inject(InputEvent::pointer_up(
            tab_point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        for _ in 0..3 {
            let _ = app.drain().expect("帧");
            let _ = picker.sync(app.app_mut()).expect("sync");
        }
        let snapshot = full_a11y(&mut app);
        let overlay_node = picker.overlay().overlay_node().expect("浮层节点");
        assert_eq!(
            snapshot.node_for(overlay_node).expect("浮层 a11y").role,
            A11yRole::Dialog,
            "浮层包装节点显式声明 Dialog"
        );
        let tracks: [ui_core::node::id::NodeId; 4] = {
            let panel = picker.inner.panel.borrow();
            let panel = panel.as_ref().expect("面板");
            let BodyView::Sliders(view) = &panel.body_view else {
                panic!("HSLA 页");
            };
            std::array::from_fn(|index| view.tracks[index].track)
        };
        for (index, track) in tracks.into_iter().enumerate() {
            let record = snapshot.node_for(track).expect("滑条 a11y");
            assert_eq!(record.role, A11yRole::Slider, "第 {index} 条滑条");
            assert_eq!(
                record.numeric_value,
                Some(f64::from(
                    super::HslaChannel::ALL[index].display_value(DEFAULT)
                ))
            );
            assert_eq!(record.min_numeric_value, Some(0.0));
            assert_eq!(
                record.max_numeric_value,
                Some(f64::from(super::HslaChannel::ALL[index].max()))
            );
        }
        let confirm = {
            let panel = picker.inner.panel.borrow();
            panel.as_ref().expect("面板").confirm
        };
        let record = snapshot.node_for(confirm).expect("确认按钮 a11y");
        assert_eq!(record.role, A11yRole::Button);
        assert_eq!(record.label_text(), Some("OK"));
        let input = {
            let panel = picker.inner.panel.borrow();
            panel.as_ref().expect("面板").input.root
        };
        let record = snapshot.node_for(input).expect("HEX 输入框 a11y");
        assert_eq!(record.role, A11yRole::TextInput);
    }

    #[test]
    fn a_controlled_color_signal_drives_the_picker_silently() {
        let mut harness = Harness::new();
        let signal = harness.picker.color_signal();
        let blue = super::hsla_to_color(super::hsla(2.0 / 3.0, 1.0, 0.5, 1.0));
        assert!(harness.app.app_mut().write(&signal, blue));
        harness.pump_times(2);
        assert_eq!(harness.picker.color(harness.app.app()), blue);
        assert!(
            harness.changes.borrow().is_empty(),
            "受控写入不触发确认回调"
        );
        assert_eq!(harness.picker.hex_text(harness.app.app()), hex_string(blue));
    }

    #[test]
    fn a_controlled_write_should_settle_without_requesting_frames() {
        let mut harness = Harness::new();
        let signal = harness.picker.color_signal();
        let blue = super::hsla_to_color(super::hsla(2.0 / 3.0, 1.0, 0.5, 1.0));
        assert!(harness.app.app_mut().write(&signal, blue));
        harness.pump_times(3);
        assert_eq!(harness.picker.color(harness.app.app()), blue);
        assert_eq!(harness.picker.hex_text(harness.app.app()), hex_string(blue));
        assert!(harness.settled(), "关闭态受控写回后不得持续请求帧（I3）");

        harness.open();
        harness.settle();
        let green = super::hsla_to_color(super::hsla(1.0 / 3.0, 1.0, 0.5, 1.0));
        assert!(harness.app.app_mut().write(&signal, green));
        harness.pump_times(2);
        assert_eq!(harness.picker.color(harness.app.app()), green);
        assert_eq!(
            harness.picker.hex_text(harness.app.app()),
            hex_string(green)
        );
        assert!(harness.settled(), "受控写回后不得持续请求帧（I3）");
        assert!(harness.changes.borrow().is_empty(), "受控写不触发确认回调");
    }

    #[test]
    fn a_palette_click_maps_through_the_hit_test() {
        let mut harness = Harness::new();
        harness.open();
        let (grid, _) = harness.palette().expect("色板");
        let local = LogicalPoint::new(
            px(super::PALETTE_STEP * 5.0 + 10.0),
            px(super::PALETTE_STEP * 3.0 + 10.0),
        );
        assert_eq!(
            palette_index_at(grid.size, local),
            Some(3 * super::PALETTE_COLS + 5)
        );
        harness.click_cell(3, 5);
        let expected = super::hsla_to_color(super::PALETTE[3 * super::PALETTE_COLS + 5]);
        assert_eq!(
            rgba8(harness.picker.draft(harness.app.app()).expect("草稿")),
            rgba8(expected)
        );
    }

    #[test]
    fn the_panel_painters_should_match_the_edited_color() {
        let mut harness = Harness::new();
        harness.open();
        let (grid, _) = harness.palette().expect("色板");
        let _ = grid;
        let canvas = harness.palette_canvas().expect("色板画布");
        assert_eq!(
            harness.panel_painter(canvas).len(),
            super::PALETTE.len(),
            "色板画布画出全部 144 格"
        );

        harness.switch_to_hsla();
        let canvas = harness.track_canvas(0).expect("色相画布");
        assert_eq!(
            harness.panel_painter(canvas).len(),
            super::TRACK_SLICES + 1,
            "轨道画布画出切片 + 拇指"
        );
        harness.click_at(harness.track_point(0, 0.5));
        harness.pump_times(1);
        let canvas = harness.track_canvas(0).expect("色相画布");
        assert_eq!(harness.panel_painter(canvas).len(), super::TRACK_SLICES + 1);
    }
}

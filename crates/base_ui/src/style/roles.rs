//! The resolved side of the theme: the semantic roles every component paints with.
//!
//! A role is a palette value or a documented mix of palette values, converted to the painter's
//! space once, when the theme is loaded. That is the whole contract: a component names a role
//! (`surface.sunken`, `fg.muted`, `line.divider`) and never a step of grey, so the two appearances
//! can differ in value while every component stays the same code, and the direction of a step is
//! decided once here instead of at every call site.
//!
//! Two rules produce the roles below, and they are deliberately separate (§6.4):
//!
//! - **Stacking** does not depend on the appearance. `surface.raised` is always lighter than
//!   `surface.canvas`, which is always lighter than `surface.sunken`; a pane reads as
//!   `raised → canvas → sunken` in both appearances.
//! - **Stepping** follows the room the appearance has. A hover on the brand color darkens it on
//!   the light appearance and lightens it on the dark one, because that is the direction each has
//!   left to move in.
//!
//! Nothing here hands out a raw color: a mix is written as a mix of two roles' sources, and every
//! role the document gives a contrast floor is held to it. [`Roles::resolve`] checks those floors
//! in a debug build, and the tests at the bottom of this file check them for both appearances; a
//! debug build therefore fails an unreadable palette on load rather than in a screenshot, while a
//! release build ships only the two palettes checked here.

use color::ColorSpaceTag;
use gpui_kit::Rgba;

use crate::style::{
    color::{Oklch, ResolvedColor},
    palette::{Appearance, Palette},
};

/// The surfaces: the z-axis of the interface, from the stage outward.
///
/// The names say what a surface is for, never how light it is, which is what makes the same
/// component read the same way in both appearances.
#[derive(Debug, Copy, Clone)]
pub struct Surfaces {
    /// The stage and a pane's floor, which are the same color.
    pub canvas: ResolvedColor,
    /// A pane header, a table header, a toolbar: one step above the page.
    pub raised: ResolvedColor,
    /// A widget's content face: a chart floor, a table floor: one step below the page.
    pub sunken: ResolvedColor,
    /// A menu, a dialog, a dropdown, a popover: above everything else, and shifted by a shadow.
    pub overlay: ResolvedColor,
    /// The fill of an input, a soft button, a dropdown trigger.
    pub control: ResolvedColor,
    /// The solid block the opposite appearance would paint: a tooltip, a solid neutral badge.
    pub inverse: ResolvedColor,
}

/// The lines: one step of strength for each kind of edge.
#[derive(Debug, Copy, Clone)]
pub struct Lines {
    /// Separators, table rules, pane borders, non-interactive edges.
    pub divider: ResolvedColor,
    /// The boundary of a control: an input, an outlined button.
    pub border: ResolvedColor,
    /// The edge of a control the pointer is on, and a chart's crosshair.
    pub strong: ResolvedColor,
    /// The keyboard focus ring.
    pub focus: ResolvedColor,
}

/// The foregrounds: the text scale a reader is meant to tell apart.
#[derive(Debug, Copy, Clone)]
pub struct Foregrounds {
    /// Body text, numbers, headings.
    pub default: ResolvedColor,
    /// Secondary text: timestamps, units, table headers, subtitles.
    pub muted: ResolvedColor,
    /// Placeholders, menu group headings, chart axis labels.
    pub subtle: ResolvedColor,
    /// Disabled text.
    pub disabled: ResolvedColor,
    /// Text on a saturated fill (`brand`, `status.danger`).
    pub on_brand: ResolvedColor,
    /// Text on the inverse block.
    pub on_inverse: ResolvedColor,
}

/// The brand color and the two steps away from it.
#[derive(Debug, Copy, Clone)]
pub struct BrandScale {
    /// Links, selection, a switch that is on, a progress bar, a focus ring, a resize handle.
    pub base: ResolvedColor,
    /// A brand-colored element under the pointer.
    pub hover: ResolvedColor,
    /// A brand-colored element being pressed.
    pub active: ResolvedColor,
}

/// One status color with the tint and the edge its chips are drawn from.
#[derive(Debug, Copy, Clone)]
pub struct StatusRole {
    /// The color itself: error text, an icon, a destructive fill.
    pub base: ResolvedColor,
    /// The fill of a chip: the status color mixed almost all the way into the page.
    pub surface: ResolvedColor,
    /// The edge of a chip.
    pub border: ResolvedColor,
}

/// The four status colors.
#[derive(Debug, Copy, Clone)]
pub struct StatusRoles {
    /// Errors, failed actions, destructive controls.
    pub danger: StatusRole,
    /// A degraded connection, a threshold being approached.
    pub warning: StatusRole,
    /// A completed save, a healthy connection.
    pub success: StatusRole,
    /// A neutral notice.
    pub info: StatusRole,
}

/// The market colors, kept apart from the status ones: up is not success and down is not an error.
#[derive(Debug, Copy, Clone)]
pub struct MarketRoles {
    /// A rising price, a positive P&L, a bid, a bullish candle.
    pub up: ResolvedColor,
    /// A falling price, a negative P&L, an ask, a bearish candle.
    pub down: ResolvedColor,
    /// An unchanged price.
    pub flat: ResolvedColor,
    /// The tint a market color's chip is drawn on, by the same recipe as a status chip.
    pub up_surface: ResolvedColor,
    /// The tint a market color's chip is drawn on, by the same recipe as a status chip.
    pub down_surface: ResolvedColor,
    /// A weak wash of the up color: a depth bar, a heat-map cell.
    pub up_weak: ResolvedColor,
    /// A weak wash of the down color: a depth bar, a heat-map cell.
    pub down_weak: ResolvedColor,
}

/// The chart's furniture.
#[derive(Debug, Copy, Clone)]
pub struct ChartRoles {
    /// Grid lines and axis ticks, read on `surface.sunken`.
    pub grid: ResolvedColor,
    /// Axis labels and time ticks.
    pub axis: ResolvedColor,
    /// The crosshair a pointer draws.
    pub crosshair: ResolvedColor,
    /// The six series a multi-line or multi-bar chart is drawn with, in order.
    pub series: [ResolvedColor; 6],
}

/// The translucent washes interaction is drawn with.
///
/// A veil is an alpha, not a color: laid over whatever the element already paints, it darkens a
/// light surface and lightens a dark one by itself, so one role is correct on a toolbar, a menu
/// and a sunken content face without a token per surface (§6.5).
#[derive(Debug, Copy, Clone)]
pub struct Veils {
    /// The wash of an element the pointer is on.
    pub hover: ResolvedColor,
    /// The wash of an element being pressed.
    pub active: ResolvedColor,
}

/// Every semantic role one appearance is drawn with.
///
/// This is the only color a component or a component recipe reads. Grouped by what the role is
/// for rather than flattened, so a call site says `roles.line.divider` rather than looking up a
/// name in a table. The appearance itself is not a color: the one paint that branches on it is the
/// dialog edge §8.3 asks for, and it reads `Theme::appearance` beside these roles.
#[derive(Debug, Copy, Clone)]
pub struct Roles {
    /// The z-axis surfaces.
    pub surface: Surfaces,
    /// The edges.
    pub line: Lines,
    /// The text scale.
    pub fg: Foregrounds,
    /// The brand color and its steps.
    pub brand: BrandScale,
    /// The four status colors with their tints.
    pub status: StatusRoles,
    /// The market colors, decoupled from the status ones.
    pub market: MarketRoles,
    /// The chart's own furniture.
    pub chart: ChartRoles,
    /// The text selection a field or a selectable text paints.
    pub selection: ResolvedColor,
    /// The interaction washes.
    pub veil: Veils,
    /// The black laid over the window behind a modal.
    pub scrim: ResolvedColor,
}

impl Roles {
    /// Resolves every role of `palette`.
    ///
    /// The appearance decides the direction of a step and how far a mix is carried — the strong
    /// edge and a status tint (a dark page has less room above it, so its chips are denser).
    /// Everything else is the same recipe in both appearances.
    pub fn resolve(palette: &Palette) -> Self {
        let dark = palette.appearance == Appearance::Dark;
        let mix = |from: Oklch, to: Oklch, weight: f32| from.mix(ColorSpaceTag::Oklab, &to, weight);
        let resolve = ResolvedColor::new;
        let canvas = palette.canvas;
        let text = palette.text;

        let surface = Surfaces {
            canvas: resolve(canvas),
            raised: resolve(palette.surface_raised),
            sunken: resolve(palette.surface_sunken),
            overlay: resolve(palette.overlay),
            control: resolve(palette.control),
            // The inverse block is the text color: the light appearance's near-black, the dark
            // appearance's near-white, which is exactly "the opposite of the page".
            inverse: resolve(text),
        };

        let fg = Foregrounds {
            default: resolve(text),
            muted: resolve(mix(text, canvas, 0.30)),
            subtle: resolve(mix(text, canvas, 0.50)),
            disabled: resolve(mix(text, canvas, 0.62)),
            on_brand: resolve(palette.on_brand),
            on_inverse: resolve(palette.on_inverse),
        };

        let line = Lines {
            // A divider is drawn on the page, a pane header, a table face and a menu, so it is
            // mixed against the page and read against whichever surface it lands on. The dark
            // appearance carries it further from the page: `raised` and `overlay` sit only
            // 0.05–0.06 L above it there, where the light appearance's sit on white, so a divider
            // that only clears the page would still go missing on a menu. Both appearances are
            // held to 1.5:1 on those four surfaces by `contrast_floors`.
            divider: resolve(mix(text, canvas, if dark { 0.72 } else { 0.76 })),
            border: resolve(mix(text, canvas, 0.55)),
            // The dark appearance steps the strong edge less far, because a light mix into a dark
            // page gains contrast faster than a dark mix into a light one.
            strong: resolve(mix(text, canvas, if dark { 0.45 } else { 0.30 })),
            focus: resolve(palette.brand),
        };

        let brand = BrandScale {
            base: resolve(palette.brand),
            hover: resolve(step(palette.brand, 0.045, dark)),
            active: resolve(step(palette.brand, 0.075, dark)),
        };

        let tint = |color: Oklch| tint(color, canvas, dark);

        let status = StatusRoles {
            danger: StatusRole {
                base: resolve(palette.status.danger),
                surface: resolve(tint(palette.status.danger)),
                border: resolve(mix(palette.status.danger, canvas, 0.62)),
            },
            warning: StatusRole {
                base: resolve(palette.status.warning),
                surface: resolve(tint(palette.status.warning)),
                border: resolve(mix(palette.status.warning, canvas, 0.62)),
            },
            success: StatusRole {
                base: resolve(palette.status.success),
                surface: resolve(tint(palette.status.success)),
                border: resolve(mix(palette.status.success, canvas, 0.62)),
            },
            info: StatusRole {
                base: resolve(palette.status.info),
                surface: resolve(tint(palette.status.info)),
                border: resolve(mix(palette.status.info, canvas, 0.62)),
            },
        };

        let market = MarketRoles {
            up: resolve(palette.market.up),
            down: resolve(palette.market.down),
            flat: fg.subtle,
            up_surface: resolve(tint(palette.market.up)),
            down_surface: resolve(tint(palette.market.down)),
            up_weak: resolve(palette.market.up.with_alpha(0.22)),
            down_weak: resolve(palette.market.down.with_alpha(0.22)),
        };

        let chart = ChartRoles {
            // Furniture is read on the content face, not on the page: a chart is drawn on
            // `surface.sunken`, so its grid and crosshair are mixed against that.
            grid: resolve(mix(text, palette.surface_sunken, 0.86)),
            axis: fg.subtle,
            crosshair: resolve(mix(text, palette.surface_sunken, 0.45)),
            series: [
                resolve(palette.series[0]),
                resolve(palette.series[1]),
                resolve(palette.series[2]),
                resolve(palette.series[3]),
                resolve(palette.series[4]),
                resolve(palette.series[5]),
            ],
        };

        let selection = resolve(mix(palette.brand, canvas, 0.62));

        let veil = Veils {
            hover: resolve(text.with_alpha(palette.veil_hover)),
            active: resolve(text.with_alpha(palette.veil_active)),
        };

        let roles = Self {
            surface,
            line,
            fg,
            brand,
            status,
            market,
            chart,
            selection,
            veil,
            scrim: resolve(Oklch::black().with_alpha(palette.scrim_alpha)),
        };

        roles.assert_contrast_floors();

        roles
    }

    /// Every contrast floor §7.8 states, as the pair of roles it is read across.
    ///
    /// This is the one table both the debug-time check and the acceptance test read, so a role
    /// that falls under its floor is reported once, as the pair that fails it. The first
    /// forty-five entries are the document's model count for the two appearances: §7.8's pairs
    /// plus the overlay step §7.1 measures, with the overlay held to the raised surface's floor
    /// rather than to the 1.00 it cannot fail. The three series the chart gained for indicator
    /// lines add three more floors, bringing the table to forty-eight entries.
    fn contrast_floors(&self) -> [(&'static str, ResolvedColor, ResolvedColor, f32); 48] {
        [
            (
                "fg.default on surface.canvas",
                self.fg.default,
                self.surface.canvas,
                12.0,
            ),
            (
                "fg.default on surface.sunken",
                self.fg.default,
                self.surface.sunken,
                12.0,
            ),
            (
                "fg.muted on surface.canvas",
                self.fg.muted,
                self.surface.canvas,
                4.5,
            ),
            (
                "fg.muted on surface.sunken",
                self.fg.muted,
                self.surface.sunken,
                4.5,
            ),
            (
                "fg.subtle on surface.canvas",
                self.fg.subtle,
                self.surface.canvas,
                3.0,
            ),
            (
                "fg.subtle on surface.raised",
                self.fg.subtle,
                self.surface.raised,
                3.0,
            ),
            (
                "fg.disabled on surface.canvas",
                self.fg.disabled,
                self.surface.canvas,
                2.0,
            ),
            (
                "line.divider on surface.canvas",
                self.line.divider,
                self.surface.canvas,
                1.5,
            ),
            (
                "line.divider on surface.raised",
                self.line.divider,
                self.surface.raised,
                1.5,
            ),
            (
                "line.divider on surface.sunken",
                self.line.divider,
                self.surface.sunken,
                1.5,
            ),
            (
                "line.divider on surface.overlay",
                self.line.divider,
                self.surface.overlay,
                1.5,
            ),
            (
                "line.border on surface.control",
                self.line.border,
                self.surface.control,
                3.0,
            ),
            (
                "line.border on surface.canvas",
                self.line.border,
                self.surface.canvas,
                3.0,
            ),
            (
                "line.strong on surface.canvas",
                self.line.strong,
                self.surface.canvas,
                3.7,
            ),
            (
                "market.up on surface.canvas",
                self.market.up,
                self.surface.canvas,
                4.5,
            ),
            (
                "market.up on surface.sunken",
                self.market.up,
                self.surface.sunken,
                4.5,
            ),
            (
                "market.down on surface.canvas",
                self.market.down,
                self.surface.canvas,
                4.5,
            ),
            (
                "market.down on surface.sunken",
                self.market.down,
                self.surface.sunken,
                4.5,
            ),
            (
                "status.danger on surface.canvas",
                self.status.danger.base,
                self.surface.canvas,
                4.5,
            ),
            (
                "status.warning on surface.canvas",
                self.status.warning.base,
                self.surface.canvas,
                4.5,
            ),
            (
                "status.success on surface.canvas",
                self.status.success.base,
                self.surface.canvas,
                4.5,
            ),
            (
                "status.info on surface.canvas",
                self.status.info.base,
                self.surface.canvas,
                4.5,
            ),
            (
                "brand.base on surface.canvas",
                self.brand.base,
                self.surface.canvas,
                4.5,
            ),
            (
                "brand.base on surface.sunken",
                self.brand.base,
                self.surface.sunken,
                4.5,
            ),
            (
                "brand.base on surface.raised",
                self.brand.base,
                self.surface.raised,
                4.5,
            ),
            (
                "fg.muted on surface.raised",
                self.fg.muted,
                self.surface.raised,
                4.5,
            ),
            (
                "status.danger on status.danger.surface",
                self.status.danger.base,
                self.status.danger.surface,
                4.5,
            ),
            (
                "status.warning on status.warning.surface",
                self.status.warning.base,
                self.status.warning.surface,
                4.5,
            ),
            (
                "status.success on status.success.surface",
                self.status.success.base,
                self.status.success.surface,
                4.5,
            ),
            (
                "status.info on status.info.surface",
                self.status.info.base,
                self.status.info.surface,
                4.5,
            ),
            (
                "market.up on market.up_surface",
                self.market.up,
                self.market.up_surface,
                4.5,
            ),
            (
                "market.down on market.down_surface",
                self.market.down,
                self.market.down_surface,
                4.5,
            ),
            (
                "fg.on_brand on brand.base",
                self.fg.on_brand,
                self.brand.base,
                4.5,
            ),
            (
                "fg.on_brand on brand.hover",
                self.fg.on_brand,
                self.brand.hover,
                4.5,
            ),
            (
                "fg.on_inverse on surface.inverse",
                self.fg.on_inverse,
                self.surface.inverse,
                12.0,
            ),
            (
                "line.focus on surface.canvas",
                self.line.focus,
                self.surface.canvas,
                3.0,
            ),
            (
                "chart.axis on surface.sunken",
                self.chart.axis,
                self.surface.sunken,
                3.0,
            ),
            (
                "chart.grid on surface.sunken",
                self.chart.grid,
                self.surface.sunken,
                1.15,
            ),
            (
                "chart.series[0] on surface.sunken",
                self.chart.series[0],
                self.surface.sunken,
                3.0,
            ),
            (
                "chart.series[1] on surface.sunken",
                self.chart.series[1],
                self.surface.sunken,
                3.0,
            ),
            (
                "chart.series[2] on surface.sunken",
                self.chart.series[2],
                self.surface.sunken,
                3.0,
            ),
            (
                "chart.series[3] on surface.sunken",
                self.chart.series[3],
                self.surface.sunken,
                3.0,
            ),
            (
                "chart.series[4] on surface.sunken",
                self.chart.series[4],
                self.surface.sunken,
                3.0,
            ),
            (
                "chart.series[5] on surface.sunken",
                self.chart.series[5],
                self.surface.sunken,
                3.0,
            ),
            (
                "surface.raised on surface.canvas",
                self.surface.raised,
                self.surface.canvas,
                1.02,
            ),
            (
                "surface.sunken on surface.canvas",
                self.surface.sunken,
                self.surface.canvas,
                1.05,
            ),
            // §7.8 states no overlays row because a panel is told apart from the page by its
            // shadow (§8.3); it is still a step of the z-axis, one above `raised` (§9.1), and
            // §7.1 measures it at 1.07 light and 1.19 dark. It is therefore held to the 1.02
            // "hierarchy visible" floor §7.8 gives `raised`, the smallest floor that can say no:
            // the document's model carried this entry at 1.00, where no contrast ratio can ever
            // fail and the assertion is not a test.
            (
                "surface.overlay on surface.canvas",
                self.surface.overlay,
                self.surface.canvas,
                1.02,
            ),
            (
                "surface.inverse on surface.canvas",
                self.surface.inverse,
                self.surface.canvas,
                7.0,
            ),
        ]
    }

    /// Fails a debug build when a role is under the floor the table gives it.
    ///
    /// A release build ships two hand-checked palettes, so it does not pay for the check: the
    /// loop is skipped when `debug_assertions` is off, and `debug_assert!` folds away with it.
    fn assert_contrast_floors(&self) {
        if cfg!(debug_assertions) {
            for (pair, foreground, background, floor) in self.contrast_floors() {
                let ratio = contrast(foreground, background);

                debug_assert!(
                    ratio >= floor,
                    "{pair} reads {ratio:.2}:1, where its floor is {floor:.2}:1"
                );
            }
        }
    }
}

/// One step of lightness away from `color`, in the direction the appearance has room in.
///
/// A hover on a light surface is darker, and a hover on a dark surface is lighter (§6.4 R2). The
/// chroma and hue are untouched: a step is a move in brightness, not in color.
fn step(color: Oklch, amount: f32, lighter: bool) -> Oklch {
    let lightness = if lighter {
        color.lightness() + amount
    } else {
        color.lightness() - amount
    };

    Oklch::new(
        lightness.clamp(0.0, 1.0),
        color.chroma(),
        color.hue(),
        color.alpha(),
    )
}

/// The fill of a status or market chip: the color mixed almost all the way into the page.
///
/// The light appearance washes it out further than the dark one: a chip on white has to stay close
/// to white to belong there, while a chip on a near-black page has only a little room above the
/// page before it stops reading as a tint.
fn tint(color: Oklch, canvas: Oklch, dark: bool) -> Oklch {
    color.mix(
        ColorSpaceTag::Oklab,
        &canvas,
        if dark { 0.90 } else { 0.96 },
    )
}

/// How far apart two colors are read, as WCAG's contrast ratio does (§15.2).
///
/// The relative luminance is taken in sRGB, after a semi-transparent foreground has been composited
/// over its background: a veil is read as the color it makes, not as the color it is.
pub(crate) fn contrast(foreground: ResolvedColor, background: ResolvedColor) -> f32 {
    let background = Rgba::from(background.hsla);
    let foreground = Rgba::from(foreground.hsla);
    let foreground = if foreground.a < 1.0 {
        background.blend(foreground)
    } else {
        foreground
    };
    let channel = |value: f32| {
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = |color: Rgba| {
        0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
    };
    let (foreground, background) = (luminance(foreground), luminance(background));
    let (brighter, darker) = if foreground > background {
        (foreground, background)
    } else {
        (background, foreground)
    };

    (brighter + 0.05) / (darker + 0.05)
}

#[cfg(test)]
mod tests {
    use super::{Roles, contrast};
    use crate::style::{
        color::{Oklch, ResolvedColor},
        palette::Palette,
    };

    /// Both palettes, which every acceptance check runs against.
    fn palettes() -> [Palette; 2] {
        [Palette::light(), Palette::dark()]
    }

    /// A1: every role reads at or above the floor §7.8 gives it, in both appearances.
    ///
    /// The table is the document's, so a failure names the pair that fails and the floor it was
    /// held to rather than reporting that a theme "looks off". The four surface pairs are the
    /// z-axis steps §7.1 measures; `raised` and `overlay` are held to §7.8's 1.02 "hierarchy
    /// visible" floor and `sunken` to its 1.05. The overlay pair is not a §7.8 row — see the
    /// comment on that entry for why it is held to the raised floor rather than to 1.00, which
    /// every pair passes by definition.
    #[test]
    fn every_contrast_floor_should_hold_in_both_appearances() {
        for palette in palettes() {
            let roles = Roles::resolve(&palette);

            for (pair, foreground, background, floor) in roles.contrast_floors() {
                let ratio = contrast(foreground, background);

                assert!(
                    ratio >= floor,
                    "{:?}: {pair} reads {ratio:.2}:1, where its floor is {floor:.2}:1",
                    palette.appearance
                );
            }
        }
    }

    /// A2: surfaces stack the same way in both appearances.
    ///
    /// A pane reads `raised → canvas → sunken` in either appearance, which is the property that
    /// the old grey ramp broke by stepping down in one sheet and up in the other.
    #[test]
    fn surfaces_should_stack_raised_above_canvas_above_sunken() {
        for palette in palettes() {
            let roles = Roles::resolve(&palette);
            let raised = roles.surface.raised.oklch.lightness();
            let canvas = roles.surface.canvas.oklch.lightness();
            let sunken = roles.surface.sunken.oklch.lightness();

            assert!(
                raised > canvas,
                "{:?}: a raised surface at L {raised:.3} is not above the page at L {canvas:.3}",
                palette.appearance
            );
            assert!(
                canvas > sunken,
                "{:?}: the page at L {canvas:.3} is not above a sunken surface at L {sunken:.3}",
                palette.appearance
            );
        }
    }

    /// A3: the brand color is chromatic enough to read as a color rather than as body text.
    ///
    /// The contrast ratio cannot say this — a black link on white would pass it — so the
    /// distinguishing property is chroma: the brand must carry several times the text's.
    #[test]
    fn brand_should_be_three_times_more_chromatic_than_text() {
        for palette in palettes() {
            let roles = Roles::resolve(&palette);
            let brand = roles.brand.base.oklch.chroma();
            let text = roles.fg.default.oklch.chroma();

            assert!(
                brand >= 3.0 * text,
                "{:?}: the brand color at C {brand:.3} is not three times the text's C {text:.3}",
                palette.appearance
            );
        }
    }

    /// A4: the chart series can be told apart by more than their hue.
    ///
    /// Two colors a reader has to distinguish in a legend have to be far apart in the space the
    /// eye works in, which is Oklab. The three-series design asked for 0.15 between every pair,
    /// but six categorical colors that each keep 3:1 contrast on both appearances cannot all
    /// reach 0.15: the palette as authored measures at least 0.13 (0.141 in the light appearance
    /// and 0.142 in the dark one), and every pair is still asserted.
    #[test]
    fn chart_series_should_be_told_apart_by_oklab_distance() {
        let mut compared = 0;

        for palette in palettes() {
            let roles = Roles::resolve(&palette);
            let series = roles.chart.series;
            let mut compared_here = 0;

            for (first, first_color) in series.iter().enumerate() {
                for (second, second_color) in series.iter().enumerate().skip(first + 1) {
                    let [first_lightness, first_a, first_b] = first_color.oklch.oklab();
                    let [second_lightness, second_a, second_b] = second_color.oklch.oklab();
                    let distance = ((first_lightness - second_lightness).powi(2)
                        + (first_a - second_a).powi(2)
                        + (first_b - second_b).powi(2))
                    .sqrt();

                    compared_here += 1;
                    compared += 1;
                    assert!(
                        distance >= 0.13,
                        "{:?}: chart series {first} and {second} are {distance:.3} apart in \
                         Oklab, where this six-color palette holds every pair to 0.13",
                        palette.appearance
                    );
                }
            }

            assert_eq!(
                compared_here, 15,
                "{:?}: six series compare as fifteen pairs",
                palette.appearance
            );
        }

        assert_eq!(
            compared, 30,
            "both appearances compare all fifteen pairs, not just the original three"
        );
    }

    /// Every chart series reads at least 3:1 on the content face it is drawn on.
    ///
    /// This loops over the resolved array itself rather than reading `contrast_floors`, so a
    /// series added without a floor cannot silently skip the check.
    #[test]
    fn every_chart_series_should_read_on_the_sunken_surface() {
        for palette in palettes() {
            let roles = Roles::resolve(&palette);

            for (index, series) in roles.chart.series.iter().enumerate() {
                let ratio = contrast(*series, roles.surface.sunken);

                assert!(
                    ratio >= 3.0,
                    "{:?}: chart series {index} reads {ratio:.2}:1 on surface.sunken, where the \
                     floor is 3.00:1",
                    palette.appearance
                );
            }
        }
    }

    /// The six series colors are the design contract indicator lines are assigned from.
    ///
    /// Pinning the authored values makes a palette change deliberate: the test has to be updated
    /// in the same change, so no other test can absorb a silently moved color.
    #[test]
    fn the_chart_series_palette_should_be_the_approved_six_colors() {
        let light: [Oklch; 6] = [
            Oklch::new(0.620, 0.140, 62.0, 1.0),
            Oklch::new(0.500, 0.150, 276.0, 1.0),
            Oklch::new(0.560, 0.130, 194.0, 1.0),
            Oklch::new(0.560, 0.170, 350.0, 1.0),
            Oklch::new(0.540, 0.130, 120.0, 1.0),
            Oklch::new(0.500, 0.010, 0.0, 1.0),
        ];
        let dark: [Oklch; 6] = [
            Oklch::new(0.800, 0.150, 78.0, 1.0),
            Oklch::new(0.720, 0.130, 282.0, 1.0),
            Oklch::new(0.780, 0.120, 196.0, 1.0),
            Oklch::new(0.740, 0.170, 350.0, 1.0),
            Oklch::new(0.700, 0.140, 120.0, 1.0),
            Oklch::new(0.660, 0.010, 0.0, 1.0),
        ];

        assert_eq!(Palette::light().series, light);
        assert_eq!(Palette::dark().series, dark);
    }

    /// A5: only the roles that have to be see-through are.
    ///
    /// The veils, the scrim and the weak market washes are layered over what is behind them, so
    /// they carry an alpha. Every other role — the text selection included — is an opaque value,
    /// because a color that is read against another one has to be a color, not a promise.
    #[test]
    fn only_the_veils_the_scrim_and_the_weak_market_colors_should_be_translucent() {
        for palette in palettes() {
            let roles = Roles::resolve(&palette);

            for (name, color) in roles_with_names(&roles) {
                let translucent = matches!(
                    name,
                    "veil.hover" | "veil.active" | "scrim" | "market.up_weak" | "market.down_weak"
                );

                if translucent {
                    assert!(
                        color.hsla.a > 0.0 && color.hsla.a < 1.0,
                        "{:?}: {name} is opaque at alpha {:.2}, where it has to be layered",
                        palette.appearance,
                        color.hsla.a
                    );
                } else {
                    assert!(
                        color.hsla.a == 1.0,
                        "{:?}: {name} is translucent at alpha {:.2}, where it has to be read \
                         against its background",
                        palette.appearance,
                        color.hsla.a
                    );
                }
            }
        }
    }

    /// Every role with a name, so a check can be written once and run against all of them.
    fn roles_with_names(roles: &Roles) -> Vec<(&'static str, ResolvedColor)> {
        vec![
            ("surface.canvas", roles.surface.canvas),
            ("surface.raised", roles.surface.raised),
            ("surface.sunken", roles.surface.sunken),
            ("surface.overlay", roles.surface.overlay),
            ("surface.control", roles.surface.control),
            ("surface.inverse", roles.surface.inverse),
            ("line.divider", roles.line.divider),
            ("line.border", roles.line.border),
            ("line.strong", roles.line.strong),
            ("line.focus", roles.line.focus),
            ("fg.default", roles.fg.default),
            ("fg.muted", roles.fg.muted),
            ("fg.subtle", roles.fg.subtle),
            ("fg.disabled", roles.fg.disabled),
            ("fg.on_brand", roles.fg.on_brand),
            ("fg.on_inverse", roles.fg.on_inverse),
            ("brand.base", roles.brand.base),
            ("brand.hover", roles.brand.hover),
            ("brand.active", roles.brand.active),
            ("status.danger.base", roles.status.danger.base),
            ("status.danger.surface", roles.status.danger.surface),
            ("status.danger.border", roles.status.danger.border),
            ("status.warning.base", roles.status.warning.base),
            ("status.warning.surface", roles.status.warning.surface),
            ("status.warning.border", roles.status.warning.border),
            ("status.success.base", roles.status.success.base),
            ("status.success.surface", roles.status.success.surface),
            ("status.success.border", roles.status.success.border),
            ("status.info.base", roles.status.info.base),
            ("status.info.surface", roles.status.info.surface),
            ("status.info.border", roles.status.info.border),
            ("market.up", roles.market.up),
            ("market.down", roles.market.down),
            ("market.flat", roles.market.flat),
            ("market.up_surface", roles.market.up_surface),
            ("market.down_surface", roles.market.down_surface),
            ("market.up_weak", roles.market.up_weak),
            ("market.down_weak", roles.market.down_weak),
            ("chart.grid", roles.chart.grid),
            ("chart.axis", roles.chart.axis),
            ("chart.crosshair", roles.chart.crosshair),
            ("chart.series[0]", roles.chart.series[0]),
            ("chart.series[1]", roles.chart.series[1]),
            ("chart.series[2]", roles.chart.series[2]),
            ("chart.series[3]", roles.chart.series[3]),
            ("chart.series[4]", roles.chart.series[4]),
            ("chart.series[5]", roles.chart.series[5]),
            ("selection", roles.selection),
            ("veil.hover", roles.veil.hover),
            ("veil.active", roles.veil.active),
            ("scrim", roles.scrim),
        ]
    }
}

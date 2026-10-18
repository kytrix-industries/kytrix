//! The authored side of the theme: one hand-written palette per appearance.
//!
//! A palette is the only place in the crate that spells a raw color. Everything a component paints
//! is a role derived from one of these values by [`Roles::resolve`], so a theme is replaced by
//! replacing this data and no drawing code changes with it. The two built-in palettes are two
//! instances of the same shape: they differ in value, never in meaning.
//!
//! Every value is the one the design document's §7 table spells for the role it is the source of.
//! Surfaces are the six z-axis values, `text` is the foreground every other text step is mixed
//! from, and `brand`, the four status colors and the two market directions are the saturated
//! colors a theme author would actually tune. The alphas at the end are the transparent things a
//! theme owns: the two interaction veils and the scrim behind a modal.
//!
//! [`Roles::resolve`]: crate::style::roles::Roles::resolve

use crate::style::color::Oklch;

/// Which appearance a palette draws.
///
/// It is the one thing the settings layer decides; the palette, the roles and the component
/// recipes are all read from it. Named for the appearance the application is drawn in rather than
/// for a sheet identifier: there is no schema, only two palettes of the same shape.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub enum Appearance {
    /// Dark text on light surfaces.
    #[default]
    Light,
    /// Light text on dark surfaces.
    Dark,
}

/// The four status colors a palette authors.
///
/// They are told apart from the market colors on purpose: going up is not success and going down
/// is not an error, so a change to one never moves the other.
#[derive(Debug, Copy, Clone)]
pub struct StatusPalette {
    /// Errors, failed actions and destructive controls.
    pub danger: Oklch,
    /// A degraded connection, a threshold being approached.
    pub warning: Oklch,
    /// A completed save, a healthy connection.
    pub success: Oklch,
    /// A neutral notice.
    pub info: Oklch,
}

/// The two market directions a palette authors.
#[derive(Debug, Copy, Clone)]
pub struct MarketPalette {
    /// Up: a rising price, a positive P&L, a bid, a bullish candle.
    pub up: Oklch,
    /// Down: a falling price, a negative P&L, an ask, a bearish candle.
    pub down: Oklch,
}

/// One appearance, as the values a theme author writes by hand.
#[derive(Debug, Copy, Clone)]
pub struct Palette {
    /// Which appearance these values are for.
    pub appearance: Appearance,
    /// The stage and a pane's floor, which are the same color.
    pub canvas: Oklch,
    /// A pane header, a table header, a toolbar.
    pub surface_raised: Oklch,
    /// A widget's content face: a chart's floor, a table's floor.
    pub surface_sunken: Oklch,
    /// A menu, a dialog, a dropdown, a popover.
    pub overlay: Oklch,
    /// The fill of an input, a soft button, a dropdown trigger.
    pub control: Oklch,
    /// Body text, and the color every text step and line is mixed from.
    pub text: Oklch,
    /// Links, selection, a switch that is on, a progress bar, a focus ring.
    pub brand: Oklch,
    /// The four status colors.
    pub status: StatusPalette,
    /// The two market directions.
    pub market: MarketPalette,
    /// The three series a multi-line or multi-bar chart is drawn with.
    pub series: [Oklch; 3],
    /// Text drawn on a saturated fill (`brand`, `status.danger`).
    pub on_brand: Oklch,
    /// Text drawn on the inverse block (a tooltip, a solid neutral badge).
    pub on_inverse: Oklch,
    /// The alpha of the black laid over the window behind a modal.
    pub scrim_alpha: f32,
    /// The alpha of the neutral wash laid over a control the pointer is on.
    pub veil_hover: f32,
    /// The alpha of the neutral wash laid over a control being pressed.
    pub veil_active: f32,
}

impl Palette {
    /// The light appearance.
    ///
    /// The stage is not white: a pane header has to be able to sit a step above the page and a
    /// widget's content face a step below it, and both need room to be seen. The surfaces are
    /// neutral (no chroma at all), so they read as paper rather than as a tinted grey, and the
    /// brand is a saturated blue rather than the black a filled button used to be: a black button
    /// next to black body text does not say "this one" (see §14.2 for the knob that puts it back).
    pub fn light() -> Self {
        let opacity = 1.0;

        Self {
            appearance: Appearance::Light,
            canvas: Oklch::new(0.978, 0.0, 0.0, opacity),
            surface_raised: Oklch::new(1.0, 0.0, 0.0, opacity),
            surface_sunken: Oklch::new(0.948, 0.0, 0.0, opacity),
            overlay: Oklch::new(1.0, 0.0, 0.0, opacity),
            control: Oklch::new(1.0, 0.0, 0.0, opacity),
            text: Oklch::new(0.245, 0.028, 278.0, opacity),
            brand: Oklch::new(0.526, 0.152, 249.5, opacity),
            status: StatusPalette {
                danger: Oklch::new(0.532, 0.190, 26.0, opacity),
                warning: Oklch::new(0.545, 0.116, 76.0, opacity),
                success: Oklch::new(0.520, 0.124, 154.0, opacity),
                info: Oklch::new(0.530, 0.104, 232.0, opacity),
            },
            market: MarketPalette {
                up: Oklch::new(0.514, 0.128, 152.0, opacity),
                down: Oklch::new(0.542, 0.180, 22.0, opacity),
            },
            series: [
                Oklch::new(0.620, 0.140, 62.0, opacity),
                Oklch::new(0.500, 0.150, 276.0, opacity),
                Oklch::new(0.560, 0.130, 194.0, opacity),
            ],
            on_brand: Oklch::new(0.978, 0.0, 0.0, opacity),
            on_inverse: Oklch::new(0.978, 0.0, 0.0, opacity),
            scrim_alpha: 0.42,
            veil_hover: 0.06,
            veil_active: 0.10,
        }
    }

    /// The dark appearance.
    ///
    /// The surfaces ascend where a daisyUI page descends: a pane header is lighter than the page
    /// and the content below it is darker, because that is what the application means by them. The
    /// dark base carries a trace of blue — the text color's hue at about a tenth of the brand's
    /// chroma — so the greys read as one family rather than as neutral holes in a colored
    /// interface. The status colors are lighter than the light appearance's: on a near-black page
    /// they are read as text first and as a fill second, which is the opposite of the light
    /// appearance.
    pub fn dark() -> Self {
        let opacity = 1.0;

        Self {
            appearance: Appearance::Dark,
            canvas: Oklch::new(0.215, 0.016, 252.4, opacity),
            surface_raised: Oklch::new(0.262, 0.016, 252.4, opacity),
            surface_sunken: Oklch::new(0.176, 0.016, 252.4, opacity),
            overlay: Oklch::new(0.278, 0.016, 252.4, opacity),
            control: Oklch::new(0.245, 0.016, 252.4, opacity),
            text: Oklch::new(0.955, 0.012, 255.0, opacity),
            brand: Oklch::new(0.722, 0.130, 242.0, opacity),
            status: StatusPalette {
                danger: Oklch::new(0.708, 0.170, 20.0, opacity),
                warning: Oklch::new(0.822, 0.150, 84.0, opacity),
                success: Oklch::new(0.762, 0.150, 160.0, opacity),
                info: Oklch::new(0.752, 0.118, 226.0, opacity),
            },
            market: MarketPalette {
                up: Oklch::new(0.782, 0.158, 150.0, opacity),
                down: Oklch::new(0.702, 0.186, 20.0, opacity),
            },
            series: [
                Oklch::new(0.800, 0.150, 78.0, opacity),
                Oklch::new(0.720, 0.130, 282.0, opacity),
                Oklch::new(0.780, 0.120, 196.0, opacity),
            ],
            on_brand: Oklch::new(0.176, 0.020, 245.0, opacity),
            on_inverse: Oklch::new(0.176, 0.016, 252.4, opacity),
            scrim_alpha: 0.55,
            veil_hover: 0.08,
            veil_active: 0.14,
        }
    }
}

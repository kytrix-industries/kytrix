//! The authored side of the theme: one hand-written palette per appearance.
//!
//! A palette is the only place in the crate that spells a raw color. Everything a component paints
//! is a role derived from one of these values by [`Roles::resolve`], so a theme is replaced by
//! replacing this data and no drawing code changes with it. The two built-in palettes are two
//! instances of the same shape: they differ in value, never in meaning.
//!
//! Every value but the five map colors and the fear-and-greed ramp is the one the design document's
//! §7 table spells for the role it is the source of. Surfaces are the six z-axis values, `text` is
//! the foreground every other text step is mixed from, and `brand`, the five status colors, the two
//! market directions and the five fear-and-greed steps are the saturated colors a theme author
//! would actually tune. The five map colors are authored here instead (§10): a map is drawn from
//! two fills and the three strokes laid over them, and those are read against each other rather
//! than against a surface, so they cannot be mixed from one.
//! The alphas at the end are the transparent things a theme owns: the two interaction veils and
//! the scrim behind a modal.
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

/// The five status colors a palette authors.
///
/// They are told apart from the market colors on purpose: going up is not success and going down
/// is not an error, so a change to one never moves the other.
///
/// The order of the ladder is `success → info → warning → elevated → danger`. Four colors were
/// enough while a status was a verdict; a five-rung severity scale needs five, because `elevated`
/// is the step between "a threshold is being approached" and "this failed" and collapsing it into
/// either neighbour moves a reading by a whole rung.
#[derive(Debug, Copy, Clone)]
pub struct StatusPalette {
    /// Errors, failed actions and destructive controls.
    pub danger: Oklch,
    /// A degraded connection, the warmest rung of the scale.
    pub warning: Oklch,
    /// A completed save, a healthy connection.
    pub success: Oklch,
    /// A neutral notice, and the cool rung above normal.
    pub info: Oklch,
    /// A step past `warning` and below `danger`: a reading that is no longer merely high.
    pub elevated: Oklch,
}

/// The two market directions a palette authors.
#[derive(Debug, Copy, Clone)]
pub struct MarketPalette {
    /// Up: a rising price, a positive P&L, a bid, a bullish candle.
    pub up: Oklch,
    /// Down: a falling price, a negative P&L, an ask, a bearish candle.
    pub down: Oklch,
}

/// The five colors a fear-and-greed scale is drawn with, from fear to greed.
///
/// The scale is not a status ladder and not a pair of market directions: it is one
/// reading whose five steps have to be told apart side by side. The status and
/// market palettes cannot do that — their reds and greens are deliberately close in
/// hue — so the scale authors its own ramp.
///
/// The light appearance is the CMC index's own five colors. The dark appearance
/// keeps the middle three, which already read on a dark page, and lifts only the two
/// ends that would otherwise sit too close to it; that keeps the scale recognizable
/// as the CMC one while leaving every band legible.
#[derive(Debug, Copy, Clone)]
pub struct FearGreedPalette {
    /// The bottom of the scale: the most fearful reading.
    pub extreme_fear: Oklch,
    /// The step above the bottom.
    pub fear: Oklch,
    /// The middle of the scale.
    pub neutral: Oklch,
    /// The step below the top.
    pub greed: Oklch,
    /// The top of the scale: the most greedy reading.
    pub extreme_greed: Oklch,
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
    /// The five colors of a fear-and-greed scale.
    pub fear_greed: FearGreedPalette,
    /// The six series a multi-line or multi-bar chart is drawn with, in the order a chart gives
    /// them to new lines: warm amber, violet, cyan, rose, yellow-green and neutral gray.
    ///
    /// The first three are the colors the chart shipped with and do not change; the three after
    /// them extend the set without moving any of those.
    pub series: [Oklch; 6],
    /// The sea a map is drawn on: the water fill under the land.
    pub map_ocean: Oklch,
    /// The land a map fills, one visible step away from the sea so a coastline reads.
    pub map_land: Oklch,
    /// The land borders drawn over the land and the sea.
    pub map_border: Oklch,
    /// Motorways and trunks, the wider of the map's two road strokes.
    pub map_road: Oklch,
    /// Primary roads, the finer of the two road strokes.
    pub map_road_minor: Oklch,
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
                elevated: Oklch::new(0.545, 0.148, 45.0, opacity),
            },
            market: MarketPalette {
                up: Oklch::new(0.514, 0.128, 152.0, opacity),
                down: Oklch::new(0.542, 0.180, 22.0, opacity),
            },
            // The CMC fear-and-greed palette, authored as the index's own five colors:
            // deep red, red, yellow, mint and deep green.
            fear_greed: FearGreedPalette {
                extreme_fear: Oklch::new(0.4958, 0.1905, 22.83, opacity),
                fear: Oklch::new(0.6529, 0.2110, 18.20, opacity),
                neutral: Oklch::new(0.8724, 0.1636, 94.59, opacity),
                greed: Oklch::new(0.8148, 0.1705, 161.41, opacity),
                extreme_greed: Oklch::new(0.6306, 0.1263, 162.21, opacity),
            },
            series: [
                Oklch::new(0.620, 0.140, 62.0, opacity),
                Oklch::new(0.500, 0.150, 276.0, opacity),
                Oklch::new(0.560, 0.130, 194.0, opacity),
                Oklch::new(0.560, 0.170, 350.0, opacity),
                Oklch::new(0.540, 0.130, 120.0, opacity),
                Oklch::new(0.500, 0.010, 0.0, opacity),
            ],
            // The map is the OSM Carto transport layer's own palette, authored here rather
            // than mixed: paper land #f2efe9 and water #aad3df are its two fills. The pair
            // reads 1.40:1, below the 1.5:1 coastline floor the design asked for, and a
            // paper-white land cannot clear a step from the chart face either (1.01:1).
            // The land-on-face separation is deliberately dropped: the palette's owner chose
            // the OSM paper land over a visible map-to-pane step (no hairline border, no
            // darker land, no moved face), so the 1.01:1 is recorded here and in `roles.rs`
            // rather than floored. The sea is where the map separates itself from the face,
            // at 1.38:1. Both fill floors are pinned to those two measurements in `roles.rs`;
            // moving either fill is a decision about the palette, not something a floor can fix.
            map_ocean: Oklch::new(0.841, 0.046, 217.5, opacity),
            map_land: Oklch::new(0.953, 0.009, 84.6, opacity),
            // The three strokes are the transport layer's own casing colours, undarkened:
            // the admin violet #a875c9 for a border, the motorway casing #dc2a67 for a
            // motorway or trunk, and the primary casing #a06b00 for a primary road. They
            // are told apart by pattern as well as hue — the border is dashed — and each
            // reads at least 2.5:1 on the land and 2.0:1 on the sea (pinned in `roles.rs`).
            map_border: Oklch::new(0.647, 0.133, 311.1, opacity),
            map_road: Oklch::new(0.591, 0.213, 7.2, opacity),
            map_road_minor: Oklch::new(0.570, 0.120, 74.9, opacity),
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
                elevated: Oklch::new(0.760, 0.150, 52.0, opacity),
            },
            market: MarketPalette {
                up: Oklch::new(0.782, 0.158, 150.0, opacity),
                down: Oklch::new(0.702, 0.186, 20.0, opacity),
            },
            // The same scale on a dark page: the middle three already read there, so
            // only the two ends move. Their lifted values keep every pair at least
            // 0.14 apart in Oklab and every band at least 3.6:1 on `surface.sunken`.
            fear_greed: FearGreedPalette {
                extreme_fear: Oklch::new(0.5600, 0.1900, 22.83, opacity),
                fear: Oklch::new(0.7000, 0.1800, 18.20, opacity),
                neutral: Oklch::new(0.8724, 0.1636, 94.59, opacity),
                greed: Oklch::new(0.8148, 0.1705, 161.41, opacity),
                extreme_greed: Oklch::new(0.6800, 0.1263, 162.21, opacity),
            },
            series: [
                Oklch::new(0.800, 0.150, 78.0, opacity),
                Oklch::new(0.720, 0.130, 282.0, opacity),
                Oklch::new(0.780, 0.120, 196.0, opacity),
                Oklch::new(0.740, 0.170, 350.0, opacity),
                Oklch::new(0.700, 0.140, 120.0, opacity),
                Oklch::new(0.660, 0.010, 0.0, opacity),
            ],
            // The night transport map: the same two OSM hues at dark lightnesses, lifted
            // until the sea reads on the near-black chart face and the land reads on the
            // sea. The land is higher than a neutral night grey would be because the sea
            // below it has to stay above that face.
            map_ocean: Oklch::new(0.352, 0.040, 217.5, opacity),
            map_land: Oklch::new(0.479, 0.020, 84.6, opacity),
            // The same three OSM strokes lifted with the page: the violet and the rose are
            // the night ends of the light appearance's own hues, and the amber is the same
            // mustard lifted further — at the light role's lightness it read 2.18:1 on the
            // night land, under the 2.5:1 every map stroke is held to. Each clears that
            // floor on the land and the 2.0:1 the sea gives.
            map_border: Oklch::new(0.813, 0.090, 311.0, opacity),
            map_road: Oklch::new(0.814, 0.080, 7.5, opacity),
            map_road_minor: Oklch::new(0.780, 0.110, 74.9, opacity),
            on_brand: Oklch::new(0.176, 0.020, 245.0, opacity),
            on_inverse: Oklch::new(0.176, 0.016, 252.4, opacity),
            scrim_alpha: 0.55,
            veil_hover: 0.08,
            veil_active: 0.14,
        }
    }
}

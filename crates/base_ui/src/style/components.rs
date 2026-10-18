//! The component side of the theme: what a control is painted with, resolved from roles.
//!
//! This is the layer §8 describes. A recipe is a role or a small set of roles combined into the
//! colors one control needs — a button's fill, text and edge in each of its states; an input's
//! field, text and border; a menu's surface; a dialog's shadow — and it is resolved once, when the
//! theme is loaded, so a component reads a field instead of combining two.
//!
//! Every recipe here is written in terms of roles, never in terms of palette values. That is what
//! makes it possible to change an appearance by changing a palette: the buttons, inputs and menus
//! move with it because none of them names a color the palette did not resolve.
//!
//! The chart's parts (§8.7) are named in [`Roles`] rather than here: a chart paints a candle with
//! `market.up`, a grid line with `chart.grid` and a series with `chart.series`, and there is no
//! combination of them a chart needs that a role does not already hold. [`ChartColors`] names
//! those parts once for a chart that would rather read a chart-shaped struct, which is the same
//! §8.7 table the roles are resolved by.

use crate::{
    interactive::{InteractiveState, InteractiveVariant},
    style::{
        color::{Oklch, ResolvedColor},
        roles::Roles,
    },
};

/// How black the shadow under a dialog is cast (§8.3).
///
/// A shadow is the one thing in the theme with no hue to author: it is black at an alpha, in both
/// appearances. A dialog is the one surface that rises above the whole window, so it is the one
/// that casts a wide, soft shadow; the dark appearance also draws a divider-colored edge on the
/// panel, because a black shadow over a near-black page says next to nothing on its own.
const DIALOG_SHADOW_ALPHA: f32 = 0.25;

/// How black the shadow under a menu is cast (§8.3).
///
/// A menu is a flat popover rather than a modal: it is lifted only far enough to say that it is
/// not the surface it covers, with a compact falloff and a lighter ink than the dialog's. The
/// light appearance needs the shadow, where `surface.overlay` and the page are close in value; the
/// dark one is told apart by the `line.divider` edge the menu draws (§8.3), for the same reason the
/// dialog draws one there.
const MENU_SHADOW_ALPHA: f32 = 0.12;

/// The alpha of a scrollbar thumb at rest (§8.6).
const SCROLLBAR_RESTING_ALPHA: f32 = 0.22;

/// The alpha of a scrollbar thumb under the pointer or being dragged (§8.6).
const SCROLLBAR_ACTIVE_ALPHA: f32 = 0.40;

/// Everything a component paints with, resolved from one set of roles.
#[derive(Debug, Copy, Clone)]
pub struct ComponentColors {
    /// The four interactive variants and their states.
    pub interactive: InteractiveVariantColors,
    /// An input field.
    pub input: InputColors,
    /// A menu, a dropdown panel.
    pub menu: MenuColors,
    /// A dialog's panel, backdrop and shadow.
    pub dialog: DialogColors,
    /// A checkbox or radio toggle.
    pub toggle: ToggleStateColors,
    /// A progress bar.
    pub progress: ProgressColors,
    /// A scrollbar's thumb.
    pub scrollbar: ScrollbarColors,
    /// A tooltip.
    pub tooltip: TooltipColors,
    /// The parts a chart is painted with (§8.7).
    pub chart: ChartColors,
}

impl ComponentColors {
    /// Resolves every recipe of `roles`, once, when the theme is loaded.
    pub fn resolve(roles: &Roles) -> Self {
        Self {
            interactive: InteractiveVariantColors::resolve(roles),
            input: InputColors::resolve(roles),
            menu: MenuColors::resolve(roles),
            dialog: DialogColors::resolve(roles),
            toggle: ToggleStateColors::resolve(roles),
            progress: ProgressColors::resolve(roles),
            scrollbar: ScrollbarColors::resolve(roles),
            tooltip: TooltipColors::resolve(roles),
            chart: ChartColors::resolve(roles),
        }
    }
}

/// The paintings of the four interactive variants.
#[derive(Debug, Copy, Clone)]
pub struct InteractiveVariantColors {
    /// The filled variant: a saturated brand block.
    pub filled: InteractiveStateColors,
    /// The tonal variant: a control-colored block.
    pub tonal: InteractiveStateColors,
    /// The outlined variant: a transparent block with an edge.
    pub outline: InteractiveStateColors,
    /// The ghost variant: text and a hover wash, no fill and no edge.
    pub ghost: InteractiveStateColors,
}

impl InteractiveVariantColors {
    fn resolve(roles: &Roles) -> Self {
        Self {
            filled: InteractiveStateColors::resolve(roles, InteractiveVariant::Filled),
            tonal: InteractiveStateColors::resolve(roles, InteractiveVariant::Tonal),
            outline: InteractiveStateColors::resolve(roles, InteractiveVariant::Outline),
            ghost: InteractiveStateColors::resolve(roles, InteractiveVariant::Ghost),
        }
    }
}

/// The four states of one interactive variant.
#[derive(Debug, Copy, Clone)]
pub struct InteractiveStateColors {
    /// What the variant looks like untouched.
    pub normal: InteractiveElementColors,
    /// What it looks like under the pointer.
    pub hover: InteractiveElementColors,
    /// What it looks like while being pressed.
    pub active: InteractiveElementColors,
    /// What it looks like when it cannot be used.
    pub disabled: InteractiveElementColors,
}

impl InteractiveStateColors {
    fn resolve(roles: &Roles, variant: InteractiveVariant) -> Self {
        Self {
            normal: InteractiveElementColors::resolve(roles, variant, InteractiveState::Normal),
            hover: InteractiveElementColors::resolve(roles, variant, InteractiveState::Hover),
            active: InteractiveElementColors::resolve(roles, variant, InteractiveState::Active),
            disabled: InteractiveElementColors::resolve(roles, variant, InteractiveState::Disabled),
        }
    }
}

/// The three paints one interactive element's current state comes to.
#[derive(Debug, Copy, Clone)]
pub struct InteractiveElementColors {
    /// The fill. `ResolvedColor::transparent` means the element fills nothing.
    pub background: ResolvedColor,
    /// The color text, icons and anything else drawn on the fill is read in.
    pub foreground: ResolvedColor,
    /// The edge. `filled` paints it in the fill and `ghost` paints it transparent, which is how
    /// "no border" is expressed while the element keeps the one-pixel border it always lays out;
    /// `tonal` and `outline` draw a real edge, so a control's area is visible even where its fill
    /// is the surface it sits on.
    pub border: ResolvedColor,
}

impl InteractiveElementColors {
    fn resolve(roles: &Roles, variant: InteractiveVariant, state: InteractiveState) -> Self {
        let colors =
            |background: ResolvedColor, foreground: ResolvedColor, border: ResolvedColor| Self {
                background,
                foreground,
                border,
            };
        let transparent = ResolvedColor::transparent();

        // §8.1: the fill, text and edge of each variant, state by state.
        match variant {
            InteractiveVariant::Filled => match state {
                InteractiveState::Normal => {
                    colors(roles.brand.base, roles.fg.on_brand, roles.brand.base)
                }
                InteractiveState::Hover => {
                    colors(roles.brand.hover, roles.fg.on_brand, roles.brand.hover)
                }
                InteractiveState::Active => {
                    colors(roles.brand.active, roles.fg.on_brand, roles.brand.active)
                }
                InteractiveState::Disabled => colors(
                    roles.surface.control,
                    roles.fg.disabled,
                    roles.surface.control,
                ),
            },
            // The tonal variant keeps one fill in every state it can be used in and moves a
            // neutral wash over it. The fill is opaque, so the wash is composited here rather
            // than at paint time: the result is the same pixels, with one color to transition.
            //
            // It draws a real edge. The light appearance gives `surface.control` and
            // `surface.overlay` one value (§7.1), so a tonal dropdown trigger or soft button on a
            // dialog would be a control-colored block on the same control-colored surface, with
            // no fill step to say where it is; §8.3's rule for that case is to draw a border or a
            // shadow, and §7.2 has the role for the border. The edge is `line.border` at rest and
            // steps to `line.strong` under the pointer or while pressed and back to
            // `line.divider` when disabled, the same ladder `outline` draws, so the two variants
            // read the same state the same way.
            InteractiveVariant::Tonal => {
                let base = roles.surface.control;

                match state {
                    InteractiveState::Normal => colors(base, roles.fg.default, roles.line.border),
                    InteractiveState::Hover => {
                        let washed = over(base, roles.veil.hover);
                        colors(washed, roles.fg.default, roles.line.strong)
                    }
                    InteractiveState::Active => {
                        let washed = over(base, roles.veil.active);
                        colors(washed, roles.fg.default, roles.line.strong)
                    }
                    InteractiveState::Disabled => {
                        colors(base, roles.fg.disabled, roles.line.divider)
                    }
                }
            }
            // The outlined and ghost variants fill nothing of their own, so their hover wash keeps
            // its alpha: it is composited over whatever the caller put the control on, which is
            // what makes one veil correct on a toolbar, a menu and a sunken body alike.
            InteractiveVariant::Outline => match state {
                InteractiveState::Normal => {
                    colors(transparent, roles.fg.default, roles.line.border)
                }
                InteractiveState::Hover => {
                    colors(roles.veil.hover, roles.fg.default, roles.line.strong)
                }
                InteractiveState::Active => {
                    colors(roles.veil.active, roles.fg.default, roles.line.strong)
                }
                InteractiveState::Disabled => {
                    colors(transparent, roles.fg.disabled, roles.line.divider)
                }
            },
            InteractiveVariant::Ghost => match state {
                InteractiveState::Normal => colors(transparent, roles.fg.default, transparent),
                InteractiveState::Hover => colors(roles.veil.hover, roles.fg.default, transparent),
                InteractiveState::Active => {
                    colors(roles.veil.active, roles.fg.default, transparent)
                }
                InteractiveState::Disabled => colors(transparent, roles.fg.disabled, transparent),
            },
        }
    }
}

/// An input field's paints (§8.2).
#[derive(Debug, Copy, Clone)]
pub struct InputColors {
    /// The field's fill.
    pub background: InputBackgroundColors,
    /// The color typed into the field, and the color of it being read.
    pub foreground: InputForegroundColors,
    /// The field's edge.
    pub border: InputBorderColors,
    /// The placeholder text, which is a step fainter than the value.
    pub placeholder: ResolvedColor,
    /// The text selection inside the field.
    pub selection: ResolvedColor,
}

/// An input's fill, enabled and disabled.
#[derive(Debug, Copy, Clone)]
pub struct InputBackgroundColors {
    /// A field that can be typed into.
    pub enabled: ResolvedColor,
    /// A field that cannot: recessed into the content face rather than raised off it.
    pub disabled: ResolvedColor,
}

/// An input's text, enabled and disabled.
#[derive(Debug, Copy, Clone)]
pub struct InputForegroundColors {
    /// The value of a field that can be typed into.
    pub enabled: ResolvedColor,
    /// The value of a field that cannot.
    pub disabled: ResolvedColor,
}

/// An input's edge, per state.
#[derive(Debug, Copy, Clone)]
pub struct InputBorderColors {
    /// A field the caret is not in.
    pub normal: ResolvedColor,
    /// A field the caret is in: the brand color, not the text color.
    pub focused: ResolvedColor,
    /// A field that cannot be typed into.
    pub disabled: ResolvedColor,
}

impl InputColors {
    fn resolve(roles: &Roles) -> Self {
        Self {
            background: InputBackgroundColors {
                enabled: roles.surface.control,
                disabled: roles.surface.sunken,
            },
            foreground: InputForegroundColors {
                enabled: roles.fg.default,
                disabled: roles.fg.disabled,
            },
            border: InputBorderColors {
                normal: roles.line.border,
                focused: roles.brand.base,
                disabled: roles.line.divider,
            },
            placeholder: roles.fg.subtle,
            selection: roles.selection,
        }
    }
}

/// A menu's paints (§8.3). Its entries are ghost buttons, and its edges are
/// [`Roles::line`]'s, so those are read from the roles the menu is drawn in.
#[derive(Debug, Copy, Clone)]
pub struct MenuColors {
    /// The menu's surface: a floating panel, not the pane it covers.
    pub background: ResolvedColor,
    /// A group heading: the faintest text the menu draws.
    pub group_foreground: ResolvedColor,
    /// The shadow's color, which the panel casts around itself. A menu is a flat popover, so the
    /// ink is lighter than a dialog's and the falloff is compact (see [`MENU_SHADOW_ALPHA`]).
    pub shadow: ResolvedColor,
}

impl MenuColors {
    fn resolve(roles: &Roles) -> Self {
        Self {
            background: roles.surface.overlay,
            group_foreground: roles.fg.subtle,
            shadow: ResolvedColor::new(Oklch::black().with_alpha(MENU_SHADOW_ALPHA)),
        }
    }
}

/// A dialog's paints (§8.3).
#[derive(Debug, Copy, Clone)]
pub struct DialogColors {
    /// The panel's surface.
    pub background: ResolvedColor,
    /// The scrim over the window behind the panel.
    pub backdrop: ResolvedColor,
    /// The shadow's color, which the panel casts around itself.
    pub shadow: ResolvedColor,
}

impl DialogColors {
    fn resolve(roles: &Roles) -> Self {
        Self {
            background: roles.surface.overlay,
            backdrop: roles.scrim,
            shadow: ResolvedColor::new(Oklch::black().with_alpha(DIALOG_SHADOW_ALPHA)),
        }
    }
}

/// A toggle's paints, enabled and disabled (§8.6).
#[derive(Debug, Copy, Clone)]
pub struct ToggleStateColors {
    /// A toggle that can be clicked.
    pub enabled: ToggleColors,
    /// A toggle that cannot.
    pub disabled: ToggleColors,
}

/// The paints of one toggle state.
#[derive(Debug, Copy, Clone)]
pub struct ToggleColors {
    /// The track of a checked toggle.
    pub checked: ResolvedColor,
    /// The track of an unchecked toggle.
    pub unchecked: ResolvedColor,
    /// The label and the geometry drawn beside the track.
    pub foreground: ResolvedColor,
}

impl ToggleStateColors {
    fn resolve(roles: &Roles) -> Self {
        Self {
            enabled: ToggleColors {
                checked: roles.brand.base,
                unchecked: roles.line.strong,
                foreground: roles.fg.default,
            },
            // A disabled toggle keeps its shape and says "not now" with the disabled text color;
            // its track steps back to the faintest edges the theme has.
            disabled: ToggleColors {
                checked: roles.fg.disabled,
                unchecked: roles.line.divider,
                foreground: roles.fg.disabled,
            },
        }
    }
}

/// A progress bar's paints (§8.6).
#[derive(Debug, Copy, Clone)]
pub struct ProgressColors {
    /// The rail the progress runs along.
    pub track: ResolvedColor,
    /// The filled part.
    pub progress: ResolvedColor,
}

impl ProgressColors {
    fn resolve(roles: &Roles) -> Self {
        Self {
            track: roles.surface.control,
            progress: roles.brand.base,
        }
    }
}

/// A scrollbar's two thumb paints (§8.6).
#[derive(Debug, Copy, Clone)]
pub struct ScrollbarColors {
    /// The thumb when the pointer is elsewhere: a hint that there is more to scroll.
    pub normal: ResolvedColor,
    /// The thumb under the pointer or being dragged: the control it is.
    pub active: ResolvedColor,
}

impl ScrollbarColors {
    fn resolve(roles: &Roles) -> Self {
        let thumb = |alpha: f32| ResolvedColor::new(roles.fg.default.oklch.with_alpha(alpha));

        Self {
            normal: thumb(SCROLLBAR_RESTING_ALPHA),
            active: thumb(SCROLLBAR_ACTIVE_ALPHA),
        }
    }
}

/// A tooltip's paints.
#[derive(Debug, Copy, Clone)]
pub struct TooltipColors {
    /// A label floating over the page, turned inside out rather than given a brand color: it is
    /// read, not pressed.
    pub background: ResolvedColor,
    /// The label's text.
    pub foreground: ResolvedColor,
}

impl TooltipColors {
    fn resolve(roles: &Roles) -> Self {
        Self {
            background: roles.surface.inverse,
            foreground: roles.fg.on_inverse,
        }
    }
}

/// Every part of a chart that a chart recipe names, as §8.7 names them.
///
/// The values are roles, not derivations of their own: a chart drawn from this struct is drawing
/// the same colors a component that read the roles directly would. A number's direction in a
/// table is not a chart part and reads `roles.market` instead (§8.4).
#[derive(Debug, Copy, Clone)]
pub struct ChartColors {
    /// Grid lines and axis ticks.
    pub grid: ResolvedColor,
    /// Axis labels and time ticks.
    pub axis: ResolvedColor,
    /// The crosshair the pointer draws.
    pub crosshair: ResolvedColor,
    /// A bullish candle or bar, a rising flow bar, an up tile.
    pub bullish: ResolvedColor,
    /// A bearish candle or bar, a falling flow bar, a down tile.
    pub bearish: ResolvedColor,
    /// The series a multi-line or multi-bar chart is drawn with, in order.
    pub series: [ResolvedColor; 3],
}

impl ChartColors {
    fn resolve(roles: &Roles) -> Self {
        Self {
            grid: roles.chart.grid,
            axis: roles.chart.axis,
            crosshair: roles.chart.crosshair,
            bullish: roles.market.up,
            bearish: roles.market.down,
            series: roles.chart.series,
        }
    }
}

/// `veil` laid over `base` as it is painted: source-over in the channels the screen shows.
///
/// This is what §8 means by `+ veil.hover`, resolved ahead of time: the base is opaque and known,
/// so the composite is the color the user will see, and the element still has one fill to
/// transition rather than one fill and a layer.
fn over(base: ResolvedColor, veil: ResolvedColor) -> ResolvedColor {
    ResolvedColor::new(Oklch::from_hsla(base.hsla.blend(veil.hsla)))
}

#[cfg(test)]
mod tests {
    use super::ComponentColors;
    use crate::style::{
        palette::Palette,
        roles::{Roles, contrast},
    };

    /// The two scrollbar thumbs are a pair, in either appearance: the one under the pointer has to
    /// stand out from the surface it is drawn on more than the one that is resting.
    ///
    /// Comparing the alphas or the un-composited colors would not say it: an alpha reads fainter
    /// over a light surface than over a dark one, so the test compares the composited thumb — the
    /// color a user actually sees — against the surface it is drawn on.
    #[test]
    fn the_active_scrollbar_thumb_should_stand_out_more_than_the_resting_one() {
        for palette in [Palette::light(), Palette::dark()] {
            let roles = Roles::resolve(&palette);
            let colors = ComponentColors::resolve(&roles);
            let base = roles.surface.canvas;
            let resting = contrast(colors.scrollbar.normal, base);
            let active = contrast(colors.scrollbar.active, base);

            assert!(
                active > resting,
                "{:?}: a scrollbar's active thumb reads {active:.2}:1 against its base and its \
                 resting one reads {resting:.2}:1, which is the wrong way round",
                palette.appearance
            );
        }
    }

    /// A veil composited over an opaque fill is opaque, so a tonal button never has to blend two
    /// layers at paint time; the variants that fill nothing keep their wash transparent so that it
    /// composites against whatever the caller put them on.
    #[test]
    fn a_washed_fill_should_be_opaque_exactly_when_the_variant_fills_something() {
        for palette in [Palette::light(), Palette::dark()] {
            let colors = ComponentColors::resolve(&Roles::resolve(&palette));

            for state in [
                colors.interactive.tonal.hover,
                colors.interactive.tonal.active,
            ] {
                assert_eq!(
                    state.background.hsla.a, 1.0,
                    "{:?}: a tonal button's hover is a wash over an opaque fill and cannot need \
                     the layer behind it",
                    palette.appearance
                );
            }

            for state in [
                colors.interactive.outline.hover,
                colors.interactive.outline.active,
                colors.interactive.ghost.hover,
                colors.interactive.ghost.active,
            ] {
                assert!(
                    state.background.hsla.a < 1.0,
                    "{:?}: an outline or ghost button's hover keeps its alpha, because the surface \
                     it is drawn on is the caller's to choose",
                    palette.appearance
                );
            }
        }
    }

    /// A tonal control draws a real boundary, because on the light appearance its fill is the
    /// same value as the dialog or menu it sits on: only the edge says where the control is.
    ///
    /// §7.1 gives `surface.control` and `surface.overlay` one value in the light appearance and a
    /// single step between them in the dark one; §8.3's rule is that a control on a surface it
    /// matches is told apart by a border or a shadow. The edge therefore has to clear WCAG's 3:1
    /// non-text floor (§7.2) against both the fill inside the control and the surface behind it,
    /// in every state the control can be interacted with, and the rest state's edge must not be
    /// painted in its own fill, which is exactly the "no border" this test prevents from coming
    /// back.
    #[test]
    fn a_tonal_control_should_draw_its_edge_against_its_fill_and_the_overlay() {
        for palette in [Palette::light(), Palette::dark()] {
            let roles = Roles::resolve(&palette);
            let tonal = ComponentColors::resolve(&roles).interactive.tonal;

            assert_ne!(
                tonal.normal.border, tonal.normal.background,
                "{:?}: a tonal control's rest border is its own fill, so the control has no edge \
                 where its fill and the surface it sits on are the same value",
                palette.appearance
            );

            for (state, colors) in [
                ("normal", tonal.normal),
                ("hover", tonal.hover),
                ("active", tonal.active),
            ] {
                for (surface_name, surface) in [
                    ("surface.control", roles.surface.control),
                    ("surface.overlay", roles.surface.overlay),
                ] {
                    let ratio = contrast(colors.border, surface);

                    assert!(
                        ratio >= 3.0,
                        "{:?}: a tonal control's {state} border reads {ratio:.2}:1 against \
                         {surface_name}, where WCAG's non-text floor is 3.00:1",
                        palette.appearance
                    );
                }
            }
        }
    }

    /// A menu is a flat popover, so §8.3 lifts it with a black shadow at a lighter ink than the
    /// dialog's: enough to tell the panel from what it covers, little enough that the interface
    /// still reads as flat. The light appearance needs the shadow because `surface.overlay` and the
    /// page are close enough to hide the step; the dark one is told apart by the `line.divider`
    /// edge the menu draws, where a black shadow over a near-black page nearly disappears.
    ///
    /// The colour must stay neutral and translucent — an opaque shadow paints a slab around the
    /// panel, and a tinted one reads as a glow on a dark page and as a smear on a light one — and
    /// it must stay lighter than the dialog's, which is the one surface raised over the whole
    /// window.
    #[test]
    fn a_menu_should_cast_a_light_translucent_black_shadow_in_both_appearances() {
        for palette in [Palette::light(), Palette::dark()] {
            let colors = ComponentColors::resolve(&Roles::resolve(&palette));
            let shadow = colors.menu.shadow;

            assert!(
                shadow.hsla.a > 0.0 && shadow.hsla.a < 1.0,
                "{:?}: a menu's shadow is opaque at alpha {:.2}, where it has to be layered",
                palette.appearance,
                shadow.hsla.a
            );
            assert!(
                shadow.oklch.lightness() < 1e-3 && shadow.oklch.chroma() < 1e-3,
                "{:?}: a menu's shadow reads L {:.3}, C {:.3}, where §8.3 casts every shadow \
                 black",
                palette.appearance,
                shadow.oklch.lightness(),
                shadow.oklch.chroma()
            );
            assert!(
                shadow.hsla.a < colors.dialog.shadow.hsla.a,
                "{:?}: a menu's shadow is as heavy as a dialog's at alpha {:.2}, where a menu is a \
                 flat popover that should only hint at being off the page",
                palette.appearance,
                shadow.hsla.a
            );
        }
    }
}

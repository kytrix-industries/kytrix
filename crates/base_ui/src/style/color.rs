//! The color types a theme is written and painted with.
//!
//! A palette is authored in OKLCH, because a step of lightness there looks like the same step in
//! either appearance and a hue can be moved without changing how bright the color reads. Colors
//! are mixed in Oklab, the rectangular form of the same space: a straight line between two colors
//! there does not pass through the dirty grey that a naive mix of two hues does. GPUI, finally,
//! paints HSLA, so every color is converted once — when the theme is loaded — and painting a frame
//! is a field read.
//!
//! [`Oklch`] is the authored and derived value. [`ResolvedColor`] is one role: the OKLCH value a
//! mix and an animation step work in, plus the `Hsla` a paint reads. Nothing outside `style/`
//! converts between color spaces: a frame reads the `hsla` a role carries, and the one conversion
//! an animation pays for happens in [`Interpolate`].

use color::{ColorSpace, ColorSpaceTag, DynamicColor, Flags, Hsl, HueDirection, Srgb};
use gpui_kit::{Hsla, base::Interpolate, hsla};

/// A color in OKLCH, the space a palette is authored in.
///
/// The value is only ever constructed from, or converted back to, OKLCH — which is what lets
/// [`Oklch::lightness`] and the rest read the components directly. A mix leaves the interpolation
/// space in Oklab and is converted back here, so a value handed around is always the same kind of
/// color, whatever was done to it.
#[derive(Debug, Copy, Clone, PartialEq)]
#[repr(transparent)]
pub struct Oklch(DynamicColor);

impl Oklch {
    /// A color from its OKLCH components.
    ///
    /// * `lightness`: 0.0 is black and 1.0 is white.
    /// * `chroma`: 0.0 is grey; typical saturated colors sit below 0.2.
    /// * `hue`: degrees around the color wheel, in `0.0..360.0`.
    /// * `alpha`: 0.0 is invisible and 1.0 is opaque.
    pub fn new(lightness: f32, chroma: f32, hue: f32, alpha: f32) -> Self {
        Self(DynamicColor {
            cs: ColorSpaceTag::Oklch,
            flags: Flags::default(),
            components: [lightness, chroma, hue, alpha],
        })
    }

    /// `target_weight` of the way from this color to `target`, mixed in `space`.
    ///
    /// The result is converted back to OKLCH, so this is the one mix the theme is derived with:
    /// surfaces, lines, text and the status tints are all one color mixed toward another.
    pub fn mix(&self, space: ColorSpaceTag, target: &Self, target_weight: f32) -> Self {
        Self(
            self.0
                .interpolate(target.0, space, HueDirection::Shorter)
                .eval(target_weight.clamp(0.0, 1.0))
                .convert(ColorSpaceTag::Oklch),
        )
    }

    /// The color an HSLA value names, which is what a mix of two painted colors is read back as.
    ///
    /// This is the one direction a component recipe needs: a veil composited over an opaque base
    /// is a color the screen shows, and it has to be resolvable like any other.
    pub fn from_hsla(color: Hsla) -> Self {
        Self(
            DynamicColor {
                cs: ColorSpaceTag::Hsl,
                flags: Flags::default(),
                components: [color.h * 360.0, color.s * 100.0, color.l * 100.0, color.a],
            }
            .convert(ColorSpaceTag::Oklch),
        )
    }

    /// The same color with a different alpha, which is how a veil is made from a text color.
    pub fn with_alpha(&self, alpha: f32) -> Self {
        Self(self.0.with_alpha(alpha))
    }

    /// The lightness component: 0.0 is black and 1.0 is white.
    pub fn lightness(&self) -> f32 {
        self.0.components[0]
    }

    /// The chroma component: 0.0 is grey.
    pub fn chroma(&self) -> f32 {
        self.0.components[1]
    }

    /// The hue component, in degrees.
    pub fn hue(&self) -> f32 {
        self.0.components[2]
    }

    /// The alpha component.
    pub fn alpha(&self) -> f32 {
        self.0.components[3]
    }

    /// The color in Oklab, the space it is mixed in.
    ///
    /// Two colors are as far apart as their Oklab components are, which is how the design says
    /// whether chart series can be told apart by more than their hue.
    pub fn oklab(&self) -> [f32; 3] {
        let components = self.0.convert(ColorSpaceTag::Oklab).components;

        [components[0], components[1], components[2]]
    }

    /// The color black.
    pub fn black() -> Self {
        Self::new(0.0, 0.0, 0.0, 1.0)
    }

    /// No color at all, which is what a variant that fills nothing is drawn with.
    pub fn transparent() -> Self {
        Self::new(0.0, 0.0, 0.0, 0.0)
    }

    /// The color as the painter reads it.
    ///
    /// The conversion passes through sRGB and is clipped to that gamut there. A few of the
    /// saturated palette colors fall a little outside sRGB — a negative red channel, say — and a
    /// screen shows the clipped color rather than the impossible one. Clipping before the HSL
    /// conversion is what makes the painted color the one the design's tables were computed from:
    /// an out-of-gamut channel run through the HSL conversion instead becomes a shifted hue and an
    /// impossible saturation, which is a different color.
    fn hsla(&self) -> Hsla {
        let [red, green, blue, alpha] = self.0.convert(ColorSpaceTag::Srgb).components;
        let srgb = [
            red.clamp(0.0, 1.0),
            green.clamp(0.0, 1.0),
            blue.clamp(0.0, 1.0),
        ];
        let [hue, saturation, lightness] = Srgb::convert::<Hsl>(srgb);

        hsla(hue / 360.0, saturation / 100.0, lightness / 100.0, alpha)
    }
}

/// One resolved role: what a semantics-driven paint is made of.
///
/// The two fields are the same color in the two spaces the theme needs. Derivations and animation
/// steps work on [`ResolvedColor::oklch`]; a paint reads [`ResolvedColor::hsla`], which was
/// computed when the role was resolved and costs nothing to read. The pair is what keeps a frame
/// free of color-space conversion: the only conversion in this crate happens inside
/// [`ResolvedColor::new`] and [`Interpolate::interpolate`], once per role and once per animated
/// step respectively.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ResolvedColor {
    /// The value derivations and animations work in.
    pub oklch: Oklch,
    /// The value a paint reads.
    pub hsla: Hsla,
}

impl ResolvedColor {
    /// Resolves `oklch` to the pair a role carries, converting to HSLA here and only here.
    pub fn new(oklch: Oklch) -> Self {
        Self {
            hsla: oklch.hsla(),
            oklch,
        }
    }

    /// No paint at all: a variant that leaves its background to whatever it sits on.
    ///
    /// Both halves are spelled out rather than converted from each other: the transparent color
    /// is known in both spaces, and a variant that paints nothing is drawn on every frame, so the
    /// one color every paint is sure about must not cost a conversion.
    pub fn transparent() -> Self {
        Self {
            oklch: Oklch::transparent(),
            hsla: hsla(0.0, 0.0, 0.0, 0.0),
        }
    }
}

impl Interpolate for ResolvedColor {
    /// The color `progress` of the way to `target`, mixed in Oklab.
    ///
    /// Both fields move together: the OKLCH value is the mix the animation is defined in, and the
    /// HSLA value is that mix resolved again. The conversion here is the one the animation pays
    /// for, once per property per frame of a transition — never on a frame that is standing still.
    fn interpolate(&self, target: &Self, progress: f32) -> Self {
        Self::new(
            self.oklch
                .mix(ColorSpaceTag::Oklab, &target.oklch, progress),
        )
    }
}

#[cfg(test)]
mod tests {
    use color::ColorSpaceTag;
    use gpui_kit::base::Interpolate;

    use super::{Oklch, ResolvedColor};

    /// A mix is a mix in Oklab, not in the space the value is stored in: halfway from black to
    /// white is Oklab's grey, which is neither the HSL midpoint nor the sRGB one.
    #[test]
    fn a_mix_should_land_in_oklab() {
        let white = Oklch::new(1.0, 0.0, 0.0, 1.0);
        let mixed = Oklch::black().mix(ColorSpaceTag::Oklab, &white, 0.5);

        assert!(
            (mixed.lightness() - 0.5).abs() < 1e-4,
            "Oklab's midpoint of black and white is L 0.5, not L {}",
            mixed.lightness()
        );
        assert!(
            mixed.chroma() < 1e-4,
            "a mix of two achromatic colors is achromatic, not C {}",
            mixed.chroma()
        );
    }

    /// An animation step updates both halves of a resolved color, so what a paint reads is the
    /// same color the step computed — including how far into the transition the alpha is.
    #[test]
    fn interpolating_should_keep_both_spaces_in_step() {
        let start = ResolvedColor::new(Oklch::new(0.5, 0.2, 20.0, 1.0));
        let end = ResolvedColor::new(Oklch::new(0.7, 0.1, 200.0, 0.5));
        let halfway = start.interpolate(&end, 0.5);

        assert_eq!(
            halfway,
            ResolvedColor::new(start.oklch.mix(ColorSpaceTag::Oklab, &end.oklch, 0.5)),
            "the resolved color is the mix of its two OKLCH values"
        );
        assert!(
            (halfway.hsla.a - 0.75).abs() < 1e-5 && (halfway.oklch.alpha() - 0.75).abs() < 1e-5,
            "the alpha of the step is the alpha halfway between {} and {}",
            start.oklch.alpha(),
            end.oklch.alpha()
        );
    }
}

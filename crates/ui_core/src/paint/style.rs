use smallvec::SmallVec;
use smol_str::SmolStr;
use taffy::{
    geometry::{Point as TaffyPoint, Rect as TaffyRect, Size as TaffySize},
    style::{
        AlignItems, AlignSelf, Dimension, Display, FlexDirection, JustifyContent, LengthPercentage,
        LengthPercentageAuto, Overflow, Position,
    },
};

use crate::{
    math::Point,
    paint::{
        color::Color,
        primitive::{Border, Gradient},
    },
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    inner: taffy::Style<SmolStr>,
    pub clip: Clip,
}

impl Style {
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn inner(&self) -> &taffy::Style<SmolStr> {
        &self.inner
    }

    #[cfg(not(any(test, feature = "test-support")))]
    #[must_use]
    pub(crate) fn inner(&self) -> &taffy::Style<SmolStr> {
        &self.inner
    }

    #[cfg(test)]
    pub(crate) fn from_taffy(inner: taffy::Style<SmolStr>) -> Self {
        Self {
            inner,
            clip: Clip::None,
        }
    }

    pub fn display(&mut self, display: Display) -> &mut Self {
        self.inner.display = display;
        self
    }

    #[must_use]
    pub fn display_mode(&self) -> Display {
        self.inner.display
    }

    pub fn position(&mut self, position: Position) -> &mut Self {
        self.inner.position = position;
        self
    }

    pub fn flex_direction(&mut self, flex_direction: FlexDirection) -> &mut Self {
        self.inner.flex_direction = flex_direction;
        self
    }

    pub fn flex(&mut self, grow: f32, shrink: f32, basis: Dimension) -> &mut Self {
        self.inner.flex_grow = grow;
        self.inner.flex_shrink = shrink;
        self.inner.flex_basis = basis;
        self
    }

    pub fn flex_grow(&mut self, grow: f32) -> &mut Self {
        self.inner.flex_grow = grow;
        self
    }

    pub fn flex_shrink(&mut self, shrink: f32) -> &mut Self {
        self.inner.flex_shrink = shrink;
        self
    }

    pub fn align_items(&mut self, align_items: AlignItems) -> &mut Self {
        self.inner.align_items = align_items;
        self
    }

    pub fn align_self(&mut self, align_self: AlignSelf) -> &mut Self {
        self.inner.align_self = Some(align_self);
        self
    }

    pub fn justify_content(&mut self, justify_content: JustifyContent) -> &mut Self {
        self.inner.justify_content = justify_content;
        self
    }

    pub fn justify_center(&mut self) -> &mut Self {
        self.inner.justify_content = JustifyContent::CENTER;
        self
    }

    pub fn overflow(&mut self, overflow: Overflow) -> &mut Self {
        self.inner.overflow = TaffyPoint {
            x: overflow,
            y: overflow,
        };
        self
    }

    pub fn gap(&mut self, px: f32) -> &mut Self {
        let gap = LengthPercentage::length(px);
        self.inner.gap = TaffySize {
            width: gap,
            height: gap,
        };
        self
    }

    pub fn gap_width(&mut self, px: f32) -> &mut Self {
        self.inner.gap.width = LengthPercentage::length(px);
        self
    }

    pub fn gap_height(&mut self, px: f32) -> &mut Self {
        self.inner.gap.height = LengthPercentage::length(px);
        self
    }

    pub fn padding(&mut self, px: f32) -> &mut Self {
        let value = LengthPercentage::length(px);
        self.inner.padding = TaffyRect {
            left: value,
            right: value,
            top: value,
            bottom: value,
        };
        self
    }

    pub fn padding_x(&mut self, px: f32) -> &mut Self {
        let value = LengthPercentage::length(px);
        self.inner.padding.left = value;
        self.inner.padding.right = value;
        self
    }

    pub fn padding_y(&mut self, px: f32) -> &mut Self {
        let value = LengthPercentage::length(px);
        self.inner.padding.top = value;
        self.inner.padding.bottom = value;
        self
    }

    pub fn padding_sides(&mut self, top: f32, right: f32, bottom: f32, left: f32) -> &mut Self {
        self.inner.padding = TaffyRect {
            left: LengthPercentage::length(left),
            right: LengthPercentage::length(right),
            top: LengthPercentage::length(top),
            bottom: LengthPercentage::length(bottom),
        };
        self
    }

    pub fn margin_sides(&mut self, top: f32, right: f32, bottom: f32, left: f32) -> &mut Self {
        self.inner.margin = TaffyRect {
            left: LengthPercentageAuto::length(left),
            right: LengthPercentageAuto::length(right),
            top: LengthPercentageAuto::length(top),
            bottom: LengthPercentageAuto::length(bottom),
        };
        self
    }

    pub fn border(&mut self, px: f32) -> &mut Self {
        let value = LengthPercentage::length(px);
        self.inner.border = TaffyRect {
            left: value,
            right: value,
            top: value,
            bottom: value,
        };
        self
    }

    pub fn border_sides(&mut self, top: f32, right: f32, bottom: f32, left: f32) -> &mut Self {
        self.inner.border = TaffyRect {
            left: LengthPercentage::length(left),
            right: LengthPercentage::length(right),
            top: LengthPercentage::length(top),
            bottom: LengthPercentage::length(bottom),
        };
        self
    }

    pub fn size_length(&mut self, width: f32, height: f32) -> &mut Self {
        self.inner.size = TaffySize {
            width: Dimension::length(width),
            height: Dimension::length(height),
        };
        self
    }

    pub fn size_percent(&mut self, width: f32, height: f32) -> &mut Self {
        self.inner.size = TaffySize {
            width: Dimension::percent(width),
            height: Dimension::percent(height),
        };
        self
    }

    pub fn width(&mut self, width: Dimension) -> &mut Self {
        self.inner.size.width = width;
        self
    }

    pub fn width_length(&mut self, px: f32) -> &mut Self {
        self.inner.size.width = Dimension::length(px);
        self
    }

    pub fn height(&mut self, height: Dimension) -> &mut Self {
        self.inner.size.height = height;
        self
    }

    pub fn height_length(&mut self, px: f32) -> &mut Self {
        self.inner.size.height = Dimension::length(px);
        self
    }

    pub fn height_percent(&mut self, percent: f32) -> &mut Self {
        self.inner.size.height = Dimension::percent(percent);
        self
    }

    pub fn min_width(&mut self, min_width: LengthPercentageAuto) -> &mut Self {
        self.inner.min_size.width = min_width;
        self
    }

    pub fn min_height(&mut self, min_height: LengthPercentageAuto) -> &mut Self {
        self.inner.min_size.height = min_height;
        self
    }

    pub fn min_height_length(&mut self, px: f32) -> &mut Self {
        self.inner.min_size.height = LengthPercentageAuto::length(px);
        self
    }

    pub fn max_width(&mut self, max_width: LengthPercentageAuto) -> &mut Self {
        self.inner.max_size.width = max_width;
        self
    }

    pub fn max_height(&mut self, max_height: LengthPercentageAuto) -> &mut Self {
        self.inner.max_size.height = max_height;
        self
    }

    pub fn inset(
        &mut self,
        top: LengthPercentageAuto,
        right: LengthPercentageAuto,
        bottom: LengthPercentageAuto,
        left: LengthPercentageAuto,
    ) -> &mut Self {
        self.inner.inset = TaffyRect {
            left,
            right,
            top,
            bottom,
        };
        self
    }

    pub fn inset_top(&mut self, px: f32) -> &mut Self {
        self.inner.inset.top = LengthPercentageAuto::length(px);
        self
    }

    pub fn inset_right(&mut self, px: f32) -> &mut Self {
        self.inner.inset.right = LengthPercentageAuto::length(px);
        self
    }

    pub fn inset_bottom(&mut self, px: f32) -> &mut Self {
        self.inner.inset.bottom = LengthPercentageAuto::length(px);
        self
    }

    pub fn inset_left(&mut self, px: f32) -> &mut Self {
        self.inner.inset.left = LengthPercentageAuto::length(px);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FocusRing {
    pub width: f32,
    pub offset: f32,
    pub color: Color,
}

impl FocusRing {
    #[must_use]
    pub const fn new(width: f32, offset: f32, color: Color) -> Self {
        Self {
            width,
            offset,
            color,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Clip {
    #[default]
    None,
    Bounds,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaintSpec {
    pub background: Color,
    pub border: Option<Border>,
    pub corner_radius: [f32; 4],
    pub shadows: Shadows,
    pub gradient: Option<Gradient>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowSpec {
    pub offset: Point,
    pub blur: f32,
    pub spread: f32,
    pub color: Color,
}

pub const MAX_SHADOWS: usize = 4;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shadows(SmallVec<[ShadowSpec; 2]>);

impl Shadows {
    pub fn new(layers: impl IntoIterator<Item = ShadowSpec>) -> Self {
        Self(layers.into_iter().take(MAX_SHADOWS).collect())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, ShadowSpec> {
        self.0.iter()
    }
}

impl From<ShadowSpec> for Shadows {
    fn from(layer: ShadowSpec) -> Self {
        Self(SmallVec::from_slice(&[layer]))
    }
}

impl FromIterator<ShadowSpec> for Shadows {
    fn from_iter<I: IntoIterator<Item = ShadowSpec>>(iter: I) -> Self {
        Self::new(iter)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StylePatch {
    pub background: Option<Color>,
    pub foreground: Option<Color>,
    pub border: Option<Border>,
    pub opacity: Option<f32>,
    pub visible: Option<bool>,
    pub interactive: Option<bool>,
    pub cursor: Option<Cursor>,
    pub affects_layout: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Cursor {
    #[default]
    Default,
    Pointer,
    Text,
    NotAllowed,
    Grab,
    Grabbing,
    ResizeHorizontal,
    ResizeVertical,
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use smol_str::SmolStr;
    use taffy::{
        geometry::{Point as TaffyPoint, Rect as TaffyRect, Size as TaffySize},
        style::{
            AlignItems, AlignSelf, Dimension, Display, FlexDirection, JustifyContent,
            LengthPercentage, LengthPercentageAuto, Overflow, Position,
        },
    };

    use super::{Clip, Cursor, MAX_SHADOWS, PaintSpec, ShadowSpec, Shadows, Style, StylePatch};
    use crate::{math::Point, paint::color::Color};

    fn taffy_style() -> taffy::Style<SmolStr> {
        taffy::Style::<SmolStr>::DEFAULT
    }

    fn layer(offset_y: f32) -> ShadowSpec {
        ShadowSpec {
            offset: Point::new(0.0, offset_y),
            blur: 4.0,
            spread: 0.0,
            color: Color::BLACK,
        }
    }

    #[test]
    fn style_default_is_taffy_default() {
        let style = Style::default();

        assert_eq!(style.inner(), &taffy::Style::<SmolStr>::DEFAULT);
        assert_eq!(
            style.inner().display,
            taffy::style::Display::Flex,
            "workspace 的 taffy 开了 flexbox，Display::DEFAULT = Flex"
        );
    }

    #[test]
    fn from_taffy_wraps_the_engine_style_without_a_clip() {
        let inner = taffy::Style {
            display: Display::None,
            position: Position::Absolute,
            ..taffy_style()
        };
        let style = Style::from_taffy(inner.clone());

        assert_eq!(style.inner(), &inner);
        assert_eq!(style.clip, Clip::None);
    }

    #[test]
    fn display_and_position_setters_match_direct_taffy_writes() {
        let mut style = Style::default();
        style.display(Display::None).position(Position::Absolute);

        let expected = taffy::Style {
            display: Display::None,
            position: Position::Absolute,
            ..taffy_style()
        };
        assert_eq!(style.inner(), &expected);
        assert_eq!(style.display_mode(), Display::None);
    }

    #[test]
    fn flex_and_alignment_setters_match_direct_taffy_writes() {
        let mut style = Style::default();
        style
            .flex_direction(FlexDirection::Column)
            .flex(1.0, 0.5, Dimension::percent(0.25))
            .align_items(AlignItems::FLEX_START)
            .align_self(AlignSelf::STRETCH)
            .justify_content(JustifyContent::SPACE_BETWEEN);

        let expected = taffy::Style {
            flex_direction: FlexDirection::Column,
            flex_grow: 1.0,
            flex_shrink: 0.5,
            flex_basis: Dimension::percent(0.25),
            align_items: AlignItems::FLEX_START,
            align_self: Some(AlignSelf::STRETCH),
            justify_content: JustifyContent::SPACE_BETWEEN,
            ..taffy_style()
        };
        assert_eq!(style.inner(), &expected);
    }

    #[test]
    fn flex_field_setters_and_justify_center_match_direct_taffy_writes() {
        let mut style = Style::default();
        style
            .flex(1.0, 1.0, Dimension::percent(0.5))
            .flex_grow(2.5)
            .flex_shrink(0.25)
            .justify_center();

        let expected = taffy::Style {
            flex_grow: 2.5,
            flex_shrink: 0.25,
            flex_basis: Dimension::percent(0.5),
            justify_content: JustifyContent::CENTER,
            ..taffy_style()
        };
        assert_eq!(style.inner(), &expected);
    }

    #[test]
    fn edge_size_setters_match_direct_taffy_writes() {
        let mut style = Style::default();
        style.size_percent(0.5, 0.5).width_length(120.0);
        assert_eq!(
            style.inner().size,
            TaffySize {
                width: Dimension::length(120.0),
                height: Dimension::percent(0.5),
            }
        );

        style.height_length(40.0);
        assert_eq!(
            style.inner().size,
            TaffySize {
                width: Dimension::length(120.0),
                height: Dimension::length(40.0),
            }
        );

        style.height_percent(0.75);
        assert_eq!(
            style.inner().size,
            TaffySize {
                width: Dimension::length(120.0),
                height: Dimension::percent(0.75),
            }
        );

        style.min_height_length(48.0);
        assert_eq!(
            style.inner().min_size,
            TaffySize {
                width: LengthPercentageAuto::auto(),
                height: LengthPercentageAuto::length(48.0),
            }
        );
    }

    #[test]
    fn overflow_setter_hides_both_axes() {
        let mut style = Style::default();
        style.overflow(Overflow::Hidden);

        let expected = taffy::Style {
            overflow: TaffyPoint {
                x: Overflow::Hidden,
                y: Overflow::Hidden,
            },
            ..taffy_style()
        };
        assert_eq!(style.inner(), &expected);
        assert_eq!(style.clip, Clip::None);
    }

    #[test]
    fn gap_setters_match_direct_taffy_writes() {
        let mut style = Style::default();
        style.gap(8.0);
        assert_eq!(
            style.inner().gap,
            TaffySize {
                width: LengthPercentage::length(8.0),
                height: LengthPercentage::length(8.0),
            }
        );

        style.gap(1.0).gap_width(3.0).gap_height(4.0);
        assert_eq!(
            style.inner().gap,
            TaffySize {
                width: LengthPercentage::length(3.0),
                height: LengthPercentage::length(4.0),
            }
        );
    }

    #[test]
    fn partial_padding_and_gap_writes_leave_the_other_edges_alone() {
        let mut style = Style::default();
        style.padding(16.0).padding_x(4.0);
        assert_eq!(
            style.inner().padding,
            TaffyRect {
                left: LengthPercentage::length(4.0),
                right: LengthPercentage::length(4.0),
                top: LengthPercentage::length(16.0),
                bottom: LengthPercentage::length(16.0),
            }
        );

        style.gap(1.0).gap_width(3.0);
        assert_eq!(
            style.inner().gap,
            TaffySize {
                width: LengthPercentage::length(3.0),
                height: LengthPercentage::length(1.0),
            }
        );
    }

    #[test]
    fn padding_setters_match_direct_taffy_writes() {
        let mut style = Style::default();
        style.padding(16.0);
        assert_eq!(
            style.inner().padding,
            TaffyRect {
                left: LengthPercentage::length(16.0),
                right: LengthPercentage::length(16.0),
                top: LengthPercentage::length(16.0),
                bottom: LengthPercentage::length(16.0),
            }
        );

        style.padding_x(12.0).padding_y(8.0);
        assert_eq!(
            style.inner().padding,
            TaffyRect {
                left: LengthPercentage::length(12.0),
                right: LengthPercentage::length(12.0),
                top: LengthPercentage::length(8.0),
                bottom: LengthPercentage::length(8.0),
            }
        );

        style.padding_sides(1.0, 2.0, 3.0, 4.0);
        assert_eq!(
            style.inner().padding,
            TaffyRect {
                left: LengthPercentage::length(4.0),
                right: LengthPercentage::length(2.0),
                top: LengthPercentage::length(1.0),
                bottom: LengthPercentage::length(3.0),
            }
        );
    }

    #[test]
    fn margin_sides_and_border_setters_match_direct_taffy_writes() {
        let mut style = Style::default();
        style.margin_sides(1.0, 2.0, 3.0, 4.0);
        assert_eq!(
            style.inner().margin,
            TaffyRect {
                left: LengthPercentageAuto::length(4.0),
                right: LengthPercentageAuto::length(2.0),
                top: LengthPercentageAuto::length(1.0),
                bottom: LengthPercentageAuto::length(3.0),
            }
        );

        style.border(2.0);
        assert_eq!(
            style.inner().border,
            TaffyRect {
                left: LengthPercentage::length(2.0),
                right: LengthPercentage::length(2.0),
                top: LengthPercentage::length(2.0),
                bottom: LengthPercentage::length(2.0),
            }
        );

        style.border_sides(3.0, 0.0, 0.0, 2.0);
        assert_eq!(
            style.inner().border,
            TaffyRect {
                left: LengthPercentage::length(2.0),
                right: LengthPercentage::length(0.0),
                top: LengthPercentage::length(3.0),
                bottom: LengthPercentage::length(0.0),
            }
        );
    }

    #[test]
    fn size_setters_match_direct_taffy_writes() {
        let mut style = Style::default();
        style.size_length(120.0, 40.0);
        assert_eq!(
            style.inner().size,
            TaffySize {
                width: Dimension::length(120.0),
                height: Dimension::length(40.0),
            }
        );

        style.size_percent(1.0, 0.5);
        assert_eq!(
            style.inner().size,
            TaffySize {
                width: Dimension::percent(1.0),
                height: Dimension::percent(0.5),
            }
        );

        style.width(Dimension::auto());
        style.height(Dimension::percent(1.0));
        assert_eq!(
            style.inner().size,
            TaffySize {
                width: Dimension::auto(),
                height: Dimension::percent(1.0),
            }
        );
    }

    #[test]
    fn min_max_size_setters_match_direct_taffy_writes() {
        let mut style = Style::default();
        style
            .min_width(LengthPercentageAuto::length(10.0))
            .min_height(LengthPercentageAuto::auto())
            .max_width(LengthPercentageAuto::length(640.0))
            .max_height(LengthPercentageAuto::percent(0.5));
        assert_eq!(
            style.inner().min_size,
            TaffySize {
                width: LengthPercentageAuto::length(10.0),
                height: LengthPercentageAuto::auto(),
            }
        );
        assert_eq!(
            style.inner().max_size,
            TaffySize {
                width: LengthPercentageAuto::length(640.0),
                height: LengthPercentageAuto::percent(0.5),
            }
        );
    }

    #[test]
    fn inset_setters_match_direct_taffy_writes() {
        let mut style = Style::default();
        style.inset(
            LengthPercentageAuto::length(1.0),
            LengthPercentageAuto::auto(),
            LengthPercentageAuto::percent(0.5),
            LengthPercentageAuto::length(3.0),
        );
        assert_eq!(
            style.inner().inset,
            TaffyRect {
                top: LengthPercentageAuto::length(1.0),
                right: LengthPercentageAuto::auto(),
                bottom: LengthPercentageAuto::percent(0.5),
                left: LengthPercentageAuto::length(3.0),
            }
        );

        style
            .inset_top(4.0)
            .inset_right(5.0)
            .inset_bottom(6.0)
            .inset_left(7.0);
        assert_eq!(
            style.inner().inset,
            TaffyRect {
                top: LengthPercentageAuto::length(4.0),
                right: LengthPercentageAuto::length(5.0),
                bottom: LengthPercentageAuto::length(6.0),
                left: LengthPercentageAuto::length(7.0),
            }
        );
    }

    #[test]
    fn style_patch_default_is_empty() {
        let patch = StylePatch::default();

        assert!(patch.background.is_none());
        assert!(patch.foreground.is_none());
        assert!(patch.border.is_none());
        assert!(patch.opacity.is_none());
        assert!(patch.visible.is_none());
        assert!(patch.interactive.is_none());
        assert!(patch.cursor.is_none());
        assert!(!patch.affects_layout, "affects_layout 默认必须是 false");
        assert_eq!(patch, StylePatch::default());
    }

    #[test]
    fn cursor_default_is_default() {
        assert_eq!(Cursor::default(), Cursor::Default);
        assert_ne!(Cursor::NotAllowed, Cursor::Pointer);
    }

    #[test]
    fn clip_default_is_none() {
        assert_eq!(Clip::default(), Clip::None);
        assert_eq!(Style::default().clip, Clip::None);
    }

    #[test]
    fn paint_spec_default_has_no_shadows() {
        let paint = PaintSpec::default();
        assert!(paint.shadows.is_empty());
        assert_eq!(paint.shadows, Shadows::default());
    }

    #[test]
    fn a_single_layer_roundtrips_through_from() {
        let shadows = Shadows::from(layer(2.0));
        assert_eq!(shadows.len(), 1);
        assert!(!shadows.is_empty());
        assert_eq!(
            shadows.iter().copied().collect::<Vec<_>>(),
            vec![layer(2.0)]
        );
    }

    #[test]
    fn shadows_truncate_at_the_layer_cap() {
        let layers: Vec<ShadowSpec> = (0..MAX_SHADOWS as i32 + 2)
            .map(|index| layer(index as f32))
            .collect();
        let shadows = Shadows::new(layers.clone());
        assert_eq!(shadows.len(), MAX_SHADOWS, "上限 {MAX_SHADOWS} 层");
        assert_eq!(
            shadows.iter().copied().collect::<Vec<_>>(),
            layers[..MAX_SHADOWS].to_vec(),
            "按数组序保留前 MAX_SHADOWS 层（绘制序 = 层序）"
        );

        let collected: Shadows = layers.iter().copied().collect();
        assert_eq!(collected, shadows);
    }

    #[test]
    fn shadows_size_budget() {
        let single = size_of::<ShadowSpec>();
        let shadows = size_of::<Shadows>();
        let paint = size_of::<PaintSpec>();
        println!("size_of::<ShadowSpec>() = {single}");
        println!("size_of::<Shadows>() = {shadows}");
        println!("size_of::<PaintSpec>() = {paint}");
        assert_eq!(single, 36, "ShadowSpec 尺寸基线（E13 §2.4）");
        assert_eq!(
            shadows, 88,
            "Shadows 尺寸基线（内联 2 层 = ShadowSpec 36×2 + SmallVec 头 16）"
        );
        assert_eq!(
            paint, 192,
            "PaintSpec 尺寸基线（E13 §2.4；装在 `Option<Box<...>>` 里，不影响 `Node` 336 B）"
        );
    }
}

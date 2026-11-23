use smallvec::SmallVec;
use smol_str::SmolStr;

use crate::{
    math::Point,
    paint::{
        color::Color,
        primitive::{Border, Gradient},
    },
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    pub inner: taffy::Style<SmolStr>,
    pub clip: Clip,
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

    use super::{Clip, Cursor, MAX_SHADOWS, PaintSpec, ShadowSpec, Shadows, Style, StylePatch};
    use crate::{math::Point, paint::color::Color};

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

        assert_eq!(style.inner, taffy::Style::<SmolStr>::DEFAULT);
        assert_eq!(
            style.inner.display,
            taffy::style::Display::Flex,
            "workspace 的 taffy 开了 flexbox，Display::DEFAULT = Flex"
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

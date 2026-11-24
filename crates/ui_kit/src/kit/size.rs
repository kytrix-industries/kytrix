use ui_core::{math::LogicalPx, paint::style_builder::StyleBuilder};

use crate::theme::components::{ComponentTokens, ControlSize, SizeTokens};

pub type UnifiedSize = ControlSize;

impl ControlSize {
    fn tokens(self) -> SizeTokens {
        ComponentTokens::standard().for_size(self)
    }

    #[must_use]
    pub fn h(self) -> LogicalPx {
        self.tokens().height
    }

    #[must_use]
    pub fn gap(self) -> LogicalPx {
        self.tokens().gap
    }

    #[must_use]
    pub fn px(self) -> LogicalPx {
        self.tokens().padding_x
    }

    #[must_use]
    pub fn text_size(self) -> LogicalPx {
        self.tokens().font_size
    }

    #[must_use]
    pub fn icon_size(self) -> LogicalPx {
        self.tokens().icon_size
    }
}

pub trait UnifiedSizable: Sized {
    fn with_unified_size(self, size: UnifiedSize) -> Self;

    fn small(self) -> Self {
        self.with_unified_size(UnifiedSize::Small)
    }

    fn medium(self) -> Self {
        self.with_unified_size(UnifiedSize::Medium)
    }

    fn large(self) -> Self {
        self.with_unified_size(UnifiedSize::Large)
    }
}

pub trait UnifiedSizeStyle {
    fn unified_sized(self, size: UnifiedSize) -> Self;
}

impl UnifiedSizeStyle for StyleBuilder {
    fn unified_sized(self, size: UnifiedSize) -> Self {
        self.height(size.h()).gap(size.gap()).padding_x(size.px())
    }
}

#[cfg(test)]
mod tests {
    use taffy::style::{Dimension, LengthPercentage};
    use ui_core::{math::px, paint::style_builder::StyleBuilder};

    use super::{UnifiedSizable, UnifiedSize, UnifiedSizeStyle};

    const FROZEN_LADDER: [(UnifiedSize, f32, f32, f32, f32, f32); 3] = [
        (UnifiedSize::Small, 20.0, 4.0, 4.0, 12.0, 8.0),
        (UnifiedSize::Medium, 30.0, 6.0, 6.0, 14.0, 12.0),
        (UnifiedSize::Large, 40.0, 8.0, 10.0, 18.0, 16.0),
    ];

    #[test]
    fn every_size_ladder_value_matches_the_frozen_legacy_ladder() {
        for (size, height, gap, padding_x, font_size, icon_size) in FROZEN_LADDER {
            assert_eq!(size.h(), px(height), "{size:?} 的高度漂移");
            assert_eq!(size.gap(), px(gap), "{size:?} 的间隙漂移");
            assert_eq!(size.px(), px(padding_x), "{size:?} 的水平内边距漂移");
            assert_eq!(size.icon_size(), px(icon_size), "{size:?} 的图标尺寸漂移");
            assert_eq!(size.text_size(), px(font_size), "{size:?} 的字号漂移");
        }
    }

    #[test]
    fn unified_sized_writes_the_engine_style() {
        let style = StyleBuilder::new()
            .unified_sized(UnifiedSize::Large)
            .build();
        assert_eq!(style.inner().size.height, Dimension::length(40.0));
        assert_eq!(style.inner().gap.width, LengthPercentage::length(8.0));
        assert_eq!(style.inner().gap.height, LengthPercentage::length(8.0));
        assert_eq!(style.inner().padding.left, LengthPercentage::length(10.0));
        assert_eq!(style.inner().padding.right, LengthPercentage::length(10.0));
        assert_eq!(style.inner().padding.top, LengthPercentage::length(0.0));
        assert_eq!(style.inner().padding.bottom, LengthPercentage::length(0.0));
    }

    #[test]
    fn the_sizable_shortcuts_forward_the_size() {
        #[derive(Debug, PartialEq)]
        struct Widget(UnifiedSize);

        impl UnifiedSizable for Widget {
            fn with_unified_size(self, size: UnifiedSize) -> Self {
                Self(size)
            }
        }

        assert_eq!(Widget(UnifiedSize::Medium).small().0, UnifiedSize::Small);
        assert_eq!(Widget(UnifiedSize::Small).medium().0, UnifiedSize::Medium);
        assert_eq!(Widget(UnifiedSize::Medium).large().0, UnifiedSize::Large);
        assert_eq!(UnifiedSize::default(), UnifiedSize::Medium);
    }
}

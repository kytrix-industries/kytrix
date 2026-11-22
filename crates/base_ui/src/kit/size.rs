//! 控件尺寸阶梯（旧 `UnifiedSize` 的引擎化移植）。
//!
//! # 数值来源
//!
//! P2-1a 已把旧 `size.rs` 的阶梯搬进主题的 `ComponentTokens`（theme spec-delta §6：
//! 「`size.rs`（P2-1b）迁移时应改为读这些 token，`UnifiedSize` 值不重复维护」）。本模块因此
//! **不写任何数值**：全部从 [`ComponentTokens::standard`] 读，旧值一改、新值自动跟着变。
//!
//! # 与旧 API 的差异（登记见 spec-delta §3）
//!
//! - 类型：`UnifiedSize` 是 [`ControlSize`] 的类型别名（引擎化主题里已有同一阶梯，不另立枚举）；
//! - `Pixels` / `Rems` → `f32` 逻辑像素：引擎的布局数值就是 `f32` 逻辑像素，没有 `Pixels`
//!   newtype；字号同理，旧 `Rems`（0.75 / 0.875 / 1.125）已按 16 px 根字号换算成 12 / 14 / 18；
//! - 旧 `UnifiedSizeStyle::unified_sized` 同时设置 `h / gap / px / text_size`；新引擎把文本样式
//!   从布局样式里拆了出去（`TextStyle` 独立），所以 `unified_sized` 只写布局三项，
//!   **字号要由组件自己写进 `TextStyle`**（`size.text_size()`）。
//!
//! # 用法（P2-4 组件迁移时的形状）
//!
//! ```ignore
//! let style = StyleBuilder::new().unified_sized(UnifiedSize::Medium).build();
//! let text = TextStyle::new(UnifiedSize::Medium.text_size());
//! let icon = IconSpec::new(kind).with_size(UnifiedSize::Medium.icon_size());
//! ```

use crate::{
    style_builder::StyleBuilder,
    theme::components::{ComponentTokens, ControlSize, SizeTokens},
};

/// 统一尺寸档（旧 `UnifiedSize`）。
pub type UnifiedSize = ControlSize;

impl ControlSize {
    /// 该档的度量 token（唯一数值来源）。
    fn tokens(self) -> SizeTokens {
        ComponentTokens::standard().for_size(self)
    }

    /// 控件高度（逻辑像素）。
    #[must_use]
    pub fn h(self) -> f32 {
        self.tokens().height
    }

    /// 内部间隙（图标与文字之间等，逻辑像素）。
    #[must_use]
    pub fn gap(self) -> f32 {
        self.tokens().gap
    }

    /// 水平内边距（逻辑像素）。
    #[must_use]
    pub fn px(self) -> f32 {
        self.tokens().padding_x
    }

    /// 字号（逻辑像素；旧 `Rems` × 16 px 根字号）。
    #[must_use]
    pub fn text_size(self) -> f32 {
        self.tokens().font_size
    }

    /// 图标边长（逻辑像素）。
    #[must_use]
    pub fn icon_size(self) -> f32 {
        self.tokens().icon_size
    }
}

/// 可设置统一尺寸的组件 builder（旧 `UnifiedSizable`）。
///
/// 新引擎的组件是挂载期 builder（P2-4 落地），本 trait 只固定**语义**：`small/medium/large`
/// 是 `with_unified_size` 的三个具名快捷方式。
pub trait UnifiedSizable: Sized {
    /// 应用一个尺寸档。
    fn with_unified_size(self, size: UnifiedSize) -> Self;

    /// 紧凑档。
    fn small(self) -> Self {
        self.with_unified_size(UnifiedSize::Small)
    }

    /// 常规档。
    fn medium(self) -> Self {
        self.with_unified_size(UnifiedSize::Medium)
    }

    /// 大号档。
    fn large(self) -> Self {
        self.with_unified_size(UnifiedSize::Large)
    }
}

/// 把尺寸档写进引擎样式（旧 `UnifiedSizeStyle`）。
pub trait UnifiedSizeStyle {
    /// 应用高度 / 间隙 / 水平内边距（字号见模块文档：由组件写进 `TextStyle`）。
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

    use super::{UnifiedSizable, UnifiedSize, UnifiedSizeStyle};
    use crate::style_builder::StyleBuilder;

    /// 冻结基线：旧 `UnifiedSize` 的三档阶梯（P5-2 在旧模块删除前抄录）。
    /// (档位, 高, 间隙, 水平内边距, 字号, 图标尺寸)。
    const FROZEN_LADDER: [(UnifiedSize, f32, f32, f32, f32, f32); 3] = [
        (UnifiedSize::Small, 20.0, 4.0, 4.0, 12.0, 8.0),
        (UnifiedSize::Medium, 30.0, 6.0, 6.0, 14.0, 12.0),
        (UnifiedSize::Large, 40.0, 8.0, 10.0, 18.0, 16.0),
    ];

    /// 三档 × 五个属性与旧实现冻结值逐值一致（字号是旧 rems × 16 px 根字号换算后的像素）。
    #[test]
    fn every_size_ladder_value_matches_the_frozen_legacy_ladder() {
        for (size, height, gap, padding_x, font_size, icon_size) in FROZEN_LADDER {
            assert_eq!(size.h(), height, "{size:?} 的高度漂移");
            assert_eq!(size.gap(), gap, "{size:?} 的间隙漂移");
            assert_eq!(size.px(), padding_x, "{size:?} 的水平内边距漂移");
            assert_eq!(size.icon_size(), icon_size, "{size:?} 的图标尺寸漂移");
            assert_eq!(size.text_size(), font_size, "{size:?} 的字号漂移");
        }
    }

    /// `unified_sized` 真的写进引擎样式（高度 / 间隙 / 水平内边距），且位置从 taffy 读得回来。
    #[test]
    fn unified_sized_writes_the_engine_style() {
        let style = StyleBuilder::new()
            .unified_sized(UnifiedSize::Large)
            .build();
        assert_eq!(style.inner.size.height, Dimension::length(40.0));
        assert_eq!(style.inner.gap.width, LengthPercentage::length(8.0));
        assert_eq!(style.inner.gap.height, LengthPercentage::length(8.0));
        assert_eq!(style.inner.padding.left, LengthPercentage::length(10.0));
        assert_eq!(style.inner.padding.right, LengthPercentage::length(10.0));
        // 旧实现还会写字号；新引擎字号在 `TextStyle`，布局样式里不该有它（不写、也不误写纵向内边距）。
        assert_eq!(style.inner.padding.top, LengthPercentage::length(0.0));
        assert_eq!(style.inner.padding.bottom, LengthPercentage::length(0.0));
    }

    /// `UnifiedSizable` 的三个具名快捷方式只是 `with_unified_size` 的转发。
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

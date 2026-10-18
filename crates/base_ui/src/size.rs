use gpui_kit::{Pixels, Rems, Styled, px, rems};

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum UnifiedSize {
    Small,
    Medium,
    Large,
}

impl UnifiedSize {
    pub fn h(&self) -> Pixels {
        match self {
            UnifiedSize::Small => px(20.),
            UnifiedSize::Medium => px(30.),
            UnifiedSize::Large => px(40.),
        }
    }

    pub fn gap(&self) -> Pixels {
        match self {
            UnifiedSize::Small => px(4.),
            UnifiedSize::Medium => px(6.),
            UnifiedSize::Large => px(8.),
        }
    }

    pub fn px(&self) -> Pixels {
        match self {
            UnifiedSize::Small => px(4.),
            UnifiedSize::Medium => px(6.),
            UnifiedSize::Large => px(10.),
        }
    }

    pub fn text_size(&self) -> Rems {
        match self {
            UnifiedSize::Small => rems(0.75),
            UnifiedSize::Medium => rems(0.875),
            UnifiedSize::Large => rems(1.125),
        }
    }

    pub fn icon_size(&self) -> Pixels {
        match self {
            UnifiedSize::Small => px(8.),
            UnifiedSize::Medium => px(12.),
            UnifiedSize::Large => px(16.),
        }
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

impl<T> UnifiedSizeStyle for T
where
    T: Styled,
{
    fn unified_sized(self, size: UnifiedSize) -> Self {
        self.h(size.h())
            .gap(size.gap())
            .px(size.px())
            .text_size(size.text_size())
    }
}

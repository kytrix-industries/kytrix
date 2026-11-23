pub mod hit_test;
pub(crate) mod measure;
pub(crate) mod taffy_tree;

use crate::math::{Rect, Size};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Axis {
    X,
    Y,
}

pub type MeasureFn =
    Box<dyn Fn(taffy::Size<Option<f32>>, taffy::Size<taffy::AvailableSpace>) -> Size>;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayoutOverride {
    pub rect: Rect,
}

impl LayoutOverride {
    #[must_use]
    pub const fn new(rect: Rect) -> Self {
        Self { rect }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayoutReport {
    pub laid_out: usize,
    pub moved: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub bounds_updated: usize,
}

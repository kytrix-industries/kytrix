use crate::{
    math::{DeviceRect, LogicalPoint, LogicalRect, PhysicalPx, physical_px},
    render::damage::DamageAccumulator,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PaintedExtent {
    bounds: LogicalRect,
    pad: PhysicalPx,
}

impl PaintedExtent {
    #[must_use]
    pub(crate) fn new(bounds: LogicalRect) -> Option<Self> {
        (!bounds.is_empty()).then_some(Self {
            bounds,
            pad: physical_px(1.0),
        })
    }

    #[must_use]
    pub(crate) fn layout_bounds(self) -> LogicalRect {
        self.bounds
    }

    pub(crate) fn push_damage(
        self,
        damage: &mut DamageAccumulator,
        offset: LogicalPoint,
        clip: Option<DeviceRect>,
    ) {
        damage.push_logical(self.bounds.translate(offset), self.pad, clip);
    }
}

//! The one thing the engine's `Rect` does not spell: its far edges.
//!
//! Every box on a chart is placed from the box before it — the plot from the content,
//! the axes from the plot, a label from its tick — and that arithmetic reads from an
//! edge far more often than from an origin. `Rect` stores the origin and the size, so
//! the two derived readings live here rather than being written out as
//! `origin.x + size.width` in every placement.

use ui_core::math::Rect;

/// A box's far edges.
pub(crate) trait RectExt {
    /// `origin.x + size.width`.
    fn right(self) -> f32;
    /// `origin.y + size.height`.
    fn bottom(self) -> f32;
}

impl RectExt for Rect {
    fn right(self) -> f32 {
        self.origin.x + self.size.width
    }

    fn bottom(self) -> f32 {
        self.origin.y + self.size.height
    }
}

#[cfg(test)]
mod tests {
    use ui_core::math::Rect;

    use super::RectExt;

    #[test]
    fn a_box_reads_its_far_edges_from_its_origin_and_size() {
        let rect = Rect::new(10.0, 20.0, 30.0, 40.0);

        assert_eq!(rect.right(), 40.0);
        assert_eq!(rect.bottom(), 60.0);
    }
}

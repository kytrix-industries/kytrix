use ui_core::math::Rect;

pub(crate) trait RectExt {
    fn right(self) -> f32;
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

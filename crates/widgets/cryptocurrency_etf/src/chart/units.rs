use ui_core::math::{LogicalPx, LogicalRect};

pub(crate) trait RectExt {
    fn right(self) -> LogicalPx;
    fn bottom(self) -> LogicalPx;
}

impl RectExt for LogicalRect {
    fn right(self) -> LogicalPx {
        self.origin.x + self.size.width
    }

    fn bottom(self) -> LogicalPx {
        self.origin.y + self.size.height
    }
}

#[cfg(test)]
mod tests {
    use ui_core::math::{LogicalRect, px};

    use super::RectExt;

    #[test]
    fn a_box_reads_its_far_edges_from_its_origin_and_size() {
        let rect = LogicalRect::new(px(10.0), px(20.0), px(30.0), px(40.0));

        assert_eq!(rect.right(), px(40.0));
        assert_eq!(rect.bottom(), px(60.0));
    }
}

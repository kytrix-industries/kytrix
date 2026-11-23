use crate::{
    error::EngineError,
    math::{Rect, Scale, Size},
};

pub(crate) fn scale_from_120(scale: u32) -> Result<Scale, EngineError> {
    if scale == 0 {
        return Err(EngineError::InvalidDpiScale { factor: 0.0 });
    }
    let factor = scale as f32 / 120.0;
    Scale::new(factor).ok_or(EngineError::InvalidDpiScale { factor })
}

pub(crate) fn physical_size(size: Size, scale: Scale) -> (u32, u32) {
    let rect = Rect::new(0.0, 0.0, size.width, size.height).to_physical_rounded(scale);
    (rect.width().max(1) as u32, rect.height().max(1) as u32)
}

pub(crate) fn viewport_destination(size: Size) -> (i32, i32) {
    (
        size.width.max(1.0).ceil() as i32,
        size.height.max(1.0).ceil() as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::{physical_size, viewport_destination};
    use crate::math::{Scale, Size};

    fn scale(factor: f32) -> Scale {
        Scale::new(factor).expect("test scale must be valid")
    }

    #[test]
    fn physical_size_rounds_the_buffer_size() {
        for (factor, expected) in [
            (1.0, (800, 600)),
            (1.25, (1000, 750)),
            (1.5, (1200, 900)),
            (1.75, (1400, 1050)),
            (2.0, (1600, 1200)),
        ] {
            assert_eq!(
                physical_size(Size::new(800.0, 600.0), scale(factor)),
                expected,
                "scale {factor}"
            );
        }

        assert_eq!(
            physical_size(Size::new(701.0, 501.0), scale(1.25)),
            (876, 626)
        );
        assert_eq!(physical_size(Size::new(3.0, 5.0), scale(1.5)), (5, 8));
        assert_eq!(physical_size(Size::new(0.0, 0.0), scale(1.0)), (1, 1));
        assert_eq!(
            physical_size(Size::new(f32::NAN, f32::INFINITY), scale(1.25)),
            (1, 1)
        );
    }

    #[test]
    fn viewport_destination_is_the_logical_size_rounded_up() {
        assert_eq!(viewport_destination(Size::new(800.0, 600.0)), (800, 600));
        assert_eq!(viewport_destination(Size::new(700.2, 500.8)), (701, 501));
        assert_eq!(viewport_destination(Size::new(0.0, 0.0)), (1, 1));
    }
}

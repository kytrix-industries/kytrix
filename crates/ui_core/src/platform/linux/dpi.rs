use crate::{
    error::EngineError,
    math::{DevicePixelRatio, DeviceSize, LogicalPoint, LogicalRect, LogicalSize, dp, px},
};

pub(crate) fn scale_from_120(scale: u32) -> Result<DevicePixelRatio, EngineError> {
    if scale == 0 {
        return Err(EngineError::InvalidDpiScale { factor: 0.0 });
    }
    let factor = scale as f32 / 120.0;
    DevicePixelRatio::new(factor).ok_or(EngineError::InvalidDpiScale { factor })
}

pub(crate) fn physical_size(size: LogicalSize, scale: DevicePixelRatio) -> (u32, u32) {
    let logical = LogicalRect::from_origin_size(LogicalPoint::ZERO, size);
    let device = if logical.is_convertible() {
        scale.to_device_size(size)
    } else {
        DeviceSize::ZERO
    };
    (
        device.width.max(dp(1)).value() as u32,
        device.height.max(dp(1)).value() as u32,
    )
}

pub(crate) fn viewport_destination(size: LogicalSize) -> (i32, i32) {
    (
        size.width.max(px(1.0)).value().ceil() as i32,
        size.height.max(px(1.0)).value().ceil() as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::{physical_size, viewport_destination};
    use crate::math::{DevicePixelRatio, LogicalSize, px};

    fn scale(factor: f32) -> DevicePixelRatio {
        DevicePixelRatio::new(factor).expect("test scale must be valid")
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
                physical_size(LogicalSize::new(px(800.0), px(600.0)), scale(factor)),
                expected,
                "scale {factor}"
            );
        }

        assert_eq!(
            physical_size(LogicalSize::new(px(701.0), px(501.0)), scale(1.25)),
            (876, 626)
        );
        assert_eq!(
            physical_size(LogicalSize::new(px(3.0), px(5.0)), scale(1.5)),
            (5, 8)
        );
        assert_eq!(
            physical_size(LogicalSize::new(px(0.0), px(0.0)), scale(1.0)),
            (1, 1)
        );
        assert_eq!(
            physical_size(
                LogicalSize::new(px(f32::NAN), px(f32::INFINITY)),
                scale(1.25)
            ),
            (1, 1)
        );
    }

    #[test]
    fn viewport_destination_is_the_logical_size_rounded_up() {
        assert_eq!(
            viewport_destination(LogicalSize::new(px(800.0), px(600.0))),
            (800, 600)
        );
        assert_eq!(
            viewport_destination(LogicalSize::new(px(700.2), px(500.8))),
            (701, 501)
        );
        assert_eq!(
            viewport_destination(LogicalSize::new(px(0.0), px(0.0))),
            (1, 1)
        );
    }
}

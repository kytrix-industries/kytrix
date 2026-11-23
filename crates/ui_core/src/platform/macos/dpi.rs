use crate::{error::EngineError, math::DevicePixelRatio};

pub(crate) fn scale_from(backing_scale_factor: f64) -> Result<DevicePixelRatio, EngineError> {
    let factor = backing_scale_factor as f32;
    DevicePixelRatio::new(factor).ok_or(EngineError::InvalidDpiScale { factor })
}

pub(crate) fn dpi_from_scale(scale: DevicePixelRatio) -> u32 {
    (scale.factor() * 96.0).round().max(48.0) as u32
}

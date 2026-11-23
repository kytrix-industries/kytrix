use crate::{error::EngineError, math::Scale};

pub(crate) fn scale_from(backing_scale_factor: f64) -> Result<Scale, EngineError> {
    let factor = backing_scale_factor as f32;
    Scale::new(factor).ok_or(EngineError::InvalidDpiScale { factor })
}

pub(crate) fn dpi_from_scale(scale: Scale) -> u32 {
    (scale.factor() * 96.0).round().max(48.0) as u32
}

//! DPI：`backingScaleFactor` → 引擎 `Scale`。

use crate::{error::EngineError, math::Scale};

/// `backingScaleFactor`（`f64`）→ `Scale`；非有限或 `<= 0` ⇒ `InvalidDpiScale`。
pub(crate) fn scale_from(backing_scale_factor: f64) -> Result<Scale, EngineError> {
    let factor = backing_scale_factor as f32;
    Scale::new(factor).ok_or(EngineError::InvalidDpiScale { factor })
}

/// `NSEvent` 语义下的 DPI 换算值（`Event::DpiChanged.dpi` 的 96 基点，只为与 Win32 字段对齐）。
pub(crate) fn dpi_from_scale(scale: Scale) -> u32 {
    (scale.factor() * 96.0).round().max(48.0) as u32
}

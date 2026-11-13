//! DPI：`backingScaleFactor` → 引擎 `Scale`（`design/phase-12-macos-linux.md` §7.4）。

use crate::{
    error::EngineError,
    math::{Rect, Scale, Size},
};

/// `backingScaleFactor`（`f64`）→ `Scale`；非有限或 `<= 0` ⇒ `InvalidDpiScale`。
pub(crate) fn scale_from(backing_scale_factor: f64) -> Result<Scale, EngineError> {
    let factor = backing_scale_factor as f32;
    Scale::new(factor).ok_or(EngineError::InvalidDpiScale { factor })
}

/// 逻辑尺寸 × scale → 物理像素（covering，至少 1 px）。
pub(crate) fn physical_size(size: Size, scale: Scale) -> (u32, u32) {
    let rect = Rect::new(0.0, 0.0, size.width, size.height).to_physical_covering(scale);
    (rect.width().max(1) as u32, rect.height().max(1) as u32)
}

/// `NSEvent` 语义下的 DPI 换算值（`Event::DpiChanged.dpi` 的 96 基点，只为与 Win32 字段对齐）。
pub(crate) fn dpi_from_scale(scale: Scale) -> u32 {
    (scale.factor() * 96.0).round().max(48.0) as u32
}

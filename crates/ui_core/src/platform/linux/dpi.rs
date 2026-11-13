//! DPI：`wp_fractional_scale_v1`（120 分之几）+ 整数倍率回退（`design/phase-12-macos-linux.md` §8.4）。

use crate::{
    error::EngineError,
    math::{Rect, Scale, Size},
};

/// `preferred_scale`（120 = 1.0）→ `Scale`。
///
/// 非正数 ⇒ `InvalidDpiScale`（compositor 不该发 0，但协议层是外部输入）。
pub(crate) fn scale_from_120(scale: u32) -> Result<Scale, EngineError> {
    if scale == 0 {
        return Err(EngineError::InvalidDpiScale { factor: 0.0 });
    }
    let factor = scale as f32 / 120.0;
    Scale::new(factor).ok_or(EngineError::InvalidDpiScale { factor })
}

/// 逻辑尺寸 × scale → 物理像素（**向外**取整，至少 1 px；契约 §12.3）。
pub(crate) fn physical_size(size: Size, scale: Scale) -> (u32, u32) {
    let rect = Rect::new(0.0, 0.0, size.width, size.height).to_physical_covering(scale);
    (rect.width().max(1) as u32, rect.height().max(1) as u32)
}

/// `wp_viewport.set_destination` 的尺寸（逻辑尺寸向上取整；compositor 负责缩放）。
pub(crate) fn viewport_destination(size: Size) -> (i32, i32) {
    (
        size.width.max(1.0).ceil() as i32,
        size.height.max(1.0).ceil() as i32,
    )
}

//! DPI：`wp_fractional_scale_v1`（120 分之几）；整数倍率回退未实现（V-110 / V-122，
//! 见 `design/phase-12-macos-linux.md` §8.4）。

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

/// 逻辑尺寸 × scale → 物理像素（**round**，half away from zero；至少 1 px）。
///
/// `wp_fractional_scale_v1` 规定 toplevel 的 buffer 尺寸按 `round(logical × scale)` 计算：
/// 向外取整会让 1.25 / 1.75 下的 buffer 大 1 px，整面被 compositor 以 0.999× 重采样。
pub(crate) fn physical_size(size: Size, scale: Scale) -> (u32, u32) {
    let rect = Rect::new(0.0, 0.0, size.width, size.height).to_physical_rounded(scale);
    (rect.width().max(1) as u32, rect.height().max(1) as u32)
}

/// `wp_viewport.set_destination` 的尺寸（逻辑尺寸向上取整；compositor 负责缩放）。
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

        // 1.25 下的奇数逻辑尺寸：701 × 1.25 = 876.25 → 876（ceil 会是 877）。
        assert_eq!(
            physical_size(Size::new(701.0, 501.0), scale(1.25)),
            (876, 626)
        );
        // 正好 .5：远离零（4.5 → 5、7.5 → 8）。
        assert_eq!(physical_size(Size::new(3.0, 5.0), scale(1.5)), (5, 8));
        // 退化尺寸至少 1 px（不能给 swapchain 0 尺寸）。
        assert_eq!(physical_size(Size::new(0.0, 0.0), scale(1.0)), (1, 1));
        assert_eq!(
            physical_size(Size::new(f32::NAN, f32::INFINITY), scale(1.25)),
            (1, 1)
        );
    }

    #[test]
    fn viewport_destination_is_the_logical_size_rounded_up() {
        assert_eq!(viewport_destination(Size::new(800.0, 600.0)), (800, 600));
        // 小数逻辑尺寸：向上取整（否则最右/最下 1 px 被裁掉）。
        assert_eq!(viewport_destination(Size::new(700.2, 500.8)), (701, 501));
        // 至少 1 px。
        assert_eq!(viewport_destination(Size::new(0.0, 0.0)), (1, 1));
    }
}

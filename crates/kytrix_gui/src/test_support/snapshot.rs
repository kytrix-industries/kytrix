//! 像素快照的接口（P08 在 Windows 上用 WARP 真渲染；其余平台仍 `NoRenderer`）。
//!
//! 无 GPU 时返回 [`SnapshotError::NoRenderer`]（不是兜底路径：D8 禁止软件回退，这里只是把
//! 「本构建没有渲染器」表达成**可断言**的错误）；GPU 路径失败用 [`SnapshotError::Gpu`] 与
//! 「没有渲染器」区分开（I10）。

use crate::math::{Scale, Size};

/// 一次快照请求（合成场景 + 分辨率 + scale）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapshotCase {
    /// 逻辑尺寸。
    pub size: Size,
    /// 设备比例。
    pub scale: Scale,
}

impl Default for SnapshotCase {
    fn default() -> Self {
        Self {
            size: Size::new(800.0, 600.0),
            scale: Scale::ONE,
        }
    }
}

/// 读回的 RGBA8 图像（P09 的 WARP / lavapipe / Metal 实现产出）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotImage {
    /// 像素宽。
    pub width: u32,
    /// 像素高。
    pub height: u32,
    /// `width * height * 4` 字节。
    pub rgba8: Vec<u8>,
}

/// 快照失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// 本构建没有渲染器（非 Windows 构建；headless 的默认）。
    NoRenderer,
    /// GPU 路径失败（设备创建 / 渲染 / 读回）。
    Gpu {
        /// 可读的诊断（含 HRESULT 或具体阶段）。
        detail: smol_str::SmolStr,
    },
}

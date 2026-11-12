//! 像素快照的接口（GPU 实现留 P09）。
//!
//! P06 没有 GPU：`TestApp::snapshot` 恒返回 [`SnapshotError::NoRenderer`]。这不是兜底路径
//! （D8 禁止软件回退），而是把「无设备」表达成**可断言**的错误，避免调用方把空图当成功。

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// 本构建没有渲染器（P06 的 headless 默认；P09 之前恒为此值）。
    NoRenderer,
}

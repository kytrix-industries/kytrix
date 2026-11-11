//! 绘制基础层：唯一的数据契约（I2）。
//!
//! 本模块是本 crate 的**公共模块根**，只做模块声明（无 item、无 re-export，见
//! `design/phase-02-paint-foundation.md` §3.1 / §3.3）：
//!
//! - 公开：`color`（D23 引擎一等颜色）、`primitive`（8 类绘制指令）、`canvas`（应用层唯一公开写入口）、
//!   `geometry`（静态几何描述）、`atlas`（图集引用数据）。
//! - crate 私有：`scene`（P02 只含 `Lane` 与 `MAX_CLIP_DEPTH`；`Scene` 在 P03 落地）。

pub mod atlas;
pub mod canvas;
pub mod color;
pub mod geometry;
pub mod primitive;

pub(crate) mod scene;

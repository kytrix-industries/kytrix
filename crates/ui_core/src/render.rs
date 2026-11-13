//! 渲染共享层（crate 私有；应用层不可见，I9）。
//!
//! 本文件只做模块声明与 crate 内出口（P01 §4.1「crate 私有模块根」模板）：**不定义任何类型**。
//!
//! 共享层（`atlas` / `compositor` / `damage` / `frame` / `retire` / `timer`）是平台无关的纯数据 + 纯函数，
//! **三平台无条件编译**；平台实现按 `#[cfg(target_os)]` 编译期选定恰好一份（I14，无 trait / 无枚举）。
//!
//! 设计见 `design/phase-08-d3d12.md` §3.1、`design/phase-12-macos-linux.md` §3.1 / §4.2（S-123 收口）。

pub(crate) mod atlas;
pub(crate) mod compositor;
pub(crate) mod damage;
pub(crate) mod frame;
pub(crate) mod retire;
pub(crate) mod stats;
pub(crate) mod timer;

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(windows)]
pub(crate) mod windows;

pub(crate) use atlas::{Allocation, AtlasAllocator, DirtyRows};
#[cfg(target_os = "linux")]
pub(crate) use linux as backend;
#[cfg(target_os = "macos")]
pub(crate) use macos as backend;
pub(crate) use stats::RenderStats;
#[cfg(windows)]
pub(crate) use windows as backend;

//! 渲染共享层（crate 私有；应用层不可见，I9）。
//!
//! 本文件只做模块声明与 crate 内出口（P01 §4.1「crate 私有模块根」模板）：**不定义任何类型**。
//! 落地范围：`atlas`（P05）、`damage`（P06）、`device`/`frame`/`retire`/`compositor`/`timer`（P08 共享层）、
//! `windows`（P08 唯一的平台实现；`macos`/`linux` 由 P11/P17 追加同形模块）。
//!
//! 共享层模块在 P08 **只在 Windows 上启用**（`#[cfg(windows)]`）：其余平台还没有后端消费者，
//! 无条件编译会让每个 `pub(crate)` item 成为 `dead_code`。P11/P17 落地各自后端时同步解除
//! （`design/phase-08-d3d12.md` §3.1、DEVIATIONS S-78）。
//!
//! 设计见 `design/phase-08-d3d12.md`、`design/phase-05-text.md` §9、
//! `design/phase-02-paint-foundation.md` §10 与 `design/phase-06-paint-runtime.md` §8。

pub(crate) mod atlas;
pub(crate) mod damage;

#[cfg(windows)]
pub(crate) mod compositor;
#[cfg(windows)]
pub(crate) mod device;
#[cfg(windows)]
pub(crate) mod frame;
#[cfg(windows)]
pub(crate) mod retire;
#[cfg(windows)]
pub(crate) mod timer;

#[cfg(windows)]
pub(crate) mod windows;
pub(crate) use atlas::{Allocation, AtlasAllocator, AtlasSet, DirtyRows};
#[cfg(windows)]
pub(crate) use windows as backend;

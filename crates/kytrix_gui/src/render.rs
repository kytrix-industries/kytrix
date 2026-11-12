//! 渲染共享层（crate 私有；应用层不可见，I9）。
//!
//! 本文件只做模块声明与 crate 内出口（P01 §4.1「crate 私有模块根」模板）：**不定义任何类型**。
//! 落地范围：`atlas`（P05）；`damage` → P06，`windows`/`macos`/`linux`/`device`/`frame`/`retire`/`timer` → P09+。
//!
//! 设计见 `design/phase-05-text.md` §9 与 `design/phase-02-paint-foundation.md` §10。

pub(crate) mod atlas;

pub(crate) use atlas::{Allocation, AtlasAllocator, AtlasSet, DirtyRows};

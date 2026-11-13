//! Windows D3D12 后端（P08 唯一的平台实现；`macos`/`linux` 由 P11/P17 追加同形模块）。
//!
//! 本文件只做模块声明与 crate 内出口（P01 §4.1 的模块根模板）：**不定义任何类型**
//! （workspace lint `definition_in_module_root`）；`Renderer` 的实现在 [`compositor`]。
//!
//! 设计见 `design/phase-08-d3d12.md` §5。

pub(crate) mod atlas;
pub(crate) mod compositor;
pub(crate) mod device;
pub(crate) mod pipeline;
pub(crate) mod queue;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod readback;
pub(crate) mod swapchain;
pub(crate) mod timer;

pub(crate) use compositor::Renderer;

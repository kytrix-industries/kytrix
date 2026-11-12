//! Linux Vulkan 后端（`design/phase-12-macos-linux.md` §6）。
//!
//! 本文件只做模块声明与 crate 内出口（P01 §4.1 的模块根模板）：**不定义任何类型**
//! （workspace lint `definition_in_module_root`）；`Renderer` 的实现在 [`compositor`]。

pub(crate) mod atlas;
pub(crate) mod compositor;
pub(crate) mod device;
pub(crate) mod instance;
pub(crate) mod pipeline;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod readback;
pub(crate) mod swapchain;
pub(crate) mod timer;

pub(crate) use compositor::Renderer;

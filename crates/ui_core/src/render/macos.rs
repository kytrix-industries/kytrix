//! macOS Metal 后端（`design/phase-12-macos-linux.md` §5）。
//!
//! 本文件只做模块声明与 crate 内出口（P01 §4.1 的模块根模板）：**不定义任何类型**
//! （workspace lint `definition_in_module_root`）；`Renderer` 的实现在 [`compositor`]。

pub(crate) mod atlas;
pub(crate) mod compositor;
/// 进程级 GPU 单例（device / metallib / PSO 缓存 / 采样器）：R09-5 / R09-13。
pub(crate) mod core;
pub(crate) mod device;
/// 静态几何的 GPU 缓冲（按需创建；`GeometryStore` 的 Metal 侧缓存）。
pub(crate) mod geometry;
pub(crate) mod pipeline;
pub(crate) mod queue;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod readback;
pub(crate) mod renderer;
pub(crate) mod swapchain;
pub(crate) mod timer;

pub(crate) use renderer::Renderer;

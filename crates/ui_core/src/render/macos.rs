//! macOS Metal 后端（`design/phase-12-macos-linux.md` §5）。
//!
//! 本文件只做模块声明与 crate 内出口（P01 §4.1 的模块根模板）：**不定义任何类型**
//! （workspace lint `definition_in_module_root`）；`Renderer` 的实现在 [`compositor`]。

pub(crate) mod atlas;
/// 合成线程的帧时钟源（display link 目标时刻 + 单调间隔兜底）：B6-2。
pub(crate) mod clock;
pub(crate) mod compositor;
/// 进程级 GPU 单例（device / metallib / PSO 缓存 / 采样器）：R09-5 / R09-13。
pub(crate) mod core;
pub(crate) mod device;
/// 静态几何的 GPU 缓冲（按需创建；`GeometryStore` 的 Metal 侧缓存）。
pub(crate) mod geometry;
pub(crate) mod pipeline;
/// drawable 上屏的**唯一**入口 + 「present 必须在 GPU 线程、且不带显式呈现时刻」的断言
/// （`CAMetalDisplayLink` 独占呈现时刻）：P0 回归防线。
pub(crate) mod present;
pub(crate) mod queue;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod readback;
pub(crate) mod renderer;
pub(crate) mod swapchain;
pub(crate) mod timer;

pub(crate) use renderer::Renderer;

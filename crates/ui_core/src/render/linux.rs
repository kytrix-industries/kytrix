//! Linux Vulkan 后端（`design/phase-12-macos-linux.md` §6、`design/async-render-pipeline.md` §7）。
//!
//! 本文件只做模块声明与 crate 内出口（P01 §4.1 的模块根模板）：**不定义任何类型**
//! （workspace lint `definition_in_module_root`）。
//!
//! - [`renderer`]：窗口门面 + 每窗口一条 `kytrix-vulkan` GPU 线程（§7.1 / §4.5）。
//! - [`compositor`]：GPU 线程独占的 `GpuRenderer`（device / swapchain / persist / 录制 / retire）。
//! - [`readback`]：离屏读回，只有 `test-support` 构建存在（快照 / golden）。

pub(crate) mod atlas;
/// 合成线程的帧时钟源（`wl_surface.frame` 回调 + 单调时钟推算）：B6-2。
pub(crate) mod clock;
pub(crate) mod compositor;
pub(crate) mod device;
/// 静态几何的 GPU 缓冲（按需创建；GeometryStore 的 Vulkan 侧缓存）。
pub(crate) mod geometry;
pub(crate) mod instance;
/// 设备内存：类型选择策略 + 两级子分配器（R11-6）。
pub(crate) mod memory;
pub(crate) mod pipeline;
/// swapchain / 设备选择的纯决策（R11-7 / R11-8 / R11-9 / R11-13 / R11-15）。
pub(crate) mod plan;
#[cfg(feature = "test-support")]
pub(crate) mod readback;
pub(crate) mod renderer;
pub(crate) mod swapchain;
pub(crate) mod timer;

pub(crate) use renderer::Renderer;

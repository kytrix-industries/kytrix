//! Linux Vulkan 后端（`design/phase-12-macos-linux.md` §6、`design/async-render-pipeline.md` §7）。
//!
//! 本文件只做模块声明与 crate 内出口（P01 §4.1 的模块根模板）：**不定义任何类型**
//! （workspace lint `definition_in_module_root`）。
//!
//! - [`renderer`]：窗口门面 + 每窗口一条 `kytrix-vulkan` GPU 线程（§7.1 / §4.5）。
//! - [`compositor`]：GPU 线程独占的 `GpuRenderer`（device / swapchain / persist / 录制 / retire）。
//! - [`readback`]：离屏读回，只有 `test-support` 构建存在（快照 / golden）。

pub(crate) mod atlas;
pub(crate) mod compositor;
pub(crate) mod device;
pub(crate) mod instance;
pub(crate) mod pipeline;
#[cfg(feature = "test-support")]
pub(crate) mod readback;
pub(crate) mod renderer;
pub(crate) mod swapchain;
pub(crate) mod timer;

pub(crate) use renderer::Renderer;

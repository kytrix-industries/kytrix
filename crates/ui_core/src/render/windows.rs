//! Windows D3D12 后端（P08 唯一的平台实现；`macos`/`linux` 由 P11/P17 追加同形模块）。
//!
//! 本文件只做模块声明与 crate 内出口（P01 §4.1 的模块根模板）：**不定义任何类型**
//! （workspace lint `definition_in_module_root`）。
//!
//! - [`renderer`]：窗口门面 + 每窗口一条 `kytrix-d3d12` GPU 线程。
//! - [`compositor`]：`GpuRenderer`，**两层**持有 —— 进程级共享部分（`SharedGpu`：设备 /
//!   命令队列 / 根签名 + PSO / 共享图集纹理）与每窗口部分（swapchain / DirectComposition
//!   对象 / persist 内容 / 提交流 / fence）。
//!

pub(crate) mod atlas;
pub(crate) mod compositor;
pub(crate) mod debug;
pub(crate) mod device;
pub(crate) mod diagnostics;
/// 显示 / 电源状态变化的恢复信号（R07-14）：平台层置位，GPU 线程消费（重建目标 + 整帧重发）。
pub(crate) mod display;
pub(crate) mod hresult;
pub(crate) mod pipeline;
pub(crate) mod queue;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod readback;
pub(crate) mod renderer;
pub(crate) mod swapchain;
pub(crate) mod timer;

pub(crate) use renderer::Renderer;

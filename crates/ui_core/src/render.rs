//! 渲染共享层（crate 私有；应用层不可见，I9）。
//!
//! 本文件只做模块声明与 crate 内出口（P01 §4.1「crate 私有模块根」模板）：**不定义任何类型**。
//!
//! 共享层（`atlas` / `compositor` / `damage` / `frame` / `retire` / `timer`）是平台无关的纯数据 + 纯函数，
//! **三平台无条件编译**；平台实现按 `#[cfg(target_os)]` 编译期选定恰好一份（I14，无 trait / 无枚举）。
//!

pub(crate) mod atlas;
/// 背景色编码（三平台消费：Windows 用 sRGB 字节、macOS 用 layer + linear、Linux 用 linear；
/// 各平台未使用的编码函数按 `cfg` 不参与编译 —— 避免 dead_code）。
pub(crate) mod color;
/// 呈现色彩空间的**唯一**声明点（三端只做翻译，不得各自决定；R06-10）。
pub(crate) mod colorspace;
/// 每窗口的合成器组件（`SceneDelta` → 增量编码 → `FramePayload`）：B6-2。
pub(crate) mod compose;
pub(crate) mod compositor;
/// 合成线程（进程内 1 条）+ 输入邮箱 + `FrameSink` 注册：B6-2。
pub(crate) mod compositor_thread;
pub(crate) mod damage;
/// `SceneDelta` 协议（UI → 合成线程）：B6-2。
pub(crate) mod delta;
pub(crate) mod frame;
/// 上屏证据与呈现节奏的**平台无关**逻辑（R3 / A4）：`presented_at` 的三个平台来源
/// （macOS display-link tick 推断 / Windows `GetFrameStatistics` / Linux `wp_presentation`）、
/// 下界钳制与反馈发布顺序。三平台无条件编译（各端专用项 `cfg` 门控）；**所有平台的
/// `cfg(test)` 构建**都编译并运行它的单元测试，让判据在开发机（Windows）可执行。
pub(crate) mod present_evidence;
pub(crate) mod retire;
/// 三套 shader 的构建期契约校验（R06-5）的唯一实现：`build.rs` 用 `include!` 编进去执行，
/// 单元测试（`#[cfg(test)]`）覆盖它的**失败路径**。生产构建不引用它的任何符号。
#[cfg(test)]
mod shader_contract;
pub(crate) mod stats;
pub(crate) mod timer;
/// `RootUniform`（112 B）的唯一 CPU 构造点（三平台共用；contract.md §6）。
pub(crate) mod uniform;

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(windows)]
pub(crate) mod windows;

pub(crate) use atlas::{Allocation, AtlasAllocator, DirtyRects, MIN_ATLAS_PAGE};
#[cfg(target_os = "linux")]
pub(crate) use linux as backend;
#[cfg(target_os = "macos")]
pub(crate) use macos as backend;
pub(crate) use stats::RenderStats;
#[cfg(windows)]
pub(crate) use windows as backend;

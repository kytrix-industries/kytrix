//! Kytrix 自研跨平台 GUI 引擎。
//!
//! 本文件只做模块声明（AGENTS §5.5）；公共 API 通过模块路径暴露，不做 re-export
//! （workspace lint `pub_use = "forbid"`，见 `design/phase-01-foundation.md` §4.3）。

pub mod a11y;
pub mod app;
pub mod error;
pub mod event;
pub mod interaction;
pub mod layout;
pub mod math;
pub mod node;
pub mod paint;
pub mod resources;
pub mod text;
pub mod window;

/// 测试基建（`TestApp`/`Probe`/帧记录/oracle）。**只在 `test-support` feature 下编译**，
/// 不进发布物（I7 / `design/phase-06-paint-runtime.md` §10）。
#[cfg(feature = "test-support")]
pub mod test_support;

#[cfg(feature = "hud")]
mod debug_hud;
mod platform;
mod render;

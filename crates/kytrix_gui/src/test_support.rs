//! 测试基建（`feature = "test-support"`；**不进发布物**，I7 / 设计 §6.6）。
//!
//! 入口：`test_support::app::TestApp`（无 GPU 的帧驱动）、`test_support::probe::{Probe, Selector}`
//! （L1 元素探针）、`test_support::record::{FrameRecord, RenderedFrame, PaintLog}`（L2 帧记录）、
//! `event::InputOutcome`（注入结果）、`test_support::snapshot`（L3 接口，GPU 实现留 P09）。
//!
//! 本文件只做模块声明：`pub use` 被 workspace lint 禁止（M-02），公开路径是
//! `kytrix_gui::test_support::app::TestApp` 这类模块限定形式。

pub mod app;
pub mod diagnostics;
pub mod probe;
pub mod record;
pub mod snapshot;

//! 测试基建（`feature = "test-support"`；**不进发布物**，I7 / 设计 §6.6）。
//!
//! 入口：`test_support::app::TestApp`（无 GPU 的帧驱动）、`test_support::probe::{Probe, Selector}`
//! （L1 元素探针）、`test_support::record::{FrameRecord, RenderedFrame, PaintLog}`（L2 帧记录）、
//! `test_support::script::{FrameScript, ScriptFrame}`（L2.5 多帧脚本执行器 + 逐帧断言）、
//! `test_support::acceptance`（最终验收合成负载的缩比夹具 + 增量不变量）、
//! `test_support::a11y`（L2.6 `TreeUpdate` 影子树 / 契约校验 / 机器可读快照 / 断言 helper）、
//! `event::InputOutcome`（注入结果）、`test_support::snapshot`（L3 golden + 容差比较）。
//!
//! 本文件只做模块声明：`pub use` 被 workspace lint 禁止（M-02），公开路径是
//! `ui_core::test_support::app::TestApp` 这类模块限定形式。

pub mod a11y;
pub mod acceptance;
pub mod app;
pub mod diagnostics;
pub mod probe;
pub mod record;
pub mod script;
pub mod snapshot;

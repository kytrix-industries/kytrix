//! 事件循环的单元测试（拆分自 `event_loop.rs`，测试代码除路径外未改动）。
//!
//! 自绘标题栏拖动 / 边框缩放的状态机已随 R07-2 删除（非客户区交互完全交系统模态循环），
//! 对应的 `caption_tests` / `resize_tests` 一并移除 —— 它们测的行为已经不存在。

mod wndproc_tests;

//! 事件循环的单元测试（拆分自 `event_loop.rs`，测试代码除路径外未改动）。
//!
//! 非客户区交互的分工（R07-2 / P-A）：caption 拖动与系统按钮交给 `DefWindowProcW` 的 move
//! 模态循环，8 个 resize 命中码由引擎自持（`resize.rs`）。因此这里保留
//! `nonclient_hit_table_returns_system_hit_codes`（命中码必须仍由系统给），并新增自持缩放
//! 会话的状态机用例（`wndproc_tests` 里的 `resize_*`）。

mod wndproc_tests;

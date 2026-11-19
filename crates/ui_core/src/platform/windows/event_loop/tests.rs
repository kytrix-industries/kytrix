//! 事件循环的单元测试（拆分自 `event_loop.rs`，测试代码除路径外未改动）。
//!
//! 非客户区交互全部交系统（R07-2）：caption 拖动、系统按钮与 8 个 resize 命中码都由
//! `DefWindowProcW` 的模态循环处理。因此这里保留 `nonclient_hit_table_returns_system_hit_codes`
//! （命中码必须仍由系统给），以及 size 模态循环里成帧驱动的用例（`wndproc_tests` 里的
//! `size_move_*`：`WM_ENTERSIZEMOVE` + `WM_SIZE` 必须走出一帧真实帧）。
//!
//! 系统模态循环本身**无法**在单测里进入（`SendMessage(WM_NCLBUTTONDOWN)` 会阻塞直到用户
//! 松手），所以「系统真的进入了 size 模态循环」这条接线由真机拖动探针
//! `.review/diag/verify_native_resize.ps1` 验收。

mod wndproc_tests;

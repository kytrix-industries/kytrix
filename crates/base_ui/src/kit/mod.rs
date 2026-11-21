//! 引擎化（`ui_core`）组件模块树。
//!
//! 迁移期的模块级增量：每个子模块都是"旧 gpui 组件 → 引擎节点树"的一对一替代品，旧模块
//! （`crate::markdown` / `crate::input` / …）在 P2-5 收口时删除。本模块树**只依赖
//! `ui_core`、`smol_str`、`comrak` 等非 gpui 依赖**，任何 gpui 类型都不得出现在这里。
//!
//! 本批次（P2-3）追加：`markdown`（解析 + 行内流 + 视图）与 `input`（应用层文本编辑状态机）。

pub mod input;
pub mod markdown;

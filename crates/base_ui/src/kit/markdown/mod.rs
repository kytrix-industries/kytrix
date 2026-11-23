//! markdown 子系统（引擎化）：comrak 解析 → 只读 IR → 引擎富文本节点树。
//!
//! 数据流（与旧 `crate::markdown` 一一对应，但整条链路不依赖 旧 UI 层）：
//!
//! ```text
//! parse::parse_document(&str, &dyn MarkdownAssets) -> document::MarkdownDocument
//!        │
//!        ├─ inline_flow::flow_block(..)  → 引擎 TextContent{runs, inline_boxes} 的行内模型
//!        └─ view::build(..)              → 引擎节点树（每块一个节点）
//! ```
//!
//! - [`parse`] / [`source`] / [`document`]：纯逻辑（comrak AST → IR、`file:` 图片 URI、源映射），
//!   与旧实现语义等价，只换类型（`SmolStr` / 引擎 `Size`）。
//! - [`inline_flow`]：行内片段 → 引擎富文本（`TextRun` 表达粗/斜/链接色、`TextDecoration` 表达
//!   下划线/删除线、`InlineBoxSpec` 表达行内图片）；链接命中用引擎 E5 的
//!   `Window::text_byte_index_at` + 本模块自持的字节区间。
//! - [`view`]：IR → 节点树；样式取自 `crate::theme` 与 `crate::style_builder`。

pub mod document;
pub mod inline_flow;
pub mod parse;
pub mod source;
pub mod view;

#[cfg(test)]
mod parse_tests;

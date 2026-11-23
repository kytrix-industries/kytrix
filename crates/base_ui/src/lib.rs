//! `base_ui`：引擎化 UI 基座（P5-2 收口后的唯一模块树）。
//!
//! - [`kit`]：组件库（55 个图标 / 7 个品牌 / 按钮·浮层·列表·表格·树·输入 / markdown / 对话框…）；
//! - [`theme`]：调色板、语义角色、组件颜色配方与度量 token；
//! - [`style_builder`]：布局样式与绘制样式的构建器。
//!
//! 旧顶层组件模块（`crate::button`、`crate::dialog`、`crate::style`、`crate::markdown`…）已在
//! P5-2 删除；本 crate 不再依赖旧组件库，只依赖引擎 `ui_core` 与非 UI 框架依赖。资产的构建期
//! 光栅化见 `build.rs`，运行期接线见 [`kit::assets`] / [`kit::icon`]。

pub mod kit;
pub mod style_builder;
pub mod theme;

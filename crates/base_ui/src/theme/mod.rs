//! 主题与颜色基座（引擎化）：调色板、语义角色、组件颜色配方与度量 token。
//!
//! 这是 `base_ui` 全量迁移的地基批次（P2-1a）。与旧 `crate::style` 并存是**模块级增量迁移**
//! 的用户裁决：旧 `style/` 仍由 gpui-kit 支撑其它未迁移组件使用，P2-5 迁完后删除；新模块只依赖
//! 引擎 `ui_core`，**不引入任何 gpui 类型**。
//!
//! 数据流（与设计 §4.9 / §5.8 的分层一致）：
//!
//! ```text
//! Palette（手写 OKLCH）──resolve_roles──▶ RoleColors（64 个引擎 Color）
//!        │                                        │
//!        └──resolve──▶ ComponentColors（组件颜色配方）◀── ComponentTokens（度量）
//!                                                 │
//!                                       Theme（appearance + palette + roles + components + tokens）
//!                                                 │
//!                                       ThemeHandle（Signal<Appearance> + 两套 Theme 缓存）
//! ```
//!
//! 颜色一律是引擎的 [`ui_core::paint::color::Color`]：调色板数据以 OKLCH 分量给出（与旧
//! `style/palette.rs` 数值逐位一致），角色解析在应用层用 `color` crate 做 OKLab 混合（设计 §5.8.1
//! 明确 Oklab 混合留在应用层），挂载期再经引擎 `ColorResolver` 解析成线性预乘 RGBA。

pub mod appearance;
pub mod color;
pub mod components;
pub mod handle;
pub mod palette;
pub mod resolved;
pub mod role;
pub mod roles;

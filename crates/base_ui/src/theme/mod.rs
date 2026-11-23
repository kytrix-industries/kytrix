//! 主题与颜色基座（引擎化）：调色板、语义角色、组件颜色配方与度量 token。
//!
//! 本模块是 P2-1a 建立、P5-2 收口后的唯一主题面：旧 `style/` 已删除，调色板数值与角色解析
//! 规则原样保留（冻结基线见各模块测试），只依赖引擎 `ui_core`，**不引入任何 UI 框架类型**。
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

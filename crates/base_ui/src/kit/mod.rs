//! base_ui 的引擎化基座（迁移新模块树 `kit/`）。
//!
//! # 迁移状态（P2 批次，2026-11-21）
//!
//! 本模块树是 `base_ui` 从 `gpui-kit` 迁到自研引擎 `ui_core` 的**目标 API**，当前处于
//! 「新旧并存」期：本文件公开的 `icon` / `assets` / `brand` / `size` / `format` / `motion` /
//! `animation` / `join` 是引擎化版本；旧顶层模块（`crate::icon`、`crate::assets`、…）由
//! 尚未迁移的 gpui 组件继续使用，P2-5 迁完后删除。按 AGENTS §1.1，这里**不提供**任何兼容层 /
//! 转发函数 —— 两个模块树各自独立，组件迁移时整模块切换过去。
//!
//! `test_support` 只在测试构建里存在（`#[cfg(test)]`）：它依赖 `ui_core` 的 `test-support`
//! feature，而该 feature 由 `base_ui` 的 dev-dependencies 打开，发布物里不能引用。
//!
//! # 本批次交付的模块
//!
//! | 模块 | 职责 | 旧实现 |
//! |---|---|---|
//! | [`icon`] | 55 个 Phosphor 图标的枚举 / 名称 / 两档构建期 PNG / E7 图片节点挂载（`Fill` + 颜色调制） | `crate::icon` |
//! | [`assets`] | 品牌图与 Widget 预览图的注册表、路径查找、引擎异步解码接线、E7 pending 节点来源 | `crate::assets` |
//! | [`brand`] | 7 个品牌 logo（路径 / 字节 / 固有尺寸 / `Contain` 图片节点 / 可选 alt） | `crate::brand` |
//! | [`size`] | 控件尺寸阶梯（读主题 `ComponentTokens`，不再重复维护数值） | `crate::size` |
//! | [`format`] | 数字分位格式化（纯文本） | `crate::format` |
//! | [`motion`] | 深度条缓动的响应曲线（纯数学，不建帧循环） | `crate::motion` |
//! | [`animation`] | 弹层淡出是否该出帧 + 动画 tick（消隐走引擎 `Transient`） | `crate::animation` |
//! | [`join`] | 拼接控件共享边角的几何规则（纯逻辑） | `crate::join` |
//! | `test_support` | 基于 `ui_core::test_support` 的测试断言辅助（TestApp/Probe/FrameRecord） | 无（新基建） |
//!
//! # 设计约束（为什么这些模块在应用层）
//!
//! 设计 §8.5 / §10.2 把「资源文件、图标转换、主题语义色」划在应用层：引擎只提供
//! `ResourceCache`（字节 → 解码 → 图集 tile）与 E7 的 `NodeKind::Image`（`ImageFit` / 调制 /
//! 异步到达 / DPI 重解析）两个原语，不依赖 `usvg` / `resvg`。因此图标是**构建期**光栅化的 PNG
//! （`build.rs`），运行期只把它们放进引擎图集，再挂成 E7 图片节点并按主题色调制。
//!
//! 动画同样不在这里建帧循环：`motion` 只给曲线，`animation` 只给「弹层这一帧要不要画」的判据；
//! 真正的动画必须注册到引擎 `App::animate`（`Transient` + 显式节点集合），见设计 §3.7 / §10.3。

pub mod animation;
pub mod assets;
pub mod brand;
pub mod format;
pub mod icon;
pub mod join;
pub mod motion;
pub mod size;

/// 测试断言辅助。**只在测试构建里存在**（依赖 `ui_core/test-support`，见本文件模块文档）。
#[cfg(test)]
pub mod test_support;

//! 节点树与状态：D16 的持久节点 + 细粒度依赖追踪（P03 落地）。
//!
//! 本模块是本 crate 的**公共模块根**，只做模块声明（无 item、无 re-export，见
//! `design/phase-03-node-state.md` §3.1 / §3.3）：
//!
//! - `builder`：`NodeBuilder`（声明式建树入口）
//! - `dependency`：`ReadCtx`（公开）+ `Owner`/`DependencyGraph`/`ReadTracker`/`EvalStack`/`EvalGuard`（crate 私有）
//! - `dirty`：`DirtyFlags` + 冒泡终止判定
//! - `id`：`NodeId`（公开）+ `Arena`（crate 私有）
//! - `signal`：`Signal<T>`/`Derived<T>`/`Transient<T>` 等句柄 + `StateStore`（crate 私有）
//! - `tree`：`Node`/`NodeKind`/`LayoutBox`/`NodeTree`

pub mod builder;
pub mod dependency;
pub mod dirty;
pub mod id;
pub mod signal;
pub mod tree;

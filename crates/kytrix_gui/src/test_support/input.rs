//! 最小事件注入的结果（输入事件本身是公开的 `crate::event::InputEvent`）。
//!
//! P06 只做「指针 → 命中」；键盘 / IME / 滚轮 / 拖拽 / Action 分发属 P10（additive 加变体）。
//! 设计见 `design/phase-06-paint-runtime.md` §10.4 与 `design/phase-09-win32-demo.md` S-79。

use crate::layout::hit_test::HitResult;

/// 一次注入的结果（P06 只做命中；Action 分发属 P10）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputOutcome {
    /// 命中结果（`None` = 窗口外/无命中）。
    pub hit: Option<HitResult>,
}

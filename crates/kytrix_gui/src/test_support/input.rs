//! 最小事件注入（P06 只有「指针 → 命中」）。
//!
//! 键盘 / IME / 滚轮 / 拖拽 / 焦点 / Action 分发属 P10（additive 加变体）；现在加空变体只会制造
//! 无消费者分支。设计见 `design/phase-06-paint-runtime.md` §10.4。

use crate::{layout::hit_test::HitResult, math::Point};

/// 指针按键。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    /// 主键（左）。
    Primary,
    /// 次键（右）。
    Secondary,
    /// 中键。
    Middle,
}

/// 注入的输入事件（最小集）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputEvent {
    /// 指针移动。
    PointerMove {
        /// 窗口逻辑坐标。
        position: Point,
    },
    /// 指针按下。
    PointerDown {
        /// 窗口逻辑坐标。
        position: Point,
        /// 按键。
        button: PointerButton,
    },
    /// 指针抬起。
    PointerUp {
        /// 窗口逻辑坐标。
        position: Point,
        /// 按键。
        button: PointerButton,
    },
}

impl InputEvent {
    /// 事件位置。
    #[must_use]
    pub fn position(self) -> Point {
        match self {
            Self::PointerMove { position }
            | Self::PointerDown { position, .. }
            | Self::PointerUp { position, .. } => position,
        }
    }
}

/// 一次注入的结果（P06 只做命中；Action 分发属 P10）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputOutcome {
    /// 命中结果（`None` = 窗口外/无命中）。
    pub hit: Option<HitResult>,
}

//! 平台事件与输入（公开面；`design/phase-09-win32-demo.md` §4）。
//!
//! 平台层（`crate::platform`，私有，I9）把 `WM_*` 翻译成 [`InputEvent`] / [`ImeEvent`]，
//! 应用层通过 [`crate::app::App::drain_events`] 收到 [`Event`]。
//!
//! **坐标一律是窗口逻辑像素**（物理像素 → 逻辑的换算只在平台层做一次，I11）。

use smol_str::SmolStr;

use crate::math::{Point, Scale, Size};

bitflags::bitflags! {
    /// 键盘修饰键（平台层按消息顺序维护，不每帧查系统状态）。
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct Modifiers: u8 {
        /// Shift。
        const SHIFT = 1 << 0;
        /// Control。
        const CONTROL = 1 << 1;
        /// Alt。
        const ALT = 1 << 2;
        /// Windows 键 / Command 键。
        const META = 1 << 3;
    }
}

/// 指针按键。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PointerButton {
    /// 主键（左）。
    Primary,
    /// 次键（右）。
    Secondary,
    /// 中键。
    Middle,
    /// 侧键 1。
    X1,
    /// 侧键 2。
    X2,
}

/// 按键状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyState {
    /// 按下（含自动重复）。
    Down,
    /// 抬起。
    Up,
}

/// 与布局无关的键（字符输入走 [`InputEvent::Text`]）。
///
/// 不含字母/数字：它们的**字符**由操作系统翻译（`WM_CHAR`）后经 [`InputEvent::Text`] 到达，
/// 引擎不做 VK → 字符的布局映射（`design/phase-09-win32-demo.md` A4）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// Esc。
    Escape,
    /// 回车。
    Enter,
    /// Tab。
    Tab,
    /// 退格。
    Backspace,
    /// Delete。
    Delete,
    /// 空格。
    Space,
    /// ↑。
    ArrowUp,
    /// ↓。
    ArrowDown,
    /// ←。
    ArrowLeft,
    /// →。
    ArrowRight,
    /// Home。
    Home,
    /// End。
    End,
    /// PageUp。
    PageUp,
    /// PageDown。
    PageDown,
    /// F1…F24。
    Function(u8),
    /// 左/右 Shift。
    Shift,
    /// 左/右 Control。
    Control,
    /// 左/右 Alt。
    Alt,
    /// 左/右 Windows 键。
    Meta,
    /// 未映射的平台键（载荷是平台虚拟键码；P10 的 `KeyBinding` 会重新定义这一层）。
    Other(u16),
}

/// 一次输入事件。
#[derive(Clone, Debug, PartialEq)]
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
    /// 指针离开客户区。
    PointerLeave,
    /// 滚轮：`delta_*` 以「行」为单位（Windows 一格 = 120 → 1.0 行，符号即方向）。
    Scroll {
        /// 窗口逻辑坐标。
        position: Point,
        /// 水平滚动（正 = 向右）。
        delta_x: f32,
        /// 垂直滚动（正 = 向上）。
        delta_y: f32,
    },
    /// 非字符键。
    Key {
        /// 键。
        key: Key,
        /// 按下/抬起。
        state: KeyState,
        /// 当前修饰键。
        modifiers: Modifiers,
    },
    /// 字符输入（`WM_CHAR` / IME 提交；死键组合已由平台完成）。
    Text {
        /// 一个或多个 `char`（代理对已合并）。
        text: SmolStr,
    },
}

impl InputEvent {
    /// 事件位置（只有指针/滚轮事件有位置）。
    #[must_use]
    pub fn position(&self) -> Option<Point> {
        match self {
            Self::PointerMove { position }
            | Self::PointerDown { position, .. }
            | Self::PointerUp { position, .. }
            | Self::Scroll { position, .. } => Some(*position),
            Self::PointerLeave | Self::Key { .. } | Self::Text { .. } => None,
        }
    }
}

/// 输入法事件（P09 只到「组合串可见 + 提交」这一层）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImeEvent {
    /// 开始组合。
    Start,
    /// 组合中的文本（`None` 表示组合串被清空）。
    Preedit(SmolStr),
    /// 已提交的文本。
    Commit(SmolStr),
    /// 组合结束。
    End,
}

/// 平台事件（应用层可观察的全部事件）。
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// 指针 / 滚轮 / 键盘。
    Input(InputEvent),
    /// 输入法。
    Ime(ImeEvent),
    /// 窗口 DPI 变化（`scale` 已生效在 `Window::scale` 上）。
    DpiChanged {
        /// 新的 DPI（96 为基准）。
        dpi: u32,
        /// 新比例。
        scale: Scale,
    },
    /// 系统外观变化（深色/浅色）。
    AppearanceChanged {
        /// `true` = 深色。
        dark: bool,
    },
    /// 焦点变化。
    FocusChanged {
        /// `true` = 获得焦点。
        focused: bool,
    },
    /// 窗口客户区尺寸变化（逻辑像素，已生效在 `Window::size` 上）。
    Resized {
        /// 新的逻辑尺寸。
        size: Size,
    },
    /// 窗口被要求关闭（平台已处理，事件只作通知）。
    CloseRequested,
}

#[cfg(test)]
mod tests {
    use super::{InputEvent, Key, KeyState, Modifiers, PointerButton};
    use crate::math::Point;

    #[test]
    fn pointer_events_expose_their_position() {
        let event = InputEvent::PointerDown {
            position: Point::new(12.5, 8.0),
            button: PointerButton::Primary,
        };

        assert_eq!(event.position(), Some(Point::new(12.5, 8.0)));
    }

    #[test]
    fn non_pointer_events_have_no_position() {
        let event = InputEvent::Key {
            key: Key::Escape,
            state: KeyState::Down,
            modifiers: Modifiers::SHIFT | Modifiers::CONTROL,
        };

        assert_eq!(event.position(), None);
        assert!(event_modifiers_include_control(&event));
    }

    fn event_modifiers_include_control(event: &InputEvent) -> bool {
        match event {
            InputEvent::Key { modifiers, .. } => modifiers.contains(Modifiers::CONTROL),
            _ => false,
        }
    }

    #[test]
    fn pointer_leave_is_positionless() {
        assert_eq!(InputEvent::PointerLeave.position(), None);
    }
}

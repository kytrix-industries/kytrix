//! 平台事件、输入与动作契约（公开面；`design/phase-09-win32-demo.md` §4、`design/phase-10-interaction.md` §4/§5）。
//!
//! 平台层（`crate::platform`，私有，I9）把 `WM_*` 翻译成 [`InputEvent`] / [`ImeEvent`]，应用层通过
//! [`crate::app::App::drain_events`] 收到 [`Event`]；**交互分发**由引擎自己完成（[`crate::app::App`] 的
//! `dispatch_input`），动作见 [`Action`]。
//!
//! **坐标一律是窗口逻辑像素**（物理像素 → 逻辑的换算只在平台层做一次，I11）。

use smol_str::SmolStr;

use crate::{
    interaction::DragPhase,
    layout::hit_test::HitResult,
    math::{Point, Scale, Size},
    node::id::NodeId,
};

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
/// 引擎不做 VK → 字符的布局映射（`design/phase-09-win32-demo.md` A4）。键位**身份**（快捷键）用
/// [`Key::Other`] 的虚拟键码，两者不混用。
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
    /// 未映射的平台键（载荷是平台虚拟键码；[`KeyBinding`] 用它表达字母/数字键位）。
    Other(u16),
}

/// 一次输入事件。
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    /// 指针移动。
    PointerMove {
        /// 窗口逻辑坐标。
        position: Point,
        /// 当前修饰键。
        modifiers: Modifiers,
    },
    /// 指针按下。
    PointerDown {
        /// 窗口逻辑坐标。
        position: Point,
        /// 按键。
        button: PointerButton,
        /// 当前修饰键。
        modifiers: Modifiers,
    },
    /// 指针抬起。
    PointerUp {
        /// 窗口逻辑坐标。
        position: Point,
        /// 按键。
        button: PointerButton,
        /// 当前修饰键。
        modifiers: Modifiers,
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
        /// 当前修饰键（`Shift + 滚轮` 的横向滚动走它）。
        modifiers: Modifiers,
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
            Self::PointerMove { position, .. }
            | Self::PointerDown { position, .. }
            | Self::PointerUp { position, .. }
            | Self::Scroll { position, .. } => Some(*position),
            Self::PointerLeave | Self::Key { .. } | Self::Text { .. } => None,
        }
    }

    /// 事件携带的修饰键（无修饰键概念的事件返回空集）。
    #[must_use]
    pub fn modifiers(&self) -> Modifiers {
        match self {
            Self::PointerMove { modifiers, .. }
            | Self::PointerDown { modifiers, .. }
            | Self::PointerUp { modifiers, .. }
            | Self::Scroll { modifiers, .. }
            | Self::Key { modifiers, .. } => *modifiers,
            Self::PointerLeave | Self::Text { .. } => Modifiers::empty(),
        }
    }
}

/// 输入法事件（P09 只到「组合串可见 + 提交」这一层，交互处理归文本 Phase）。
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

/// 引擎动作身份（`design/phase-10-interaction.md` §4.1）。
///
/// **身份而非载荷**：同一个 `Action` 可以同时被「点击」「快捷键」「a11y 动作」触达，三者的差异走
/// [`ActionSource`]，不进 `Action` 本身（否则不可 `Hash`，也没法做 handler 表的键）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// 焦点移到下一个可聚焦节点（Tab 的默认绑定；末尾回绕）。
    FocusNext,
    /// 焦点移到上一个（Shift+Tab 的默认绑定）。
    FocusPrev,
    /// 关闭最上层可关闭的浮层（Esc 的默认绑定；无浮层时无副作用）。
    Dismiss,
    /// 应用自定义动作：`SmolStr` 是**应用层稳定 key**（建议 `"<模块>.<动词>"`，如 `"chart.resetZoom"`）。
    Custom(SmolStr),
}

impl Action {
    /// 自定义动作的便利构造（走 `SmolStr::new_static`，无分配）。
    #[must_use]
    pub fn custom(key: &'static str) -> Self {
        Self::Custom(SmolStr::new_static(key))
    }
}

/// 动作来源（把「怎么触发的」从 [`Action`] 里剥出来）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ActionSource {
    /// 指针点击，或焦点节点的 Enter/Space 激活：连击计数（1 = 单击）。
    Click {
        /// 连击计数（1 起；双击 = 2）。
        click_count: u8,
    },
    /// 键盘绑定命中。
    Key,
    /// 拖拽（按下 / 移动 / 抬起 / 取消）。
    Drag {
        /// 阶段。
        phase: DragPhase,
        /// 相对拖拽起点的位移（逻辑像素）。
        delta: Point,
    },
    /// 程序调用（[`crate::app::App::dispatch_action`]；a11y 的 `ActionRequest` 也走这个）。
    Programmatic,
}

/// 一条键盘绑定（全应用一份表；快捷键是应用级语义）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyBinding {
    /// 物理键（`Key::Other(vk)` 覆盖字母/数字：字符输入走 [`InputEvent::Text`]）。
    pub key: Key,
    /// **精确**修饰键集合（`Ctrl+S` 不匹配 `Ctrl+Shift+S`；未按的修饰键必须为空）。
    pub modifiers: Modifiers,
    /// 命中的动作。
    pub action: Action,
}

/// 动作处理器（应用注册；`FnMut`，可写状态）。
pub type ActionHandler = Box<dyn FnMut(&mut crate::interaction::ActionCtx<'_>)>;

/// 一次输入分发的产物（`TestApp::inject` 返回；也是分发路径的可断言结果）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputOutcome {
    /// 命中结果（`None` = 窗口外 / 无命中 / 键盘事件）。
    pub hit: Option<HitResult>,
    /// 本次分发触发的动作（`None` = 未处理）。
    pub action: Option<Action>,
    /// 声明该动作的节点（`on_click` / `on_drag` 的落点；键盘为焦点节点）。
    pub target: Option<NodeId>,
    /// 连击计数（0 = 非点击事件）。
    pub click_count: u8,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
    };

    use super::{
        Action, ActionSource, InputEvent, InputOutcome, Key, KeyBinding, KeyState, Modifiers,
        PointerButton,
    };
    use crate::math::Point;

    fn hash_of(action: &Action) -> u64 {
        let mut hasher = DefaultHasher::new();
        action.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn pointer_events_expose_their_position() {
        let event = InputEvent::PointerDown {
            position: Point::new(12.5, 8.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::SHIFT,
        };

        assert_eq!(event.position(), Some(Point::new(12.5, 8.0)));
        assert_eq!(event.modifiers(), Modifiers::SHIFT);
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

    #[test]
    fn action_identity_is_hashable_and_distinct() {
        assert_ne!(Action::custom("a"), Action::custom("b"));
        assert_eq!(Action::custom("a"), Action::Custom("a".into()));
        assert_ne!(Action::FocusNext, Action::FocusPrev);
        assert_ne!(Action::FocusNext, Action::custom("focus.next"));
        assert_ne!(hash_of(&Action::FocusNext), hash_of(&Action::FocusPrev));
        // 可作 handler 表的键（`HashMap<Action, _>` 的编译期要求）。
        let mut map = std::collections::HashMap::new();
        map.insert(Action::custom("a"), 1u8);
        map.insert(Action::FocusNext, 2u8);
        assert_eq!(map.get(&Action::custom("a")), Some(&1));
    }

    #[test]
    fn key_binding_modifiers_are_exact() {
        let binding = KeyBinding {
            key: Key::Other(0x53),
            modifiers: Modifiers::CONTROL,
            action: Action::custom("save"),
        };

        let matches = |modifiers: Modifiers| {
            binding.key == Key::Other(0x53) && binding.modifiers == modifiers
        };

        assert!(matches(Modifiers::CONTROL));
        assert!(!matches(Modifiers::CONTROL | Modifiers::SHIFT), "精确匹配");
        assert!(!matches(Modifiers::empty()), "未按修饰键不匹配");
    }

    #[test]
    fn key_binding_distinguishes_other_vk() {
        let binding = KeyBinding {
            key: Key::Other(0x41),
            modifiers: Modifiers::empty(),
            action: Action::custom("a"),
        };

        assert_eq!(binding.key, Key::Other(0x41));
        assert_ne!(binding.key, Key::Other(0x42));
    }

    #[test]
    fn input_outcome_default_is_unhandled() {
        let outcome = InputOutcome::default();

        assert_eq!(outcome.hit, None);
        assert_eq!(outcome.action, None);
        assert_eq!(outcome.target, None);
        assert_eq!(outcome.click_count, 0);
    }

    #[test]
    fn pointer_events_carry_modifiers() {
        let event = InputEvent::PointerDown {
            position: Point::new(1.0, 2.0),
            button: PointerButton::Secondary,
            modifiers: Modifiers::CONTROL | Modifiers::ALT,
        };

        assert_eq!(event.modifiers(), Modifiers::CONTROL | Modifiers::ALT);
        assert_eq!(event.position(), Some(Point::new(1.0, 2.0)));
    }

    #[test]
    fn click_source_carries_the_count() {
        let source = ActionSource::Click { click_count: 2 };

        assert_eq!(source, ActionSource::Click { click_count: 2 });
        assert_ne!(source, ActionSource::Key);
    }
}

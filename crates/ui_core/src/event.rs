use std::{ops::Range, time::Instant};

use smallvec::SmallVec;
use smol_str::SmolStr;

use crate::{
    interaction::DragPhase,
    layout::hit_test::HitResult,
    math::{Point, Scale, Size},
    node::id::NodeId,
};

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct Modifiers: u8 {
        const SHIFT = 1 << 0;
        const CONTROL = 1 << 1;
        const ALT = 1 << 2;
        const META = 1 << 3;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PointerButton {
    Primary,
    Secondary,
    Middle,
    X1,
    X2,
}

impl PointerButton {
    #[must_use]
    pub const fn is_primary(self) -> bool {
        matches!(self, Self::Primary)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Backquote,
    Backslash,
    BracketLeft,
    BracketRight,
    Comma,
    Equal,
    IntlBackslash,
    IntlRo,
    IntlYen,
    Minus,
    Period,
    Quote,
    Semicolon,
    Slash,
    KeyA,
    KeyB,
    KeyC,
    KeyD,
    KeyE,
    KeyF,
    KeyG,
    KeyH,
    KeyI,
    KeyJ,
    KeyK,
    KeyL,
    KeyM,
    KeyN,
    KeyO,
    KeyP,
    KeyQ,
    KeyR,
    KeyS,
    KeyT,
    KeyU,
    KeyV,
    KeyW,
    KeyX,
    KeyY,
    KeyZ,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,

    AltLeft,
    AltRight,
    Backspace,
    CapsLock,
    ContextMenu,
    ControlLeft,
    ControlRight,
    Enter,
    MetaLeft,
    MetaRight,
    ShiftLeft,
    ShiftRight,
    Space,
    Tab,
    Delete,
    End,
    Help,
    Home,
    Insert,
    PageDown,
    PageUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    ArrowUp,

    NumLock,
    Numpad0,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad4,
    Numpad5,
    Numpad6,
    Numpad7,
    Numpad8,
    Numpad9,
    NumpadAdd,
    NumpadComma,
    NumpadDecimal,
    NumpadDivide,
    NumpadEnter,
    NumpadEqual,
    NumpadMultiply,
    NumpadSubtract,

    Escape,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F20,
    F21,
    F22,
    F23,
    F24,
    Fn,
    FnLock,
    Pause,
    PrintScreen,
    ScrollLock,

    AudioVolumeUp,
    AudioVolumeDown,
    AudioVolumeMute,
    MediaPlayPause,
    MediaStop,
    MediaTrackNext,
    MediaTrackPrevious,
    MediaSelect,
    LaunchMail,
    LaunchApp1,
    LaunchApp2,
    BrowserBack,
    BrowserForward,
    BrowserHome,
    BrowserRefresh,
    BrowserStop,
    BrowserSearch,
    BrowserFavorites,
    Power,
    Sleep,
    WakeUp,

    Unidentified(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Escape,
    Enter,
    Tab,
    Backspace,
    Delete,
    Insert,
    Space,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Function(u8),
    Shift,
    Control,
    Alt,
    Meta,
}

impl Key {
    #[must_use]
    pub const fn from_code(code: KeyCode) -> Option<Self> {
        let key = match code {
            KeyCode::Escape => Self::Escape,
            KeyCode::Enter | KeyCode::NumpadEnter => Self::Enter,
            KeyCode::Tab => Self::Tab,
            KeyCode::Backspace => Self::Backspace,
            KeyCode::Delete => Self::Delete,
            KeyCode::Insert => Self::Insert,
            KeyCode::Space => Self::Space,
            KeyCode::ArrowUp => Self::ArrowUp,
            KeyCode::ArrowDown => Self::ArrowDown,
            KeyCode::ArrowLeft => Self::ArrowLeft,
            KeyCode::ArrowRight => Self::ArrowRight,
            KeyCode::Home => Self::Home,
            KeyCode::End => Self::End,
            KeyCode::PageUp => Self::PageUp,
            KeyCode::PageDown => Self::PageDown,
            KeyCode::F1 => Self::Function(1),
            KeyCode::F2 => Self::Function(2),
            KeyCode::F3 => Self::Function(3),
            KeyCode::F4 => Self::Function(4),
            KeyCode::F5 => Self::Function(5),
            KeyCode::F6 => Self::Function(6),
            KeyCode::F7 => Self::Function(7),
            KeyCode::F8 => Self::Function(8),
            KeyCode::F9 => Self::Function(9),
            KeyCode::F10 => Self::Function(10),
            KeyCode::F11 => Self::Function(11),
            KeyCode::F12 => Self::Function(12),
            KeyCode::F13 => Self::Function(13),
            KeyCode::F14 => Self::Function(14),
            KeyCode::F15 => Self::Function(15),
            KeyCode::F16 => Self::Function(16),
            KeyCode::F17 => Self::Function(17),
            KeyCode::F18 => Self::Function(18),
            KeyCode::F19 => Self::Function(19),
            KeyCode::F20 => Self::Function(20),
            KeyCode::F21 => Self::Function(21),
            KeyCode::F22 => Self::Function(22),
            KeyCode::F23 => Self::Function(23),
            KeyCode::F24 => Self::Function(24),
            KeyCode::ShiftLeft | KeyCode::ShiftRight => Self::Shift,
            KeyCode::ControlLeft | KeyCode::ControlRight => Self::Control,
            KeyCode::AltLeft | KeyCode::AltRight => Self::Alt,
            KeyCode::MetaLeft | KeyCode::MetaRight => Self::Meta,
            _ => return None,
        };
        Some(key)
    }

    #[must_use]
    pub const fn is_modifier(self) -> bool {
        matches!(self, Self::Shift | Self::Control | Self::Alt | Self::Meta)
    }
}

pub fn modifier_key_transitions(
    previous: Modifiers,
    current: Modifiers,
) -> impl Iterator<Item = (KeyCode, KeyState)> {
    const MODIFIER_KEYS: [(Modifiers, KeyCode); 4] = [
        (Modifiers::SHIFT, KeyCode::ShiftLeft),
        (Modifiers::CONTROL, KeyCode::ControlLeft),
        (Modifiers::ALT, KeyCode::AltLeft),
        (Modifiers::META, KeyCode::MetaLeft),
    ];
    MODIFIER_KEYS.into_iter().filter_map(move |(flag, code)| {
        let was = previous.contains(flag);
        let now = current.contains(flag);
        (was != now).then_some((code, if now { KeyState::Down } else { KeyState::Up }))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyState {
    Down,
    Up,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollUnit {
    Line,
    Pixel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollPhase {
    Discrete,
    Begin,
    Update,
    End,
    Momentum,
    Cancel,
}

impl ScrollPhase {
    #[must_use]
    pub const fn is_continuous(self) -> bool {
        !matches!(self, Self::Discrete)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GesturePhase {
    Begin,
    Update,
    End,
    Cancel,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GestureFlags {
    pub may_begin: bool,
    pub began: bool,
    pub changed: bool,
    pub ended: bool,
    pub cancelled: bool,
}

impl GestureFlags {
    #[must_use]
    pub const fn phase(self) -> Option<GesturePhase> {
        if self.cancelled {
            return Some(GesturePhase::Cancel);
        }
        if self.ended {
            return Some(GesturePhase::End);
        }
        if self.began {
            return Some(GesturePhase::Begin);
        }
        if self.changed {
            return Some(GesturePhase::Update);
        }
        if self.may_begin {
            return None;
        }
        Some(GesturePhase::Update)
    }
}

#[must_use]
pub fn scroll_delta(precise: bool, delta_x: f64, delta_y: f64) -> Option<(Point, ScrollUnit)> {
    if precise {
        return Some((
            Point::new(delta_x as f32, delta_y as f32),
            ScrollUnit::Pixel,
        ));
    }
    if delta_x == 0.0 && delta_y == 0.0 {
        return None;
    }
    Some((Point::new(delta_x as f32, delta_y as f32), ScrollUnit::Line))
}

#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    PointerEnter {
        position: Point,
        modifiers: Modifiers,
        timestamp: Instant,
    },
    PointerMove {
        position: Point,
        modifiers: Modifiers,
        timestamp: Instant,
    },
    PointerDown {
        position: Point,
        button: PointerButton,
        modifiers: Modifiers,
        timestamp: Instant,
    },
    PointerUp {
        position: Point,
        button: PointerButton,
        modifiers: Modifiers,
        timestamp: Instant,
    },
    PointerLeave {
        position: Point,
        modifiers: Modifiers,
        timestamp: Instant,
    },
    PointerCancel {
        timestamp: Instant,
    },
    Scroll {
        position: Point,
        delta: Point,
        unit: ScrollUnit,
        phase: ScrollPhase,
        modifiers: Modifiers,
        timestamp: Instant,
    },
    Magnify {
        factor: f32,
        phase: GesturePhase,
        center: Point,
        modifiers: Modifiers,
        timestamp: Instant,
    },
    Rotate {
        degrees: f32,
        phase: GesturePhase,
        modifiers: Modifiers,
        timestamp: Instant,
    },
    SmartMagnify {
        center: Point,
        modifiers: Modifiers,
        timestamp: Instant,
    },
    Key {
        code: KeyCode,
        state: KeyState,
        repeat: bool,
        modifiers: Modifiers,
        timestamp: Instant,
    },
    Text {
        text: SmolStr,
        timestamp: Instant,
    },
}

impl InputEvent {
    #[must_use]
    pub fn position(&self) -> Option<Point> {
        match self {
            Self::PointerEnter { position, .. }
            | Self::PointerMove { position, .. }
            | Self::PointerDown { position, .. }
            | Self::PointerUp { position, .. }
            | Self::PointerLeave { position, .. }
            | Self::Scroll { position, .. } => Some(*position),
            Self::Magnify { center, .. } | Self::SmartMagnify { center, .. } => Some(*center),
            Self::PointerCancel { .. }
            | Self::Rotate { .. }
            | Self::Key { .. }
            | Self::Text { .. } => None,
        }
    }

    #[must_use]
    pub fn modifiers(&self) -> Modifiers {
        match self {
            Self::PointerEnter { modifiers, .. }
            | Self::PointerMove { modifiers, .. }
            | Self::PointerDown { modifiers, .. }
            | Self::PointerUp { modifiers, .. }
            | Self::PointerLeave { modifiers, .. }
            | Self::Scroll { modifiers, .. }
            | Self::Magnify { modifiers, .. }
            | Self::Rotate { modifiers, .. }
            | Self::SmartMagnify { modifiers, .. }
            | Self::Key { modifiers, .. } => *modifiers,
            Self::PointerCancel { .. } | Self::Text { .. } => Modifiers::empty(),
        }
    }

    #[must_use]
    pub fn timestamp(&self) -> Instant {
        match self {
            Self::PointerEnter { timestamp, .. }
            | Self::PointerMove { timestamp, .. }
            | Self::PointerDown { timestamp, .. }
            | Self::PointerUp { timestamp, .. }
            | Self::PointerLeave { timestamp, .. }
            | Self::PointerCancel { timestamp }
            | Self::Scroll { timestamp, .. }
            | Self::Magnify { timestamp, .. }
            | Self::Rotate { timestamp, .. }
            | Self::SmartMagnify { timestamp, .. }
            | Self::Key { timestamp, .. }
            | Self::Text { timestamp, .. } => *timestamp,
        }
    }

    #[must_use]
    pub fn with_timestamp(self, at: Instant) -> Self {
        match self {
            Self::PointerEnter {
                position,
                modifiers,
                ..
            } => Self::PointerEnter {
                position,
                modifiers,
                timestamp: at,
            },
            Self::PointerMove {
                position,
                modifiers,
                ..
            } => Self::PointerMove {
                position,
                modifiers,
                timestamp: at,
            },
            Self::PointerDown {
                position,
                button,
                modifiers,
                ..
            } => Self::PointerDown {
                position,
                button,
                modifiers,
                timestamp: at,
            },
            Self::PointerUp {
                position,
                button,
                modifiers,
                ..
            } => Self::PointerUp {
                position,
                button,
                modifiers,
                timestamp: at,
            },
            Self::PointerLeave {
                position,
                modifiers,
                ..
            } => Self::PointerLeave {
                position,
                modifiers,
                timestamp: at,
            },
            Self::PointerCancel { .. } => Self::PointerCancel { timestamp: at },
            Self::Scroll {
                position,
                delta,
                unit,
                phase,
                modifiers,
                ..
            } => Self::Scroll {
                position,
                delta,
                unit,
                phase,
                modifiers,
                timestamp: at,
            },
            Self::Magnify {
                factor,
                phase,
                center,
                modifiers,
                ..
            } => Self::Magnify {
                factor,
                phase,
                center,
                modifiers,
                timestamp: at,
            },
            Self::Rotate {
                degrees,
                phase,
                modifiers,
                ..
            } => Self::Rotate {
                degrees,
                phase,
                modifiers,
                timestamp: at,
            },
            Self::SmartMagnify {
                center, modifiers, ..
            } => Self::SmartMagnify {
                center,
                modifiers,
                timestamp: at,
            },
            Self::Key {
                code,
                state,
                repeat,
                modifiers,
                ..
            } => Self::Key {
                code,
                state,
                repeat,
                modifiers,
                timestamp: at,
            },
            Self::Text { text, .. } => Self::Text {
                text,
                timestamp: at,
            },
        }
    }

    #[must_use]
    pub fn pointer_move(position: Point, modifiers: Modifiers) -> Self {
        Self::PointerMove {
            position,
            modifiers,
            timestamp: Instant::now(),
        }
    }

    #[must_use]
    pub fn pointer_down(position: Point, button: PointerButton, modifiers: Modifiers) -> Self {
        Self::PointerDown {
            position,
            button,
            modifiers,
            timestamp: Instant::now(),
        }
    }

    #[must_use]
    pub fn pointer_up(position: Point, button: PointerButton, modifiers: Modifiers) -> Self {
        Self::PointerUp {
            position,
            button,
            modifiers,
            timestamp: Instant::now(),
        }
    }

    #[must_use]
    pub fn scroll_lines(position: Point, delta: Point, modifiers: Modifiers) -> Self {
        Self::Scroll {
            position,
            delta,
            unit: ScrollUnit::Line,
            phase: ScrollPhase::Discrete,
            modifiers,
            timestamp: Instant::now(),
        }
    }

    #[must_use]
    pub fn key_down(code: KeyCode, modifiers: Modifiers) -> Self {
        Self::Key {
            code,
            state: KeyState::Down,
            repeat: false,
            modifiers,
            timestamp: Instant::now(),
        }
    }

    #[must_use]
    pub fn key_up(code: KeyCode, modifiers: Modifiers) -> Self {
        Self::Key {
            code,
            state: KeyState::Up,
            repeat: false,
            modifiers,
            timestamp: Instant::now(),
        }
    }
}

#[must_use]
pub fn should_emit_text(
    state: KeyState,
    _repeat: bool,
    ime_takes_text: bool,
    composing: bool,
) -> bool {
    matches!(state, KeyState::Down) && !ime_takes_text && !composing
}

#[must_use]
pub fn timestamp_from_ticks(now: Instant, now_ticks_ms: u64, event_ticks_ms: u64) -> Instant {
    let behind = now_ticks_ms.saturating_sub(event_ticks_ms);
    now.checked_sub(std::time::Duration::from_millis(behind))
        .unwrap_or(now)
}

#[must_use]
pub fn timestamp_from_ticks_32(now: Instant, now_ticks_ms: u32, event_ticks_ms: u32) -> Instant {
    let behind = now_ticks_ms.wrapping_sub(event_ticks_ms);
    if behind > MAX_PLAUSIBLE_BEHIND_MS {
        return now;
    }
    now.checked_sub(std::time::Duration::from_millis(u64::from(behind)))
        .unwrap_or(now)
}

pub const MAX_PLAUSIBLE_BEHIND_MS: u32 = 60 * 60 * 1_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImeEvent {
    Start,
    Preedit {
        text: SmolStr,
        caret: Option<u32>,
        clauses: SmallVec<[Range<usize>; 4]>,
        replace: Option<Range<usize>>,
    },
    Commit {
        text: SmolStr,
        replace: Option<Range<usize>>,
    },
    End,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Input(InputEvent),
    Ime(ImeEvent),
    DpiChanged {
        dpi: u32,
        scale: Scale,
    },
    AppearanceChanged {
        dark: bool,
    },
    FocusChanged {
        focused: bool,
    },
    Resized {
        size: Size,
    },
    Moved {
        position: Point,
    },
    HoverChanged {
        hovered: Option<NodeId>,
        left: Vec<NodeId>,
        entered: Vec<NodeId>,
    },
    CloseRequested,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    FocusNext,
    FocusPrev,
    Dismiss,
    Custom(SmolStr),
}

impl Action {
    #[must_use]
    pub fn custom(key: &'static str) -> Self {
        Self::Custom(SmolStr::new_static(key))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ActionSource {
    Click {
        click_count: u8,
        button: PointerButton,
    },
    Key,
    Drag {
        phase: DragPhase,
        delta: Point,
    },
    Programmatic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyBinding {
    pub key: KeyCode,
    pub modifiers: Modifiers,
    pub action: Action,
}

pub type ActionHandler = Box<dyn FnMut(&mut crate::interaction::ActionCtx<'_>)>;

pub type CloseHandler = Box<dyn FnMut(&mut crate::app::App) -> bool>;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputOutcome {
    pub hit: Option<HitResult>,
    pub action: Option<Action>,
    pub target: Option<NodeId>,
    pub click_count: u8,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
        time::{Duration, Instant},
    };

    use super::{
        Action, ActionSource, InputEvent, InputOutcome, Key, KeyBinding, KeyCode, KeyState,
        Modifiers, PointerButton, ScrollPhase, ScrollUnit, modifier_key_transitions,
        should_emit_text, timestamp_from_ticks, timestamp_from_ticks_32,
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
            timestamp: Instant::now(),
        };

        assert_eq!(event.position(), Some(Point::new(12.5, 8.0)));
        assert_eq!(event.modifiers(), Modifiers::SHIFT);
    }

    #[test]
    fn non_pointer_events_have_no_position() {
        let event = InputEvent::key_down(KeyCode::Escape, Modifiers::SHIFT | Modifiers::CONTROL);

        assert_eq!(event.position(), None);
        assert!(matches!(
            event,
            InputEvent::Key {
                modifiers: ref value,
                ..
            } if value.contains(Modifiers::CONTROL)
        ));
    }

    #[test]
    fn pointer_leave_keeps_the_leave_point() {
        let event = InputEvent::PointerLeave {
            position: Point::new(3.0, 4.0),
            modifiers: Modifiers::empty(),
            timestamp: Instant::now(),
        };

        assert_eq!(event.position(), Some(Point::new(3.0, 4.0)));
    }

    #[test]
    fn every_event_carries_a_timestamp() {
        let stamp = Instant::now();
        let events = [
            InputEvent::PointerMove {
                position: Point::ZERO,
                modifiers: Modifiers::empty(),
                timestamp: stamp,
            },
            InputEvent::PointerCancel { timestamp: stamp },
            InputEvent::Key {
                code: KeyCode::KeyA,
                state: KeyState::Up,
                repeat: false,
                modifiers: Modifiers::empty(),
                timestamp: stamp,
            },
            InputEvent::Text {
                text: "a".into(),
                timestamp: stamp,
            },
            InputEvent::Scroll {
                position: Point::ZERO,
                delta: Point::new(0.0, 1.0),
                unit: ScrollUnit::Line,
                phase: ScrollPhase::Discrete,
                modifiers: Modifiers::empty(),
                timestamp: stamp,
            },
        ];

        for event in &events {
            assert_eq!(event.timestamp(), stamp, "每个变体都必须带事件时刻");
        }
    }

    #[test]
    fn platform_ticks_are_mapped_onto_the_engine_clock() {
        let now = Instant::now();
        let stamp = timestamp_from_ticks(now, 5_000, 4_980);
        assert_eq!(stamp, now - Duration::from_millis(20));
        assert_eq!(timestamp_from_ticks(now, 5_000, 5_000), now);
        assert_eq!(timestamp_from_ticks(now, 5_000, 9_000), now);
    }

    #[test]
    fn wrapping_32_bit_ticks_convert_without_drifting_across_the_wrap() {
        let now = Instant::now();
        assert_eq!(
            timestamp_from_ticks_32(now, 5_000, 4_980),
            now - Duration::from_millis(20)
        );
        assert_eq!(
            timestamp_from_ticks_32(now, 5, u32::MAX - 15),
            now - Duration::from_millis(21),
            "回绕必须用 wrapping_sub 计算差值，不能当成 49.7 天"
        );
        assert_eq!(timestamp_from_ticks_32(now, 5_000, 9_000), now);
        assert_eq!(
            timestamp_from_ticks_32(now, 0, 2 * crate::event::MAX_PLAUSIBLE_BEHIND_MS),
            now
        );
    }

    #[test]
    fn should_emit_text_ignores_auto_repeat() {
        assert!(should_emit_text(KeyState::Down, false, false, false));
        assert!(
            should_emit_text(KeyState::Down, true, false, false),
            "合成重复必须继续产生文本（P0 回归）"
        );
        assert!(!should_emit_text(KeyState::Up, false, false, false));
        assert!(!should_emit_text(KeyState::Down, false, true, false));
        assert!(!should_emit_text(KeyState::Down, false, false, true));
    }

    #[test]
    fn action_identity_is_hashable_and_distinct() {
        assert_ne!(Action::custom("a"), Action::custom("b"));
        assert_eq!(Action::custom("a"), Action::Custom("a".into()));
        assert_ne!(Action::FocusNext, Action::FocusPrev);
        assert_ne!(Action::FocusNext, Action::custom("focus.next"));
        assert_ne!(hash_of(&Action::FocusNext), hash_of(&Action::FocusPrev));
        let mut map = std::collections::HashMap::new();
        map.insert(Action::custom("a"), 1u8);
        map.insert(Action::FocusNext, 2u8);
        assert_eq!(map.get(&Action::custom("a")), Some(&1));
    }

    #[test]
    fn key_binding_modifiers_are_exact() {
        let binding = KeyBinding {
            key: KeyCode::KeyS,
            modifiers: Modifiers::CONTROL,
            action: Action::custom("save"),
        };

        let matches =
            |modifiers: Modifiers| binding.key == KeyCode::KeyS && binding.modifiers == modifiers;

        assert!(matches(Modifiers::CONTROL));
        assert!(!matches(Modifiers::CONTROL | Modifiers::SHIFT), "精确匹配");
        assert!(!matches(Modifiers::empty()), "未按修饰键不匹配");
    }

    #[test]
    fn key_bindings_use_layout_independent_identities() {
        let binding = KeyBinding {
            key: KeyCode::KeyA,
            modifiers: Modifiers::empty(),
            action: Action::custom("a"),
        };

        assert_eq!(binding.key, KeyCode::KeyA);
        assert_ne!(binding.key, KeyCode::KeyB);
        assert_ne!(binding.key, KeyCode::Unidentified(0x41));
    }

    #[test]
    fn modifier_transitions_synthesize_left_sided_key_events() {
        let collect = |previous: Modifiers, current: Modifiers| {
            modifier_key_transitions(previous, current).collect::<Vec<_>>()
        };

        assert_eq!(
            collect(Modifiers::empty(), Modifiers::SHIFT),
            vec![(KeyCode::ShiftLeft, KeyState::Down)],
            "按下 Shift ⇒ 合成 ShiftLeft Down"
        );
        assert_eq!(
            collect(Modifiers::SHIFT, Modifiers::empty()),
            vec![(KeyCode::ShiftLeft, KeyState::Up)],
            "松开 Shift ⇒ 合成 ShiftLeft Up"
        );
        assert_eq!(
            collect(Modifiers::empty(), Modifiers::CONTROL | Modifiers::META),
            vec![
                (KeyCode::ControlLeft, KeyState::Down),
                (KeyCode::MetaLeft, KeyState::Down),
            ],
            "顺序固定：Shift、Control、Alt、Meta"
        );
        assert_eq!(
            collect(Modifiers::ALT, Modifiers::ALT),
            vec![],
            "位图没变 ⇒ 不合成事件"
        );
    }

    #[test]
    fn media_keys_have_no_logical_projection() {
        for code in [
            KeyCode::AudioVolumeUp,
            KeyCode::AudioVolumeDown,
            KeyCode::AudioVolumeMute,
            KeyCode::MediaPlayPause,
            KeyCode::MediaTrackNext,
            KeyCode::MediaTrackPrevious,
            KeyCode::MediaStop,
            KeyCode::BrowserHome,
            KeyCode::BrowserBack,
            KeyCode::BrowserForward,
        ] {
            assert_eq!(Key::from_code(code), None, "{code:?} 没有命名逻辑身份");
        }
    }

    #[test]
    fn logical_keys_project_from_physical_codes() {
        assert_eq!(Key::from_code(KeyCode::Enter), Some(Key::Enter));
        assert_eq!(Key::from_code(KeyCode::NumpadEnter), Some(Key::Enter));
        assert_eq!(Key::from_code(KeyCode::ArrowUp), Some(Key::ArrowUp));
        assert_eq!(Key::from_code(KeyCode::F12), Some(Key::Function(12)));
        assert_eq!(Key::from_code(KeyCode::ShiftLeft), Some(Key::Shift));
        assert_eq!(Key::from_code(KeyCode::ControlRight), Some(Key::Control));
        assert!(Key::from_code(KeyCode::ShiftLeft).is_some_and(Key::is_modifier));
        assert!(!Key::from_code(KeyCode::Enter).is_some_and(Key::is_modifier));
        assert_eq!(Key::from_code(KeyCode::KeyS), None);
        assert_eq!(Key::from_code(KeyCode::Digit1), None);
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
        let event = InputEvent::pointer_down(
            Point::new(1.0, 2.0),
            PointerButton::Secondary,
            Modifiers::CONTROL | Modifiers::ALT,
        );

        assert_eq!(event.modifiers(), Modifiers::CONTROL | Modifiers::ALT);
        assert_eq!(event.position(), Some(Point::new(1.0, 2.0)));
    }

    #[test]
    fn click_source_carries_the_count_and_the_button() {
        let source = ActionSource::Click {
            click_count: 2,
            button: PointerButton::Primary,
        };

        assert_eq!(
            source,
            ActionSource::Click {
                click_count: 2,
                button: PointerButton::Primary
            }
        );
        assert_ne!(
            source,
            ActionSource::Click {
                click_count: 2,
                button: PointerButton::Secondary
            }
        );
        assert_ne!(source, ActionSource::Key);
        assert!(PointerButton::Primary.is_primary());
        assert!(!PointerButton::Secondary.is_primary());
        assert!(!PointerButton::Middle.is_primary());
    }

    #[test]
    fn scroll_phases_distinguish_discrete_from_continuous() {
        assert!(!ScrollPhase::Discrete.is_continuous());
        for phase in [
            ScrollPhase::Begin,
            ScrollPhase::Update,
            ScrollPhase::End,
            ScrollPhase::Momentum,
            ScrollPhase::Cancel,
        ] {
            assert!(phase.is_continuous(), "{phase:?} 是连续设备阶段");
        }
    }

    #[test]
    fn scroll_events_carry_two_dimensional_deltas() {
        let event = InputEvent::scroll_lines(
            Point::new(5.0, 5.0),
            Point::new(-1.0, 2.0),
            Modifiers::empty(),
        );

        let InputEvent::Scroll {
            delta, unit, phase, ..
        } = event
        else {
            panic!("必须是滚动事件");
        };
        assert_eq!(delta, Point::new(-1.0, 2.0));
        assert_eq!(unit, ScrollUnit::Line);
        assert_eq!(phase, ScrollPhase::Discrete);
    }
}

#[cfg(test)]
mod gesture_tests {
    use std::time::Instant;

    use super::{GestureFlags, GesturePhase, InputEvent, Point, ScrollUnit, scroll_delta};
    use crate::event::Modifiers;

    fn flags(
        may_begin: bool,
        began: bool,
        changed: bool,
        ended: bool,
        cancelled: bool,
    ) -> GestureFlags {
        GestureFlags {
            may_begin,
            began,
            changed,
            ended,
            cancelled,
        }
    }

    #[test]
    fn gesture_flags_map_to_the_four_phases() {
        assert_eq!(
            flags(false, true, false, false, false).phase(),
            Some(GesturePhase::Begin)
        );
        assert_eq!(
            flags(false, false, true, false, false).phase(),
            Some(GesturePhase::Update)
        );
        assert_eq!(
            flags(false, false, false, true, false).phase(),
            Some(GesturePhase::End)
        );
        assert_eq!(
            flags(false, false, false, false, true).phase(),
            Some(GesturePhase::Cancel)
        );
    }

    #[test]
    fn cancellation_wins_over_any_other_flag() {
        assert_eq!(
            flags(false, false, true, true, true).phase(),
            Some(GesturePhase::Cancel)
        );
        assert_eq!(
            flags(false, true, false, true, true).phase(),
            Some(GesturePhase::Cancel)
        );
    }

    #[test]
    fn a_may_begin_预告_is_not_a_gesture() {
        assert_eq!(flags(true, false, false, false, false).phase(), None);
        assert_eq!(
            flags(true, true, false, false, false).phase(),
            Some(GesturePhase::Begin)
        );
    }

    #[test]
    fn a_phaseless_gesture_is_an_update() {
        assert_eq!(
            flags(false, false, false, false, false).phase(),
            Some(GesturePhase::Update)
        );
    }

    #[test]
    fn precise_scrolling_is_passed_through_as_pixels() {
        let (delta, unit) = scroll_delta(true, 3.5, -12.25).expect("precise 事件总是交付");
        assert_eq!(unit, ScrollUnit::Pixel);
        assert_eq!(delta, Point::new(3.5, -12.25));
    }

    #[test]
    fn a_zero_precise_scroll_still_carries_the_phase() {
        let (delta, unit) = scroll_delta(true, 0.0, 0.0).expect("phase 载体");
        assert_eq!(unit, ScrollUnit::Pixel);
        assert_eq!(delta, Point::ZERO);
    }

    #[test]
    fn wheel_notches_are_passed_through_as_lines() {
        let (delta, unit) = scroll_delta(false, 0.0, 1.0).expect("离散滚轮一格");
        assert_eq!(unit, ScrollUnit::Line);
        assert_eq!(delta, Point::new(0.0, 1.0));

        let (delta, unit) = scroll_delta(false, -2.0, 3.0).expect("多格滚轮");
        assert_eq!(unit, ScrollUnit::Line);
        assert_eq!(delta, Point::new(-2.0, 3.0));
    }

    #[test]
    fn a_zero_wheel_event_is_dropped() {
        assert_eq!(scroll_delta(false, 0.0, 0.0), None);
    }

    #[test]
    fn gesture_events_expose_their_payload() {
        let stamp = Instant::now();
        let magnify = InputEvent::Magnify {
            factor: 1.25,
            phase: GesturePhase::Update,
            center: Point::new(40.0, 12.0),
            modifiers: Modifiers::SHIFT,
            timestamp: stamp,
        };
        assert_eq!(magnify.position(), Some(Point::new(40.0, 12.0)));
        assert_eq!(magnify.modifiers(), Modifiers::SHIFT);
        assert_eq!(magnify.timestamp(), stamp);

        let rotate = InputEvent::Rotate {
            degrees: -12.0,
            phase: GesturePhase::End,
            modifiers: Modifiers::empty(),
            timestamp: stamp,
        };
        assert_eq!(rotate.position(), None);
        assert_eq!(rotate.timestamp(), stamp);

        let smart = InputEvent::SmartMagnify {
            center: Point::new(1.0, 2.0),
            modifiers: Modifiers::empty(),
            timestamp: stamp,
        };
        assert_eq!(smart.position(), Some(Point::new(1.0, 2.0)));

        assert_eq!(magnify.with_timestamp(stamp).timestamp(), stamp);
        assert_eq!(rotate.with_timestamp(stamp).timestamp(), stamp);
        assert_eq!(smart.with_timestamp(stamp).timestamp(), stamp);
    }
}

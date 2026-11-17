//! 平台事件、输入与动作契约（公开面；`design/phase-09-win32-demo.md` §4、`design/phase-10-interaction.md` §4/§5）。
//!
//! 平台层（`crate::platform`，私有，I9）把 `WM_*` / `NSEvent` / `wl_*` 翻译成 [`InputEvent`] /
//! [`ImeEvent`]，应用层通过 [`crate::app::App::drain_events`] 收到 [`Event`]；**交互分发**由引擎
//! 自己完成（[`crate::app::App`] 的 `dispatch_input`），动作见 [`Action`]。
//!
//! **坐标一律是窗口逻辑像素**（物理像素 → 逻辑的换算只在平台层做一次，I11）。
//!
//! ## 事件模型的语义来源
//!
//! 输入模型按成熟实现对齐，不按平台直觉发明：
//!
//! | 概念 | 本文件的类型 | 参照 |
//! |---|---|---|
//! | 物理键身份（布局无关） | [`KeyCode`] | W3C UI Events `KeyboardEvent.code`、Chrome `ui::DomCode`、winit `KeyCode`、SDL3 `SDL_Scancode` |
//! | 逻辑键身份 | [`Key`] | W3C `KeyboardEvent.key`（命名部分）、Chrome `ui::DomKey`、winit `Key` |
//! | 自动重复 | [`InputEvent::Key`] 的 `repeat` | winit `KeyEvent.repeat`、Qt `QKeyEvent::isAutoRepeat`、Flutter `KeyEvent.repeat` |
//! | 事件时间戳 | [`InputEvent`] 各变体的 `timestamp` | Win32 `MSG.time`、AppKit `NSEvent.timestamp`、Wayland `input_event.time`、Chrome `WebInputEvent::TimeTicks` |
//! | 点击归属按键 | [`ActionSource::Click`] 的 `button` | Win32 `WM_LBUTTONUP` / AppKit `mouseUp:`、Blink `click` vs `auxclick`、Qt `clicked()` 仅左键 |
//! | 滚动阶段 / 单位 | [`ScrollPhase`] / [`ScrollUnit`] | Chrome `WebMouseWheelEvent.phase`、AppKit `NSEvent.phase`/`momentumPhase`、Qt `QWheelEvent::phase()`、Wayland `axis_value120` |
//! | 字符文本生成判据 | `should_emit_text`（crate 私有） | Win32 `WM_CHAR`、AppKit `characters`、winit 的 xkb 回退通路 |

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

impl PointerButton {
    /// 是否是**主键**：只有主键产生 `click` / 抢焦点（Win32 的 `WM_LBUTTONUP`、AppKit 的
    /// `mouseUp:`、Qt `QAbstractButton::mouseReleaseEvent` 的 `Qt::LeftButton` 判定、
    /// Blink 的 `click`（次键走 `auxclick`））。
    #[must_use]
    pub const fn is_primary(self) -> bool {
        matches!(self, Self::Primary)
    }
}

/// 物理键身份（W3C UI Events `KeyboardEvent.code`；**布局无关**）。
///
/// 同一个物理键在**所有键盘布局**下都是同一个 `KeyCode`：US 布局的字母 `S`、Dvorak 的同一个
/// 物理位置、俄语布局的同一个物理位置都映射到 [`KeyCode::KeyS`]。因此 [`KeyBinding`] 只写一份
/// 就能跨布局、跨平台生效。
///
/// 这与 Chrome（`ui::DomCode`，由 USB HID usage 派生）、winit（`KeyCode`）、SDL3
/// （`SDL_Scancode`）的选择一致。**平台私有码**（Win32 `VK_*` / macOS `NSEvent.keyCode` /
/// Linux evdev）只出现在平台层，不再泄漏进公共契约 —— 旧 `Key::Other(u16)` 的缺陷是同一物理
/// `S` 在三个平台分别得到 `83` / `1` / `23`，`KeyBinding` 无法同时命中，错平台的值还会静默
/// 绑到另一个键。
///
/// 字符输入**不**走这里：字符由操作系统按当前布局翻译后经 [`InputEvent::Text`] 到达
/// （引擎不做 VK → 字符的布局映射）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyCode {
    // ── 书写区（W3C `KeyboardEvent.code` 的 "writing system keys"）──
    /// `` ` ``/`~`（W3C `Backquote`）。
    Backquote,
    /// `\`/`|`（W3C `Backslash`）。
    Backslash,
    /// `[`/`{`（W3C `BracketLeft`）。
    BracketLeft,
    /// `]`/`}`（W3C `BracketRight`）。
    BracketRight,
    /// `,`/`<`（W3C `Comma`）。
    Comma,
    /// `=`/`+`（W3C `Equal`）。
    Equal,
    /// ISO 键盘左边多出来的那个 `\`/`|`（W3C `IntlBackslash`；UK/DE/FR 布局）。
    IntlBackslash,
    /// 日本键盘的 `\`/`_`（W3C `IntlRo`）。
    IntlRo,
    /// 日本键盘的 `¥`/`|`（W3C `IntlYen`）。
    IntlYen,
    /// `-`/`_`（W3C `Minus`）。
    Minus,
    /// `.`/`>`（W3C `Period`）。
    Period,
    /// `'`/`"`（W3C `Quote`）。
    Quote,
    /// `;`/`:`（W3C `Semicolon`）。
    Semicolon,
    /// `/`/`?`（W3C `Slash`）。
    Slash,
    /// 字母 `A` 的物理位置（W3C `KeyA`）。
    KeyA,
    /// 字母 `B` 的物理位置（W3C `KeyB`）。
    KeyB,
    /// 字母 `C` 的物理位置（W3C `KeyC`）。
    KeyC,
    /// 字母 `D` 的物理位置（W3C `KeyD`）。
    KeyD,
    /// 字母 `E` 的物理位置（W3C `KeyE`）。
    KeyE,
    /// 字母 `F` 的物理位置（W3C `KeyF`）。
    KeyF,
    /// 字母 `G` 的物理位置（W3C `KeyG`）。
    KeyG,
    /// 字母 `H` 的物理位置（W3C `KeyH`）。
    KeyH,
    /// 字母 `I` 的物理位置（W3C `KeyI`）。
    KeyI,
    /// 字母 `J` 的物理位置（W3C `KeyJ`）。
    KeyJ,
    /// 字母 `K` 的物理位置（W3C `KeyK`）。
    KeyK,
    /// 字母 `L` 的物理位置（W3C `KeyL`）。
    KeyL,
    /// 字母 `M` 的物理位置（W3C `KeyM`）。
    KeyM,
    /// 字母 `N` 的物理位置（W3C `KeyN`）。
    KeyN,
    /// 字母 `O` 的物理位置（W3C `KeyO`）。
    KeyO,
    /// 字母 `P` 的物理位置（W3C `KeyP`）。
    KeyP,
    /// 字母 `Q` 的物理位置（W3C `KeyQ`）。
    KeyQ,
    /// 字母 `R` 的物理位置（W3C `KeyR`）。
    KeyR,
    /// 字母 `S` 的物理位置（W3C `KeyS`）。
    KeyS,
    /// 字母 `T` 的物理位置（W3C `KeyT`）。
    KeyT,
    /// 字母 `U` 的物理位置（W3C `KeyU`）。
    KeyU,
    /// 字母 `V` 的物理位置（W3C `KeyV`）。
    KeyV,
    /// 字母 `W` 的物理位置（W3C `KeyW`）。
    KeyW,
    /// 字母 `X` 的物理位置（W3C `KeyX`）。
    KeyX,
    /// 字母 `Y` 的物理位置（W3C `KeyY`）。
    KeyY,
    /// 字母 `Z` 的物理位置（W3C `KeyZ`）。
    KeyZ,
    /// 主键盘区的数字 `1`（W3C `Digit1`）。
    Digit0,
    /// 主键盘区的数字 `1`（W3C `Digit1`）。
    Digit1,
    /// 主键盘区的数字 `2`（W3C `Digit2`）。
    Digit2,
    /// 主键盘区的数字 `3`（W3C `Digit3`）。
    Digit3,
    /// 主键盘区的数字 `4`（W3C `Digit4`）。
    Digit4,
    /// 主键盘区的数字 `5`（W3C `Digit5`）。
    Digit5,
    /// 主键盘区的数字 `6`（W3C `Digit6`）。
    Digit6,
    /// 主键盘区的数字 `7`（W3C `Digit7`）。
    Digit7,
    /// 主键盘区的数字 `8`（W3C `Digit8`）。
    Digit8,
    /// 主键盘区的数字 `9`（W3C `Digit9`）。
    Digit9,

    // ── 功能键区（W3C "functional keys"）──
    /// 左 Alt（W3C `AltLeft`）。
    AltLeft,
    /// 右 Alt / AltGr（W3C `AltRight`）。
    AltRight,
    /// 退格（W3C `Backspace`）。
    Backspace,
    /// Caps Lock（W3C `CapsLock`）。
    CapsLock,
    /// 菜单键（W3C `ContextMenu`）。
    ContextMenu,
    /// 左 Control（W3C `ControlLeft`）。
    ControlLeft,
    /// 右 Control（W3C `ControlRight`）。
    ControlRight,
    /// 主键盘区回车（W3C `Enter`）。
    Enter,
    /// 左 Windows / Command 键（W3C `MetaLeft`）。
    MetaLeft,
    /// 右 Windows / Command 键（W3C `MetaRight`）。
    MetaRight,
    /// 左 Shift（W3C `ShiftLeft`）。
    ShiftLeft,
    /// 右 Shift（W3C `ShiftRight`）。
    ShiftRight,
    /// 空格（W3C `Space`）。
    Space,
    /// Tab（W3C `Tab`）。
    Tab,
    // ── 控制键区与方向键 ──
    /// Delete（W3C `Delete`）。
    Delete,
    /// End（W3C `End`）。
    End,
    /// Help（W3C `Help`）。
    Help,
    /// Home（W3C `Home`）。
    Home,
    /// Insert（W3C `Insert`）。
    Insert,
    /// Page Down（W3C `PageDown`）。
    PageDown,
    /// Page Up（W3C `PageUp`）。
    PageUp,
    /// ↓（W3C `ArrowDown`）。
    ArrowDown,
    /// ←（W3C `ArrowLeft`）。
    ArrowLeft,
    /// →（W3C `ArrowRight`）。
    ArrowRight,
    /// ↑（W3C `ArrowUp`）。
    ArrowUp,

    // ── 小键盘（W3C "Numpad"）──
    /// Num Lock（W3C `NumLock`）。
    NumLock,
    /// 小键盘 `0`（W3C `Numpad0`）。
    Numpad0,
    /// 小键盘 `1`（W3C `Numpad1`）。
    Numpad1,
    /// 小键盘 `2`（W3C `Numpad2`）。
    Numpad2,
    /// 小键盘 `3`（W3C `Numpad3`）。
    Numpad3,
    /// 小键盘 `4`（W3C `Numpad4`）。
    Numpad4,
    /// 小键盘 `5`（W3C `Numpad5`）。
    Numpad5,
    /// 小键盘 `6`（W3C `Numpad6`）。
    Numpad6,
    /// 小键盘 `7`（W3C `Numpad7`）。
    Numpad7,
    /// 小键盘 `8`（W3C `Numpad8`）。
    Numpad8,
    /// 小键盘 `9`（W3C `Numpad9`）。
    Numpad9,
    /// 小键盘 `+`（W3C `NumpadAdd`）。
    NumpadAdd,
    /// 小键盘 `,`（W3C `NumpadComma`；欧洲布局）。
    NumpadComma,
    /// 小键盘 `.`（W3C `NumpadDecimal`）。
    NumpadDecimal,
    /// 小键盘 `/`（W3C `NumpadDivide`）。
    NumpadDivide,
    /// 小键盘回车（W3C `NumpadEnter`）。
    NumpadEnter,
    /// 小键盘 `=`（W3C `NumpadEqual`；macOS 的小键盘 `=`）。
    NumpadEqual,
    /// 小键盘 `*`（W3C `NumpadMultiply`）。
    NumpadMultiply,
    /// 小键盘 `-`（W3C `NumpadSubtract`）。
    NumpadSubtract,

    // ── 功能键 ──
    /// Esc（W3C `Escape`）。
    Escape,
    /// F1（W3C `F1`）。
    F1,
    /// F2（W3C `F2`）。
    F2,
    /// F3（W3C `F3`）。
    F3,
    /// F4（W3C `F4`）。
    F4,
    /// F5（W3C `F5`）。
    F5,
    /// F6（W3C `F6`）。
    F6,
    /// F7（W3C `F7`）。
    F7,
    /// F8（W3C `F8`）。
    F8,
    /// F9（W3C `F9`）。
    F9,
    /// F10（W3C `F10`）。
    F10,
    /// F11（W3C `F11`）。
    F11,
    /// F12（W3C `F12`）。
    F12,
    /// F13（W3C `F13`）。
    F13,
    /// F14（W3C `F14`）。
    F14,
    /// F15（W3C `F15`）。
    F15,
    /// F16（W3C `F16`）。
    F16,
    /// F17（W3C `F17`）。
    F17,
    /// F18（W3C `F18`）。
    F18,
    /// F19（W3C `F19`）。
    F19,
    /// F20（W3C `F20`）。
    F20,
    /// F21（W3C `F21`）。
    F21,
    /// F22（W3C `F22`）。
    F22,
    /// F23（W3C `F23`）。
    F23,
    /// F24（W3C `F24`）。
    F24,
    /// Fn（W3C `Fn`；Mac 的 Globe/Fn 键）。
    Fn,
    /// Fn Lock（W3C `FnLock`）。
    FnLock,
    /// Pause / Break（W3C `Pause`）。
    Pause,
    /// Print Screen / SysRq（W3C `PrintScreen`）。
    PrintScreen,
    /// Scroll Lock（W3C `ScrollLock`）。
    ScrollLock,

    // ── 媒体 / 浏览器 / 系统键（W3C `KeyboardEvent.code` 的专有键）──
    //
    // 平台可达性：Windows 全部由 `E0` 扩展扫描码给出（见 `platform::event` 的扩展行表）；
    // Linux 由 evdev 的 `KEY_VOLUME*` / `KEY_PLAYPAUSE` / `KEY_BROWSER_*` 等给出。
    // **macOS 不可达**：AppKit 把媒体键作为 `NSSystemDefined` 事件交给系统处理，不产生
    // `keyDown:` / `NSEvent.keyCode`（引擎不接管系统媒体控制，这是有意的）。
    /// 音量加（W3C `AudioVolumeUp`；Win32 `VK_VOLUME_UP`、Linux `KEY_VOLUMEUP`）。
    AudioVolumeUp,
    /// 音量减（W3C `AudioVolumeDown`）。
    AudioVolumeDown,
    /// 静音（W3C `AudioVolumeMute`）。
    AudioVolumeMute,
    /// 播放/暂停（W3C `MediaPlayPause`）。
    MediaPlayPause,
    /// 停止播放（W3C `MediaStop`）。
    MediaStop,
    /// 下一曲（W3C `MediaTrackNext`）。
    MediaTrackNext,
    /// 上一曲（W3C `MediaTrackPrevious`）。
    MediaTrackPrevious,
    /// 媒体选择（W3C `MediaSelect`）。
    MediaSelect,
    /// 邮件（W3C `LaunchMail`）。
    LaunchMail,
    /// 应用键 1（W3C `LaunchApp1`）。
    LaunchApp1,
    /// 应用键 2（W3C `LaunchApp2`）。
    LaunchApp2,
    /// 浏览器后退（W3C `BrowserBack`）。
    BrowserBack,
    /// 浏览器前进（W3C `BrowserForward`）。
    BrowserForward,
    /// 浏览器主页（W3C `BrowserHome`）。
    BrowserHome,
    /// 浏览器刷新（W3C `BrowserRefresh`）。
    BrowserRefresh,
    /// 浏览器停止（W3C `BrowserStop`）。
    BrowserStop,
    /// 浏览器搜索（W3C `BrowserSearch`）。
    BrowserSearch,
    /// 浏览器收藏夹（W3C `BrowserFavorites`）。
    BrowserFavorites,
    /// 电源（W3C `Power`）。
    Power,
    /// 睡眠（W3C `Sleep`）。
    Sleep,
    /// 唤醒（W3C `WakeUp`）。
    WakeUp,

    // ── 未识别 ──
    /// 平台给了本枚举没有覆盖的键；载荷是**平台私有码**，**不可跨平台**。
    ///
    /// 编码：Windows 是扫描码（bit 8 置位表示 `E0` 扩展键；**没有**扫描码时是
    /// `0x8000 | VK`）；macOS 是 `NSEvent.keyCode`；Linux 是 evdev 码。
    /// `Unidentified(31)` 在 Windows 是字母 `S` 的扫描码（`0x1F` = 31），在 macOS 是别的键
    /// —— 应用**不得**把它写进跨平台绑定表（引擎自己的默认绑定只用命名变体）。
    Unidentified(u16),
}

/// 逻辑键（W3C `KeyboardEvent.key` 的**命名**部分）。
///
/// 与 [`KeyCode`] 的分工和 Chrome（`DomCode`/`DomKey`）、winit（`KeyCode`/`Key`）、SDL3
/// （`Scancode`/`Keycode`）相同：
///
/// - [`KeyCode`] = **物理**身份，布局无关，用于快捷键绑定与「按住集合」；
/// - [`Key`] = **逻辑**身份，即「这个键在语义上是什么」（回车 / Esc / F5 / 方向键…），
///   用于「按语义区分」的默认行为。
///
/// 可打印字符的逻辑身份依赖键盘布局（US 布局的物理 `KeyS` 是 `s`，俄语布局是 `ы`），
/// 因此字符键**没有** `Key` —— 字符输入一律走 [`InputEvent::Text`]。
///
/// 由 [`KeyCode`] 投影得到（[`Key::from_code`]），所以三个平台不需要各写一份逻辑键映射表。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// Esc。
    Escape,
    /// 回车（含小键盘回车）。
    Enter,
    /// Tab。
    Tab,
    /// 退格。
    Backspace,
    /// Delete。
    Delete,
    /// Insert。
    Insert,
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
    /// F1…F24（`1..=24`）。
    Function(u8),
    /// 左/右 Shift。
    Shift,
    /// 左/右 Control。
    Control,
    /// 左/右 Alt。
    Alt,
    /// 左/右 Windows 键 / Command。
    Meta,
}

impl Key {
    /// 物理键 → 逻辑键（`None` = 该物理键没有命名逻辑身份，字符输入走 [`InputEvent::Text`]）。
    ///
    /// 左右修饰键在逻辑层合并（`Modifiers` 位图也表达不了左右；需要区分左右时用 [`KeyCode`]）。
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

    /// 是否是修饰键（`Shift`/`Control`/`Alt`/`Meta`）。
    #[must_use]
    pub const fn is_modifier(self) -> bool {
        matches!(self, Self::Shift | Self::Control | Self::Alt | Self::Meta)
    }
}

/// 修饰键位图变化 → 需要合成的 `Key` 事件 `(物理键身份, 按下/抬起)`。
///
/// AppKit **不**为修饰键投递 `keyDown:` / `keyUp:`（只有 `flagsChanged:`），而 Win32 与
/// Wayland 都把 Shift/Ctrl/Alt/Meta 当普通按键投递。macOS 平台层据此把修饰键补成同样的
/// 序列，否则 `App::held_keys` / `App::key_down` 在 macOS 上永远不含修饰键 —— 三平台语义
/// 分叉（Win/Linux 为 true、macOS 恒 false）。
///
/// 位图不区分左右（[`Modifiers`] 只有四个位）⇒ 统一返回**左**变体；需要区分左右时用平台
/// 原生码（[`KeyCode`]）。
pub fn modifier_key_transitions(
    previous: Modifiers,
    current: Modifiers,
) -> impl Iterator<Item = (KeyCode, KeyState)> {
    // 顺序固定（Shift、Control、Alt、Meta）：同一次 `flagsChanged:` 可能同时改变多个位，
    // 事件顺序必须稳定，应用的 `key_down` 观察才可预期。
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

/// 按键状态。
///
/// 自动重复**不是**第三种状态：它是「按下」的一种属性，由 [`InputEvent::Key`] 的 `repeat`
/// 字段表达（winit `KeyEvent.repeat`、Qt `QKeyEvent::isAutoRepeat` 同一建模）。这样
/// `state` 的取值只有物理上的两态，不会出现「`Repeat` 之后又收到 `Up`」这种需要额外状态机
/// 才能对齐的序列。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyState {
    /// 按下（自动重复时 `repeat == true`）。
    Down,
    /// 抬起。
    Up,
}

/// 滚动单位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollUnit {
    /// `delta` 的单位是**行**（离散滚轮；`1` 行 = 容器自己的 `line_height`）。
    Line,
    /// `delta` 的单位是**逻辑像素**（触控板 / 精确滚动设备 1:1 跟随）。
    Pixel,
}

/// 滚动阶段（Chrome `WebMouseWheelEvent::Phase`、Qt `QWheelEvent::phase()`、
/// AppKit `NSEvent.phase` + `momentumPhase`、Blink `WebMouseWheelEvent.phase`）。
///
/// 离散滚轮（一格 = 一条消息）用 [`ScrollPhase::Discrete`]；触控板 / 精确设备才区分
/// 开始 / 更新 / 结束 / 惯性，引擎据此决定「立刻到位」还是「交给系统惯性」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollPhase {
    /// 离散输入（滚轮一格）：单条事件自成一笔，没有阶段。
    Discrete,
    /// 连续手势开始（AppKit `NSEventPhaseBegan`）。
    Begin,
    /// 连续手势进行中（AppKit `NSEventPhaseChanged`）。
    Update,
    /// 连续手势结束（AppKit `NSEventPhaseEnded`）。
    End,
    /// 系统惯性滑行（AppKit `momentumPhase`）。
    Momentum,
    /// 手势被取消（AppKit `NSEventPhaseCancelled`）。
    Cancel,
}

impl ScrollPhase {
    /// 是否是**连续设备**的阶段（触控板 / 精确滚动）。离散滚轮为 `false`。
    #[must_use]
    pub const fn is_continuous(self) -> bool {
        !matches!(self, Self::Discrete)
    }
}

/// 手势（捏合 / 旋转）的阶段。
///
/// 与 [`ScrollPhase`] 分开是因为二者的**生命周期来源不同**：滚动是一串可惯性延续的事件流
/// （还有 `Momentum`），手势是「按下 → 移动 → 抬起」的封闭会话，平台层必须把开始/结束完整映射，
/// 应用才能做锚点变换、结束提交与惯性收尾（AppKit `NSEventPhase`、Windows `WM_GESTURE`
/// 的 `GID_BEGIN`/`GID_END`、Wayland `zwp_pointer_gesture_*` 的 `begin`/`update`/`end` 同构）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GesturePhase {
    /// 手势开始（AppKit `NSEventPhaseBegan`）。
    Begin,
    /// 手势进行中（AppKit `NSEventPhaseChanged`；增量事件都落在这里）。
    Update,
    /// 手势结束（AppKit `NSEventPhaseEnded`）：本阶段通常携带 0 增量，只作收尾信号。
    End,
    /// 手势被取消（AppKit `NSEventPhaseCancelled`）：必须回滚，不得提交。
    Cancel,
}

/// 原生手势事件的阶段位图（AppKit `NSEventPhase`、Windows `WM_GESTURE` 的 `GF_BEGIN`、
/// Wayland `zwp_pointer_gesture_*` 的 `begin`/`update`/`end`）。
///
/// 纯数据 + 纯函数，放在平台无关层是为了让「阶段归一化」这条规则能在开发机上单测 ——
/// 各平台的包装器只在各自目标上编译，测不到。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GestureFlags {
    /// 「手势可能开始」的预告阶段（AppKit `NSEventPhaseMayBegin`；没有增量）。
    pub may_begin: bool,
    /// 开始。
    pub began: bool,
    /// 进行中。
    pub changed: bool,
    /// 结束。
    pub ended: bool,
    /// 取消。
    pub cancelled: bool,
}

impl GestureFlags {
    /// 归一化成引擎的手势阶段；`None` = 本事件没有可交付的阶段（只有「可能开始」的预告）。
    ///
    /// 优先级：取消 > 结束 > 开始 > 进行中。空位图（没有阶段信息的离散手势）按「进行中」处理：
    /// 它带的是一次增量，既不是开始也不是结束信号。
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
            // 「可能开始」不是手势：此时还没有任何增量，交付 `Begin` 会让应用的锚点变换
            // 以 0 增量开始、并在真正的 `Began` 到来时重复初始化。
            return None;
        }
        Some(GesturePhase::Update)
    }
}

/// 原生滚动增量 → 引擎滚动量与单位（R09-8）。
///
/// 契约：**同一个 delta 数值在三个后端表示同一件事**，平台层不再用「除一个常数」把物理单位
/// 藏起来。单位由 [`ScrollUnit`] 表达，应用自己按容器语义使用：
///
/// - 精确设备（触控板 / Magic Mouse，`hasPreciseScrollingDeltas == true`）：AppKit 给的
///   `scrollingDeltaX/Y` 单位是**点**，与引擎的逻辑像素 1:1 ⇒ [`ScrollUnit::Pixel`]，**直传**。
///   两个轴都为 0 的事件仍要交付（`phase` 的开始/结束标记常常是 0 增量）。
/// - 传统滚轮（`hasPreciseScrollingDeltas == false`）：单位是**行** ⇒ [`ScrollUnit::Line`]，
///   **直传**，不再除 3。对齐依据：Win32 一格 `WM_MOUSEWHEEL` = `WHEEL_DELTA`(120) =
///   `SPI_GETWHEELSCROLLLINES` 行（缺省 3 行，见 `platform::event::lines_per_tick`）—— 也就是
///   「一格 = 若干行」这个语义在两端一致，差的是**一个物理缺口产生几行**（Windows 的系统设置
///   可配 1/3/…，macOS 不暴露该设置，AppKit 直接给行数）。旧实现的 `/3` 把 macOS 再缩小 3 倍，
///   于是同一个滚轮在两端差 3 倍，且触控板的 `/10` 让灵敏度随滑动速度剧烈变化。
/// - 传统滚轮且两轴都为 0：没有可交付的增量 ⇒ `None`（AppKit 会发这种空事件）。
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

/// 一次输入事件。
///
/// 每个变体都带 `timestamp`：**平台的真实事件时刻**（Win32 的 `MSG.time` 经
/// [`timestamp_from_ticks_32`]、AppKit 的 `NSEvent.timestamp` 经 [`timestamp_from_ticks`]、
/// Wayland 的 `input_event.time` 换算成单调时钟）。
/// 双击判定、连击计数、拖拽起点都用它，而不是「分发时刻」——UI 线程卡顿时两者能差几十毫秒。
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    /// 指针进入窗口客户区（W3C `pointerenter` 的窗口级对应；AppKit `mouseEntered:` /
    /// Win32 `TrackMouseEvent` 的首次 `WM_MOUSEMOVE` / Wayland `wl_pointer.enter`）。
    PointerEnter {
        /// 窗口逻辑坐标。
        position: Point,
        /// 当前修饰键。
        modifiers: Modifiers,
        /// 事件时刻。
        timestamp: Instant,
    },
    /// 指针移动。
    PointerMove {
        /// 窗口逻辑坐标。
        position: Point,
        /// 当前修饰键。
        modifiers: Modifiers,
        /// 事件时刻。
        timestamp: Instant,
    },
    /// 指针按下。
    PointerDown {
        /// 窗口逻辑坐标。
        position: Point,
        /// 按键。
        button: PointerButton,
        /// 当前修饰键。
        modifiers: Modifiers,
        /// 事件时刻。
        timestamp: Instant,
    },
    /// 指针抬起。
    PointerUp {
        /// 窗口逻辑坐标。
        position: Point,
        /// 按键。
        button: PointerButton,
        /// 当前修饰键。
        modifiers: Modifiers,
        /// 事件时刻。
        timestamp: Instant,
    },
    /// 指针离开客户区（W3C `pointerleave`；带离开点，AppKit `mouseExited:` /
    /// Wayland `wl_pointer.leave` 都能给出，Win32 `WM_MOUSELEAVE` 没有坐标时用最近一次位置）。
    PointerLeave {
        /// 离开时的窗口逻辑坐标（最近一次已知位置）。
        position: Point,
        /// 当前修饰键。
        modifiers: Modifiers,
        /// 事件时刻。
        timestamp: Instant,
    },
    /// 指针会话被系统**取消**（不是用户松手）：指针捕获丢失、窗口失焦、系统弹窗夺走输入。
    ///
    /// 与 [`Self::PointerUp`] 的区别是语义：`Up` 可能触发点击，`Cancel` 必须**不**触发任何
    /// 提交，只结束拖拽/按压并派发 `DragPhase::Cancel`。丢了这个事件，拖拽会话会永久卡在
    /// 「按住」状态（Windows 的 `WM_CAPTURECHANGED`、macOS 的 `resignKey`、Linux 的指针
    /// `leave` 都可能在没有 `Up` 的情况下结束一次拖拽）。
    PointerCancel {
        /// 事件时刻。
        timestamp: Instant,
    },
    /// 滚轮 / 触控板滚动。
    Scroll {
        /// 窗口逻辑坐标。
        position: Point,
        /// 二维滚动量（正 = 内容向右 / 向上移动；`y` 与 Win32 `WM_MOUSEWHEEL` 的正方向一致）。
        ///
        /// 单位由 [`Self::Scroll`] 的 `unit` 决定：`Line` 时 `1.0` = 一格（再乘容器的
        /// `line_height`），`Pixel` 时就是逻辑像素。二维 delta 是**必须**可达的：
        /// K 线图 / 深度图的双指平移要同时消费两轴。
        ///
        /// **容器是单轴的**（[`crate::interaction::ScrollAxis`]）：引擎按轴拆分二维 delta 并
        /// 沿滚动链逐层消费 —— 内层容器只吃自己声明的轴，另一轴原样传给外层（浏览器的
        /// scroll chaining）。需要同时平移两轴的区域由**两层容器**表达（内层
        /// `Horizontal` + 外层 `Vertical`，每轴各由一个容器消费）；单容器同时吃两轴不在
        /// 本引擎的契约内（R02-11 的取舍，见 `design/phase-10-interaction.md` §7）。
        delta: Point,
        /// 单位。
        unit: ScrollUnit,
        /// 阶段。
        phase: ScrollPhase,
        /// 当前修饰键（`Shift + 滚轮` 的横向滚动走它）。
        modifiers: Modifiers,
        /// 事件时刻。
        timestamp: Instant,
    },
    /// 捏合缩放（macOS `magnifyWithEvent:`；R09-4）。
    ///
    /// `factor` 是**本次增量**（不是累计倍率）：应用自己累乘到视图缩放上（与 iPadOS /
    /// Windows `WM_GESTURE` 的增量语义一致，平台层不做累积、不猜手势结束时机）。
    /// `1.0` = 无变化，`> 1.0` = 放大，`< 1.0` = 缩小。
    Magnify {
        /// 本次缩放因子（增量）。
        factor: f32,
        /// 手势阶段。
        phase: GesturePhase,
        /// 缩放锚点（窗口逻辑坐标：双指中心）—— 不做锚点变换就只能以左上角为中心缩放。
        center: Point,
        /// 当前修饰键。
        modifiers: Modifiers,
        /// 事件时刻。
        timestamp: Instant,
    },
    /// 旋转手势（macOS `rotateWithEvent:`；Windows `WM_GESTURE` 的 `GID_ROTATE`）。
    Rotate {
        /// 本次旋转角度（**度**，增量；正负方向由平台定义）。
        degrees: f32,
        /// 手势阶段。
        phase: GesturePhase,
        /// 当前修饰键。
        modifiers: Modifiers,
        /// 事件时刻。
        timestamp: Instant,
    },
    /// 系统判定的「智能缩放」双击（macOS `smartMagnifyWithEvent:`：触控板双指双击）。
    ///
    /// 与 [`Self::Magnify`] 分开是因为它**没有增量也没有阶段**：系统已经决定了「缩放到适配/
    /// 原始尺寸」这一整步，应用按自己的语义切换（图表「复位视图」是典型用法）。把它硬塞进
    /// `Magnify` 会逼应用去猜一个魔法 `factor`。
    SmartMagnify {
        /// 缩放中心（窗口逻辑坐标）。
        center: Point,
        /// 当前修饰键。
        modifiers: Modifiers,
        /// 事件时刻。
        timestamp: Instant,
    },
    /// 非字符键。
    Key {
        /// 物理键身份（布局无关）。
        code: KeyCode,
        /// 按下/抬起。
        state: KeyState,
        /// 是否**自动重复**（操作系统合成的重复按键，不是新的按下）。
        ///
        /// 来源：Win32 `WM_KEYDOWN.lParam` bit 30、AppKit `NSEvent.isARepeat`、
        /// Wayland 客户端合成（`wl_keyboard.repeat_info` + `xkb_keymap_key_repeats`）。
        /// 引擎的绑定派发**忽略重复**（长按不会重复触发动作），只有默认的导航/滚动行为
        /// 允许重复执行。
        repeat: bool,
        /// 当前修饰键。
        modifiers: Modifiers,
        /// 事件时刻。
        timestamp: Instant,
    },
    /// 字符输入（`WM_CHAR`、AppKit `characters`、xkb `key_get_utf8`；死键组合已由平台完成）。
    ///
    /// **不含** IME 提交文本：已确认的输入法文本走 [`ImeEvent::Commit`]（两者由平台层保证
    /// 互斥：IME 启用时 `Text` 通路静默）。自动重复**不**抑制本事件。
    Text {
        /// 一个或多个 `char`（代理对已合并）。
        text: SmolStr,
        /// 事件时刻。
        timestamp: Instant,
    },
}

impl InputEvent {
    /// 事件位置（只有指针/滚轮/手势事件有位置）。
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

    /// 事件携带的修饰键（无修饰键概念的事件返回空集）。
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

    /// 平台的**事件时刻**（所有变体都有；双击/连击/拖拽起点以它为基准）。
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

    /// 换掉事件时间戳（`TestApp::inject` 用**可控时钟**盖戳，双击/连击/拖拽判定才可测）。
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

    /// 指针移动事件（时间戳 = 当前时刻）。
    ///
    /// 供**测试与程序化注入**使用；平台层必须用平台时间戳构造具体变体，否则双击判定会退化
    /// 成「分发时刻」。
    #[must_use]
    pub fn pointer_move(position: Point, modifiers: Modifiers) -> Self {
        Self::PointerMove {
            position,
            modifiers,
            timestamp: Instant::now(),
        }
    }

    /// 指针按下事件（时间戳 = 当前时刻；供测试与程序化注入）。
    #[must_use]
    pub fn pointer_down(position: Point, button: PointerButton, modifiers: Modifiers) -> Self {
        Self::PointerDown {
            position,
            button,
            modifiers,
            timestamp: Instant::now(),
        }
    }

    /// 指针抬起事件（时间戳 = 当前时刻；供测试与程序化注入）。
    #[must_use]
    pub fn pointer_up(position: Point, button: PointerButton, modifiers: Modifiers) -> Self {
        Self::PointerUp {
            position,
            button,
            modifiers,
            timestamp: Instant::now(),
        }
    }

    /// 滚轮事件（离散一格，单位 = 行，时间戳 = 当前时刻；供测试与程序化注入）。
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

    /// 按键按下事件（时间戳 = 当前时刻；供测试与程序化注入）。
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

    /// 按键抬起事件（时间戳 = 当前时刻；供测试与程序化注入）。
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

/// 按键是否产生**字符文本**（[`InputEvent::Text`]）。
///
/// 这是三条文本通路共用的**唯一判据**（Win32 `WM_CHAR`、AppKit `characters`、Wayland xkb
/// 回退），因此把它做成公开纯函数，让「重复不得抑制文本」这条契约有可执行的定义与回归测试。
///
/// 判据只有三条：
/// - 必须是**按下**（抬起不产生文本）；
/// - 文本通路没有被 IME 接管（IME 启用时文本只走 [`ImeEvent::Commit`]，否则一次按键会得到
///   两份文本）；
/// - 不在 IME 组合期（组合串由 `ImeEvent::Preedit` 表达，组合期的物理按键不产生正文）。
///
/// `repeat` **有意不参与判定**：自动重复只影响绑定/动作派发（`app::input::on_key` 忽略重复），
/// 不抑制字符生成。Wayland 的字符文本**正是**随客户端合成的重复产生的（服务端从不重发
/// `key` 事件），Win32 的 `WM_CHAR` 与 AppKit 的 `characters` 同样不受 `isARepeat` 过滤。
/// 保留这个形参是契约的一部分：一旦有人重新拿 `repeat` 过滤文本，
/// `should_emit_text_ignores_auto_repeat` 就会失败。
#[must_use]
pub fn should_emit_text(
    state: KeyState,
    _repeat: bool,
    ime_takes_text: bool,
    composing: bool,
) -> bool {
    matches!(state, KeyState::Down) && !ime_takes_text && !composing
}

/// 平台毫秒时钟 → [`Instant`]（R02-14：事件时间戳）。
///
/// 用于**不回绕**的 64 位 tick 域（AppKit `NSEvent.timestamp` / `NSProcessInfo.systemUptime`
/// 的毫秒）。Win32 的 `MSG.time` 是 32 位回绕域，必须用 [`timestamp_from_ticks_32`]。
///
/// 三个平台的原生事件时间戳都是「单调毫秒 tick」，但原点不同（Win32 `MSG.time` 与
/// `GetTickCount64` 同源、AppKit `NSEvent.timestamp` 是系统启动起的秒、Wayland
/// `input_event.time` 是毫秒）；`Instant` 的原点**未定义且跨进程不可比较**。因此换算方式是
/// 在同一处同时取一次 `Instant::now()` 与平台 tick，再把事件 tick 按差值回推：
///
/// ```text
/// event_instant ≈ now - (now_ticks - event_ticks)
/// ```
///
/// 差值为负（事件比「现在」还新，例如时钟源不同步）时饱和到 `now`；事件时间过旧时同理钳到
/// `now` 之前能表示的最远点 —— 绝不 panic，最坏退化成分发时刻。
#[must_use]
pub fn timestamp_from_ticks(now: Instant, now_ticks_ms: u64, event_ticks_ms: u64) -> Instant {
    let behind = now_ticks_ms.saturating_sub(event_ticks_ms);
    now.checked_sub(std::time::Duration::from_millis(behind))
        .unwrap_or(now)
}

/// 平台毫秒 tick（**32 位回绕域**）→ [`Instant`]（R02-14：Win32 `MSG.time` / `GetTickCount`）。
///
/// Win32 的事件时间戳是 `MSG.time`：与 `GetTickCount` 同源的 32 位毫秒，**49.7 天回绕一次**。
/// 因此「现在」也必须取同一来源、同一宽度的 tick（`GetTickCount64() as u32`），差值用
/// `wrapping_sub` 计算：回绕两侧（事件在回绕前、现在在回绕后）仍然得到正确的短差值，
/// 而不是把 49.7 天的量当成「很久以前」。
///
/// 混用两套时钟原点（旧实现的 QPC 毫秒 + `MSG.time`）会**系统性地**平移所有时间戳，且回绕后
/// 静默退化；这条约束由 [`timestamp_from_ticks_32`] 的签名与单测钉住。
///
/// 差值大于 [`MAX_PLAUSIBLE_BEHIND_MS`]（事件比现在早 1 h 以上，说明时钟源不一致或事件陈旧）
/// 时退化为「现在」：宁可把陈旧事件当成刚发生，也不返回一个 49 天前的时间戳。
#[must_use]
pub fn timestamp_from_ticks_32(now: Instant, now_ticks_ms: u32, event_ticks_ms: u32) -> Instant {
    let behind = now_ticks_ms.wrapping_sub(event_ticks_ms);
    if behind > MAX_PLAUSIBLE_BEHIND_MS {
        return now;
    }
    now.checked_sub(std::time::Duration::from_millis(u64::from(behind)))
        .unwrap_or(now)
}

/// [`timestamp_from_ticks_32`] 认为「可信」的最大落后量（1 小时）。
///
/// UI 线程卡顿 / 事件排队最多是秒级；超过 1 h 只可能来自时钟源不一致（原点不同）或
/// 32 位 tick 回绕后差值算错 —— 两种情况都必须退化而不是产出错误时间。
pub const MAX_PLAUSIBLE_BEHIND_MS: u32 = 60 * 60 * 1_000;

/// 输入法事件（P09 只到「组合串可见 + 提交」这一层，交互处理归文本 Phase）。
///
/// 组合串 / 提交文本可能只替换已有文本的一部分（macOS `NSTextInputClient` 的
/// `replacementRange`、输入法逐子句更新与重转换），因此两个正文变体都带一个
/// **UTF-16 码元**替换区间 `replace`：
///
/// - `None`：AppKit 的 `{NSNotFound, 0}` —— 没有显式区间，替换**整个**当前组合串
///   （没有组合串时在插入点插入）；
/// - `Some(range)`：区间相对**当前组合串起点**（UTF-16 码元），新文本替换该区间里的旧文本，
///   区间外的组合串余下文本保留。当前平台层给出的端点已吸附到码点边界（不会拆开代理对）。
///
/// **已知限制**（macOS）：AppKit 给的 `replacementRange` 是**文档坐标**，平台层按
/// `markedRange.location`（当前恒为 0）换算成上面的相对区间；但本 view 还没有
/// surrounding-text 通路 —— 没有实现 `attributedSubstringForProposedRange:`（返回 `nil`），
/// `selectedRange` / `markedRange` 的文档坐标也只覆盖组合串。因此输入法对**已提交文本**的
/// 重转换（韩文 Hanja、日文再変換）无法表达：这类区间会被 clamp 到当前组合串边界（区间在
/// 组合串之前时起点收敛到 0，在组合串之后时收敛到组合串末尾）。逐子句更新组合串与普通提交
/// 不受影响。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImeEvent {
    /// 开始组合。
    Start,
    /// 组合中的文本 + 组合串内的插入点与子句信息。
    Preedit {
        /// 组合中的文本。
        text: SmolStr,
        /// 组合串内的插入点（**UTF-16 码元**偏移，相对 `text` 起点，`0..=text.encode_utf16().count()`）。
        ///
        /// Win32 来源是 `ImmGetCompositionStringW(.., GCS_CURSORPOS, ..)`，macOS 来源是
        /// `setMarkedText:selectedRange:replacementRange:` 的 `selectedRange.location`：用户用
        /// ←/→/Home/End 在拼音串里改字时，插入点停在串中间，文本层必须按它画插入点（否则光标
        /// 恒在末尾，用户看不到自己正在改哪个字）。`None` = 平台没有提供该信息（Linux 当前
        /// 不提供，或本次更新只改了文本）——文本层退化为「插入点在末尾」。
        caret: Option<u32>,
        /// 组合串内需要**高亮/区分**的子句区间（UTF-16 码元，相对 `text` 起点，按起点升序、
        /// 互不重叠、全部落在 `0..=len`）。
        ///
        /// Win32 来源是 `GCS_COMPATTR`（每码元一个属性字节）与 `GCS_COMPCLAUSE`（子句边界）：
        /// 引擎只交付「非 `ATTR_INPUT` 的那几段」，也就是已转换 / 转换中 / 已固定的子句 ——
        /// 文本层据此画下划线/加粗，与原生输入法控件观感一致（日文 IME 的按子句跳转也依赖它）。
        /// 空 = 没有子句信息（整串都还在输入中，或平台不提供）。
        clauses: SmallVec<[Range<usize>; 4]>,
        /// 要替换的旧文本区间（UTF-16 码元，相对当前组合串起点）；`None` = 整个当前组合串。
        replace: Option<Range<usize>>,
    },
    /// 已提交的文本。
    Commit {
        /// 已提交的文本。
        text: SmolStr,
        /// 要替换的旧文本区间（UTF-16 码元，相对当前组合串起点）；`None` = 整个当前组合串。
        replace: Option<Range<usize>>,
    },
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
    /// 节点级悬停迁移（**引擎派生**，R02-4；对应 W3C `pointerover`/`pointerout` 的节点级语义
    /// 与 CSS `:hover` 的传播方式）。
    ///
    /// 指针的悬停节点从旧链变成新链时产生一条：`left` 是离开链的部分（由内到外），
    /// `entered` 是进入链的部分（由外到内）。祖先节点因此也能收到自己的 enter/leave ——
    /// 「子节点悬停时父容器也高亮」不需要应用自己走祖先链。同一批变化的节点由引擎标
    /// `PAINT` 并请求一帧，应用只需在自己的 `bind_style` 里读 [`crate::app::App::hovered`]。
    HoverChanged {
        /// 指针当前悬停的节点（`None` = 空白处 / 窗口外）。
        hovered: Option<NodeId>,
        /// 离开悬停链的节点（由内到外）。
        left: Vec<NodeId>,
        /// 进入悬停链的节点（由外到内）。
        entered: Vec<NodeId>,
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
    /// 指针点击（**只有主键**），或焦点节点的 Enter/Space 激活：连击计数（1 = 单击）。
    Click {
        /// 连击计数（1 起；双击 = 2）。
        click_count: u8,
        /// 产生这次点击的按键（键盘激活恒为 [`PointerButton::Primary`]）。
        ///
        /// 次键（右键/中键）**不**产生 `Click`：Win32 只有 `WM_LBUTTONUP` 对应按钮的
        /// `BN_CLICKED`、AppKit 的 target/action 只挂左键、Qt 的
        /// `QAbstractButton::mouseReleaseEvent` 显式 `if (e->button() != Qt::LeftButton) return`、
        /// Blink 的 `click` 也只由主键产生。携带 button 是为了让应用能区分「键盘激活」与
        /// 「指针点击」（`source()` 是 `Copy`，可安全比较）。
        button: PointerButton,
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
    /// 物理键（[`KeyCode`]：布局无关，三平台同一张表；字符输入走 [`InputEvent::Text`]）。
    pub key: KeyCode,
    /// **精确**修饰键集合（`Ctrl+S` 不匹配 `Ctrl+Shift+S`；未按的修饰键必须为空）。
    pub modifiers: Modifiers,
    /// 命中的动作。
    pub action: Action,
}

/// 动作处理器（应用注册；`FnMut`，可写状态）。
pub type ActionHandler = Box<dyn FnMut(&mut crate::interaction::ActionCtx<'_>)>;

/// 关闭前处理器（`App::on_close`）。返回 `false` = 拦下这次关闭。
///
/// 平台在用户点窗口 X / Cmd-W 时**先问它**；应用可以在这里保存未提交状态、弹确认框，
/// 或直接拒绝关闭。没有处理器 ⇒ 直接关（与旧行为一致）。
pub type CloseHandler = Box<dyn FnMut(&mut crate::app::App) -> bool>;

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
        // 事件 20 ms 之前发生。
        let stamp = timestamp_from_ticks(now, 5_000, 4_980);
        assert_eq!(stamp, now - Duration::from_millis(20));
        // 同一时刻。
        assert_eq!(timestamp_from_ticks(now, 5_000, 5_000), now);
        // tick 倒退（时钟源不同步 / 回绕）：饱和到 `now`，绝不 panic。
        assert_eq!(timestamp_from_ticks(now, 5_000, 9_000), now);
    }

    /// R02-14（Windows）：`MSG.time` 是 **32 位**、49.7 天回绕的 tick，与「现在」同源同宽。
    ///
    /// 回归点：旧实现拿 `QueryPerformanceCounter` 的毫秒当「现在 tick」—— 原点与宽度都与
    /// `MSG.time` 不同，所有公开时间戳被系统性平移；回绕之后差值变成 49.7 天、整批退化。
    #[test]
    fn wrapping_32_bit_ticks_convert_without_drifting_across_the_wrap() {
        let now = Instant::now();
        // 普通情况：20 ms 之前。
        assert_eq!(
            timestamp_from_ticks_32(now, 5_000, 4_980),
            now - Duration::from_millis(20)
        );
        // 回绕两侧：现在在 0x0000_0005，事件在 0xFFFF_FFF0（回绕前 21 ms）。
        assert_eq!(
            timestamp_from_ticks_32(now, 5, u32::MAX - 15),
            now - Duration::from_millis(21),
            "回绕必须用 wrapping_sub 计算差值，不能当成 49.7 天"
        );
        // 事件比现在还新（时钟源不一致）：退化成分发时刻，绝不返回未来时间。
        assert_eq!(timestamp_from_ticks_32(now, 5_000, 9_000), now);
        // 差值大得不合理（> 1 h，说明原点不同）：同样退化，而不是产出 49 天前的时间戳。
        assert_eq!(
            timestamp_from_ticks_32(now, 0, 2 * crate::event::MAX_PLAUSIBLE_BEHIND_MS),
            now
        );
    }

    /// R02-2（P0 回归，Linux）：自动重复**不得**抑制字符文本。
    ///
    /// Wayland 的字符文本正是随客户端合成的重复产生的（服务端从不重发 `key` 事件），而
    /// `ime_enabled` 默认 `false`（XKB 回退是默认通路）。旧实现把 `!repeat` 写进文本条件，
    /// 于是按住可打印键只输入一个字符；Win32 `WM_CHAR` / AppKit `characters` 不受
    /// `isARepeat` 过滤，三平台行为因此分叉。
    #[test]
    fn should_emit_text_ignores_auto_repeat() {
        // 按下 + 未启用 IME + 不在组合期 ⇒ 产生文本；重复位不影响。
        assert!(should_emit_text(KeyState::Down, false, false, false));
        assert!(
            should_emit_text(KeyState::Down, true, false, false),
            "合成重复必须继续产生文本（P0 回归）"
        );
        // 抬起不产生文本。
        assert!(!should_emit_text(KeyState::Up, false, false, false));
        // IME 接管文本时 XKB/字符通路必须静默（否则一次按键两份文本）。
        assert!(!should_emit_text(KeyState::Down, false, true, false));
        // 组合期的物理按键不产生正文。
        assert!(!should_emit_text(KeyState::Down, false, false, true));
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
        // R02-1：绑定表的键是布局无关的 `KeyCode`，不是平台私有码。
        assert_ne!(binding.key, KeyCode::Unidentified(0x41));
    }

    /// macOS `flagsChanged:` 的合成判据（R02-16）：修饰键必须进「按住集合」。
    ///
    /// 回归点：AppKit 不为修饰键发 `keyDown:`/`keyUp:`，旧实现只更新位图 ⇒
    /// `App::key_down(ShiftLeft)` 在 macOS 上恒为 false（Win32/Wayland 为 true）。
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

    /// 媒体 / 浏览器键没有逻辑 `Key` 投影：默认导航（方向键/翻页键）不会被音量键触发，
    /// 应用也只能通过 [`KeyCode`] 绑定它们。
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
        // 字符键没有命名逻辑身份：字符输入走 `InputEvent::Text`。
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
        // 只有主键是「click」语义。
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

/// 手势阶段归一化与滚轮单位换算（R09-4 / R09-8）。
///
/// 这两个规则是**跨后端契约**（macOS 的 `NSEventPhase`、Windows `WM_GESTURE`、Wayland 的
/// gesture 协议都要落到同一套枚举上），因此单独成块钉死，不随 `tests` 里的交互用例漂移。
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

    /// 四个阶段必须完整映射（平台层不做累积、不猜结束时机）。
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

    /// 取消优先于其它位（手势被系统打断时绝不能报成 End ⇒ 应用会提交一个被取消的变换）。
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

    /// `MayBegin` 只是「可能会开始」：没有增量，交付 `Begin` 会让应用的锚点变换重复初始化。
    #[test]
    fn a_may_begin_预告_is_not_a_gesture() {
        assert_eq!(flags(true, false, false, false, false).phase(), None);
        // 同一事件里既有 `MayBegin` 又有 `Began`（边界情况）⇒ 仍按 `Begin` 交付。
        assert_eq!(
            flags(true, true, false, false, false).phase(),
            Some(GesturePhase::Begin)
        );
    }

    /// 没有阶段位图的离散手势按「进行中」处理：它带的是一次增量。
    #[test]
    fn a_phaseless_gesture_is_an_update() {
        assert_eq!(
            flags(false, false, false, false, false).phase(),
            Some(GesturePhase::Update)
        );
    }

    /// R09-8：精确设备（触控板）单位是像素，**直传**，不除 10。
    #[test]
    fn precise_scrolling_is_passed_through_as_pixels() {
        let (delta, unit) = scroll_delta(true, 3.5, -12.25).expect("precise 事件总是交付");
        assert_eq!(unit, ScrollUnit::Pixel);
        assert_eq!(delta, Point::new(3.5, -12.25));
    }

    /// 精确设备的 0 增量事件仍然要交付：`phase` 的开始/结束标记常常就是 0 增量。
    #[test]
    fn a_zero_precise_scroll_still_carries_the_phase() {
        let (delta, unit) = scroll_delta(true, 0.0, 0.0).expect("phase 载体");
        assert_eq!(unit, ScrollUnit::Pixel);
        assert_eq!(delta, Point::ZERO);
    }

    /// R09-8：传统滚轮单位是**行**，直传，不除 3（Win32 一格 = 系统行数，缺省 3 行）。
    #[test]
    fn wheel_notches_are_passed_through_as_lines() {
        let (delta, unit) = scroll_delta(false, 0.0, 1.0).expect("离散滚轮一格");
        assert_eq!(unit, ScrollUnit::Line);
        assert_eq!(delta, Point::new(0.0, 1.0));

        let (delta, unit) = scroll_delta(false, -2.0, 3.0).expect("多格滚轮");
        assert_eq!(unit, ScrollUnit::Line);
        assert_eq!(delta, Point::new(-2.0, 3.0));
    }

    /// 传统滚轮的 0 增量事件没有可交付的语义（AppKit 会发）。
    #[test]
    fn a_zero_wheel_event_is_dropped() {
        assert_eq!(scroll_delta(false, 0.0, 0.0), None);
    }

    /// 手势事件暴露锚点/角度/阶段，与其它输入事件同构。
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

        // 旋转没有锚点（AppKit 只给角度），`position()` 必须诚实地返回 `None`。
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

        // `with_timestamp` 必须覆盖新增变体（测试注入用可控时钟）。
        assert_eq!(magnify.with_timestamp(stamp).timestamp(), stamp);
        assert_eq!(rotate.with_timestamp(stamp).timestamp(), stamp);
        assert_eq!(smart.with_timestamp(stamp).timestamp(), stamp);
    }
}

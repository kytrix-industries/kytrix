//! `WM_*` → [`PlatformEvent`] 的**纯函数**翻译（`design/phase-09-win32-demo.md` §4.2）。
//!
//! 这里不出现任何 Win32 句柄/结构体：输入是 `(msg, wparam, lparam)` 三个整数（外加消息时间与
//! 系统滚轮行数），输出是平台事件。唯一的平台耦合是 `WM_*` / `VK_*` 常量与「滚轮的屏幕坐标已由
//! 调用方换成客户区坐标」这一约定。因此全部翻译规则都能用内联单测钉死（`SendMessageW` 合成
//! 消息在 P09 的实机验收里另测）。
//!
//! ## 键位身份（R02-1）
//!
//! Windows 的 `wParam` 是**虚拟键码**，而字母/数字的 `VK_A..VK_Z` / `VK_0..VK_9` 是**布局相关**
//! 的：AZERTY 键盘上物理 `Q` 位置产生 `VK_A`。要让快捷键跨布局、跨平台生效，物理身份只能取自
//! **硬件扫描码**（`lParam` bit 16–23，`E0` 前缀由 bit 24 标记）—— 它与键盘布局无关，映射到
//! W3C `KeyboardEvent.code` 的 [`KeyCode`]。只有扫描码缺失（注入消息、无扫描码的虚拟键）时才
//! 退回 VK 的命名键映射。
//!
//! ## 自动重复（R02-2）
//!
//! `WM_KEYDOWN` 的 `lParam` bit 30 是 previous key state：置位表示这是**自动重复**而不是新的
//! 按下（同一 `lParam` 的 bit 16–23 是硬件扫描码，bit 24 是 `E0` 扩展位）。

use smol_str::SmolStr;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::{
    event::{ImeEvent, KeyCode, KeyState, Modifiers, PointerButton},
    platform::windows::ime::ImeState,
};

/// 指针事件的细分（`WM_*` 决定按钮，不读 `wParam` 的 `MK_*`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PointerKind {
    Move,
    Down(PointerButton),
    Up(PointerButton),
}

/// 平台层事件（应用层看到的是 [`crate::event::Event`]）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PlatformEvent {
    Pointer {
        x: i32,
        y: i32,
        kind: PointerKind,
    },
    /// 指针进入客户区（`TrackMouseEvent` 之后的首次移动，对应 `WM_MOUSELEAVE` 的另一半）。
    PointerEnter,
    Scroll {
        x: i32,
        y: i32,
        /// 水平滚动**行数**（正 = 向右）。
        dx_lines: f32,
        /// 垂直滚动**行数**（正 = 向上，与 `WM_MOUSEWHEEL` 的正方向一致）。
        dy_lines: f32,
        /// 一格滚轮对应的行数（`SPI_GETWHEELSCROLLLINES`；`0.0` = 未知）。
        lines_per_tick: f32,
    },
    Key {
        code: KeyCode,
        state: KeyState,
        repeat: bool,
    },
    Text(SmolStr),
    Ime(ImeEvent),
    Resize {
        width: u32,
        height: u32,
    },
    Dpi {
        dpi: u32,
        /// 系统建议的新窗口矩形（`lParam` 的 `RECT`；消息处理期之外不可读）。
        suggested: Option<(i32, i32, i32, i32)>,
    },
    Focus(bool),
    Appearance(bool),
    PointerLeave,
    /// 指针会话被系统**取消**（捕获丢失 / 窗口失焦 / 系统弹窗夺走输入）。
    ///
    /// 映射到 `InputEvent::PointerCancel`：只收尾（结束拖拽、派发 `DragPhase::Cancel`），
    /// 绝不提交点击。
    PointerCancel,
    Close,
}

/// 一条消息 → 平台事件的**带时间戳**产物（R02-14）。
///
/// `ticks_ms` 是消息的 `MSG.time`（与 `GetTickCount64` 同源、系统启动为原点），
/// `WndProc` 用 `GetMessageTime()` 取到它；平台事件转 `InputEvent` 时经
/// [`crate::event::timestamp_from_ticks`] 换算成引擎单调时钟。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StampedEvent {
    /// 平台事件。
    pub event: PlatformEvent,
    /// 消息时间（毫秒）。
    pub ticks_ms: u64,
}

/// 每条消息都要带的输入上下文（时间戳 + 系统滚轮行数）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MessageContext {
    /// 消息时间（`GetMessageTime()`，毫秒）。
    pub ticks_ms: u64,
    /// 一格滚轮的行数（`SPI_GETWHEELSCROLLLINES`；`0.0` = 未知/按页）。
    pub wheel_lines: f32,
}

/// 消息之间需要保持的状态（修饰键、代理对、IME 组合）。
#[derive(Debug, Default)]
pub(crate) struct TranslateState {
    modifiers: Modifiers,
    /// `WM_CHAR` 可能把一对代理拆成两条消息。
    high_surrogate: Option<u16>,
    ime: ImeState,
}

impl TranslateState {
    pub(crate) fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    /// 复位瞬时输入状态（失焦时调用）。
    ///
    /// 修饰键按消息顺序维护：失焦期间抬起消息不再到达本窗口，不清位就会让 Shift/Ctrl
    /// 永久保持按下（Alt+Tab 回来后所有输入都被当成组合键）。半高代理对同理：
    /// 失焦前残留的高位代理会和回来后的字符拼成乱码。
    pub(crate) fn reset_transient(&mut self) {
        self.modifiers = Modifiers::empty();
        self.high_surrogate = None;
    }

    /// IME 状态（`event_loop` 在 `WM_IME_*` 分支里驱动它）。
    pub(crate) fn ime_mut(&mut self) -> &mut ImeState {
        &mut self.ime
    }
}

/// 一条消息 → 至多一个平台事件。`None` = 该消息不影响应用层。
///
/// **契约**：滚轮的 `lParam` 在调用前已被换成客户区坐标（`WM_MOUSEWHEEL` 原值是屏幕坐标）。
pub(crate) fn translate(
    msg: u32,
    wparam: usize,
    lparam: isize,
    context: MessageContext,
    state: &mut TranslateState,
) -> Option<StampedEvent> {
    let x = loword_signed(lparam);
    let y = hiword_signed(lparam);
    let event = match msg {
        WM_MOUSEMOVE => Some(PlatformEvent::Pointer {
            x,
            y,
            kind: PointerKind::Move,
        }),
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let button = match msg {
                WM_LBUTTONDOWN | WM_LBUTTONUP => PointerButton::Primary,
                WM_RBUTTONDOWN | WM_RBUTTONUP => PointerButton::Secondary,
                WM_MBUTTONDOWN | WM_MBUTTONUP => PointerButton::Middle,
                _ => match xbutton(wparam) {
                    1 => PointerButton::X1,
                    _ => PointerButton::X2,
                },
            };
            let kind = match msg {
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
                    PointerKind::Down(button)
                }
                _ => PointerKind::Up(button),
            };
            Some(PlatformEvent::Pointer { x, y, kind })
        }
        WM_MOUSEWHEEL => {
            let ticks = f32::from(high_word(wparam) as i16) / WHEEL_DELTA_F;
            let lines = lines_per_tick(context.wheel_lines);
            Some(PlatformEvent::Scroll {
                x,
                y,
                dx_lines: 0.0,
                dy_lines: ticks * lines,
                lines_per_tick: lines,
            })
        }
        WM_MOUSEHWHEEL => {
            let ticks = f32::from(high_word(wparam) as i16) / WHEEL_DELTA_F;
            let lines = lines_per_tick(context.wheel_lines);
            Some(PlatformEvent::Scroll {
                x,
                y,
                dx_lines: ticks * lines,
                dy_lines: 0.0,
                lines_per_tick: lines,
            })
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let vk = (wparam & 0xFF) as u16;
            let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
            update_modifiers(vk, down, &mut state.modifiers);
            let raw = lparam as u32;
            // R02-2：bit 30 = previous key state（置位 = 自动重复）。只对按下有意义。
            let repeat = down && (raw & (1 << 30)) != 0;
            // 硬件扫描码 bit 16–23；bit 24 = `E0` 扩展键（右 Ctrl/Alt、小键盘回车、导航键…）。
            let scancode = ((raw >> 16) & 0xFF) as u16;
            let extended = (raw & (1 << 24)) != 0;
            let code = key_code_from_native(vk, scancode, extended);
            Some(PlatformEvent::Key {
                code,
                state: if down { KeyState::Down } else { KeyState::Up },
                repeat,
            })
        }
        WM_CHAR => {
            // IME 提交之后 Windows 会再发同内容的 `WM_CHAR`：跳过，避免文字重复。
            if state.ime.take_char_suppression() {
                return None;
            }
            decode_char(wparam as u16, state).map(PlatformEvent::Text)
        }
        // `WM_MOUSELEAVE` 由 `Win32::UI::Controls` 导出（不是 `WindowsAndMessaging`）。
        windows::Win32::UI::Controls::WM_MOUSELEAVE => Some(PlatformEvent::PointerLeave),
        WM_SETFOCUS => Some(PlatformEvent::Focus(true)),
        WM_KILLFOCUS => Some(PlatformEvent::Focus(false)),
        WM_SIZE => {
            // `SIZE_MINIMIZED` 没有可用的客户区尺寸。
            if wparam == SIZE_MINIMIZED as usize {
                return None;
            }
            let width = lparam as u32 & 0xFFFF;
            let height = (lparam as u32 >> 16) & 0xFFFF;
            (width > 0 && height > 0).then_some(PlatformEvent::Resize { width, height })
        }
        WM_DPICHANGED => {
            let dpi = (wparam as u32) & 0xFFFF;
            if dpi == 0 {
                return None;
            }
            // SAFETY: `WM_DPICHANGED` 的 `lParam` 指向系统提供的 `RECT`，只在本次消息处理期间有效；
            // 这里立刻按值拷出（不保存指针）。
            let suggested = if lparam == 0 {
                None
            } else {
                let rect = unsafe { &*(lparam as *const windows::Win32::Foundation::RECT) };
                Some((rect.left, rect.top, rect.right, rect.bottom))
            };
            Some(PlatformEvent::Dpi { dpi, suggested })
        }
        WM_CLOSE => Some(PlatformEvent::Close),
        _ => None,
    };
    event.map(|event| StampedEvent {
        event,
        ticks_ms: context.ticks_ms,
    })
}

/// `WHEEL_DELTA`（`WM_MOUSEWHEEL` 的一格 = 120）。
const WHEEL_DELTA_F: f32 = 120.0;

/// 一格滚轮的行数：系统值不可用（`SPI_GETWHEELSCROLLLINES` = 0 / `WHEEL_PAGESCROLL`）时按
/// Win32 缺省的 3 行处理（控制面板「一次滚动下列行数」默认为 3）。
pub(crate) fn lines_per_tick(wheel_lines: f32) -> f32 {
    if wheel_lines.is_finite() && wheel_lines > 0.0 && wheel_lines <= 100.0 {
        wheel_lines
    } else {
        3.0
    }
}

/// `LOWORD` 按**有符号** 16 位解码（客户端坐标可以为负）。
pub(crate) fn loword_signed(value: isize) -> i32 {
    (value as u32 & 0xFFFF) as u16 as i16 as i32
}

/// `HIWORD` 按**有符号** 16 位解码（滚轮 delta 用它，±120 的符号即方向）。
pub(crate) fn hiword_signed(value: isize) -> i32 {
    ((value as u32 >> 16) & 0xFFFF) as u16 as i16 as i32
}

/// `GET_XBUTTON_WPARAM`：高 16 位是 1（X1）或 2（X2）。
fn xbutton(wparam: usize) -> u16 {
    ((wparam as u32 >> 16) & 0xFFFF) as u16
}

/// `wParam` 的高 16 位（滚轮 delta；按有符号 16 位解释）。
fn high_word(wparam: usize) -> u16 {
    ((wparam as u32 >> 16) & 0xFFFF) as u16
}

/// `WM_CHAR` 载荷 → 文本；代理对被拆成两条消息时缓存高代理。
fn decode_char(unit: u16, state: &mut TranslateState) -> Option<SmolStr> {
    // 低代理：与前一个高代理合并。
    if (0xDC00..=0xDFFF).contains(&unit) {
        let high = state.high_surrogate.take()?;
        let combined = 0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
        return char::from_u32(combined).map(|c| SmolStr::new(c.to_string()));
    }
    state.high_surrogate = None;
    if (0xD800..=0xDBFF).contains(&unit) {
        state.high_surrogate = Some(unit);
        return None;
    }
    char::from_u32(u32::from(unit)).map(|c| SmolStr::new(c.to_string()))
}

/// 修饰键的按下/抬起（按消息顺序维护，不查系统状态）。
fn update_modifiers(vk: u16, down: bool, modifiers: &mut Modifiers) {
    let flag = match vk {
        VK_SHIFT_CODE => Modifiers::SHIFT,
        VK_CONTROL_CODE => Modifiers::CONTROL,
        VK_MENU_CODE => Modifiers::ALT,
        0x5B | 0x5C => Modifiers::META, // VK_LWIN / VK_RWIN
        _ => return,
    };
    modifiers.set(flag, down);
}

/// `VK_SHIFT`（0x10）。
const VK_SHIFT_CODE: u16 = 0x10;
/// `VK_CONTROL`（0x11）。
const VK_CONTROL_CODE: u16 = 0x11;
/// `VK_MENU`（Alt，0x12）。
const VK_MENU_CODE: u16 = 0x12;
/// `VK_PAUSE`（0x13）：`Pause` 的 `WM_KEYDOWN` 也带扫描码 0x45（与 `NumLock` 相同），
/// 只能用 VK 区分。
const VK_PAUSE_CODE: u16 = 0x13;
/// `VK_SNAPSHOT`（0x2C，Print Screen）。
const VK_SNAPSHOT_CODE: u16 = 0x2C;

/// 原生键身份 → [`KeyCode`]（R02-1）。
///
/// 优先用**硬件扫描码**（布局无关：AZERTY 上物理 `Q` 也得到 [`KeyCode::KeyQ`]），扫描码缺失
/// 或无对应项时退回虚拟键的命名键映射；两者都不认识 ⇒
/// [`KeyCode::Unidentified`]（Windows 载荷 = 扫描码，bit 8 = `E0` 扩展键；没有扫描码时
/// `0x8000 | VK`），文档明确它**不可跨平台**。
pub(crate) fn key_code_from_native(vk: u16, scancode: u16, extended: bool) -> KeyCode {
    // 扫描码有歧义的少数键：用 VK 定夺。
    match vk {
        VK_PAUSE_CODE => return KeyCode::Pause,
        VK_SNAPSHOT_CODE => return KeyCode::PrintScreen,
        _ => {}
    }
    if scancode != 0
        && let Some(code) = key_code_from_scancode(scancode, extended)
    {
        return code;
    }
    key_code_from_vk(vk).unwrap_or(KeyCode::Unidentified(if scancode != 0 {
        scancode | if extended { 0x100 } else { 0 }
    } else {
        0x8000 | vk
    }))
}

/// 扫描码（set 1）+ `E0` 扩展位 → [`KeyCode`]（W3C `KeyboardEvent.code` 语义）。
///
/// 小键盘与导航键共用扫描码（0x47 既是 `Numpad7` 也是 `Home`），由 `E0` 区分 —— 这正是
/// W3C 的划分：`code` 表达**物理键位**（`Numpad7`），NumLock 只影响 `key`（`7` / `Home`）。
fn key_code_from_scancode(scancode: u16, extended: bool) -> Option<KeyCode> {
    let code = match (scancode, extended) {
        (0x01, _) => KeyCode::Escape,
        (0x02, _) => KeyCode::Digit1,
        (0x03, _) => KeyCode::Digit2,
        (0x04, _) => KeyCode::Digit3,
        (0x05, _) => KeyCode::Digit4,
        (0x06, _) => KeyCode::Digit5,
        (0x07, _) => KeyCode::Digit6,
        (0x08, _) => KeyCode::Digit7,
        (0x09, _) => KeyCode::Digit8,
        (0x0A, _) => KeyCode::Digit9,
        (0x0B, _) => KeyCode::Digit0,
        (0x0C, _) => KeyCode::Minus,
        (0x0D, _) => KeyCode::Equal,
        (0x0E, _) => KeyCode::Backspace,
        (0x0F, _) => KeyCode::Tab,
        (0x10, _) => KeyCode::KeyQ,
        (0x11, _) => KeyCode::KeyW,
        (0x12, _) => KeyCode::KeyE,
        (0x13, _) => KeyCode::KeyR,
        (0x14, _) => KeyCode::KeyT,
        (0x15, _) => KeyCode::KeyY,
        (0x16, _) => KeyCode::KeyU,
        (0x17, _) => KeyCode::KeyI,
        (0x18, _) => KeyCode::KeyO,
        (0x19, _) => KeyCode::KeyP,
        (0x1A, _) => KeyCode::BracketLeft,
        (0x1B, _) => KeyCode::BracketRight,
        (0x1C, false) => KeyCode::Enter,
        (0x1C, true) => KeyCode::NumpadEnter,
        (0x1D, false) => KeyCode::ControlLeft,
        (0x1D, true) => KeyCode::ControlRight,
        (0x1E, _) => KeyCode::KeyA,
        (0x1F, _) => KeyCode::KeyS,
        (0x20, _) => KeyCode::KeyD,
        (0x21, _) => KeyCode::KeyF,
        (0x22, _) => KeyCode::KeyG,
        (0x23, _) => KeyCode::KeyH,
        (0x24, _) => KeyCode::KeyJ,
        (0x25, _) => KeyCode::KeyK,
        (0x26, _) => KeyCode::KeyL,
        (0x27, _) => KeyCode::Semicolon,
        (0x28, _) => KeyCode::Quote,
        (0x29, _) => KeyCode::Backquote,
        (0x2A, _) => KeyCode::ShiftLeft,
        (0x2B, _) => KeyCode::Backslash,
        (0x2C, _) => KeyCode::KeyZ,
        (0x2D, _) => KeyCode::KeyX,
        (0x2E, _) => KeyCode::KeyC,
        (0x2F, _) => KeyCode::KeyV,
        (0x30, _) => KeyCode::KeyB,
        (0x31, _) => KeyCode::KeyN,
        (0x32, _) => KeyCode::KeyM,
        (0x33, _) => KeyCode::Comma,
        (0x34, _) => KeyCode::Period,
        (0x35, false) => KeyCode::Slash,
        (0x35, true) => KeyCode::NumpadDivide,
        (0x36, _) => KeyCode::ShiftRight,
        (0x37, false) => KeyCode::NumpadMultiply,
        (0x38, false) => KeyCode::AltLeft,
        (0x38, true) => KeyCode::AltRight,
        (0x39, _) => KeyCode::Space,
        (0x3A, _) => KeyCode::CapsLock,
        (0x3B, _) => KeyCode::F1,
        (0x3C, _) => KeyCode::F2,
        (0x3D, _) => KeyCode::F3,
        (0x3E, _) => KeyCode::F4,
        (0x3F, _) => KeyCode::F5,
        (0x40, _) => KeyCode::F6,
        (0x41, _) => KeyCode::F7,
        (0x42, _) => KeyCode::F8,
        (0x43, _) => KeyCode::F9,
        (0x44, _) => KeyCode::F10,
        (0x45, _) => KeyCode::NumLock,
        (0x46, _) => KeyCode::ScrollLock,
        (0x47, false) => KeyCode::Numpad7,
        (0x47, true) => KeyCode::Home,
        (0x48, false) => KeyCode::Numpad8,
        (0x48, true) => KeyCode::ArrowUp,
        (0x49, false) => KeyCode::Numpad9,
        (0x49, true) => KeyCode::PageUp,
        (0x4A, _) => KeyCode::NumpadSubtract,
        (0x4B, false) => KeyCode::Numpad4,
        (0x4B, true) => KeyCode::ArrowLeft,
        (0x4C, _) => KeyCode::Numpad5,
        (0x4D, false) => KeyCode::Numpad6,
        (0x4D, true) => KeyCode::ArrowRight,
        (0x4E, _) => KeyCode::NumpadAdd,
        (0x4F, false) => KeyCode::Numpad1,
        (0x4F, true) => KeyCode::End,
        (0x50, false) => KeyCode::Numpad2,
        (0x50, true) => KeyCode::ArrowDown,
        (0x51, false) => KeyCode::Numpad3,
        (0x51, true) => KeyCode::PageDown,
        (0x52, false) => KeyCode::Numpad0,
        (0x52, true) => KeyCode::Insert,
        (0x53, false) => KeyCode::NumpadDecimal,
        (0x53, true) => KeyCode::Delete,
        (0x56, _) => KeyCode::IntlBackslash,
        (0x57, _) => KeyCode::F11,
        (0x58, _) => KeyCode::F12,
        (0x5B, true) => KeyCode::MetaLeft,
        (0x5C, true) => KeyCode::MetaRight,
        (0x5D, true) => KeyCode::ContextMenu,
        (0x64, _) => KeyCode::F13,
        (0x65, _) => KeyCode::F14,
        (0x66, _) => KeyCode::F15,
        (0x67, _) => KeyCode::F16,
        (0x68, _) => KeyCode::F17,
        (0x69, _) => KeyCode::F18,
        (0x6A, _) => KeyCode::F19,
        (0x6B, _) => KeyCode::F20,
        (0x6C, _) => KeyCode::F21,
        (0x6D, _) => KeyCode::F22,
        (0x6E, _) => KeyCode::F23,
        (0x6F, _) => KeyCode::F24,
        (0x73, _) => KeyCode::IntlRo,
        (0x7D, _) => KeyCode::IntlYen,
        _ => return None,
    };
    Some(code)
}

/// 虚拟键的命名键映射（扫描码缺失时的回退；**不用**它表达字母/数字 —— VK 字母是布局相关的）。
fn key_code_from_vk(vk: u16) -> Option<KeyCode> {
    let code = match vk {
        0x1B => KeyCode::Escape,
        0x0D => KeyCode::Enter,
        0x09 => KeyCode::Tab,
        0x08 => KeyCode::Backspace,
        0x2E => KeyCode::Delete,
        0x2D => KeyCode::Insert,
        0x20 => KeyCode::Space,
        0x25 => KeyCode::ArrowLeft,
        0x26 => KeyCode::ArrowUp,
        0x27 => KeyCode::ArrowRight,
        0x28 => KeyCode::ArrowDown,
        0x24 => KeyCode::Home,
        0x23 => KeyCode::End,
        0x21 => KeyCode::PageUp,
        0x22 => KeyCode::PageDown,
        0x2C => KeyCode::PrintScreen,
        0x90 => KeyCode::NumLock,
        0x91 => KeyCode::ScrollLock,
        0x5B => KeyCode::MetaLeft,
        0x5C => KeyCode::MetaRight,
        0x5D => KeyCode::ContextMenu,
        0x10 => KeyCode::ShiftLeft,
        0x11 => KeyCode::ControlLeft,
        0x12 => KeyCode::AltLeft,
        0x70..=0x87 => return f_key((vk - 0x6F) as u8),
        _ => return None,
    };
    Some(code)
}

/// F1…F24 的 `KeyCode`（`n` 越界时 `None`）。
fn f_key(n: u8) -> Option<KeyCode> {
    let code = match n {
        1 => KeyCode::F1,
        2 => KeyCode::F2,
        3 => KeyCode::F3,
        4 => KeyCode::F4,
        5 => KeyCode::F5,
        6 => KeyCode::F6,
        7 => KeyCode::F7,
        8 => KeyCode::F8,
        9 => KeyCode::F9,
        10 => KeyCode::F10,
        11 => KeyCode::F11,
        12 => KeyCode::F12,
        13 => KeyCode::F13,
        14 => KeyCode::F14,
        15 => KeyCode::F15,
        16 => KeyCode::F16,
        17 => KeyCode::F17,
        18 => KeyCode::F18,
        19 => KeyCode::F19,
        20 => KeyCode::F20,
        21 => KeyCode::F21,
        22 => KeyCode::F22,
        23 => KeyCode::F23,
        24 => KeyCode::F24,
        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::WindowsAndMessaging::*;

    use super::{
        MessageContext, PlatformEvent, PointerKind, TranslateState, key_code_from_native,
        lines_per_tick, translate,
    };
    use crate::event::{KeyCode, KeyState, Modifiers, PointerButton};

    /// 一个 KEYDOWN 的 `lParam`：扫描码在 bit 16–23，`E0` 在 bit 24，重复位在 bit 30。
    fn key_lparam(scancode: u16, extended: bool, previous_down: bool) -> isize {
        let mut raw = u32::from(scancode) << 16;
        if extended {
            raw |= 1 << 24;
        }
        if previous_down {
            raw |= 1 << 30;
        }
        raw as isize
    }

    fn lparam(x: i32, y: i32) -> isize {
        ((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize
    }

    fn context() -> MessageContext {
        MessageContext {
            ticks_ms: 1_000,
            wheel_lines: 3.0,
        }
    }

    fn translate_raw(msg: u32, wparam: usize, lparam: isize) -> Option<PlatformEvent> {
        let mut state = TranslateState::default();
        translate(msg, wparam, lparam, context(), &mut state).map(|stamped| stamped.event)
    }

    #[test]
    fn every_message_carries_the_message_time() {
        let mut state = TranslateState::default();
        let stamped = translate(
            WM_MOUSEMOVE,
            0,
            lparam(1, 2),
            MessageContext {
                ticks_ms: 4_321,
                wheel_lines: 3.0,
            },
            &mut state,
        )
        .expect("mousemove");

        assert_eq!(stamped.ticks_ms, 4_321, "R02-14：时间戳来自 MSG.time");
    }

    #[test]
    fn mouse_move_decodes_negative_client_coordinates() {
        let event = translate_raw(WM_MOUSEMOVE, 0, lparam(-7, -300));

        assert_eq!(
            event,
            Some(PlatformEvent::Pointer {
                x: -7,
                y: -300,
                kind: PointerKind::Move
            })
        );
    }

    #[test]
    fn buttons_come_from_the_message_not_the_wparam() {
        // 按下时真实消息的 wParam 带 MK_LBUTTON，但抬起时 wParam 是 0：按钮必须由 msg 决定。
        let down = translate_raw(WM_RBUTTONDOWN, 0, lparam(3, 4));
        let up = translate_raw(WM_RBUTTONUP, 0, lparam(3, 4));

        assert_eq!(
            down,
            Some(PlatformEvent::Pointer {
                x: 3,
                y: 4,
                kind: PointerKind::Down(PointerButton::Secondary)
            })
        );
        assert_eq!(
            up,
            Some(PlatformEvent::Pointer {
                x: 3,
                y: 4,
                kind: PointerKind::Up(PointerButton::Secondary)
            })
        );
    }

    #[test]
    fn xbuttons_map_to_x1_and_x2() {
        let x1 = translate_raw(WM_XBUTTONDOWN, 1 << 16, lparam(0, 0));
        let x2 = translate_raw(WM_XBUTTONUP, 2 << 16, lparam(0, 0));

        assert!(matches!(
            x1,
            Some(PlatformEvent::Pointer {
                kind: PointerKind::Down(PointerButton::X1),
                ..
            })
        ));
        assert!(matches!(
            x2,
            Some(PlatformEvent::Pointer {
                kind: PointerKind::Up(PointerButton::X2),
                ..
            })
        ));
    }

    /// R02-10：滚轮一格 = `120` = **系统行数**行（`SPI_GETWHEELSCROLLLINES`，缺省 3）。
    #[test]
    fn wheel_ticks_become_system_lines() {
        let up = translate_raw(WM_MOUSEWHEEL, 120 << 16, lparam(10, 20));
        let down = translate_raw(
            WM_MOUSEWHEEL,
            ((-120i32 as u16 as u32) << 16) as usize,
            lparam(10, 20),
        );
        let horizontal = translate_raw(
            WM_MOUSEHWHEEL,
            ((-120i32 as u16 as u32) << 16) as usize,
            lparam(10, 20),
        );

        assert_eq!(
            up,
            Some(PlatformEvent::Scroll {
                x: 10,
                y: 20,
                dx_lines: 0.0,
                dy_lines: 3.0,
                lines_per_tick: 3.0,
            })
        );
        assert!(matches!(
            down,
            Some(PlatformEvent::Scroll { dy_lines, .. }) if (dy_lines + 3.0).abs() < f32::EPSILON
        ));
        assert!(matches!(
            horizontal,
            Some(PlatformEvent::Scroll { dx_lines, .. }) if (dx_lines + 3.0).abs() < f32::EPSILON
        ));
    }

    #[test]
    fn unknown_wheel_lines_fall_back_to_three() {
        assert_eq!(lines_per_tick(0.0), 3.0, "SPI 为 0 / WHEEL_PAGESCROLL");
        assert_eq!(lines_per_tick(f32::NAN), 3.0);
        assert_eq!(lines_per_tick(1.0), 1.0);
        assert_eq!(lines_per_tick(5.0), 5.0);
    }

    #[test]
    fn size_notifications_decode_the_client_extent_and_skip_minimized() {
        let event = translate_raw(WM_SIZE, SIZE_RESTORED as usize, lparam(640, 480));
        let minimized = translate_raw(WM_SIZE, SIZE_MINIMIZED as usize, lparam(0, 0));

        assert_eq!(
            event,
            Some(PlatformEvent::Resize {
                width: 640,
                height: 480
            })
        );
        assert_eq!(minimized, None, "最小化没有可用的客户区尺寸");
    }

    #[test]
    fn dpi_change_reads_the_low_word() {
        let event = translate_raw(WM_DPICHANGED, (192usize << 16) | 144, 0);

        assert_eq!(
            event,
            Some(PlatformEvent::Dpi {
                dpi: 144,
                suggested: None
            })
        );
    }

    #[test]
    fn char_messages_merge_surrogate_pairs() {
        let mut state = TranslateState::default();
        // 🌍 = U+1F30D = D83C DF0D
        let high = translate(WM_CHAR, 0xD83C, 0, context(), &mut state);
        let low = translate(WM_CHAR, 0xDF0D, 0, context(), &mut state);
        let plain = translate(WM_CHAR, 'a' as usize, 0, context(), &mut state);

        assert_eq!(high, None, "高代理单独不产生文本");
        assert_eq!(
            low.map(|stamped| stamped.event),
            Some(PlatformEvent::Text("🌍".into()))
        );
        assert_eq!(
            plain.map(|stamped| stamped.event),
            Some(PlatformEvent::Text("a".into()))
        );
    }

    /// P2-6 回归：失焦必须清修饰键。
    ///
    /// 修饰键按消息顺序维护，而失焦期间的抬起消息不会到达本窗口。不复位的话
    /// Alt+Tab 回来后 Shift/Ctrl 会永久"粘住"，所有输入都被当成组合键。
    #[test]
    fn losing_focus_clears_stuck_modifiers() {
        let mut state = TranslateState::default();
        // Shift 按下 + Ctrl 按下。
        let _ = translate(
            WM_KEYDOWN,
            0x10,
            key_lparam(0x2A, false, false),
            context(),
            &mut state,
        );
        let _ = translate(
            WM_KEYDOWN,
            0x11,
            key_lparam(0x1D, false, false),
            context(),
            &mut state,
        );
        assert_eq!(state.modifiers(), Modifiers::SHIFT | Modifiers::CONTROL);

        // 失焦：抬起消息永远不会到达，只能靠显式复位。
        state.reset_transient();
        assert_eq!(state.modifiers(), Modifiers::empty());

        // 复位幂等，且不影响之后的正常跟踪。
        state.reset_transient();
        assert_eq!(state.modifiers(), Modifiers::empty());
        let _ = translate(
            WM_KEYDOWN,
            0x10,
            key_lparam(0x2A, false, false),
            context(),
            &mut state,
        );
        assert_eq!(state.modifiers(), Modifiers::SHIFT);
    }

    #[test]
    fn key_events_track_modifiers_in_message_order() {
        let mut state = TranslateState::default();
        let shift_down = translate(
            WM_KEYDOWN,
            0x10,
            key_lparam(0x2A, false, false),
            context(),
            &mut state,
        );
        let a_down = translate(
            WM_KEYDOWN,
            0x41,
            key_lparam(0x1E, false, false),
            context(),
            &mut state,
        );

        assert_eq!(
            shift_down.map(|stamped| stamped.event),
            Some(PlatformEvent::Key {
                code: KeyCode::ShiftLeft,
                state: KeyState::Down,
                repeat: false,
            })
        );
        assert_eq!(state.modifiers(), Modifiers::SHIFT);
        assert_eq!(
            a_down.map(|stamped| stamped.event),
            Some(PlatformEvent::Key {
                code: KeyCode::KeyA,
                state: KeyState::Down,
                repeat: false,
            })
        );

        let _ = translate(
            WM_KEYUP,
            0x10,
            key_lparam(0x2A, false, false),
            context(),
            &mut state,
        );
        assert_eq!(state.modifiers(), Modifiers::empty());
    }

    /// R02-2 回归：`lParam` bit 30 = 自动重复，必须与真实按下区分。
    #[test]
    fn previous_key_state_marks_auto_repeat() {
        let mut state = TranslateState::default();
        let first = translate(
            WM_KEYDOWN,
            0x0D,
            key_lparam(0x1C, false, false),
            context(),
            &mut state,
        );
        let repeat = translate(
            WM_KEYDOWN,
            0x0D,
            key_lparam(0x1C, false, true),
            context(),
            &mut state,
        );
        let up = translate(
            WM_KEYUP,
            0x0D,
            key_lparam(0x1C, false, true),
            context(),
            &mut state,
        );

        assert!(matches!(
            first.map(|stamped| stamped.event),
            Some(PlatformEvent::Key { repeat: false, .. })
        ));
        assert!(
            matches!(
                repeat.map(|stamped| stamped.event),
                Some(PlatformEvent::Key {
                    repeat: true,
                    state: KeyState::Down,
                    ..
                })
            ),
            "长按 Enter 的重复消息必须带 repeat 位"
        );
        assert!(
            matches!(
                up.map(|stamped| stamped.event),
                Some(PlatformEvent::Key {
                    repeat: false,
                    state: KeyState::Up,
                    ..
                })
            ),
            "抬起没有重复语义，即使 bit30 置位"
        );
    }

    #[test]
    fn navigation_and_function_keys_map_to_named_codes() {
        assert_eq!(key_code_from_native(0x1B, 0x01, false), KeyCode::Escape);
        assert_eq!(key_code_from_native(0x26, 0x48, true), KeyCode::ArrowUp);
        assert_eq!(key_code_from_native(0x70, 0x3B, false), KeyCode::F1);
        assert_eq!(key_code_from_native(0x87, 0x6F, false), KeyCode::F24);
        // 扫描码优先：`VK_A` + 字母 `A` 的扫描码 ⇒ `KeyA`（布局无关）。
        assert_eq!(key_code_from_native(0x41, 0x1E, false), KeyCode::KeyA);
        // 扫描码缺失时退回 VK 的命名键；字母 VK 没有命名键 ⇒ 未识别。
        assert_eq!(
            key_code_from_native(0x41, 0, false),
            KeyCode::Unidentified(0x8041)
        );
    }

    /// R02-1：同一物理键在不同布局下得到同一个 `KeyCode`（扫描码是布局无关的）。
    ///
    /// AZERTY 键盘上物理 `Q` 位置产生 `VK_A`，但扫描码仍是 `0x10` ⇒ `KeyQ`（而不是 `KeyA`）。
    #[test]
    fn physical_identity_ignores_the_layout_dependent_vk() {
        assert_eq!(key_code_from_native(0x41, 0x10, false), KeyCode::KeyQ);
        assert_eq!(key_code_from_native(0x51, 0x1E, false), KeyCode::KeyA);
        // 小键盘与导航键共用扫描码，由 E0 扩展位区分（W3C 的 `Numpad7` vs `Home`）。
        assert_eq!(key_code_from_native(0x24, 0x47, false), KeyCode::Numpad7);
        assert_eq!(key_code_from_native(0x24, 0x47, true), KeyCode::Home);
        // 右 Ctrl 的 E0 扩展位也参与身份。
        assert_eq!(
            key_code_from_native(0xA3, 0x1D, true),
            KeyCode::ControlRight
        );
        assert_eq!(
            key_code_from_native(0xA2, 0x1D, false),
            KeyCode::ControlLeft
        );
    }

    #[test]
    fn pause_and_print_screen_use_the_vk_because_scancodes_collide() {
        assert_eq!(key_code_from_native(0x13, 0x45, false), KeyCode::Pause);
        assert_eq!(key_code_from_native(0x2C, 0x37, true), KeyCode::PrintScreen);
        assert_eq!(key_code_from_native(0x90, 0x45, false), KeyCode::NumLock);
    }

    #[test]
    fn focus_and_close_have_no_payload_variants() {
        assert_eq!(
            translate_raw(WM_SETFOCUS, 0, 0),
            Some(PlatformEvent::Focus(true))
        );
        assert_eq!(
            translate_raw(WM_KILLFOCUS, 0, 0),
            Some(PlatformEvent::Focus(false))
        );
        assert_eq!(translate_raw(WM_CLOSE, 0, 0), Some(PlatformEvent::Close));
    }
}

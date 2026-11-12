//! `WM_*` → [`PlatformEvent`] 的**纯函数**翻译（`design/phase-09-win32-demo.md` §4.2）。
//!
//! 这里不出现任何 Win32 句柄/结构体：输入是 `(msg, wparam, lparam)` 三个整数，输出是平台事件。
//! 唯一的平台耦合是 `WM_*` / `VK_*` 常量与「滚轮的屏幕坐标已由调用方换成客户区坐标」这一约定。
//! 因此全部翻译规则都能用内联单测钉死（`SendMessageW` 合成消息在 P09 的实机验收里另测）。

use smol_str::SmolStr;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::{
    event::{ImeEvent, Key, KeyState, Modifiers, PointerButton},
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
    Scroll {
        x: i32,
        y: i32,
        dx_ticks: f32,
        dy_ticks: f32,
    },
    Key {
        key: Key,
        state: KeyState,
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
    Close,
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
    state: &mut TranslateState,
) -> Option<PlatformEvent> {
    let x = loword_signed(lparam);
    let y = hiword_signed(lparam);
    match msg {
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
        WM_MOUSEWHEEL => Some(PlatformEvent::Scroll {
            x,
            y,
            dx_ticks: 0.0,
            dy_ticks: f32::from(high_word(wparam) as i16) / 120.0,
        }),
        WM_MOUSEHWHEEL => Some(PlatformEvent::Scroll {
            x,
            y,
            dx_ticks: f32::from(high_word(wparam) as i16) / 120.0,
            dy_ticks: 0.0,
        }),
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let vk = (wparam & 0xFF) as u16;
            let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
            update_modifiers(vk, down, &mut state.modifiers);
            let key = key_from_vk(vk)?;
            Some(PlatformEvent::Key {
                key,
                state: if down { KeyState::Down } else { KeyState::Up },
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
        0x10 => Modifiers::SHIFT,       // VK_SHIFT
        0x11 => Modifiers::CONTROL,     // VK_CONTROL
        0x12 => Modifiers::ALT,         // VK_MENU
        0x5B | 0x5C => Modifiers::META, // VK_LWIN / VK_RWIN
        _ => return,
    };
    modifiers.set(flag, down);
}

/// 虚拟键 → [`Key`]；修饰键与未映射键都返回 `Some`（字符键由 `WM_CHAR` 给文本）。
pub(crate) fn key_from_vk(vk: u16) -> Option<Key> {
    let key = match vk {
        0x1B => Key::Escape,
        0x0D => Key::Enter,
        0x09 => Key::Tab,
        0x08 => Key::Backspace,
        0x2E => Key::Delete,
        0x20 => Key::Space,
        0x25 => Key::ArrowLeft,
        0x26 => Key::ArrowUp,
        0x27 => Key::ArrowRight,
        0x28 => Key::ArrowDown,
        0x24 => Key::Home,
        0x23 => Key::End,
        0x21 => Key::PageUp,
        0x22 => Key::PageDown,
        0x70..=0x87 => Key::Function((vk - 0x6F) as u8),
        0x10 => Key::Shift,
        0x11 => Key::Control,
        0x12 => Key::Alt,
        0x5B | 0x5C => Key::Meta,
        other => Key::Other(other),
    };
    Some(key)
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::WindowsAndMessaging::*;

    use super::{PlatformEvent, PointerKind, TranslateState, key_from_vk, translate};
    use crate::event::{Key, KeyState, Modifiers, PointerButton};

    fn lparam(x: i32, y: i32) -> isize {
        ((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize
    }

    fn translate_raw(msg: u32, wparam: usize, lparam: isize) -> Option<PlatformEvent> {
        let mut state = TranslateState::default();
        translate(msg, wparam, lparam, &mut state)
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

    #[test]
    fn wheel_sign_and_ticks_come_from_the_high_word() {
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
                dx_ticks: 0.0,
                dy_ticks: 1.0
            })
        );
        assert!(matches!(
            down,
            Some(PlatformEvent::Scroll { dy_ticks, .. }) if (dy_ticks + 1.0).abs() < f32::EPSILON
        ));
        assert!(matches!(
            horizontal,
            Some(PlatformEvent::Scroll { dx_ticks, .. }) if (dx_ticks + 1.0).abs() < f32::EPSILON
        ));
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
        let high = translate(WM_CHAR, 0xD83C, 0, &mut state);
        let low = translate(WM_CHAR, 0xDF0D, 0, &mut state);
        let plain = translate(WM_CHAR, 'a' as usize, 0, &mut state);

        assert_eq!(high, None, "高代理单独不产生文本");
        assert_eq!(low, Some(PlatformEvent::Text("🌍".into())));
        assert_eq!(plain, Some(PlatformEvent::Text("a".into())));
    }

    #[test]
    fn key_events_track_modifiers_in_message_order() {
        let mut state = TranslateState::default();
        let shift_down = translate(WM_KEYDOWN, 0x10, 0, &mut state);
        let a_down = translate(WM_KEYDOWN, 'A' as usize, 0, &mut state);

        assert_eq!(
            shift_down,
            Some(PlatformEvent::Key {
                key: Key::Shift,
                state: KeyState::Down
            })
        );
        assert_eq!(state.modifiers(), Modifiers::SHIFT);
        assert!(matches!(
            a_down,
            Some(PlatformEvent::Key {
                key: Key::Other(0x41),
                state: KeyState::Down
            })
        ));

        let _ = translate(WM_KEYUP, 0x10, 0, &mut state);
        assert_eq!(state.modifiers(), Modifiers::empty());
    }

    #[test]
    fn navigation_and_function_keys_map_to_named_keys() {
        assert_eq!(key_from_vk(0x1B), Some(Key::Escape));
        assert_eq!(key_from_vk(0x26), Some(Key::ArrowUp));
        assert_eq!(key_from_vk(0x70), Some(Key::Function(1)));
        assert_eq!(key_from_vk(0x87), Some(Key::Function(24)));
        assert_eq!(key_from_vk(0x41), Some(Key::Other(0x41)));
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

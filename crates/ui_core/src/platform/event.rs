use smol_str::SmolStr;
use windows::Win32::UI::{
    Input::KeyboardAndMouse::{VK_PACKET, VK_PROCESSKEY},
    WindowsAndMessaging::*,
};

use crate::{
    event::{ImeEvent, KeyCode, KeyState, Modifiers, PointerButton},
    math::DevicePixelRatio,
    platform::windows::ime::ImeState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PointerKind {
    Move,
    Down(PointerButton),
    Up(PointerButton),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PlatformEvent {
    Pointer {
        x: i32,
        y: i32,
        kind: PointerKind,
    },
    PointerEnter,
    Scroll {
        x: i32,
        y: i32,
        dx_lines: f32,
        dy_lines: f32,
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
    },
    Moved {
        x: i32,
        y: i32,
    },
    Focus(bool),
    Appearance(bool),
    PointerLeave,
    PointerCancel,
    Close,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StampedEvent {
    pub event: PlatformEvent,
    pub ticks_ms: u32,
    pub scale: DevicePixelRatio,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MessageContext {
    pub ticks_ms: u32,
    pub wheel_lines: f32,
    pub scale: DevicePixelRatio,
}

#[derive(Debug, Default)]
pub(crate) struct TranslateState {
    modifiers: Modifiers,
    high_surrogate: Option<u16>,
    ime: ImeState,
}

impl TranslateState {
    pub(crate) fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    pub(crate) fn reset_transient(&mut self) {
        self.modifiers = Modifiers::empty();
        self.high_surrogate = None;
        self.ime.clear_pending();
    }

    pub(crate) fn ime_mut(&mut self) -> &mut ImeState {
        &mut self.ime
    }

    pub(crate) fn ime(&self) -> &ImeState {
        &self.ime
    }

    pub(crate) fn end_message_batch(&mut self) {
        self.ime.end_message_batch();
    }
}

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
            })
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            if matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) {
                state.ime.clear_pending();
            }
            let vk = (wparam & 0xFF) as u16;
            if matches!(vk, v if v == VK_PROCESSKEY.0 || v == VK_PACKET.0) {
                return None;
            }
            let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
            update_modifiers(vk, down, &mut state.modifiers);
            let raw = lparam as u32;
            let repeat = down && (raw & (1 << 30)) != 0;
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
            if state.ime.consume_char(wparam as u16, context.ticks_ms) {
                return None;
            }
            let text = decode_char(wparam as u16, state)?;
            let modifiers = state.modifiers;
            let alt_gr = modifiers.contains(Modifiers::CONTROL | Modifiers::ALT);
            let command_modifier = modifiers.contains(Modifiers::META)
                || (!alt_gr && modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT));
            if state.ime.is_composing() || command_modifier || text.chars().any(char::is_control) {
                return None;
            }
            Some(PlatformEvent::Text(text))
        }
        windows::Win32::UI::Controls::WM_MOUSELEAVE => Some(PlatformEvent::PointerLeave),
        WM_SETFOCUS => Some(PlatformEvent::Focus(true)),
        WM_KILLFOCUS => Some(PlatformEvent::Focus(false)),
        WM_SIZE => {
            if wparam == SIZE_MINIMIZED as usize {
                return None;
            }
            let width = lparam as u32 & 0xFFFF;
            let height = (lparam as u32 >> 16) & 0xFFFF;
            (width > 0 && height > 0).then_some(PlatformEvent::Resize { width, height })
        }
        WM_MOVE => Some(PlatformEvent::Moved { x, y }),
        WM_DPICHANGED => {
            let dpi = (wparam as u32) & 0xFFFF;
            (dpi != 0).then_some(PlatformEvent::Dpi { dpi })
        }
        WM_CLOSE => Some(PlatformEvent::Close),
        _ => None,
    };
    event.map(|event| StampedEvent {
        event,
        ticks_ms: context.ticks_ms,
        scale: context.scale,
    })
}

const WHEEL_DELTA_F: f32 = 120.0;

pub(crate) fn lines_per_tick(wheel_lines: f32) -> f32 {
    if wheel_lines.is_finite() && wheel_lines > 0.0 && wheel_lines <= 100.0 {
        wheel_lines
    } else {
        3.0
    }
}

pub(crate) fn loword_signed(value: isize) -> i32 {
    (value as u32 & 0xFFFF) as u16 as i16 as i32
}

pub(crate) fn hiword_signed(value: isize) -> i32 {
    ((value as u32 >> 16) & 0xFFFF) as u16 as i16 as i32
}

fn xbutton(wparam: usize) -> u16 {
    ((wparam as u32 >> 16) & 0xFFFF) as u16
}

fn high_word(wparam: usize) -> u16 {
    ((wparam as u32 >> 16) & 0xFFFF) as u16
}

fn decode_char(unit: u16, state: &mut TranslateState) -> Option<SmolStr> {
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

fn update_modifiers(vk: u16, down: bool, modifiers: &mut Modifiers) {
    let flag = match vk {
        VK_SHIFT_CODE => Modifiers::SHIFT,
        VK_CONTROL_CODE => Modifiers::CONTROL,
        VK_MENU_CODE => Modifiers::ALT,
        0x5B | 0x5C => Modifiers::META,
        _ => return,
    };
    modifiers.set(flag, down);
}

const VK_SHIFT_CODE: u16 = 0x10;
const VK_CONTROL_CODE: u16 = 0x11;
const VK_MENU_CODE: u16 = 0x12;
const VK_PAUSE_CODE: u16 = 0x13;
const VK_SNAPSHOT_CODE: u16 = 0x2C;

pub(crate) fn key_code_from_native(vk: u16, scancode: u16, extended: bool) -> KeyCode {
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
    let fallback = if extended { None } else { key_code_from_vk(vk) };
    fallback.unwrap_or(KeyCode::Unidentified(if scancode != 0 {
        scancode | if extended { 0x100 } else { 0 }
    } else {
        0x8000 | vk
    }))
}

fn key_code_from_scancode(scancode: u16, extended: bool) -> Option<KeyCode> {
    if extended {
        return key_code_from_extended_scancode(scancode);
    }
    key_code_from_base_scancode(scancode)
}

fn key_code_from_base_scancode(scancode: u16) -> Option<KeyCode> {
    let code = match scancode {
        0x01 => KeyCode::Escape,
        0x02 => KeyCode::Digit1,
        0x03 => KeyCode::Digit2,
        0x04 => KeyCode::Digit3,
        0x05 => KeyCode::Digit4,
        0x06 => KeyCode::Digit5,
        0x07 => KeyCode::Digit6,
        0x08 => KeyCode::Digit7,
        0x09 => KeyCode::Digit8,
        0x0A => KeyCode::Digit9,
        0x0B => KeyCode::Digit0,
        0x0C => KeyCode::Minus,
        0x0D => KeyCode::Equal,
        0x0E => KeyCode::Backspace,
        0x0F => KeyCode::Tab,
        0x10 => KeyCode::KeyQ,
        0x11 => KeyCode::KeyW,
        0x12 => KeyCode::KeyE,
        0x13 => KeyCode::KeyR,
        0x14 => KeyCode::KeyT,
        0x15 => KeyCode::KeyY,
        0x16 => KeyCode::KeyU,
        0x17 => KeyCode::KeyI,
        0x18 => KeyCode::KeyO,
        0x19 => KeyCode::KeyP,
        0x1A => KeyCode::BracketLeft,
        0x1B => KeyCode::BracketRight,
        0x1C => KeyCode::Enter,
        0x1D => KeyCode::ControlLeft,
        0x1E => KeyCode::KeyA,
        0x1F => KeyCode::KeyS,
        0x20 => KeyCode::KeyD,
        0x21 => KeyCode::KeyF,
        0x22 => KeyCode::KeyG,
        0x23 => KeyCode::KeyH,
        0x24 => KeyCode::KeyJ,
        0x25 => KeyCode::KeyK,
        0x26 => KeyCode::KeyL,
        0x27 => KeyCode::Semicolon,
        0x28 => KeyCode::Quote,
        0x29 => KeyCode::Backquote,
        0x2A => KeyCode::ShiftLeft,
        0x2B => KeyCode::Backslash,
        0x2C => KeyCode::KeyZ,
        0x2D => KeyCode::KeyX,
        0x2E => KeyCode::KeyC,
        0x2F => KeyCode::KeyV,
        0x30 => KeyCode::KeyB,
        0x31 => KeyCode::KeyN,
        0x32 => KeyCode::KeyM,
        0x33 => KeyCode::Comma,
        0x34 => KeyCode::Period,
        0x35 => KeyCode::Slash,
        0x36 => KeyCode::ShiftRight,
        0x37 => KeyCode::NumpadMultiply,
        0x38 => KeyCode::AltLeft,
        0x39 => KeyCode::Space,
        0x3A => KeyCode::CapsLock,
        0x3B => KeyCode::F1,
        0x3C => KeyCode::F2,
        0x3D => KeyCode::F3,
        0x3E => KeyCode::F4,
        0x3F => KeyCode::F5,
        0x40 => KeyCode::F6,
        0x41 => KeyCode::F7,
        0x42 => KeyCode::F8,
        0x43 => KeyCode::F9,
        0x44 => KeyCode::F10,
        0x45 => KeyCode::NumLock,
        0x46 => KeyCode::ScrollLock,
        0x47 => KeyCode::Numpad7,
        0x48 => KeyCode::Numpad8,
        0x49 => KeyCode::Numpad9,
        0x4A => KeyCode::NumpadSubtract,
        0x4B => KeyCode::Numpad4,
        0x4C => KeyCode::Numpad5,
        0x4D => KeyCode::Numpad6,
        0x4E => KeyCode::NumpadAdd,
        0x4F => KeyCode::Numpad1,
        0x50 => KeyCode::Numpad2,
        0x51 => KeyCode::Numpad3,
        0x52 => KeyCode::Numpad0,
        0x53 => KeyCode::NumpadDecimal,
        0x56 => KeyCode::IntlBackslash,
        0x57 => KeyCode::F11,
        0x58 => KeyCode::F12,
        0x59 => KeyCode::NumpadEqual,
        0x64 => KeyCode::F13,
        0x65 => KeyCode::F14,
        0x66 => KeyCode::F15,
        0x67 => KeyCode::F16,
        0x68 => KeyCode::F17,
        0x69 => KeyCode::F18,
        0x6A => KeyCode::F19,
        0x6B => KeyCode::F20,
        0x6C => KeyCode::F21,
        0x6D => KeyCode::F22,
        0x6E => KeyCode::F23,
        0x6F => KeyCode::F24,
        0x73 => KeyCode::IntlRo,
        0x7D => KeyCode::IntlYen,
        0x7E => KeyCode::NumpadComma,
        _ => return None,
    };
    Some(code)
}

fn key_code_from_extended_scancode(scancode: u16) -> Option<KeyCode> {
    let code = match scancode {
        0x1C => KeyCode::NumpadEnter,
        0x1D => KeyCode::ControlRight,
        0x35 => KeyCode::NumpadDivide,
        0x38 => KeyCode::AltRight,
        0x47 => KeyCode::Home,
        0x48 => KeyCode::ArrowUp,
        0x49 => KeyCode::PageUp,
        0x4B => KeyCode::ArrowLeft,
        0x4D => KeyCode::ArrowRight,
        0x4F => KeyCode::End,
        0x50 => KeyCode::ArrowDown,
        0x51 => KeyCode::PageDown,
        0x52 => KeyCode::Insert,
        0x53 => KeyCode::Delete,
        0x5B => KeyCode::MetaLeft,
        0x5C => KeyCode::MetaRight,
        0x5D => KeyCode::ContextMenu,
        0x10 => KeyCode::MediaTrackPrevious,
        0x19 => KeyCode::MediaTrackNext,
        0x20 => KeyCode::AudioVolumeMute,
        0x21 => KeyCode::LaunchApp2,
        0x22 => KeyCode::MediaPlayPause,
        0x24 => KeyCode::MediaStop,
        0x2E => KeyCode::AudioVolumeDown,
        0x30 => KeyCode::AudioVolumeUp,
        0x32 => KeyCode::BrowserHome,
        0x5E => KeyCode::Power,
        0x5F => KeyCode::Sleep,
        0x63 => KeyCode::WakeUp,
        0x65 => KeyCode::BrowserSearch,
        0x66 => KeyCode::BrowserFavorites,
        0x67 => KeyCode::BrowserRefresh,
        0x68 => KeyCode::BrowserStop,
        0x69 => KeyCode::BrowserForward,
        0x6A => KeyCode::BrowserBack,
        0x6B => KeyCode::LaunchApp1,
        0x6C => KeyCode::LaunchMail,
        0x6D => KeyCode::MediaSelect,
        _ => return None,
    };
    Some(code)
}

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
        0x2F => KeyCode::Help,
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
    use smol_str::SmolStr;
    use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::*};

    use super::{
        MessageContext, PlatformEvent, PointerKind, TranslateState, key_code_from_native,
        lines_per_tick, translate,
    };
    use crate::{
        event::{ImeEvent, KeyCode, KeyState, Modifiers, PointerButton},
        math::DevicePixelRatio,
    };

    fn key_lparam(scancode: u16, extended: bool, previous_down: bool) -> isize {
        key_lparam_full(scancode, extended, previous_down, false, false)
    }

    fn key_lparam_full(
        scancode: u16,
        extended: bool,
        previous_down: bool,
        released: bool,
        alt_context: bool,
    ) -> isize {
        let mut raw = 1u32 | (u32::from(scancode) << 16);
        if extended {
            raw |= 1 << 24;
        }
        if alt_context {
            raw |= 1 << 29;
        }
        if previous_down {
            raw |= 1 << 30;
        }
        if released {
            raw |= 1 << 31;
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
            scale: DevicePixelRatio::ONE,
        }
    }

    fn translate_raw(msg: u32, wparam: usize, lparam: isize) -> Option<PlatformEvent> {
        let mut state = TranslateState::default();
        translate(msg, wparam, lparam, context(), &mut state).map(|stamped| stamped.event)
    }

    fn char_event(state: &mut TranslateState, unit: u16) -> Option<PlatformEvent> {
        char_message(state, unit, 0)
    }

    fn char_message(state: &mut TranslateState, unit: u16, lparam: isize) -> Option<PlatformEvent> {
        translate(WM_CHAR, usize::from(unit), lparam, context(), state).map(|stamped| stamped.event)
    }

    fn key_message(
        state: &mut TranslateState,
        msg: u32,
        vk: u16,
        lparam: isize,
    ) -> Option<PlatformEvent> {
        translate(msg, usize::from(vk), lparam, context(), state).map(|stamped| stamped.event)
    }

    fn key_down(state: &mut TranslateState, vk: u16, scancode: u16, extended: bool) {
        let _ = key_message(state, WM_KEYDOWN, vk, key_lparam(scancode, extended, false));
    }

    fn key_up(state: &mut TranslateState, vk: u16, scancode: u16, extended: bool) {
        let _ = key_message(
            state,
            WM_KEYUP,
            vk,
            key_lparam_full(scancode, extended, true, true, false),
        );
    }

    fn sys_key(state: &mut TranslateState, vk: u16, scancode: u16, down: bool) {
        let _ = key_message(
            state,
            if down { WM_SYSKEYDOWN } else { WM_SYSKEYUP },
            vk,
            key_lparam_full(scancode, false, !down, !down, true),
        );
    }

    const ALTGR_CTRL_DOWN: isize = 0x001D_0001;
    const ALTGR_ALT_DOWN: isize = 0x2138_0001;
    const ALTGR_LETTER_DOWN: isize = 0x2010_0001;
    const ALTGR_CHAR: isize = 0x2010_0001;
    const ALTGR_LETTER_UP: isize = 0xE010_0001;
    const ALTGR_CTRL_UP: isize = 0xE01D_0001;
    const ALTGR_ALT_UP: isize = 0xE038_0001;

    fn altgr_char_event(state: &mut TranslateState, unit: u16) -> Option<PlatformEvent> {
        let _ = key_message(state, WM_KEYDOWN, 0x11, ALTGR_CTRL_DOWN);
        let _ = key_message(state, WM_KEYDOWN, 0x12, ALTGR_ALT_DOWN);
        let _ = key_message(state, WM_KEYDOWN, 0x51, ALTGR_LETTER_DOWN);
        char_message(state, unit, ALTGR_CHAR)
    }

    fn altgr_release(state: &mut TranslateState) {
        let _ = key_message(state, WM_KEYUP, 0x51, ALTGR_LETTER_UP);
        let _ = key_message(state, WM_SYSKEYUP, 0x11, ALTGR_CTRL_UP);
        let _ = key_message(state, WM_KEYUP, 0x12, ALTGR_ALT_UP);
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
                scale: DevicePixelRatio::ONE,
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
    fn window_moves_decode_signed_screen_coordinates() {
        assert_eq!(
            translate_raw(WM_MOVE, 0, lparam(320, 180)),
            Some(PlatformEvent::Moved { x: 320, y: 180 })
        );
        assert_eq!(
            translate_raw(WM_MOVE, 0, lparam(-1920, -200)),
            Some(PlatformEvent::Moved { x: -1920, y: -200 })
        );
    }

    #[test]
    fn buttons_come_from_the_message_not_the_wparam() {
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

        assert_eq!(event, Some(PlatformEvent::Dpi { dpi: 144 }));
    }

    #[test]
    fn char_messages_merge_surrogate_pairs() {
        let mut state = TranslateState::default();
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

    #[test]
    fn control_char_messages_never_become_text() {
        let mut state = TranslateState::default();
        for unit in [0x08_u16, 0x09, 0x0D, 0x1B, 0x7F, 0x85, 0x9F] {
            assert_eq!(
                char_event(&mut state, unit),
                None,
                "WM_CHAR {unit:#04x} 是控制字符，不得变成 Text"
            );
        }
        assert_eq!(
            char_event(&mut state, u16::from(b'a')),
            Some(PlatformEvent::Text("a".into())),
            "控制字符过滤不得吞掉普通键入"
        );
    }

    #[test]
    fn command_modifier_messages_never_become_text() {
        let mut state = TranslateState::default();
        assert_eq!(
            char_event(&mut state, u16::from(b'x')),
            Some(PlatformEvent::Text("x".into()))
        );

        key_down(&mut state, 0x11, 0x1D, false);
        assert_eq!(state.modifiers(), Modifiers::CONTROL);
        for unit in [0x01_u16, 0x03, 0x16, 0x18, 0x1A, 0x19] {
            assert_eq!(
                char_event(&mut state, unit),
                None,
                "Ctrl 组合的 WM_CHAR {unit:#04x} 不得变成 Text"
            );
        }
        assert_eq!(
            char_event(&mut state, 0x20),
            None,
            "Ctrl+Space 是真实可达的非控制字符 Ctrl 组合，必须拦截"
        );
        key_up(&mut state, 0x11, 0x1D, false);

        sys_key(&mut state, 0x12, 0x38, true);
        assert_eq!(state.modifiers(), Modifiers::ALT);
        assert_eq!(
            char_event(&mut state, u16::from(b'x')),
            None,
            "单独 ALT 仍拦截（物理左 Alt+字母走 WM_SYSCHAR，本断言是防御）"
        );
        sys_key(&mut state, 0x12, 0x38, false);
        assert_eq!(
            char_event(&mut state, u16::from(b'x')),
            Some(PlatformEvent::Text("x".into())),
            "修饰键全部抬起后必须恢复文本"
        );

        key_down(&mut state, 0x5B, 0x5B, true);
        assert_eq!(state.modifiers(), Modifiers::META);
        assert_eq!(
            char_event(&mut state, u16::from(b'x')),
            None,
            "META 不得产生 Text"
        );
        key_up(&mut state, 0x5B, 0x5B, true);
        assert_eq!(
            char_event(&mut state, u16::from(b'x')),
            Some(PlatformEvent::Text("x".into()))
        );
    }

    #[test]
    fn altgr_char_messages_become_text() {
        let mut state = TranslateState::default();
        for (unit, expected) in [(0x40_u16, "@"), (0x20AC, "€"), (0x0105, "ą")] {
            assert_eq!(
                altgr_char_event(&mut state, unit),
                Some(PlatformEvent::Text(expected.into())),
                "AltGr 字符 {expected} 必须放行"
            );
            assert_eq!(
                state.modifiers(),
                Modifiers::CONTROL | Modifiers::ALT,
                "AltGr 投递 WM_CHAR 时是合成的 Ctrl+Alt"
            );
            assert_eq!(char_event(&mut state, 0x08), None, "AltGr 下控制字符仍拦截");
            assert_eq!(char_event(&mut state, 0x0D), None);
            altgr_release(&mut state);
            assert_eq!(state.modifiers(), Modifiers::empty());
        }

        key_down(&mut state, 0x10, 0x2A, false);
        assert_eq!(
            altgr_char_event(&mut state, 0x20AC),
            Some(PlatformEvent::Text("€".into())),
            "AltGr+Shift 仍放行"
        );
        key_down(&mut state, 0x5B, 0x5B, true);
        assert_eq!(
            char_event(&mut state, 0x40),
            None,
            "META 与 AltGr 同按时 META 优先拦截"
        );
    }

    #[test]
    fn alt_numpad_char_after_alt_release_becomes_text() {
        let mut state = TranslateState::default();
        sys_key(&mut state, 0x12, 0x38, true);
        assert_eq!(state.modifiers(), Modifiers::ALT);
        sys_key(&mut state, 0x61, 0x4F, true);
        sys_key(&mut state, 0x61, 0x4F, false);
        sys_key(&mut state, 0x12, 0x38, false);
        assert_eq!(state.modifiers(), Modifiers::empty());
        assert_eq!(
            char_event(&mut state, 0xE9),
            Some(PlatformEvent::Text("é".into())),
            "Alt+numpad 的 WM_CHAR 在 Alt 抬起后到达，不得被 Alt 规则误杀"
        );
    }

    #[test]
    fn shift_never_blocks_text() {
        let mut state = TranslateState::default();
        key_down(&mut state, 0x10, 0x2A, false);
        assert_eq!(state.modifiers(), Modifiers::SHIFT);
        assert_eq!(
            char_event(&mut state, u16::from(b'A')),
            Some(PlatformEvent::Text("A".into()))
        );
    }

    #[test]
    fn emoji_surrogate_pairs_survive_the_char_filter() {
        let mut state = TranslateState::default();
        key_down(&mut state, 0x10, 0x2A, false);
        assert_eq!(char_event(&mut state, 0xD83C), None, "高代理单独不产生文本");
        assert_eq!(
            char_event(&mut state, 0xDF0D),
            Some(PlatformEvent::Text("🌍".into())),
            "Shift 组合下的代理对必须保留"
        );
        assert_eq!(char_event(&mut state, 0xDF0D), None, "孤立低代理不产生文本");
        assert_eq!(
            char_event(&mut state, u16::from(b'a')),
            Some(PlatformEvent::Text("a".into())),
            "孤立低代理不得污染后续键入"
        );
    }

    #[test]
    fn ime_replay_suppression_survives_modifier_changes() {
        let mut state = TranslateState::default();
        key_down(&mut state, 0x11, 0x1D, false);
        state.ime_mut().queue_commit("🌍ab", context().ticks_ms);
        assert_eq!(char_event(&mut state, 0xD83C), None);
        assert_eq!(char_event(&mut state, 0xDF0D), None);
        assert_eq!(char_event(&mut state, u16::from(b'a')), None);
        key_up(&mut state, 0x11, 0x1D, false);
        assert_eq!(
            char_event(&mut state, u16::from(b'b')),
            None,
            "Ctrl 抬起后队列里剩余的补发字符仍必须被抑制（抑制先于过滤器）"
        );
        assert_eq!(
            char_event(&mut state, u16::from(b'c')),
            Some(PlatformEvent::Text("c".into())),
            "队列耗尽后真实键入必须放行"
        );
    }

    #[test]
    fn composition_char_messages_never_become_text() {
        let mut state = TranslateState::default();
        let start = state
            .ime_mut()
            .handle(HWND::default(), WM_IME_STARTCOMPOSITION, 0, 0);
        assert!(matches!(start.events.as_slice(), [ImeEvent::Start]));
        assert!(state.ime().is_composing());
        assert_eq!(
            char_event(&mut state, u16::from(b'a')),
            None,
            "组合期普通字符只信 IME 事件"
        );
        assert_eq!(char_event(&mut state, 0x08), None);

        let end = state
            .ime_mut()
            .handle(HWND::default(), WM_IME_ENDCOMPOSITION, 0, 0);
        assert!(matches!(end.events.as_slice(), [ImeEvent::End]));
        assert!(!state.ime().is_composing());
        assert_eq!(
            char_event(&mut state, u16::from(b'a')),
            Some(PlatformEvent::Text("a".into())),
            "组合结束后必须恢复文本"
        );
    }

    #[test]
    fn losing_focus_clears_stuck_modifiers() {
        let mut state = TranslateState::default();
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

        state.reset_transient();
        assert_eq!(state.modifiers(), Modifiers::empty());

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
        assert_eq!(key_code_from_native(0x41, 0x1E, false), KeyCode::KeyA);
        assert_eq!(
            key_code_from_native(0x41, 0, false),
            KeyCode::Unidentified(0x8041)
        );
    }

    #[test]
    fn physical_identity_ignores_the_layout_dependent_vk() {
        assert_eq!(key_code_from_native(0x41, 0x10, false), KeyCode::KeyQ);
        assert_eq!(key_code_from_native(0x51, 0x1E, false), KeyCode::KeyA);
        assert_eq!(key_code_from_native(0x24, 0x47, false), KeyCode::Numpad7);
        assert_eq!(key_code_from_native(0x24, 0x47, true), KeyCode::Home);
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
    fn extended_row_media_keys_never_fall_through_to_letters_or_function_keys() {
        let table: &[(u16, KeyCode)] = &[
            (0x10, KeyCode::MediaTrackPrevious),
            (0x19, KeyCode::MediaTrackNext),
            (0x20, KeyCode::AudioVolumeMute),
            (0x21, KeyCode::LaunchApp2),
            (0x22, KeyCode::MediaPlayPause),
            (0x24, KeyCode::MediaStop),
            (0x2E, KeyCode::AudioVolumeDown),
            (0x30, KeyCode::AudioVolumeUp),
            (0x32, KeyCode::BrowserHome),
            (0x5E, KeyCode::Power),
            (0x5F, KeyCode::Sleep),
            (0x63, KeyCode::WakeUp),
            (0x65, KeyCode::BrowserSearch),
            (0x66, KeyCode::BrowserFavorites),
            (0x67, KeyCode::BrowserRefresh),
            (0x68, KeyCode::BrowserStop),
            (0x69, KeyCode::BrowserForward),
            (0x6A, KeyCode::BrowserBack),
            (0x6B, KeyCode::LaunchApp1),
            (0x6C, KeyCode::LaunchMail),
            (0x6D, KeyCode::MediaSelect),
            (0x1C, KeyCode::NumpadEnter),
            (0x1D, KeyCode::ControlRight),
            (0x35, KeyCode::NumpadDivide),
            (0x38, KeyCode::AltRight),
            (0x47, KeyCode::Home),
            (0x48, KeyCode::ArrowUp),
            (0x49, KeyCode::PageUp),
            (0x4B, KeyCode::ArrowLeft),
            (0x4D, KeyCode::ArrowRight),
            (0x4F, KeyCode::End),
            (0x50, KeyCode::ArrowDown),
            (0x51, KeyCode::PageDown),
            (0x52, KeyCode::Insert),
            (0x53, KeyCode::Delete),
            (0x5B, KeyCode::MetaLeft),
            (0x5C, KeyCode::MetaRight),
            (0x5D, KeyCode::ContextMenu),
        ];
        for (scancode, expected) in table {
            assert_eq!(
                key_code_from_native(0, *scancode, true),
                *expected,
                "E0 {scancode:#04x} 必须是 {expected:?}"
            );
        }

        let base: &[(u16, KeyCode)] = &[
            (0x30, KeyCode::KeyB),
            (0x2E, KeyCode::KeyC),
            (0x20, KeyCode::KeyD),
            (0x22, KeyCode::KeyG),
            (0x24, KeyCode::KeyJ),
            (0x19, KeyCode::KeyP),
            (0x10, KeyCode::KeyQ),
            (0x32, KeyCode::KeyM),
            (0x65, KeyCode::F14),
            (0x6F, KeyCode::F24),
        ];
        for (scancode, expected) in base {
            assert_eq!(
                key_code_from_native(0, *scancode, false),
                *expected,
                "基础行 {scancode:#04x} 必须是 {expected:?}"
            );
        }

        assert_eq!(
            key_code_from_native(0, 0x6E, true),
            KeyCode::Unidentified(0x100 | 0x6E)
        );
        assert_eq!(
            key_code_from_native(0, 0x2A, true),
            KeyCode::Unidentified(0x100 | 0x2A),
            "E0 2A（部分键盘的伪 Shift）不得报告成 ShiftLeft"
        );
    }

    #[test]
    fn extended_keys_never_fall_back_to_the_vk_table() {
        let known: &[(u16, u16, KeyCode)] = &[
            (0x2E, 0x2E, KeyCode::AudioVolumeDown),
            (0x30, 0x30, KeyCode::AudioVolumeUp),
            (0x20, 0x20, KeyCode::AudioVolumeMute),
            (0x22, 0xB3, KeyCode::MediaPlayPause),
            (0x10, 0xB1, KeyCode::MediaTrackPrevious),
            (0x19, 0xB0, KeyCode::MediaTrackNext),
            (0x24, 0xB2, KeyCode::MediaStop),
            (0x32, 0xAC, KeyCode::BrowserHome),
            (0x6A, 0xA6, KeyCode::BrowserBack),
            (0x6B, 0xB4, KeyCode::LaunchApp1),
            (0x47, 0x24, KeyCode::Home),
            (0x53, 0x2E, KeyCode::Delete),
            (0x1D, 0xA3, KeyCode::ControlRight),
            (0x38, 0xA5, KeyCode::AltRight),
            (0x5B, 0x5B, KeyCode::MetaLeft),
            (0x5D, 0x5D, KeyCode::ContextMenu),
        ];
        for (scancode, vk, expected) in known {
            assert_eq!(
                key_code_from_native(*vk, *scancode, true),
                *expected,
                "E0 {scancode:#04x} + VK {vk:#04x} 必须是 {expected:?}（扫描码表说了算）"
            );
        }

        let unknown: &[(u16, u16, u16)] = &[
            (0x6E, 0x2E, 0x16E),
            (0x2A, 0x10, 0x12A),
            (0x64, 0x70, 0x164),
            (0x46, 0x0D, 0x146),
            (0x37, 0x24, 0x137),
            (0x1F, 0x25, 0x11F),
        ];
        for (scancode, vk, payload) in unknown {
            assert_eq!(
                key_code_from_native(*vk, *scancode, true),
                KeyCode::Unidentified(*payload),
                "E0 {scancode:#04x} + VK {vk:#04x} 不得被 VK 兜底美化"
            );
        }

        assert_eq!(key_code_from_native(0x2E, 0, false), KeyCode::Delete);
        assert_eq!(key_code_from_native(0x0D, 0, false), KeyCode::Enter);
        assert_eq!(key_code_from_native(0x70, 0, false), KeyCode::F1);
        assert_eq!(key_code_from_native(0x0D, 0x55, false), KeyCode::Enter);
        assert_eq!(
            key_code_from_native(0x2E, 0, true),
            KeyCode::Unidentified(0x8000 | 0x2E),
            "没有扫描码的扩展消息只能用 VK 载荷，不得命名成 Delete"
        );
    }

    #[test]
    fn numpad_comma_and_equal_are_reachable() {
        assert_eq!(
            key_code_from_native(0x7E, 0x7E, false),
            KeyCode::NumpadComma
        );
        assert_eq!(
            key_code_from_native(0x59, 0x59, false),
            KeyCode::NumpadEqual
        );
        assert_eq!(key_code_from_native(0x2F, 0, false), KeyCode::Help);
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

    #[test]
    fn ime_processed_and_packet_keys_never_become_key_events() {
        assert_eq!(translate_raw(WM_KEYDOWN, 0xE5, 0), None);
        assert_eq!(translate_raw(WM_KEYUP, 0xE5, 0), None);
        assert_eq!(translate_raw(WM_KEYDOWN, 0xE7, 0), None);
        assert_eq!(translate_raw(WM_KEYUP, 0xE7, 0), None);
        assert_eq!(translate_raw(WM_KEYDOWN, 0x01_00E5, 0), None);
        assert!(matches!(
            translate_raw(WM_KEYDOWN, 0x41, key_lparam(0x1E, false, false)),
            Some(PlatformEvent::Key {
                code: KeyCode::KeyA,
                state: KeyState::Down,
                ..
            })
        ));
    }

    #[test]
    fn committed_text_suppresses_only_the_matching_char_units() {
        let mut state = TranslateState::default();
        state.ime_mut().queue_commit("🌍a", context().ticks_ms);
        for unit in "🌍a".encode_utf16() {
            assert_eq!(
                translate(WM_CHAR, usize::from(unit), 0, context(), &mut state),
                None,
                "提交后补发的 WM_CHAR 必须被抑制"
            );
        }
        let text = translate(
            WM_CHAR,
            usize::from(u16::from(b'x')),
            0,
            context(),
            &mut state,
        )
        .expect("WM_CHAR");
        assert_eq!(text.event, PlatformEvent::Text(SmolStr::new_static("x")));
    }

    #[test]
    fn a_real_key_press_after_a_commit_is_never_swallowed() {
        let mut state = TranslateState::default();
        state.ime_mut().queue_commit("a", context().ticks_ms);
        let key = translate(
            WM_KEYDOWN,
            0x41,
            key_lparam(0x1E, false, false),
            context(),
            &mut state,
        )
        .expect("按键");
        assert!(matches!(
            key.event,
            PlatformEvent::Key {
                state: KeyState::Down,
                ..
            }
        ));
        let text = translate(
            WM_CHAR,
            usize::from(u16::from(b'a')),
            0,
            context(),
            &mut state,
        )
        .expect("真实键入必须放行");
        assert_eq!(text.event, PlatformEvent::Text(SmolStr::new_static("a")));
    }

    #[test]
    fn a_queue_left_over_from_an_older_message_batch_is_dropped() {
        let mut state = TranslateState::default();
        state.ime_mut().queue_commit("a", context().ticks_ms);
        state.end_message_batch();
        state.end_message_batch();
        let text = translate(
            WM_CHAR,
            usize::from(u16::from(b'a')),
            0,
            context(),
            &mut state,
        )
        .expect("过期队列必须放行");
        assert_eq!(text.event, PlatformEvent::Text(SmolStr::new_static("a")));
    }

    #[test]
    fn losing_focus_drops_pending_ime_suppression() {
        let mut state = TranslateState::default();
        state.ime_mut().queue_commit("ni", context().ticks_ms);

        state.reset_transient();

        let event = translate(
            WM_CHAR,
            usize::from(u16::from(b'n')),
            0,
            context(),
            &mut state,
        );
        assert_eq!(
            event.map(|stamped| stamped.event),
            Some(PlatformEvent::Text(SmolStr::new_static("n"))),
            "失焦后第一次键入必须放行"
        );
    }
}

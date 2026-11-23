use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT, ScreenToClient},
    UI::{
        Input::KeyboardAndMouse::{
            GetKeyState, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_LBUTTON, VK_MBUTTON,
            VK_RBUTTON, VK_SPACE, VK_XBUTTON1, VK_XBUTTON2,
        },
        WindowsAndMessaging::*,
    },
};

use super::{
    WndProcState,
    pump::{A11Y_TIMER_ID, cursor_handle, message_ticks},
    recovery,
};
use crate::{
    math::Point,
    platform::{
        dispatcher::{WM_APP_COMPOSITION_WAKE, WM_APP_FRAME_WAKE},
        event::{PlatformEvent, StampedEvent, hiword_signed, loword_signed, translate},
        windows::{composition, dark_mode, dpi, ime},
    },
};

pub(crate) extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndProcState;
        if state.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        if msg == WM_NCCALCSIZE
            && wparam.0 != 0
            && (*state).decorations() == crate::window::Decorations::None
        {
            let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
            if IsZoomed(hwnd).as_bool() {
                let border = (*state).resize_border();
                let current = params.rgrc[0];
                let inset = super::super::chrome::inset_for_maximized(
                    (current.left, current.top, current.right, current.bottom),
                    border,
                );
                params.rgrc[0] = windows::Win32::Foundation::RECT {
                    left: inset.0,
                    top: inset.1,
                    right: inset.2,
                    bottom: inset.3,
                };
            }
            return LRESULT(0);
        }

        if msg == WM_NCHITTEST {
            let mut point = POINT {
                x: loword_signed(lparam.0),
                y: hiword_signed(lparam.0),
            };
            if ScreenToClient(hwnd, &mut point).as_bool() {
                let frameless = (*state).decorations() == crate::window::Decorations::None;
                if frameless && !IsZoomed(hwnd).as_bool() {
                    let mut client = windows::Win32::Foundation::RECT::default();
                    if GetClientRect(hwnd, &mut client).is_ok()
                        && let Some(edge) = super::super::chrome::resize_edge_at(
                            (point.x, point.y),
                            (client.right - client.left, client.bottom - client.top),
                            (*state).resize_border(),
                        )
                    {
                        return LRESULT(super::super::chrome::hit_test_code(edge) as isize);
                    }
                }
                let scale = (*state).scale();
                let logical = Point::new(
                    scale.to_logical(point.x as f32),
                    scale.to_logical(point.y as f32),
                );
                if (*state)
                    .drag_regions()
                    .iter()
                    .any(|region| region.contains(logical))
                {
                    return LRESULT(HTCAPTION as isize);
                }
                if frameless {
                    return LRESULT(HTCLIENT as isize);
                }
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        if msg == WM_GETMINMAXINFO {
            let info = &mut *(lparam.0 as *mut MINMAXINFO);
            info.ptMinTrackSize = POINT {
                x: (*state).min_track_size.0 as i32,
                y: (*state).min_track_size.1 as i32,
            };
            if let Some((width, height)) = (*state).max_track_size {
                info.ptMaxTrackSize = POINT {
                    x: width as i32,
                    y: height as i32,
                };
            }
            return LRESULT(0);
        }
        if msg == WM_SETCURSOR {
            let hit = (lparam.0 as u32) & 0xFFFF;
            if hit == HTCLIENT {
                SetCursor(Some(cursor_handle((*state).cursor)));
                return LRESULT(1);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        if msg == WM_ERASEBKGND {
            return LRESULT(1);
        }
        if msg == WM_PAINT {
            let mut paint = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut paint);
            let _ = EndPaint(hwnd, &paint);
            composition::note_device_check();
            (*state).wake();
            return LRESULT(0);
        }
        if recovery::recovery_plan(msg, wparam.0).is_some() {
            (*state).note_display_recovery();
            (*state).wake();
            match msg {
                WM_POWERBROADCAST => return LRESULT(1),
                WM_DISPLAYCHANGE => return LRESULT(0),
                _ => {}
            }
        }
        if !(*state).ime_enabled() && (is_any_ime_message(msg) || msg == WM_INPUTLANGCHANGE) {
            if msg == WM_IME_SETCONTEXT || msg == WM_INPUTLANGCHANGE {
                let command = (*state).plan_ime_reassert();
                if let Some(command) = command {
                    let applied = ime::execute(hwnd, command);
                    (*state).note_ime_reasserted(applied);
                }
            }
            return LRESULT(0);
        }
        if msg == WM_IME_SETCONTEXT {
            let masked = ime::mask_setcontext(wparam.0, lparam.0);
            return DefWindowProcW(hwnd, msg, wparam, LPARAM(masked));
        }
        if msg == WM_DPICHANGED {
            let dpi = (wparam.0 as u32) & 0xFFFF;
            let Some(scale) = dpi::scale_from_dpi(dpi) else {
                return LRESULT(0);
            };
            let ticks = message_ticks();
            (*state).note_dpi(dpi, scale);
            let context = (*state).context(ticks);
            if let Some(stamped) =
                translate(msg, wparam.0, lparam.0, context, &mut (*state).translate)
            {
                (*state).push_stamped_event(stamped);
            }
            let suggested = (!lparam.0.is_negative() && lparam.0 != 0).then(|| {
                let rect = &*(lparam.0 as *const windows::Win32::Foundation::RECT);
                (rect.left, rect.top, rect.right, rect.bottom)
            });
            if let Some((left, top, right, bottom)) = suggested {
                (*state).suppress_move = true;
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    left,
                    top,
                    right - left,
                    bottom - top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                (*state).suppress_move = false;
            }
            (*state).wake();
            return LRESULT(0);
        }
        if msg == WM_CAPTURECHANGED {
            if (*state).suppress_capture_cancel {
                (*state).suppress_capture_cancel = false;
                return LRESULT(0);
            }
            let mut button_down = false;
            for key in [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2] {
                if GetKeyState(i32::from(key.0)) < 0 {
                    button_down = true;
                    break;
                }
            }
            if !button_down {
                return LRESULT(0);
            }
            (*state).push_now(PlatformEvent::PointerCancel);
            (*state).wake();
            return LRESULT(0);
        }

        if msg == WM_GETOBJECT {
            if !(*state).a11y_mut().begin_getobject() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let pending = (*state).a11y_mut().wm_getobject(wparam, lparam);
            let result = match pending {
                Some(value) => value.into(),
                None => DefWindowProcW(hwnd, msg, wparam, lparam),
            };
            (*state).a11y_mut().end_getobject();
            return result;
        }
        if msg == WM_EXITSIZEMOVE {
            (*state).end_size_move();
        }
        if matches!(msg, WM_SETFOCUS | WM_EXITMENULOOP | WM_EXITSIZEMOVE) {
            let events = (*state).a11y_mut().focus_changed(true);
            if let Some(events) = events {
                events.raise();
            }
        }
        if matches!(msg, WM_KILLFOCUS | WM_ENTERMENULOOP | WM_ENTERSIZEMOVE) {
            if msg == WM_KILLFOCUS {
                let ticks = message_ticks();
                if let Some(end) = (*state).translate.ime_mut().finish_composition() {
                    let scale = (*state).scale();
                    (*state).push_stamped_event(StampedEvent {
                        event: PlatformEvent::Ime(end),
                        ticks_ms: ticks,
                        scale,
                    });
                    (*state).wake();
                }
            }
            (*state).reset_transient_input();
            if msg == WM_ENTERSIZEMOVE {
                (*state).begin_size_move();
            }
            let events = (*state).a11y_mut().focus_changed(false);
            if let Some(events) = events {
                events.raise();
            }
        }

        if msg == WM_APP_FRAME_WAKE {
            if (*state).dispatcher().in_modal_loop()
                && let Some(step) = (*state).modal_tick_step()
                && let Some(error) = step.run()
            {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndProcState;
                if !state.is_null() {
                    (*state).note_pending_error(error);
                }
            }
            return LRESULT(0);
        }
        if msg == WM_APP_COMPOSITION_WAKE {
            return LRESULT(0);
        }
        if msg == WM_TIMER {
            if wparam.0 == A11Y_TIMER_ID {
                (*state).wake();
                return LRESULT(0);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        if msg == WM_SYSCOMMAND && (wparam.0 as u32 & 0xFFF0) == SC_KEYMENU {
            let is_system_menu = lparam.0 == VK_SPACE.0 as isize;
            if !is_system_menu {
                return LRESULT(0);
            }
        }
        if msg == WM_MOUSEMOVE && !(*state).mouse_tracking {
            let mut track = TRACKMOUSEEVENT {
                cbSize: core::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            if TrackMouseEvent(&mut track).is_ok() {
                (*state).mouse_tracking = true;
                (*state).note_client_pos(loword_signed(lparam.0), hiword_signed(lparam.0));
                (*state).push_now(PlatformEvent::PointerEnter);
            }
        }

        let (wparam_value, lparam_value) = (wparam.0, lparam.0);
        let lparam_value = if msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL {
            let mut point = POINT {
                x: loword_signed(lparam_value),
                y: hiword_signed(lparam_value),
            };
            let _ = ScreenToClient(hwnd, &mut point);
            encode_point(point.x, point.y)
        } else {
            lparam_value
        };

        let ticks = message_ticks();
        if is_ime_message(msg) {
            let outcome = (*state)
                .translate
                .ime_mut()
                .handle(hwnd, msg, lparam_value, ticks);
            if let Some(command) = outcome.command {
                let applied = ime::execute(hwnd, command);
                (*state).note_ime_applied(applied);
            }
            if !outcome.events.is_empty() {
                let scale = (*state).scale();
                for event in outcome.events {
                    (*state).push_stamped_event(StampedEvent {
                        event: PlatformEvent::Ime(event),
                        ticks_ms: ticks,
                        scale,
                    });
                }
                (*state).wake();
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        let stamped = if msg == WM_SETTINGCHANGE {
            (*state).refresh_wheel_lines();
            appearance_dark(lparam_value).map(|dark| {
                let applied = dark_mode::set_titlebar(hwnd, dark);
                #[cfg(test)]
                {
                    (*state).last_dark_titlebar = applied.then_some(dark);
                }
                #[cfg(not(test))]
                let _ = applied;
                StampedEvent {
                    event: PlatformEvent::Appearance(dark),
                    ticks_ms: ticks,
                    scale: (*state).scale(),
                }
            })
        } else {
            let context = (*state).context(ticks);
            translate(
                msg,
                wparam_value,
                lparam_value,
                context,
                &mut (*state).translate,
            )
        };

        if let Some(stamped) = stamped {
            if (*state).suppress_move && matches!(stamped.event, PlatformEvent::Moved { .. }) {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            match &stamped.event {
                PlatformEvent::PointerLeave => (*state).mouse_tracking = false,
                PlatformEvent::Pointer { x, y, .. } => (*state).note_client_pos(*x, *y),
                _ => {}
            }
            let is_close = matches!(stamped.event, PlatformEvent::Close);
            (*state).push_stamped_event(stamped);
            (*state).wake();
            if msg == WM_SIZE {
                (*state).note_chrome_dirty();
                let raw = lparam_value as usize;
                let client = ((raw & 0xFFFF) as u32, ((raw >> 16) & 0xFFFF) as u32);
                let step = (*state).resize_step(client);
                if let Some(step) = step
                    && let Some(error) = step.run()
                {
                    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndProcState;
                    if !state.is_null() {
                        (*state).note_pending_error(error);
                    }
                }
            }
            if is_close {
                (*state).close_requested = true;
                return LRESULT(0);
            }
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

pub(super) fn is_ime_message(msg: u32) -> bool {
    matches!(
        msg,
        WM_IME_STARTCOMPOSITION | WM_IME_COMPOSITION | WM_IME_ENDCOMPOSITION
    )
}

pub(super) fn is_any_ime_message(msg: u32) -> bool {
    matches!(
        msg,
        WM_IME_STARTCOMPOSITION
            | WM_IME_COMPOSITION
            | WM_IME_ENDCOMPOSITION
            | WM_IME_SETCONTEXT
            | WM_IME_NOTIFY
            | WM_IME_CONTROL
            | WM_IME_COMPOSITIONFULL
            | WM_IME_SELECT
            | WM_IME_CHAR
            | WM_IME_REQUEST
            | WM_IME_KEYDOWN
            | WM_IME_KEYUP
    )
}

pub(super) fn appearance_dark(lparam: isize) -> Option<bool> {
    if lparam == 0 {
        return None;
    }
    let changed = unsafe {
        let text = lparam as *const u16;
        if text.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *text.add(len) != 0 && len < 64 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(text, len);
        String::from_utf16_lossy(slice)
    };
    if changed != "ImmersiveColorSet" {
        return None;
    }
    dark_mode::current()
}

pub(super) fn encode_point(x: i32, y: i32) -> isize {
    ((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize
}

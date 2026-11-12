//! 最小 IME 通路（`design/phase-09-win32-demo.md` §4.4）。
//!
//! P09 只做三件事：组合开始/更新/结束的事件、`GCS_RESULTSTR` 的提交、以及把组合窗放到光标处。
//! 候选窗与 TSF 属 P12。probe 实测本机 `ImmGetContext` 返回有效 `HIMC`，
//! `ImmSetCompositionWindow(CFS_POINT)` 与 `ImmAssociateContextEx` 均成功。

use smol_str::SmolStr;
use windows::Win32::{
    Foundation::{HWND, POINT},
    UI::{
        Input::Ime::{
            CFS_POINT, COMPOSITIONFORM, GCS_COMPSTR, GCS_RESULTSTR, HIMC, IME_COMPOSITION_STRING,
            ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, ImmSetCompositionWindow,
        },
        WindowsAndMessaging::{WM_IME_COMPOSITION, WM_IME_ENDCOMPOSITION, WM_IME_STARTCOMPOSITION},
    },
};

use crate::event::ImeEvent;

/// IME 组合状态（每个窗口一份，由 `WndProcState` 持有）。
#[derive(Debug, Default)]
pub(crate) struct ImeState {
    /// 上一次提交的字符数：随后到达的 `WM_CHAR` 要跳过，避免提交两次。
    suppress_chars: usize,
}

impl ImeState {
    /// 处理一条 `WM_IME_*`；`None` = 该消息不产生应用事件。
    pub(crate) fn handle(&mut self, hwnd: HWND, msg: u32, lparam: isize) -> Option<ImeEvent> {
        match msg {
            WM_IME_STARTCOMPOSITION => Some(ImeEvent::Start),
            WM_IME_COMPOSITION => {
                let flags = lparam as u32;
                if flags & GCS_RESULTSTR.0 != 0 {
                    let text = composition_string(hwnd, GCS_RESULTSTR)?;
                    self.suppress_chars = text.chars().count();
                    return Some(ImeEvent::Commit(text));
                }
                if flags & GCS_COMPSTR.0 != 0 {
                    let text = composition_string(hwnd, GCS_COMPSTR).unwrap_or_default();
                    return Some(ImeEvent::Preedit(text));
                }
                None
            }
            WM_IME_ENDCOMPOSITION => Some(ImeEvent::End),
            _ => None,
        }
    }

    /// `WM_CHAR` 是否应被 IME 提交吞掉（提交后 Windows 会再发同样的字符）。
    pub(crate) fn take_char_suppression(&mut self) -> bool {
        if self.suppress_chars == 0 {
            return false;
        }
        self.suppress_chars -= 1;
        true
    }

    /// 组合窗放到窗口客户区的 `(x, y)`（物理像素）。
    pub(crate) fn set_composition_point(&self, hwnd: HWND, x: i32, y: i32) {
        // SAFETY: `himc` 由 `ImmGetContext` 取得、用后立即释放；`form` 是栈上值。
        unsafe {
            let himc: HIMC = ImmGetContext(hwnd);
            if himc.is_invalid() {
                return;
            }
            let form = COMPOSITIONFORM {
                dwStyle: CFS_POINT,
                ptCurrentPos: POINT { x, y },
                rcArea: Default::default(),
            };
            let _ = ImmSetCompositionWindow(himc, &form);
            let _ = ImmReleaseContext(hwnd, himc);
        }
    }
}

/// 取组合串（UTF-16 → `SmolStr`）；上下文无效或长度 ≤ 0 时返回 `None`。
fn composition_string(hwnd: HWND, flag: IME_COMPOSITION_STRING) -> Option<SmolStr> {
    // SAFETY: `himc` 成对释放；缓冲区由本函数独占，`written` 之后才读。
    unsafe {
        let himc: HIMC = ImmGetContext(hwnd);
        if himc.is_invalid() {
            return None;
        }
        let bytes = ImmGetCompositionStringW(himc, flag, None, 0);
        if bytes <= 0 {
            let _ = ImmReleaseContext(hwnd, himc);
            return None;
        }
        let mut units = vec![0u16; (bytes as usize).div_ceil(2)];
        let written = ImmGetCompositionStringW(
            himc,
            flag,
            Some(units.as_mut_ptr().cast()),
            (units.len() * 2) as u32,
        );
        let _ = ImmReleaseContext(hwnd, himc);
        if written <= 0 {
            return None;
        }
        let len = written as usize / 2;
        Some(SmolStr::new(String::from_utf16_lossy(&units[..len])))
    }
}

#[cfg(test)]
mod tests {
    use super::ImeState;

    #[test]
    fn committed_text_suppresses_the_matching_wm_chars() {
        let mut state = ImeState { suppress_chars: 3 };

        assert!(state.take_char_suppression());
        assert!(state.take_char_suppression());
        assert!(state.take_char_suppression());
        assert!(!state.take_char_suppression(), "只有提交过的字符会被吞");
    }

    #[test]
    fn unknown_messages_do_not_produce_ime_events() {
        let mut state = ImeState::default();
        // SAFETY: 该消息分支不读 `hwnd`；传空句柄是安全的（只走 `_` 分支）。
        let event = state.handle(
            windows::Win32::Foundation::HWND::default(),
            windows::Win32::UI::WindowsAndMessaging::WM_PAINT,
            0,
        );

        assert!(event.is_none());
    }
}

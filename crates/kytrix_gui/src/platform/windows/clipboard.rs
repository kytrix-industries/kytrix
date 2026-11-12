//! 剪贴板（`CF_UNICODETEXT`；`design/phase-09-win32-demo.md` §4.5）。
//!
//! probe 实测：`OpenClipboard` → `EmptyClipboard` → `GlobalAlloc(GMEM_MOVEABLE)` → `SetClipboardData`
//! → `CloseClipboard` 往返逐字节相等（含中文与 emoji），空串（只有 `NUL`）也成立。
//! 富文本 / 图片 / 拖放属 P12。

use smol_str::SmolStr;
use windows::Win32::{
    Foundation::{HANDLE, HGLOBAL, HWND},
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
        },
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
        Ole::CF_UNICODETEXT,
    },
};

use crate::error::EngineError;

/// 写入剪贴板文本（`CF_UNICODETEXT`，UTF-16 + `NUL`）。
///
/// # Errors
/// `EngineError::Clipboard`：剪贴板被其它进程占用（`OpenClipboard` 失败）或分配失败。
pub(crate) fn set_text(hwnd: HWND, text: &str) -> Result<(), EngineError> {
    let units = utf16_with_nul(text);
    let bytes = units.len() * 2;
    // SAFETY: 全局内存由本函数分配并交给剪贴板；成功后所有权归系统，不再释放。
    unsafe {
        OpenClipboard(Some(hwnd)).map_err(|error| clipboard_error("OpenClipboard", error))?;
        EmptyClipboard().map_err(|error| clipboard_error("EmptyClipboard", error))?;
        let global = match GlobalAlloc(GMEM_MOVEABLE, bytes) {
            Ok(global) => global,
            Err(error) => {
                let _ = CloseClipboard();
                return Err(clipboard_error("GlobalAlloc", error));
            }
        };
        let pointer = GlobalLock(global);
        if pointer.is_null() {
            let _ = CloseClipboard();
            return Err(EngineError::Clipboard {
                detail: SmolStr::new_static("GlobalLock returned null"),
            });
        }
        std::ptr::copy_nonoverlapping(units.as_ptr().cast::<u8>(), pointer.cast::<u8>(), bytes);
        let _ = GlobalUnlock(global);
        if let Err(error) = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(global.0))) {
            let _ = CloseClipboard();
            return Err(clipboard_error("SetClipboardData", error));
        }
        CloseClipboard().map_err(|error| clipboard_error("CloseClipboard", error))?;
    }
    Ok(())
}

/// 读取剪贴板文本（无 `CF_UNICODETEXT` 时返回空串）。
///
/// # Errors
/// `EngineError::Clipboard`：剪贴板被占用或内存映射失败。
pub(crate) fn text(hwnd: HWND) -> Result<SmolStr, EngineError> {
    // SAFETY: 句柄只在打开的剪贴板会话内使用；读取范围由系统给出的 `NUL` 终止。
    unsafe {
        OpenClipboard(Some(hwnd)).map_err(|error| clipboard_error("OpenClipboard", error))?;
        let handle = match GetClipboardData(CF_UNICODETEXT.0 as u32) {
            Ok(handle) => handle,
            Err(_) => {
                let _ = CloseClipboard();
                return Ok(SmolStr::default());
            }
        };
        let global = HGLOBAL(handle.0);
        let pointer = GlobalLock(global);
        if pointer.is_null() {
            let _ = CloseClipboard();
            return Err(EngineError::Clipboard {
                detail: SmolStr::new_static("GlobalLock returned null"),
            });
        }
        let mut units = Vec::new();
        let mut index = 0usize;
        loop {
            let unit = *(pointer.cast::<u16>().add(index));
            if unit == 0 {
                break;
            }
            units.push(unit);
            index += 1;
        }
        let _ = GlobalUnlock(global);
        let _ = CloseClipboard();
        Ok(SmolStr::new(String::from_utf16_lossy(&units)))
    }
}

/// `&str` → UTF-16 + `NUL` 终止（剪贴板格式要求）。
pub(crate) fn utf16_with_nul(text: &str) -> Vec<u16> {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0);
    units
}

fn clipboard_error(stage: &'static str, error: windows::core::Error) -> EngineError {
    EngineError::Clipboard {
        detail: smol_str::format_smolstr!("{stage}: {:#010X}", error.code().0 as u32),
    }
}

#[cfg(test)]
mod tests {
    use super::utf16_with_nul;

    #[test]
    fn empty_text_is_a_single_nul() {
        assert_eq!(utf16_with_nul(""), vec![0u16]);
    }

    #[test]
    fn ascii_and_cjk_are_encoded_as_utf16_with_a_terminator() {
        let units = utf16_with_nul("ab你好");

        assert_eq!(units.len(), 5, "4 个码元 + NUL");
        assert_eq!(units[0], u16::from(b'a'));
        assert_eq!(units[1], u16::from(b'b'));
        assert_eq!(units[2], '你' as u16);
        assert_eq!(units[3], '好' as u16);
        assert_eq!(units[4], 0);
    }

    #[test]
    fn astral_characters_take_a_surrogate_pair() {
        // 🌍 = U+1F30D = 2 个 UTF-16 码元。
        let units = utf16_with_nul("🌍");

        assert_eq!(units.len(), 3, "代理对 2 个码元 + NUL");
        assert_eq!(units[units.len() - 1], 0);
    }
}

//! 剪贴板（`CF_UNICODETEXT`；`design/phase-09-win32-demo.md` §4.5）。
//!
//! probe 实测：`OpenClipboard` → `EmptyClipboard` → `GlobalAlloc(GMEM_MOVEABLE)` → `SetClipboardData`
//! → `CloseClipboard` 往返逐字节相等（含中文与 emoji），空串（只有 `NUL`）也成立。
//! 富文本 / 图片 / 拖放属 P12。

use std::sync::atomic::{AtomicIsize, Ordering};

use smol_str::SmolStr;
use windows::Win32::{
    Foundation::{HANDLE, HGLOBAL, HWND},
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
        },
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
        Ole::CF_UNICODETEXT,
    },
};

use crate::error::EngineError;

/// 剪贴板 owner 窗口（`OpenClipboard` 需要一个 owner）。
///
/// 由平台循环在原生窗口建好后登记（`Application::run`）；`0` = 尚无窗口，
/// 此时 `set_text` / `get_text` 返回 `EngineError::Clipboard`（不静默失败）。
static OWNER: AtomicIsize = AtomicIsize::new(0);

/// 登记 owner 窗口句柄（平台层唯一写点）。
pub(crate) fn set_owner(hwnd: isize) {
    OWNER.store(hwnd, Ordering::Release);
}

fn owner() -> Result<HWND, EngineError> {
    let raw = OWNER.load(Ordering::Acquire);
    if raw == 0 {
        return Err(EngineError::Clipboard {
            detail: SmolStr::new_static("no native window; clipboard needs an owner"),
        });
    }
    Ok(HWND(raw as *mut core::ffi::c_void))
}

/// 写入剪贴板文本（`CF_UNICODETEXT`，UTF-16 + `NUL`；owner 来自 [`set_owner`]）。
///
/// # Errors
/// `EngineError::Clipboard`：剪贴板被其它进程占用（`OpenClipboard` 失败）或分配失败。
pub(crate) fn set_text(text: &str) -> Result<(), EngineError> {
    let hwnd = owner()?;
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
            // `SetClipboardData` 失败时剪贴板没有接管这块内存；`GlobalFree` 不在本 crate 的
            // windows feature 集内，这里只关剪贴板。泄漏仅在失败路径发生一次。
            let _ = CloseClipboard();
            return Err(clipboard_error("SetClipboardData", error));
        }
        CloseClipboard().map_err(|error| clipboard_error("CloseClipboard", error))?;
    }
    Ok(())
}

/// 读取剪贴板文本（无 `CF_UNICODETEXT` 时返回空串）。
///
/// **边界**：`CF_UNICODETEXT` 由**其它进程**写入，Windows 只保证「约定上以 NUL 结尾」。
/// 若块内没有 NUL，逐字扫描会越过块尾继续读 —— 未定义行为 / 访问违例。因此先用
/// `GlobalSize` 取可读字节上限，扫描范围夹在 `size / 2` 个 `u16` 之内。
///
/// 三种异常块的处理（明确契约，不是静默）：
/// - 奇数长度：按 `size / 2` 向下取整（尾部半字节不可读）；
/// - 空块：返回空串；
/// - 块内无 NUL：把整个块当作文本（与 Chrome / Qt 的「按长度取数据」一致），不报错。
///
/// # Errors
/// `EngineError::Clipboard`：剪贴板被占用或内存映射失败。
pub(crate) fn get_text() -> Result<SmolStr, EngineError> {
    let hwnd = owner()?;
    // SAFETY: 句柄只在打开的剪贴板会话内使用；读取范围由 `GlobalSize` 显式限定。
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
        // 可读字节上限：**必须**取，否则下面的扫描没有边界（P1-2.10 的核心）。
        let size = GlobalSize(global);
        let capacity = size / size_of::<u16>();
        if capacity == 0 {
            let _ = CloseClipboard();
            return Ok(SmolStr::default());
        }
        let pointer = GlobalLock(global);
        if pointer.is_null() {
            let _ = CloseClipboard();
            return Err(EngineError::Clipboard {
                detail: SmolStr::new_static("GlobalLock returned null"),
            });
        }
        let units_ptr = pointer.cast::<u16>();
        let mut units = Vec::with_capacity(capacity.min(4096));
        for index in 0..capacity {
            let unit = *units_ptr.add(index);
            if unit == 0 {
                break;
            }
            units.push(unit);
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

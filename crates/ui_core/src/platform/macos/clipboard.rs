//! 剪贴板：`NSPasteboard` + `NSPasteboardTypeString`（`design/phase-12-macos-linux.md` §7.6）。

use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;
use smol_str::SmolStr;

use crate::error::EngineError;

/// 写剪贴板文本。
///
/// # Errors
/// `EngineError::Clipboard`：`setString:forType:` 返回 `false`。
pub(crate) fn set_text(text: &str) -> Result<(), EngineError> {
    let board = NSPasteboard::generalPasteboard();
    board.clearContents();
    // SAFETY: `NSPasteboardTypeString` 是 AppKit 的静态 `NSString` 常量（进程生命周期内有效）。
    let kind = unsafe { NSPasteboardTypeString };
    if board.setString_forType(&NSString::from_str(text), kind) {
        Ok(())
    } else {
        Err(EngineError::Clipboard {
            detail: smol_str::SmolStr::new_static("NSPasteboard setString:forType: returned false"),
        })
    }
}

/// 读剪贴板文本（没有文本内容时返回空串）。
///
/// # Errors
/// 当前实现不产生错误（`stringForType:` 为 `None` 时按空串处理，与 Win32 一致）。
pub(crate) fn get_text() -> Result<SmolStr, EngineError> {
    let board = NSPasteboard::generalPasteboard();
    // SAFETY: 同 `set_text`。
    let kind = unsafe { NSPasteboardTypeString };
    Ok(board
        .stringForType(kind)
        .map(|text| SmolStr::new(text.to_string()))
        .unwrap_or_default())
}

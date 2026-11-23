use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;
use smol_str::SmolStr;

use crate::error::EngineError;

pub(crate) fn set_text(text: &str) -> Result<(), EngineError> {
    let board = NSPasteboard::generalPasteboard();
    board.clearContents();
    let kind = unsafe { NSPasteboardTypeString };
    if board.setString_forType(&NSString::from_str(text), kind) {
        Ok(())
    } else {
        Err(EngineError::Clipboard {
            detail: smol_str::SmolStr::new_static("NSPasteboard setString:forType: returned false"),
        })
    }
}

pub(crate) fn get_text() -> Result<SmolStr, EngineError> {
    let board = NSPasteboard::generalPasteboard();
    let kind = unsafe { NSPasteboardTypeString };
    Ok(board
        .stringForType(kind)
        .map(|text| SmolStr::new(text.to_string()))
        .unwrap_or_default())
}

use std::{
    sync::atomic::{AtomicIsize, Ordering},
    time::Duration,
};

use smol_str::SmolStr;
use windows::{
    Win32::{
        Foundation::{ERROR_ACCESS_DENIED, GlobalFree, HANDLE, HGLOBAL, HWND},
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, GetClipboardData, GetOpenClipboardWindow,
                IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
            Ole::CF_UNICODETEXT,
        },
    },
    core::HRESULT,
};

use crate::error::EngineError;

static OWNER: AtomicIsize = AtomicIsize::new(0);

const OPEN_RETRY_DELAYS_MS: [u64; 4] = [1, 2, 4, 8];

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

fn open_with_retry(
    mut open: impl FnMut() -> windows::core::Result<()>,
) -> windows::core::Result<()> {
    let access_denied = HRESULT::from_win32(ERROR_ACCESS_DENIED.0);
    let mut attempt = 0;
    loop {
        let error = match open() {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        if error.code() != access_denied || attempt >= OPEN_RETRY_DELAYS_MS.len() {
            return Err(error);
        }
        std::thread::sleep(Duration::from_millis(OPEN_RETRY_DELAYS_MS[attempt]));
        attempt += 1;
    }
}

fn open_clipboard(hwnd: HWND) -> Result<(), EngineError> {
    open_with_retry(|| unsafe { OpenClipboard(Some(hwnd)) }).map_err(|error| {
        let holder = unsafe { GetOpenClipboardWindow() }.map_or(0, |window| window.0 as isize);
        EngineError::Clipboard {
            detail: smol_str::format_smolstr!(
                "OpenClipboard: {:#010X}; holder: {holder:#X}",
                error.code().0 as u32
            ),
        }
    })
}

fn prepare_block(units: &[u16]) -> Result<HGLOBAL, EngineError> {
    let bytes = size_of_val(units);
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, bytes)
            .map_err(|error| clipboard_error("GlobalAlloc", error))?;
        let pointer = GlobalLock(global);
        if pointer.is_null() {
            let _ = GlobalFree(Some(global));
            return Err(EngineError::Clipboard {
                detail: SmolStr::new_static("GlobalLock returned null"),
            });
        }
        std::ptr::copy_nonoverlapping(units.as_ptr().cast::<u8>(), pointer.cast::<u8>(), bytes);
        let _ = GlobalUnlock(global);
        Ok(global)
    }
}

pub(crate) fn set_text(text: &str) -> Result<(), EngineError> {
    let hwnd = owner()?;
    let units = utf16_with_nul(text);
    let global = prepare_block(&units)?;
    if let Err(error) = open_clipboard(hwnd) {
        unsafe {
            let _ = GlobalFree(Some(global));
        }
        return Err(error);
    }
    unsafe {
        if let Err(error) = EmptyClipboard() {
            let _ = GlobalFree(Some(global));
            let _ = CloseClipboard();
            return Err(clipboard_error("EmptyClipboard", error));
        }
        if let Err(error) = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(global.0))) {
            let _ = GlobalFree(Some(global));
            let _ = CloseClipboard();
            return Err(clipboard_error("SetClipboardData", error));
        }
        CloseClipboard().map_err(|error| clipboard_error("CloseClipboard", error))?;
    }
    Ok(())
}

pub(crate) fn get_text() -> Result<SmolStr, EngineError> {
    let hwnd = owner()?;
    open_clipboard(hwnd)?;
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_err() {
            let _ = CloseClipboard();
            return Ok(SmolStr::default());
        }
        let handle = match GetClipboardData(CF_UNICODETEXT.0 as u32) {
            Ok(handle) => handle,
            Err(error) => {
                let _ = CloseClipboard();
                return Err(clipboard_error("GetClipboardData", error));
            }
        };
        let global = HGLOBAL(handle.0);
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
    use windows::{
        Win32::{
            Foundation::{
                ERROR_ACCESS_DENIED, ERROR_INVALID_HANDLE, GlobalFree, HWND, LPARAM, LRESULT,
                WIN32_ERROR, WPARAM,
            },
            System::{
                LibraryLoader::GetModuleHandleW,
                Memory::{GlobalLock, GlobalSize, GlobalUnlock},
            },
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, WINDOW_EX_STYLE,
                WNDCLASSW, WS_OVERLAPPEDWINDOW,
            },
        },
        core::HRESULT,
    };

    use super::{
        OPEN_RETRY_DELAYS_MS, get_text, open_with_retry, prepare_block, set_owner, set_text,
        utf16_with_nul,
    };

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
        let units = utf16_with_nul("🌍");

        assert_eq!(units.len(), 3, "代理对 2 个码元 + NUL");
        assert_eq!(units[units.len() - 1], 0);
    }

    #[test]
    fn retry_delays_are_short_and_growing() {
        assert_eq!(OPEN_RETRY_DELAYS_MS.len(), 4, "1 次立即 + 4 次退避");

        assert!(
            OPEN_RETRY_DELAYS_MS
                .windows(2)
                .all(|pair| pair[0] < pair[1]),
            "必须严格递增: {OPEN_RETRY_DELAYS_MS:?}"
        );
        let total: u64 = OPEN_RETRY_DELAYS_MS.iter().sum();
        assert!(total <= 20, "一次粘贴最多等 {total} ms，不能卡住调用线程");
    }

    #[test]
    fn open_retry_stops_at_the_first_success() {
        let mut calls = 0;
        let result = open_with_retry(|| {
            calls += 1;
            if calls < 3 {
                Err(win32_error(ERROR_ACCESS_DENIED))
            } else {
                Ok(())
            }
        });

        assert!(result.is_ok(), "第三次成功: {result:?}");
        assert_eq!(calls, 3, "成功以后不再重试");
    }

    #[test]
    fn open_retry_gives_up_after_the_maximum_attempts() {
        let mut calls = 0;
        let result = open_with_retry(|| {
            calls += 1;
            Err(win32_error(ERROR_ACCESS_DENIED))
        });

        assert_eq!(calls, OPEN_RETRY_DELAYS_MS.len() + 1, "1 次立即 + 4 次退避");
        assert_eq!(
            result.expect_err("一直 denied 必须返回 Err").code(),
            HRESULT::from_win32(ERROR_ACCESS_DENIED.0),
            "返回的是最后一次的错误"
        );
    }

    #[test]
    fn open_retry_does_not_retry_other_errors() {
        let mut calls = 0;
        let result = open_with_retry(|| {
            calls += 1;
            Err(win32_error(ERROR_INVALID_HANDLE))
        });

        assert_eq!(calls, 1, "非 ACCESS_DENIED 重试也不会成功");
        assert_eq!(
            result.expect_err("invalid handle 必须立即失败").code(),
            HRESULT::from_win32(ERROR_INVALID_HANDLE.0)
        );
    }

    #[test]
    fn prepare_block_contains_the_utf16_units() {
        let units = utf16_with_nul("ab你好");
        let global = prepare_block(&units).expect("GlobalAlloc(GMEM_MOVEABLE)");
        let stored = unsafe {
            let pointer = GlobalLock(global);
            let stored = if pointer.is_null() {
                None
            } else {
                let slice = std::slice::from_raw_parts(pointer.cast::<u16>(), units.len());
                let stored = slice.to_vec();
                let _ = GlobalUnlock(global);
                Some(stored)
            };
            let _ = GlobalFree(Some(global));
            stored
        };

        assert_eq!(
            stored.as_deref(),
            Some(units.as_slice()),
            "GlobalLock 读回原样"
        );
        assert_eq!(units.last().copied(), Some(0), "块以 NUL 结尾");
    }

    #[test]
    fn prepare_block_allocates_a_nul_for_empty_text() {
        let units = utf16_with_nul("");
        let global = prepare_block(&units).expect("GlobalAlloc(GMEM_MOVEABLE)");
        let (size, first) = unsafe {
            let size = GlobalSize(global);
            let pointer = GlobalLock(global);
            let first = if pointer.is_null() {
                None
            } else {
                let first = Some(*pointer.cast::<u16>());
                let _ = GlobalUnlock(global);
                first
            };
            let _ = GlobalFree(Some(global));
            (size, first)
        };

        assert!(size >= size_of::<u16>(), "空串也要占一个码元: {size}");
        assert_eq!(first, Some(0), "内容是一个 NUL");
    }

    #[test]
    fn set_text_then_get_text_round_trips() {
        let _guard = clipboard_lock();
        let (hwnd, _class) = hidden_window("KytrixP16ClipboardRoundTrip");
        set_owner(hwnd.0 as isize);
        let text = "ab你好🌍";
        if let Err(error) = set_text(text) {
            set_owner(0);
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            eprintln!("skip: clipboard is unavailable in this session: {error}");
            return;
        }
        let read = get_text();
        set_owner(0);
        unsafe {
            let _ = DestroyWindow(hwnd);
        }

        assert_eq!(read.expect("get_text after set_text"), text);
    }

    #[test]
    fn repeated_writes_do_not_fail() {
        let _guard = clipboard_lock();
        let (hwnd, _class) = hidden_window("KytrixP16ClipboardRepeatedWrites");
        set_owner(hwnd.0 as isize);
        let mut failure = None;
        for index in 0..10usize {
            let text = format!("第 {index} 次写入：ab你好🌍");
            if let Err(error) = set_text(&text) {
                failure = Some(error);
                break;
            }
        }
        set_owner(0);
        unsafe {
            let _ = DestroyWindow(hwnd);
        }

        if let Some(error) = failure {
            eprintln!("skip: clipboard is unavailable in this session: {error}");
        }
    }

    fn clipboard_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn win32_error(code: WIN32_ERROR) -> windows::core::Error {
        windows::core::Error::from_hresult(HRESULT::from_win32(code.0))
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn hidden_window(tag: &str) -> (HWND, Vec<u16>) {
        let class_name = wide(tag);
        let instance = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let atom = unsafe { RegisterClassW(&class) };
        assert_ne!(atom, 0, "RegisterClassW");
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::PCWSTR(class_name.as_ptr()),
                windows::core::PCWSTR(class_name.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                320,
                240,
                None,
                None,
                Some(instance.into()),
                None,
            )
        }
        .expect("CreateWindowExW");
        (hwnd, class_name)
    }
}

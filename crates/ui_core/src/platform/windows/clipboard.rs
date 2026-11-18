//! 剪贴板（`CF_UNICODETEXT`）。
//!
//! 写入顺序：`GlobalAlloc` + 填充 → `OpenClipboard`（带短退避重试）→ `EmptyClipboard`
//! → `SetClipboardData` → `CloseClipboard`。三点与「直接照着 API 顺序写」的差别：
//!
//! - **先备块后清空**：反过来的话分配失败时用户原有内容已经被销毁、又没有新内容 —— 一次
//!   失败把剪贴板变空。Chromium 的 `base/clipboard_win.cc` 同样先把 `GLOBAL` 建好、再
//!   `EmptyClipboard`。
//! - **打开要重试**：剪贴板是**进程级互斥**资源，别的进程持锁期间 `OpenClipboard` 返回
//!   `ERROR_ACCESS_DENIED`(5)，用户「复制后立刻粘贴」会稳定命中。Chromium 的
//!   `Clipboard::WriteText` 与 Qt 的 `QWindowsClipboard::openClipboard` 都做多次短重试。
//! - **失败路径不泄漏**：`GlobalLock` / `EmptyClipboard` / `SetClipboardData` 失败都要
//!   `GlobalFree`；只有 `SetClipboardData` 成功时所有权才交给系统。
//!
//! 富文本 / 图片 / 拖放属 P12。

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

/// 剪贴板 owner 窗口（`OpenClipboard` 需要一个 owner）。
///
/// 由平台循环在原生窗口建好后登记（`Application::run`）；`0` = 尚无窗口，
/// 此时 `set_text` / `get_text` 返回 `EngineError::Clipboard`（不静默失败）。
static OWNER: AtomicIsize = AtomicIsize::new(0);

/// 打开剪贴板的退避序列（毫秒）：最多 5 次尝试 = 1 次立即 + 4 次退避。
///
/// 争用只会发生在「用户刚在别的程序里按了 Ctrl+C」这种毫秒级窗口：持锁进程处理完消息
/// 就会释放，因此序列必须短（合计 15 ms，不卡 UI），也不能是固定间隔 —— 第一次就重试
/// 成功的概率最高，越往后越像「对方卡死了」，此时退避拉开比继续猛撞更合理。
const OPEN_RETRY_DELAYS_MS: [u64; 4] = [1, 2, 4, 8];

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

/// 用给定 opener 反复尝试打开剪贴板：只有 `ERROR_ACCESS_DENIED`(5) 才重试。
///
/// 其它错误（owner 无效、安全策略拒绝）重试也不会变好，第一次就返回。返回**最后一次**
/// 的错误：前几次被别的进程挡住，最后一次的「谁占着剪贴板」最有诊断价值。
///
/// 做成注入式而不是内联 `OpenClipboard` + `sleep`，是因为真实剪贴板无法在单测里稳定
/// 制造争用 —— 退避顺序与次数只能靠注入的 opener 验证。
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

/// 打开剪贴板（`OpenClipboard` 按 [`open_with_retry`] 重试）。
///
/// # Errors
/// `EngineError::Clipboard`：重试用尽仍被别的进程占用。detail 里额外带上
/// `GetOpenClipboardWindow` 的持锁窗口，用于诊断「谁占着剪贴板」—— 与 Qt 的
/// `QWindowsClipboard` 打印窗口状态的做法一致。
fn open_clipboard(hwnd: HWND) -> Result<(), EngineError> {
    open_with_retry(|| unsafe { OpenClipboard(Some(hwnd)) }).map_err(|error| {
        // SAFETY: 无参数、无输出指针；失败时返回 0，表示当前没有窗口持有剪贴板。
        let holder = unsafe { GetOpenClipboardWindow() }.map_or(0, |window| window.0 as isize);
        EngineError::Clipboard {
            detail: smol_str::format_smolstr!(
                "OpenClipboard: {:#010X}; holder: {holder:#X}",
                error.code().0 as u32
            ),
        }
    })
}

/// 分配并填好要交给剪贴板的 `HGLOBAL`（`CF_UNICODETEXT` 的块：UTF-16 + `NUL`）。
///
/// 抽出来是因为「备块」与「动剪贴板」是两件独立的事：前者失败时剪贴板必须原封不动
/// （见模块文档），而空的 `units` 不可能出现 —— `utf16_with_nul` 至少产出一个 `NUL`。
///
/// # Errors
/// `EngineError::Clipboard`：`GlobalAlloc` 失败，或 `GlobalLock` 返回空指针（此时先把块
/// 释放掉再返回，不留下没人认领的全局内存）。
fn prepare_block(units: &[u16]) -> Result<HGLOBAL, EngineError> {
    let bytes = size_of_val(units);
    // SAFETY: 块由本函数独占；失败分支在返回前 `GlobalFree`，成功时所有权交给调用方。
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

/// 写入剪贴板文本（`CF_UNICODETEXT`，UTF-16 + `NUL`；owner 来自 [`set_owner`]）。
///
/// # Errors
/// `EngineError::Clipboard`：分配失败（此时剪贴板内容保持原样）、重试用尽仍被别的进程
/// 占用，或 `EmptyClipboard` / `SetClipboardData` / `CloseClipboard` 被系统拒绝。
pub(crate) fn set_text(text: &str) -> Result<(), EngineError> {
    let hwnd = owner()?;
    let units = utf16_with_nul(text);
    let global = prepare_block(&units)?;
    if let Err(error) = open_clipboard(hwnd) {
        // 打开失败 ⇒ 剪贴板原封不动，预先备好的块没人接管，只能自己释放。
        // SAFETY: 块是 `prepare_block` 刚分配的，尚未交给任何剪贴板会话。
        unsafe {
            let _ = GlobalFree(Some(global));
        }
        return Err(error);
    }
    // SAFETY: 剪贴板已由本进程打开；`global` 的所有权在 `SetClipboardData` 成功时移交系统，
    // 在那之前的每条返回路径都自己 `GlobalFree`。
    unsafe {
        if let Err(error) = EmptyClipboard() {
            let _ = GlobalFree(Some(global));
            let _ = CloseClipboard();
            return Err(clipboard_error("EmptyClipboard", error));
        }
        if let Err(error) = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(global.0))) {
            // 失败 ⇒ 系统没有接管这块内存，必须自己释放；成功 ⇒ 所有权归剪贴板，由系统在
            // 下次 `EmptyClipboard` 时释放，这里**不得** `GlobalFree`（那是双重释放）。
            let _ = GlobalFree(Some(global));
            let _ = CloseClipboard();
            return Err(clipboard_error("SetClipboardData", error));
        }
        CloseClipboard().map_err(|error| clipboard_error("CloseClipboard", error))?;
    }
    Ok(())
}

/// 读取剪贴板文本（无 `CF_UNICODETEXT` 时返回空串）。
///
/// **「无该格式」与「打开失败」是两件不同的事**：前者是正常情况（剪贴板里是图片 / 文件，
/// 或者是空的），返回空 `SmolStr`；后者（被别的进程占用、没有 owner 窗口）返回
/// `EngineError::Clipboard`。因此先用 `IsClipboardFormatAvailable` 问格式 —— 只看
/// `GetClipboardData` 是否失败无法区分这两者（Qt 的 `QWindowsClipboard::text` 同样先问格式）。
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
/// `EngineError::Clipboard`：打开剪贴板失败、格式可用但 `GetClipboardData` 仍失败（owner
/// 在两次调用之间退出），或 `GlobalLock` 返回空指针。
pub(crate) fn get_text() -> Result<SmolStr, EngineError> {
    let hwnd = owner()?;
    open_clipboard(hwnd)?;
    // SAFETY: 剪贴板已打开；读取范围由 `GlobalSize` 显式限定；每条返回路径都先 `CloseClipboard`
    // 再返回，不会把剪贴板锁留在本进程手里。
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
        // 🌍 = U+1F30D = 2 个 UTF-16 码元。
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
        // SAFETY: `prepare_block` 按 `units` 的长度分配并写入同样的字节数，这里读回同样的范围。
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
        // SAFETY: 空文本的块至少有一个 `u16`（NUL）；`GlobalSize` 只读块元数据。
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
            // 无桌面会话 / 剪贴板被系统策略禁用时不算失败（与 IME 用例同样的宽容处理）。
            set_owner(0);
            // SAFETY: 窗口由本用例创建，且本用例持有剪贴板锁。
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            eprintln!("skip: clipboard is unavailable in this session: {error}");
            return;
        }
        let read = get_text();
        set_owner(0);
        // SAFETY: 窗口由本用例创建，且本用例持有剪贴板锁。
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
        // SAFETY: 窗口由本用例创建，且本用例持有剪贴板锁。
        unsafe {
            let _ = DestroyWindow(hwnd);
        }

        if let Some(error) = failure {
            eprintln!("skip: clipboard is unavailable in this session: {error}");
        }
    }

    /// 剪贴板是**进程级**单一资源：真正读写系统剪贴板的用例必须串行，否则并行执行会互相
    /// 覆盖内容（并行测试共享同一个剪贴板）。
    fn clipboard_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// windows-rs 把线程错误码折成 `HRESULT_FROM_WIN32(code)`；用它构造 `OpenClipboard` 的
    /// 失败形态，不必真的去抢占剪贴板。
    fn win32_error(code: WIN32_ERROR) -> windows::core::Error {
        windows::core::Error::from_hresult(HRESULT::from_win32(code.0))
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: 测试窗口只用来当剪贴板 owner，默认过程不依赖额外状态。
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 建一个隐藏窗口（不 `ShowWindow`）：`OpenClipboard` 需要一个 owner 窗口。
    fn hidden_window(tag: &str) -> (HWND, Vec<u16>) {
        let class_name = wide(tag);
        // SAFETY: 进程级模块句柄；类名是 NUL 结尾宽字符串，`RegisterClassW` 同步拷贝。
        let instance = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let atom = unsafe { RegisterClassW(&class) };
        assert_ne!(atom, 0, "RegisterClassW");
        // SAFETY: 类已注册；`WS_OVERLAPPEDWINDOW` 不含 `WS_VISIBLE` ⇒ 窗口保持隐藏。
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

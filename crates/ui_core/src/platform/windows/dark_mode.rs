use std::{
    mem::size_of,
    sync::atomic::{AtomicBool, Ordering},
};

use windows::{
    Win32::{
        Foundation::HWND,
        Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute},
        System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    },
    core::BOOL,
};

pub(crate) fn current() -> Option<bool> {
    let subkey = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let value = wide("AppsUseLightTheme");
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(subkey.as_ptr()),
            windows::core::PCWSTR(value.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut data as *mut u32).cast()),
            Some(&mut size),
        )
    };
    if status != windows::Win32::Foundation::ERROR_SUCCESS {
        return None;
    }
    Some(data == 0)
}

pub(crate) fn set_titlebar(hwnd: HWND, dark: bool) -> bool {
    let value = BOOL::from(dark);
    let result = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&raw const value).cast(),
            size_of::<BOOL>() as u32,
        )
    };
    if let Err(error) = result {
        if !WINDOW_ATTRIBUTE_FAILURE_REPORTED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "kytrix: DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE={dark}) failed: \
                 {error:?}; the titlebar keeps the system default color (window remains usable)"
            );
        }
        return false;
    }
    true
}

static WINDOW_ATTRIBUTE_FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use windows::{
        Win32::{
            Foundation::HWND,
            Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmGetWindowAttribute},
        },
        core::BOOL,
    };

    use super::{current, set_titlebar};
    use crate::{
        platform::{dispatcher::Dispatcher, windows::window::NativeWindow},
        window::{WindowId, WindowOptions},
    };

    fn read_titlebar(hwnd: HWND) -> bool {
        let mut value = BOOL::default();
        unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                (&raw mut value).cast(),
                size_of::<BOOL>() as u32,
            )
        }
        .expect("DwmGetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)");
        value.as_bool()
    }

    fn hidden_window() -> (NativeWindow, HWND) {
        let options = WindowOptions::default();
        let native = NativeWindow::create(
            WindowId::for_test(1),
            &options,
            None,
            Dispatcher::new(),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
        )
        .expect("NativeWindow::create");
        let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
        (native, hwnd)
    }

    #[test]
    fn a_real_window_titlebar_follows_the_system_theme_and_the_setter() {
        let (_native, hwnd) = hidden_window();

        let system_dark = current();
        assert_eq!(
            read_titlebar(hwnd),
            system_dark.unwrap_or(false),
            "建窗必须先应用系统主题（注册表读取 = {system_dark:?}）"
        );

        for dark in [true, true, false, true] {
            assert!(
                set_titlebar(hwnd, dark),
                "Win11 隐藏窗口上设置深色标题栏必须成功"
            );
            assert_eq!(
                read_titlebar(hwnd),
                dark,
                "DwmGetWindowAttribute 必须回读到刚设置的值"
            );
        }
        assert!(set_titlebar(hwnd, system_dark.unwrap_or(false)));
    }

    #[test]
    #[ignore = "真机证据（.review/p1-b-win-b2.log）：需要真实隐藏窗口 + DWM"]
    fn evidence_dark_titlebar_round_trips_on_a_real_window() {
        let system_dark = current();
        println!("HKCU AppsUseLightTheme -> dark_mode::current() = {system_dark:?}");
        let (_native, hwnd) = hidden_window();
        println!(
            "NativeWindow::create 后 DwmGetWindowAttribute 回读 = {}",
            read_titlebar(hwnd)
        );
        for dark in [true, false, true] {
            let applied = set_titlebar(hwnd, dark);
            let observed = read_titlebar(hwnd);
            println!("set_titlebar({dark}) -> {applied}; DwmGetWindowAttribute 回读 = {observed}");
            assert!(applied, "set_titlebar({dark}) 必须成功");
            assert_eq!(observed, dark, "DWM 回读必须跟随显式设置");
        }
        let restored = system_dark.unwrap_or(false);
        println!("恢复系统主题值 set_titlebar({restored})");
        assert!(set_titlebar(hwnd, restored));
    }
}

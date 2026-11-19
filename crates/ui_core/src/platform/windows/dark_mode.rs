//! Win11 深色标题栏：系统主题读取（`HKCU\…\Themes\Personalize`）与
//! `DWMWA_USE_IMMERSIVE_DARK_MODE` 的应用。
//!
//! ## 为什么在平台层做（报告 §3.2 B2 / 用户裁决 D2）
//!
//! 窗口是系统标题栏的 `WS_OVERLAPPEDWINDOW`，客户区由 DirectComposition 自绘；标题栏明暗
//! 完全由 DWM 属性决定。若不设置，系统主题为深色时标题栏仍是亮色（应用内容深色 + 亮标题栏）。
//! 应用层拿不到 HWND（引擎拥有原生窗口），也没有主题 API，所以在平台层自己跟随：
//!
//! - 建窗后、**显示之前**按当前系统主题设置一次（首帧不闪亮条）；
//! - `WM_SETTINGCHANGE("ImmersiveColorSet")` 时重新读注册表并同步（见 `wndproc`）。
//!
//! 与 Qt `QWindowsWindow` 在 `WM_SETTINGCHANGE` 里同步窗口边框属性的做法同构；Chrome /
//! Electron 同样由平台层决定标题栏明暗，应用不参与。
//!
//! ## 范围：**只做深色标题栏**
//!
//! 不设置 `DWMWA_WINDOW_CORNER_PREFERENCE`（Win11 对 `WS_OVERLAPPEDWINDOW` 顶层窗口默认
//! 带圆角，显式设置没有收益）与 Mica/backdrop（客户区全部被 DComp 内容覆盖，云母不可见）。
//! 报告 §13.1 已确认原审查「还缺圆角/云母」的说法不成立。
//!
//! ## 失败策略
//!
//! `DwmSetWindowAttribute` 失败（DWM 异常、远程会话的合成器变体）⇒ 返回 `false` + 调试诊断，
//! **不 panic、不影响窗口创建与交互**：标题栏回落到系统默认亮色。Windows 11 门禁
//! （[`super::version`]）保证属性号 20 合法，不需要 Win10 的 19/20 双属性号兼容。

use std::mem::size_of;

use windows::{
    Win32::{
        Foundation::HWND,
        Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute},
        System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    },
    core::BOOL,
};

/// 读系统主题（`AppsUseLightTheme`：1 = 浅色，0 = 深色）→ 是否深色。
///
/// `None` = 注册表值不可读（键不存在 / 值类型异常 / 权限）。调用方按**浅色**处理
/// （与 DWM 属性的默认值一致，也是 Windows 的出厂默认）。
pub(crate) fn current() -> Option<bool> {
    let subkey = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let value = wide("AppsUseLightTheme");
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: 输出缓冲是栈上的 `u32`，`size` 同步给出字节数；键路径是 NUL 结尾的宽字符串。
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

/// 设置窗口标题栏明暗（`DWMWA_USE_IMMERSIVE_DARK_MODE`）；返回是否成功应用。
///
/// 隐藏窗口上设置是标准做法（属性在显示时由 DWM 消费），因此建窗路径可以在 `ShowWindow`
/// 之前调用。`DwmSetWindowAttribute` 只改 DWM 属性、**不派发窗口消息** —— 可以在
/// `WndProc` 里（包括持有 `&mut WndProcState` 的短借用内）调用而不违反「不得重入」纪律。
pub(crate) fn set_titlebar(hwnd: HWND, dark: bool) -> bool {
    let value = BOOL::from(dark);
    // SAFETY: `hwnd` 由调用方保证存活；属性值是指向栈上 `BOOL` 的只读指针，长度 = 4 字节。
    let result = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&raw const value).cast(),
            size_of::<BOOL>() as u32,
        )
    };
    if let Err(error) = result {
        // 不 panic：DWM 不可用（远程会话 / 合成器变体）时窗口仍必须可用，标题栏退回系统默认。
        // 调试构建下断言失败点，便于真机测试立刻暴露「属性号写错 / DWM 异常」。
        debug_assert!(
            false,
            "DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE={dark}) 失败：{error:?}"
        );
        return false;
    }
    true
}

/// 消费者：`current` / 注册表键名。
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

    /// 从 DWM 回读标题栏明暗（测试/证据用：`set_titlebar` 是只写接口）。
    fn read_titlebar(hwnd: HWND) -> bool {
        let mut value = BOOL::default();
        // SAFETY: `hwnd` 由调用方保证存活；输出是指向栈上 `BOOL` 的可写指针，长度 = 4 字节。
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
        let native = NativeWindow::create(WindowId::for_test(1), &options, Dispatcher::new(), None)
            .expect("NativeWindow::create");
        let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
        (native, hwnd)
    }

    /// B2：建窗时按当前系统主题设置标题栏（首帧不闪亮条）；显式设置两种值后 DWM 回读一致，
    /// 且重复设置幂等。
    ///
    /// 变异守卫：删掉 `NativeWindow::create` 的 `set_titlebar` 调用 ⇒ 系统为深色主题时
    /// 「建窗回读 == 系统主题」变红（DWM 默认值是浅色）；把 `set_titlebar` 改成只返回 `true`
    /// 不真正调用 DWM ⇒ 显式设置的两次回读断言变红。
    #[test]
    fn a_real_window_titlebar_follows_the_system_theme_and_the_setter() {
        let (_native, hwnd) = hidden_window();

        // 建窗路径：`create` 已经按当前系统主题设置过一次（首帧不闪亮条）。
        let system_dark = current();
        assert_eq!(
            read_titlebar(hwnd),
            system_dark.unwrap_or(false),
            "建窗必须先应用系统主题（注册表读取 = {system_dark:?}）"
        );

        // 显式设置两种值：DWM 属性必须真的生效（回读），重复设置幂等。
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
        // 恢复成系统主题，避免测试窗口销毁前的残留状态影响其它用例（无实际必需，语义自洽）。
        assert!(set_titlebar(hwnd, system_dark.unwrap_or(false)));
    }

    /// 真机证据（`#[ignore]`）：显式设置两种标题栏明暗 → DWM 回读一致，并记录系统主题来源。
    ///
    /// 运行：
    /// `cargo test -p ui_core --features "hud,test-support" --lib platform::windows::dark_mode -- --ignored --nocapture`
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

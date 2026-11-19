//! Windows 版本门禁：ui_core 只支持 Windows 11（build 22000+）。
//!
//! `DCompositionWaitForCompositorClock` 是 Win11 才有的导出。本 crate 不承诺 Windows 10：
//! 启动时用 `RtlGetVersion`（与 Chromium `base::win::GetVersion` 同源，不受 manifest
//! 兼容性 shim 影响）检查 build 号，低于 22000 直接返回可读的 `EngineError::AppInit`，
//! 而不是让进程在加载期以 `STATUS_ENTRYPOINT_NOT_FOUND` 静默死亡。
//!
//! `RtlGetVersion` 走 `GetProcAddress` 动态解析：这样即使某台机器上的 `ntdll` 异常缺失该
//! 导出，也只会走到「版本检查失败」分支，而不会把静态导入失败提前到进程加载期。

use std::{mem::size_of, sync::OnceLock};

use windows::{
    Win32::System::{
        LibraryLoader::{GetProcAddress, LoadLibraryW},
        SystemInformation::OSVERSIONINFOW,
    },
    core::{s, w},
};

use crate::error::EngineError;

type RtlGetVersionFn = unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32;

/// 进程级缓存：`ntdll` 是系统 DLL，解析一次后不需要释放（进程生命周期内常驻）。
static RTL_GET_VERSION: OnceLock<Option<RtlGetVersionFn>> = OnceLock::new();

fn rtl_get_version() -> Option<RtlGetVersionFn> {
    *RTL_GET_VERSION.get_or_init(|| {
        // SAFETY: `LoadLibraryW` 只加载系统 DLL；返回值用于 `GetProcAddress`，不解除引用。
        let module = unsafe { LoadLibraryW(w!("ntdll.dll")) }.ok()?;
        // SAFETY: `module` 是上面加载的有效模块；`s!` 是 NUL 结尾的静态字节串。
        let proc = unsafe { GetProcAddress(module, s!("RtlGetVersion")) };
        // FARPROC 是 `Option<unsafe extern "system" fn() -> isize>`；签名由 ntddk.h 保证。
        proc.map(|proc| unsafe { std::mem::transmute::<_, RtlGetVersionFn>(proc) })
    })
}

/// 判定一个 `(major, build)` 是否满足 Windows 11（build 22000+）。
///
/// 纯函数，边界值可单测：Windows 10 22H2 是 build 19045，Server 2022 是 build 20348，
/// 两者都必须被拒；Win11 21H2 正是 build 22000。
fn satisfies(major: u32, build: u32) -> bool {
    major >= 10 && build >= 22_000
}

/// 启动门禁的纯判据（可注入任意版本号测试拒绝路径）。
fn gate(major: u32, minor: u32, build: u32) -> Result<(), EngineError> {
    if satisfies(major, build) {
        return Ok(());
    }
    Err(EngineError::AppInit {
        detail: smol_str::format_smolstr!(
            "ui_core requires Windows 11 build 22000+; current Windows {major}.{minor}.{build}"
        ),
    })
}

/// 启动门禁：不满足 Windows 11 build 22000+ 时返回带实际版本的可读错误。
pub(crate) fn require_windows11() -> Result<(), EngineError> {
    let (major, minor, build) = version_numbers();
    gate(major, minor, build)
}

/// 诊断用版本号（读不到时全 0）。
fn version_numbers() -> (u32, u32, u32) {
    let Some(rtl_get_version) = rtl_get_version() else {
        return (0, 0, 0);
    };
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..OSVERSIONINFOW::default()
    };
    // SAFETY: `info` 是栈上、按 276 B `RTL_OSVERSIONINFOW` 布局初始化的输出参数。
    if unsafe { rtl_get_version(&mut info) } == 0 {
        (info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber)
    } else {
        (0, 0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::{gate, require_windows11, satisfies, version_numbers};
    use crate::error::EngineError;

    /// 门禁判据的边界：任何阈值/比较方向的改动都必须让这条变红（不依赖本机版本）。
    #[test]
    fn the_build_threshold_rejects_windows_10_and_server_2022() {
        assert!(satisfies(10, 22_000), "Win11 21H2 必须通过");
        assert!(satisfies(10, 26_100), "Win11 24H2 必须通过");
        assert!(!satisfies(10, 21_999), "build 21999 不是 Win11");
        assert!(!satisfies(10, 19_045), "Win10 22H2 必须被拒");
        assert!(!satisfies(10, 20_348), "Server 2022 必须被拒");
        assert!(!satisfies(6, 22_000), "版本号 major < 10 必须被拒");
    }

    /// 拒绝路径：Win10 必须得到可读的 `EngineError::AppInit`，且 detail 带实际版本号。
    ///
    /// 变异守卫：把 `gate` 改成恒 `Ok(())`（门禁恒放行）⇒ 本条变红；这是
    /// `the_version_gate_matches_the_reported_build_number` 在 Win11 上测不到的分支。
    #[test]
    fn the_gate_rejects_windows_10_with_a_readable_error() {
        let error = gate(10, 0, 19_045).expect_err("Win10 必须被拒绝");
        let EngineError::AppInit { detail } = &error else {
            panic!("门禁必须返回 AppInit，实际 {error:?}");
        };
        assert!(
            detail.contains("Windows 11") && detail.contains("19045"),
            "错误详情必须同时说明目标版本与实际版本：{detail}"
        );

        let error = gate(10, 0, 20_348).expect_err("Server 2022 必须被拒绝");
        assert!(matches!(error, EngineError::AppInit { .. }));
        let error = gate(6, 1, 7_600).expect_err("Win7 必须被拒绝");
        assert!(matches!(error, EngineError::AppInit { .. }));

        assert!(gate(10, 0, 22_000).is_ok(), "Win11 21H2 必须放行");
        assert!(gate(10, 0, 26_100).is_ok(), "Win11 24H2 必须放行");
    }

    /// 依赖复用的 `OSVERSIONINFOW` 布局：`RtlGetVersion` 只接受 276 B 的 RTL 结构。
    #[test]
    fn the_os_version_info_layout_is_the_rtl_contract() {
        assert_eq!(
            std::mem::size_of::<windows::Win32::System::SystemInformation::OSVERSIONINFOW>(),
            276
        );
    }

    #[test]
    fn the_version_gate_matches_the_reported_build_number() {
        let (major, _, build) = version_numbers();
        assert_eq!(
            require_windows11().is_ok(),
            satisfies(major, build),
            "gate must be derived from the actual RtlGetVersion result ({major}.{build})"
        );
    }

    /// 宿主成功路径：本仓只支持 Win11，测试机必须是 Win11 且门禁必须真实放行。
    ///
    /// 变异守卫：把 `version_numbers` 的 major/build 字段映射互换（major 变成 build 数）
    /// 时，本条的 `major == 10` 会立刻变红。
    #[test]
    fn the_gate_accepts_this_windows_11_host() {
        let (major, minor, build) = version_numbers();
        assert_eq!(
            major, 10,
            "RtlGetVersion major 字段映射错误：{major}.{minor}.{build}"
        );
        assert!(
            build >= 22_000,
            "测试机必须是 Windows 11：{major}.{minor}.{build}"
        );
        assert!(
            require_windows11().is_ok(),
            "Windows 11 宿主必须通过门禁：{major}.{minor}.{build}"
        );
    }
}

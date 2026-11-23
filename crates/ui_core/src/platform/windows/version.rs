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

static RTL_GET_VERSION: OnceLock<Option<RtlGetVersionFn>> = OnceLock::new();

fn rtl_get_version() -> Option<RtlGetVersionFn> {
    *RTL_GET_VERSION.get_or_init(|| {
        let module = unsafe { LoadLibraryW(w!("ntdll.dll")) }.ok()?;
        let proc = unsafe { GetProcAddress(module, s!("RtlGetVersion")) };
        proc.map(|proc| unsafe { std::mem::transmute::<_, RtlGetVersionFn>(proc) })
    })
}

fn satisfies(major: u32, build: u32) -> bool {
    major >= 10 && build >= 22_000
}

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

pub(crate) fn require_windows11() -> Result<(), EngineError> {
    let (major, minor, build) = version_numbers();
    gate(major, minor, build)
}

fn version_numbers() -> (u32, u32, u32) {
    let Some(rtl_get_version) = rtl_get_version() else {
        return (0, 0, 0);
    };
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..OSVERSIONINFOW::default()
    };
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

    #[test]
    fn the_build_threshold_rejects_windows_10_and_server_2022() {
        assert!(satisfies(10, 22_000), "Win11 21H2 必须通过");
        assert!(satisfies(10, 26_100), "Win11 24H2 必须通过");
        assert!(!satisfies(10, 21_999), "build 21999 不是 Win11");
        assert!(!satisfies(10, 19_045), "Win10 22H2 必须被拒");
        assert!(!satisfies(10, 20_348), "Server 2022 必须被拒");
        assert!(!satisfies(6, 22_000), "版本号 major < 10 必须被拒");
    }

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

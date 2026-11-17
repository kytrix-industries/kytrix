//! DXGI `HRESULT` 的**分类**（纯函数，无 COM 调用）：错误码 → 后端要执行的动作。
//!
//! 这个模块存在的理由（R08-3 / R08-7）：DXGI 的失败语义分成三类，混用会造成
//! 「可恢复瞬态 → 粘性设备丢失」这类致命误判：
//!
//! | 类别 | 例子 | 后端动作 |
//! | --- | --- | --- |
//! | 时序（正 HRESULT，`S_FALSE` 家族） | `DXGI_STATUS_OCCLUDED`、`MODE_CHANGED` | 不是错误：退避 / 重建，本帧算「未呈现」 |
//! | 可恢复调用错误 | `DXGI_ERROR_INVALID_CALL`、`OUT_OF_DATE` | 整帧重录一次（不是换 present 参数）/ 重建交换链 |
//! | 设备级错误 | `DEVICE_REMOVED` / `HUNG` / `RESET` / `DRIVER_INTERNAL_ERROR` | 唯一可以置**粘性** `DeviceLost` 的判据 |
//!
//! `windows` crate 只提供了一部分 DXGI 错误常量（没有 `DXGI_STATUS_*`），且本后端需要在
//! `GetDeviceRemovedReason` 与 `Present1` 两处用同一套判据，因此常量集中在这里、以
//! `dxgi.h` 的 `MAKE_DXGI_STATUS` / `MAKE_DXGI_HRESULT` 编码为准。

use windows::core::HRESULT;

/// `MAKE_DXGI_HRESULT(n)`（`SEVERITY_ERROR`，facility `0x87a`）。
const fn dxgi_error(code: u32) -> HRESULT {
    HRESULT((0x887A_0000u32 | code) as i32)
}

/// `MAKE_DXGI_STATUS(n)`（`SEVERITY_SUCCESS`，facility `0x87a`）。
const fn dxgi_status(code: u32) -> HRESULT {
    HRESULT((0x087A_0000u32 | code) as i32)
}

/// `DXGI_ERROR_INVALID_CALL`：参数/状态非法（本后端最常见的来源是越界 dirty rect）。
pub(crate) const DXGI_ERROR_INVALID_CALL: HRESULT = dxgi_error(0x0001);
/// `DXGI_ERROR_DEVICE_REMOVED`。
pub(crate) const DXGI_ERROR_DEVICE_REMOVED: HRESULT = dxgi_error(0x0005);
/// `DXGI_ERROR_DEVICE_HUNG`。
pub(crate) const DXGI_ERROR_DEVICE_HUNG: HRESULT = dxgi_error(0x0006);
/// `DXGI_ERROR_DEVICE_RESET`。
pub(crate) const DXGI_ERROR_DEVICE_RESET: HRESULT = dxgi_error(0x0007);
/// `DXGI_ERROR_DEVICE_RESET` 之外的驱动内部错误（TDR 前后的常见返回值）。
pub(crate) const DXGI_ERROR_DRIVER_INTERNAL_ERROR: HRESULT = dxgi_error(0x0020);
/// `DXGI_STATUS_OCCLUDED`：**正** HRESULT，内容未上屏但设备健康。
pub(crate) const DXGI_STATUS_OCCLUDED: HRESULT = dxgi_status(0x0001);
/// `DXGI_STATUS_CLIPPED`：被窗口裁剪，同样不是错误。
pub(crate) const DXGI_STATUS_CLIPPED: HRESULT = dxgi_status(0x0002);
/// `DXGI_STATUS_MODE_CHANGED`：显示模式变了，需要重建交换链。
pub(crate) const DXGI_STATUS_MODE_CHANGED: HRESULT = dxgi_status(0x0007);
/// `DXGI_STATUS_MODE_CHANGE_IN_PROGRESS`：模式切换进行中，本帧未呈现。
pub(crate) const DXGI_STATUS_MODE_CHANGE_IN_PROGRESS: HRESULT = dxgi_status(0x0008);

/// 是否是「设备真的没了」类错误 —— **唯一**允许升级成粘性 `DeviceLost` 的判据。
pub(crate) fn is_device_removal(hr: HRESULT) -> bool {
    let code = hr.0 as u32;
    [
        DXGI_ERROR_DEVICE_REMOVED.0 as u32,
        DXGI_ERROR_DEVICE_HUNG.0 as u32,
        DXGI_ERROR_DEVICE_RESET.0 as u32,
        DXGI_ERROR_DRIVER_INTERNAL_ERROR.0 as u32,
    ]
    .contains(&code)
}

/// 设备被移除的原因短名（日志 / 错误文案；未知码返回 `"unknown"`）。
pub(crate) fn removal_reason_label(hr: HRESULT) -> &'static str {
    let code = hr.0 as u32;
    if code == DXGI_ERROR_DEVICE_REMOVED.0 as u32 {
        "DEVICE_REMOVED"
    } else if code == DXGI_ERROR_DEVICE_HUNG.0 as u32 {
        "DEVICE_HUNG"
    } else if code == DXGI_ERROR_DEVICE_RESET.0 as u32 {
        "DEVICE_RESET"
    } else if code == DXGI_ERROR_DRIVER_INTERNAL_ERROR.0 as u32 {
        "DRIVER_INTERNAL_ERROR"
    } else {
        "unknown"
    }
}

/// `Present1` 的返回值分类（R08-7）。
///
/// 关键点：**正 HRESULT**（遮挡 / 模式变化 / 裁剪）不是失败 —— 它们表示「这一帧没有正常
/// 上屏」，调用方按各自的语义退避或重建，绝不能当致命错误处理（旧实现用 `hr.is_ok()`
/// 把遮挡吞成成功、把其余一切当致命，两个方向都错）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentOutcome {
    /// 已提交给 DWM。
    Presented,
    /// 被遮挡（正 HRESULT）：GPU 线程应当停帧并等可见性恢复，不重试。
    Occluded,
    /// 被裁剪（正 HRESULT）：内容仍然有效，按已呈现处理但记一次诊断。
    Clipped,
    /// 显示模式变化（正 HRESULT）：需要重建交换链。
    ModeChanged,
    /// `DXGI_ERROR_INVALID_CALL`：**不要**换成「整面 dirty」再试（增量帧的 back buffer 只同步了
    /// 一部分，整面 present 会把未同步区域上屏，AR3 P2-4）；后端必须整帧重录
    /// （`compositor::timings::invalid_call_recovery`）。
    InvalidCall,
    /// 设备级错误：只有这一类可以进设备丢失路径。
    DeviceLost,
    /// 其它 **负** HRESULT：分类保留，由调用方决定是否致命。
    Failed,
}

/// `Present1` 的 HRESULT → [`PresentOutcome`]。
pub(crate) fn classify_present(hr: HRESULT) -> PresentOutcome {
    if hr == HRESULT(0) {
        return PresentOutcome::Presented;
    }
    if hr.is_ok() {
        // 正 HRESULT：`SEVERITY_SUCCESS`。逐个识别已知语义，其余按「未呈现」处理。
        return match hr.0 as u32 {
            x if x == DXGI_STATUS_OCCLUDED.0 as u32 => PresentOutcome::Occluded,
            x if x == DXGI_STATUS_CLIPPED.0 as u32 => PresentOutcome::Clipped,
            x if x == DXGI_STATUS_MODE_CHANGED.0 as u32
                || x == DXGI_STATUS_MODE_CHANGE_IN_PROGRESS.0 as u32 =>
            {
                PresentOutcome::ModeChanged
            }
            _ => PresentOutcome::Failed,
        };
    }
    if hr == DXGI_ERROR_INVALID_CALL {
        return PresentOutcome::InvalidCall;
    }
    if is_device_removal(hr) {
        return PresentOutcome::DeviceLost;
    }
    PresentOutcome::Failed
}

/// `ResizeBuffers` 的失败是否可以回退到「整链替换」（R08-1 的回退开关）。
///
/// 只有 `INVALID_CALL`（旧 back buffer 引用没释放干净 / 参数被拒）值得回退；设备级错误与
/// `OUT_OF_MEMORY` 回退也没用。
pub(crate) fn resize_fallback_worthwhile(hr: HRESULT) -> bool {
    hr == DXGI_ERROR_INVALID_CALL
}

#[cfg(test)]
mod tests {
    use windows::core::HRESULT;

    use super::{
        DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
        DXGI_ERROR_DRIVER_INTERNAL_ERROR, DXGI_ERROR_INVALID_CALL, DXGI_STATUS_CLIPPED,
        DXGI_STATUS_MODE_CHANGE_IN_PROGRESS, DXGI_STATUS_MODE_CHANGED, DXGI_STATUS_OCCLUDED,
        PresentOutcome, classify_present, is_device_removal, removal_reason_label,
        resize_fallback_worthwhile,
    };

    /// 原始码必须与 `dxgi.h` 一致（`MAKE_DXGI_STATUS/HRESULT` 的 facility 是 `0x87a`）。
    #[test]
    fn the_raw_codes_match_dxgi_h() {
        assert_eq!(DXGI_ERROR_INVALID_CALL.0 as u32, 0x887A_0001);
        assert_eq!(DXGI_ERROR_DEVICE_REMOVED.0 as u32, 0x887A_0005);
        assert_eq!(DXGI_ERROR_DEVICE_HUNG.0 as u32, 0x887A_0006);
        assert_eq!(DXGI_ERROR_DEVICE_RESET.0 as u32, 0x887A_0007);
        assert_eq!(DXGI_ERROR_DRIVER_INTERNAL_ERROR.0 as u32, 0x887A_0020);
        assert_eq!(DXGI_STATUS_OCCLUDED.0 as u32, 0x087A_0001);
        assert_eq!(DXGI_STATUS_CLIPPED.0 as u32, 0x087A_0002);
        assert_eq!(DXGI_STATUS_MODE_CHANGED.0 as u32, 0x087A_0007);
        assert_eq!(DXGI_STATUS_MODE_CHANGE_IN_PROGRESS.0 as u32, 0x087A_0008);
        // 正 HRESULT 与负 HRESULT 必须落在两个不同的 severity 位。
        assert!(DXGI_STATUS_OCCLUDED.is_ok());
        assert!(!DXGI_ERROR_INVALID_CALL.is_ok());
    }

    /// R08-7 的核心分类表：时序现象不得变成设备丢失，反之亦然。
    #[test]
    fn present_results_are_classified_by_severity_and_code() {
        assert_eq!(classify_present(HRESULT(0)), PresentOutcome::Presented);
        assert_eq!(
            classify_present(DXGI_STATUS_OCCLUDED),
            PresentOutcome::Occluded,
            "遮挡是正常现象，不是失败"
        );
        assert_eq!(
            classify_present(DXGI_STATUS_CLIPPED),
            PresentOutcome::Clipped
        );
        assert_eq!(
            classify_present(DXGI_STATUS_MODE_CHANGED),
            PresentOutcome::ModeChanged
        );
        assert_eq!(
            classify_present(DXGI_STATUS_MODE_CHANGE_IN_PROGRESS),
            PresentOutcome::ModeChanged
        );
        assert_eq!(
            classify_present(DXGI_ERROR_INVALID_CALL),
            PresentOutcome::InvalidCall,
            "越界 dirty rect 之类的调用错误必须可重试"
        );
        assert_eq!(
            classify_present(DXGI_ERROR_DEVICE_REMOVED),
            PresentOutcome::DeviceLost
        );
        assert_eq!(
            classify_present(DXGI_ERROR_DEVICE_HUNG),
            PresentOutcome::DeviceLost
        );
        // 其它负 HRESULT（例如 OUT_OF_MEMORY）保留分类但不进设备丢失路径。
        assert_eq!(
            classify_present(HRESULT(0x887A_000Eu32 as i32)),
            PresentOutcome::Failed
        );
        // 未知的正 HRESULT 也不是「已呈现」。
        assert_eq!(
            classify_present(HRESULT(0x087A_0010u32 as i32)),
            PresentOutcome::Failed
        );
    }

    #[test]
    fn device_removal_is_the_only_sticky_condition() {
        assert!(is_device_removal(DXGI_ERROR_DEVICE_REMOVED));
        assert!(is_device_removal(DXGI_ERROR_DEVICE_HUNG));
        assert!(is_device_removal(DXGI_ERROR_DEVICE_RESET));
        assert!(is_device_removal(DXGI_ERROR_DRIVER_INTERNAL_ERROR));
        assert!(!is_device_removal(DXGI_ERROR_INVALID_CALL));
        assert!(!is_device_removal(DXGI_STATUS_OCCLUDED));
        assert!(!is_device_removal(HRESULT(0x8007_0005u32 as i32)));

        assert_eq!(
            removal_reason_label(DXGI_ERROR_DEVICE_REMOVED),
            "DEVICE_REMOVED"
        );
        assert_eq!(removal_reason_label(DXGI_ERROR_DEVICE_HUNG), "DEVICE_HUNG");
        assert_eq!(removal_reason_label(DXGI_ERROR_INVALID_CALL), "unknown");
    }

    /// 只有「参数被拒」值得回退到整链替换；设备级错误回退也没用。
    #[test]
    fn only_invalid_call_justifies_a_chain_replacement_fallback() {
        assert!(resize_fallback_worthwhile(DXGI_ERROR_INVALID_CALL));
        assert!(!resize_fallback_worthwhile(DXGI_ERROR_DEVICE_REMOVED));
        assert!(!resize_fallback_worthwhile(HRESULT(0x8007_000Eu32 as i32)));
    }
}

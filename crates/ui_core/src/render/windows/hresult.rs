use windows::core::HRESULT;

const fn dxgi_error(code: u32) -> HRESULT {
    HRESULT((0x887A_0000u32 | code) as i32)
}

const fn dxgi_status(code: u32) -> HRESULT {
    HRESULT((0x087A_0000u32 | code) as i32)
}

pub(crate) const DXGI_ERROR_INVALID_CALL: HRESULT = dxgi_error(0x0001);
pub(crate) const DXGI_ERROR_DEVICE_REMOVED: HRESULT = dxgi_error(0x0005);
pub(crate) const DXGI_ERROR_DEVICE_HUNG: HRESULT = dxgi_error(0x0006);
pub(crate) const DXGI_ERROR_DEVICE_RESET: HRESULT = dxgi_error(0x0007);
pub(crate) const DXGI_ERROR_DRIVER_INTERNAL_ERROR: HRESULT = dxgi_error(0x0020);
pub(crate) const DXGI_STATUS_OCCLUDED: HRESULT = dxgi_status(0x0001);
pub(crate) const DXGI_STATUS_CLIPPED: HRESULT = dxgi_status(0x0002);
pub(crate) const DXGI_STATUS_MODE_CHANGED: HRESULT = dxgi_status(0x0007);
pub(crate) const DXGI_STATUS_MODE_CHANGE_IN_PROGRESS: HRESULT = dxgi_status(0x0008);

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentOutcome {
    Presented,
    Occluded,
    Clipped,
    ModeChanged,
    InvalidCall,
    DeviceLost,
    Failed,
}

pub(crate) fn classify_present(hr: HRESULT) -> PresentOutcome {
    if hr == HRESULT(0) {
        return PresentOutcome::Presented;
    }
    if hr.is_ok() {
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
        assert!(DXGI_STATUS_OCCLUDED.is_ok());
        assert!(!DXGI_ERROR_INVALID_CALL.is_ok());
    }

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
        assert_eq!(
            classify_present(HRESULT(0x887A_000Eu32 as i32)),
            PresentOutcome::Failed
        );
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

    #[test]
    fn only_invalid_call_justifies_a_chain_replacement_fallback() {
        assert!(resize_fallback_worthwhile(DXGI_ERROR_INVALID_CALL));
        assert!(!resize_fallback_worthwhile(DXGI_ERROR_DEVICE_REMOVED));
        assert!(!resize_fallback_worthwhile(HRESULT(0x8007_000Eu32 as i32)));
    }
}

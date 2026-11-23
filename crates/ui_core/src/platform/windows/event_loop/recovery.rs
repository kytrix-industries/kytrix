use windows::Win32::UI::WindowsAndMessaging::{
    PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, SPI_SETWORKAREA, WM_DISPLAYCHANGE,
    WM_POWERBROADCAST, WM_SETTINGCHANGE,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RecoveryAction {
    RequeryDisplays,
    RebuildTargets,
    RequestFullRepaint,
}

pub(super) const RECOVERY_ACTIONS: &[RecoveryAction] = &[
    RecoveryAction::RequeryDisplays,
    RecoveryAction::RebuildTargets,
    RecoveryAction::RequestFullRepaint,
];

const WORK_AREA_CHANGE: usize = SPI_SETWORKAREA.0 as usize;

pub(super) fn recovery_plan(msg: u32, wparam: usize) -> Option<&'static [RecoveryAction]> {
    match msg {
        WM_DISPLAYCHANGE => Some(RECOVERY_ACTIONS),
        WM_SETTINGCHANGE if wparam == WORK_AREA_CHANGE => Some(RECOVERY_ACTIONS),
        WM_POWERBROADCAST
            if matches!(wparam as u32, PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND) =>
        {
            Some(RECOVERY_ACTIONS)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::WindowsAndMessaging::{
        PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND, SPI_SETANIMATION,
        SPI_SETWORKAREA, WM_DISPLAYCHANGE, WM_MOUSEMOVE, WM_POWERBROADCAST, WM_SETTINGCHANGE,
    };

    use super::{RECOVERY_ACTIONS, RecoveryAction, recovery_plan};

    #[test]
    fn topology_work_area_and_resume_share_the_display_recovery_sequence() {
        for (msg, wparam) in [
            (WM_DISPLAYCHANGE, 0),
            (WM_SETTINGCHANGE, SPI_SETWORKAREA.0 as usize),
            (WM_POWERBROADCAST, PBT_APMRESUMEAUTOMATIC as usize),
            (WM_POWERBROADCAST, PBT_APMRESUMESUSPEND as usize),
        ] {
            assert_eq!(
                recovery_plan(msg, wparam),
                Some(RECOVERY_ACTIONS),
                "msg={msg:#06x} wparam={wparam}"
            );
        }
        assert_eq!(
            RECOVERY_ACTIONS.first(),
            Some(&RecoveryAction::RequeryDisplays),
            "恢复必须先重新查询显示器信息"
        );
        assert!(
            RECOVERY_ACTIONS.contains(&RecoveryAction::RebuildTargets),
            "拓扑变化必须重建交换链（⇒ reset_epoch + 清遮挡退避）"
        );
        assert!(
            RECOVERY_ACTIONS.contains(&RecoveryAction::RequestFullRepaint),
            "拓扑变化必须全量重绘一帧"
        );
    }

    #[test]
    fn unrelated_messages_do_not_request_a_display_recovery() {
        assert_eq!(
            recovery_plan(WM_SETTINGCHANGE, SPI_SETANIMATION.0 as usize),
            None
        );
        assert_eq!(recovery_plan(WM_SETTINGCHANGE, 0), None);
        assert_eq!(
            recovery_plan(WM_POWERBROADCAST, PBT_APMSUSPEND as usize),
            None
        );
        assert_eq!(recovery_plan(WM_MOUSEMOVE, 0), None);
    }
}

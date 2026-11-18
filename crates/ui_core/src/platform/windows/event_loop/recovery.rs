//! 显示 / 电源状态变化的恢复判定（R07-14）。
//!
//! 三条平台消息会**在应用不知情的情况下**改变引擎依赖的显示状态，必须由平台层自己恢复：
//!
//! | 消息 | 触发条件 | 系统改了什么 |
//! | --- | --- | --- |
//! | [`WM_DISPLAYCHANGE`] | 分辨率 / 色深 / 显示器拓扑变化 | 每屏 DPI、交换链关联的显示器 |
//! | [`WM_SETTINGCHANGE`] | `wparam == SPI_SETWORKAREA` | 工作区（任务栏 / 停靠栏）几何 |
//! | [`WM_POWERBROADCAST`] | `PBT_APMRESUMEAUTOMATIC` / `PBT_APMRESUMESUSPEND` | 设备与交换链状态（睡眠期间可能被驱动重置） |
//!
//! 本模块只做**纯判定**（消息 → 动作序列），不碰窗口、不碰 GPU：整条恢复路径因此可以在没有
//! 窗口的单元测试里断言。执行点有两处：
//! - 窗口侧 [`RecoveryAction::RequeryDisplays`] / [`RecoveryAction::RequestFullRepaint`]：
//!   泵的 ②a′（`native_window::apply_display_recovery`）；
//! - GPU 侧 [`RecoveryAction::RebuildTargets`]：`render::windows::display` 的进程级代际，
//!   每条 `kytrix-d3d12` 线程各消费一次。
//!
//! 三个触发原因共用同一套动作：拓扑、工作区与唤醒之后，引擎持有的显示派生状态（每窗口 DPI、
//! 客户区、交换链的显示器关联）都不可信；能证明「一定正确」的恢复只有「重新查询显示器 →
//! 重建目标 → 全量重绘一帧」，少做任何一步都会留下需要用户拖动窗口才消失的画面。

use windows::Win32::UI::WindowsAndMessaging::{
    PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, SPI_SETWORKAREA, WM_DISPLAYCHANGE,
    WM_POWERBROADCAST, WM_SETTINGCHANGE,
};

/// 一次显示 / 电源恢复要执行的动作（**按列出的顺序执行**）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RecoveryAction {
    /// 重新查询显示器信息：每窗口 DPI（`GetDpiForWindow`）与客户区（`GetClientRect`），
    /// 并写回 `Window`（`set_scale` / `set_size` / `set_surface`）。
    RequeryDisplays,
    /// 请求 GPU 侧重建交换链与持久目标（`render::windows::display`）：重建后
    /// `FrameSink::reset_epoch` +1 ⇒ UI 下一帧整帧重发（`ResyncGate`），并清掉本窗口的遮挡
    /// 退避（唤醒后的第一帧必须出）。
    RebuildTargets,
    /// 请求一帧全量重绘（`Window::request_repaint` ⇒ 本帧 `Plan::Full`）。
    RequestFullRepaint,
}

/// 恢复动作序列（**唯一**一份；见模块文档「三个触发原因共用同一套动作」）。
pub(super) const RECOVERY_ACTIONS: &[RecoveryAction] = &[
    RecoveryAction::RequeryDisplays,
    RecoveryAction::RebuildTargets,
    RecoveryAction::RequestFullRepaint,
];

/// 工作区变化的 `WM_SETTINGCHANGE` 的 `wparam`（`SPI_SETWORKAREA`）。
const WORK_AREA_CHANGE: usize = SPI_SETWORKAREA.0 as usize;

/// 消息 → 期望动作序列（**纯函数**：没有窗口、没有 GPU 也能执行）。
///
/// `None` = 该消息与显示 / 电源恢复无关（例如主题色变化的 `WM_SETTINGCHANGE`、挂起通知
/// `PBT_APMSUSPEND`），调用方按原路径处理。
pub(super) fn recovery_plan(msg: u32, wparam: usize) -> Option<&'static [RecoveryAction]> {
    match msg {
        WM_DISPLAYCHANGE => Some(RECOVERY_ACTIONS),
        // `SPI_SETWORKAREA` 之外的 `WM_SETTINGCHANGE`（主题色、滚轮行数、字体平滑…）与
        // 显示恢复无关：它们不需要重新查询显示器，更不需要重建交换链。
        WM_SETTINGCHANGE if wparam == WORK_AREA_CHANGE => Some(RECOVERY_ACTIONS),
        // 只认「从睡眠回来」的两个 resume 通知：`PBT_APMSUSPEND`（准备挂起）不是恢复。
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

    /// R07-14 的核心：拓扑 / 工作区 / 睡眠唤醒都必须触发**同一串**恢复动作，且第一个动作是
    /// 重新查询显示器 —— 后面两步（重建目标、全量重绘）都依赖它刚查到的 DPI / 客户区。
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

    /// 无关消息 / 无关设置项不得触发恢复：否则每次主题色变化都要重建交换链。
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

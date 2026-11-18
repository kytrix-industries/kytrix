//! 显示 / 电源状态变化的恢复信号（R07-14）：平台层置位，每条 GPU 线程各消费一次。
//!
//! 平台层（`platform::windows::event_loop`）在 `WM_DISPLAYCHANGE` /
//! `WM_SETTINGCHANGE(SPI_SETWORKAREA)` / 睡眠唤醒（`WM_POWERBROADCAST`）之后，重新查询完显示器
//! 信息就调用 [`note_display_change`]。GPU 线程在自己的循环里观察 [`epoch`] 的变化，然后：
//!
//! 1. `GpuRenderer::rebuild_target`：重建组合交换链与持久目标 —— 睡眠期间驱动可能已经重置了
//!    交换链，显示器拓扑变化之后交换链的显示器关联也不再可信；
//! 2. `Shared::note_renderer_rebuilt`：`FrameSink::reset_epoch` +1 ⇒ UI 下一帧整帧重发
//!    （`ResyncGate`），恢复后的第一帧因此不依赖任何帧前状态；
//! 3. 清退避：本窗口下一次取帧不再因为「不可见 / 被遮挡」被跳过（唤醒后必须出帧）。
//!
//! 为什么是进程级代际而不是每窗口标志：显示拓扑与电源状态是**进程级**事实，而平台层拿不到
//! 每窗口的 `Renderer`（渲染门面是 `window` 模块私有）。代际是单调信号，多窗口各自消费，
//! 「恢复期间又变化一次」也不会丢。
//!
//! 为什么不用 Windows 的消息广播：GPU 线程不泵消息（它阻塞在 fence / waitable 上），把恢复
//! 交给它自己的循环观察，才不会在「等 waitable」里错过事件。

use std::sync::atomic::{AtomicU64, Ordering};

/// 显示 / 电源恢复代际（平台层 +1，GPU 线程消费）。
///
/// 只用于比较相等，不发布任何数据（帧请求本身经 `Mutex` 过线），因此 `Relaxed` 足够。
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// 平台层：登记一次显示 / 电源恢复（调用点：`native_window::apply_display_recovery`）。
pub(crate) fn note_display_change() {
    EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// 当前代际（GPU 线程每轮读一次，与自己的上次观测值比较）。
pub(crate) fn epoch() -> u64 {
    EPOCH.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::{epoch, note_display_change};

    /// 平台层每登记一次恢复，代际必须前进：GPU 线程据此只消费一次（单调信号）。
    #[test]
    fn every_registered_display_change_advances_the_epoch() {
        let first = epoch();
        note_display_change();
        let second = epoch();
        assert!(second > first, "恢复代际必须单调前进：{first} → {second}");
        note_display_change();
        assert!(epoch() > second, "连续两次恢复必须各推进一步");
    }
}

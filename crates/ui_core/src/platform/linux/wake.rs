//! 跨线程唤醒：`calloop::ping`（`design/phase-12-macos-linux.md` §8.1）。
//!
//! `Ping` 是 `Clone + Send + Sync`；句柄存在 `OnceLock` 里，`Dispatcher::wake` 从这里打断
//! `calloop::EventLoop::dispatch` 的阻塞等待（「先置位再 ping」由 `Dispatcher` 保证）。

use std::sync::OnceLock;

use calloop::ping::Ping;

/// `None` = 尚未安装（`signal()` 返回 `false`，主线程的 drain-then-check 兜住）。
static PING: OnceLock<Ping> = OnceLock::new();

/// 安装唤醒句柄（主线程，`run` 里调用一次）。
pub(crate) fn install(ping: Ping) {
    let _ = PING.set(ping);
}

/// 任意线程：打断主循环的阻塞等待。
pub(crate) fn signal() -> bool {
    match PING.get() {
        Some(ping) => {
            ping.ping();
            true
        }
        None => false,
    }
}

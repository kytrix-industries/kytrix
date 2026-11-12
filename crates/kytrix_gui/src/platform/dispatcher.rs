//! 跨线程唤醒 + 用户任务回投（`design/phase-09-win32-demo.md` §5.1）。
//!
//! 两条规则（都来自本机 probe）：
//! 1. **唤醒必须合并**：`PostMessageW` 队列上限 10,000 条（`0x80070718`），每次状态写入都 post
//!    会打满队列（DEVIATIONS F-87）。这里用 `frame_pending` 做电平信号，只有首次置位才 post。
//! 2. **先置位再 post**：post 失败（配额/窗口已销毁）时主线程的下一次 drain 仍能看到待处理帧。

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicIsize, Ordering},
    },
};

use crate::app::App;

/// 一条「在主线程执行」的任务（业务异步结果的唯一回投形式）。
pub(crate) struct UserTask(Box<dyn FnOnce(&mut App) + Send>);

impl UserTask {
    pub(crate) fn new(task: impl FnOnce(&mut App) + Send + 'static) -> Self {
        Self(Box::new(task))
    }

    /// 在主线程执行（`EventLoop` 的 drain 点唯一调用）。
    pub(crate) fn run(self, app: &mut App) {
        (self.0)(app);
    }
}

/// 引擎自有的唤醒消息（`WM_APP + 1`；值必须在投递与识别两侧一致）。
///
/// 只在 Windows 构建里存在：`WM_*` 常量来自 `windows` crate（非 Windows 构建没有该依赖）。
#[cfg(windows)]
pub(crate) const WM_APP_FRAME_WAKE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;

/// 唤醒投递：收目标 token（Windows = 窗口句柄），返回是否成功投递。
type PostFn = fn(isize) -> bool;

/// 线程安全的唤醒队列（`AppHandle` 与平台事件循环共享）。
pub(crate) struct Dispatcher {
    post: PostFn,
    /// 电平信号：`true` = 已有一帧待处理（合并位）。
    frame_pending: AtomicBool,
    /// tokio / a11y 回投的任务（FIFO；不在热路径）。
    tasks: Mutex<VecDeque<UserTask>>,
    /// 唤醒目标：Windows 是窗口句柄；macOS / Linux 用平台自己安装的唤醒句柄（此值忽略）。
    hwnd: AtomicIsize,
}

impl Dispatcher {
    pub(crate) fn new() -> Arc<Self> {
        Self::with_post(post_frame_wake)
    }

    fn with_post(post: PostFn) -> Arc<Self> {
        Arc::new(Self {
            post,
            frame_pending: AtomicBool::new(false),
            tasks: Mutex::new(VecDeque::new()),
            hwnd: AtomicIsize::new(0),
        })
    }

    /// 绑定唤醒目标（Win32：窗口句柄；macOS / Linux：占位 token，唤醒句柄由平台自己安装）。
    pub(crate) fn attach(&self, hwnd: isize) {
        self.hwnd.store(hwnd, Ordering::Release);
    }

    /// 请求一帧。任意线程可调；**幂等**（同一时刻最多一条 `WM_APP` 在队列里）。
    pub(crate) fn wake(&self) {
        if !self.frame_pending.swap(true, Ordering::AcqRel) {
            // 失败不 panic：`frame_pending` 已置位，主线程的 drain-then-check 会看到它。
            let _ = (self.post)(self.hwnd.load(Ordering::Acquire));
        }
    }

    /// 取走「有一帧要画」的信号（主线程，取走即清零）。
    pub(crate) fn take_frame_wake(&self) -> bool {
        self.frame_pending.swap(false, Ordering::AcqRel)
    }

    /// 入队一个主线程任务并唤醒。
    pub(crate) fn push(&self, task: UserTask) {
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.push_back(task);
        }
        self.wake();
    }

    /// 取走全部待执行任务（主线程）。
    pub(crate) fn drain_tasks(&self) -> Vec<UserTask> {
        match self.tasks.lock() {
            Ok(mut tasks) => tasks.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// 生产投递：`PostMessageW(WM_APP + 1)`；窗口未 attach 或平台不支持时返回 `false`。
#[cfg(windows)]
fn post_frame_wake(hwnd: isize) -> bool {
    use windows::Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        UI::WindowsAndMessaging::PostMessageW,
    };

    if hwnd == 0 {
        return false;
    }
    // SAFETY: `hwnd` 由本进程的 `NativeWindow` 持有；`PostMessageW` 只入队，不解引用其它指针。
    let result = unsafe {
        PostMessageW(
            Some(HWND(hwnd as *mut core::ffi::c_void)),
            WM_APP_FRAME_WAKE,
            WPARAM(0),
            LPARAM(0),
        )
    };
    result.is_ok()
}

/// macOS：`CFRunLoopSourceSignal` + `CFRunLoopWakeUp`（`platform/macos/wake.rs` 安装的句柄）。
#[cfg(target_os = "macos")]
fn post_frame_wake(_token: isize) -> bool {
    crate::platform::macos::wake::signal()
}

/// Linux：`calloop::ping::Ping::ping`（`platform/linux/event_loop.rs` 安装的句柄）。
#[cfg(target_os = "linux")]
fn post_frame_wake(_token: isize) -> bool {
    crate::platform::linux::wake::signal()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{Dispatcher, UserTask};

    static POSTS: AtomicUsize = AtomicUsize::new(0);

    fn counting_post(_hwnd: isize) -> bool {
        POSTS.fetch_add(1, Ordering::SeqCst);
        true
    }

    #[test]
    fn repeated_wakes_coalesce_into_one_post() {
        POSTS.store(0, Ordering::SeqCst);
        let dispatcher = Dispatcher::with_post(counting_post);

        dispatcher.wake();
        dispatcher.wake();
        dispatcher.wake();

        assert_eq!(POSTS.load(Ordering::SeqCst), 1, "三次 wake 只允许一次 post");
        assert!(dispatcher.take_frame_wake());
        assert!(!dispatcher.take_frame_wake(), "取走后必须清零");
    }

    #[test]
    fn a_wake_after_consumption_posts_again() {
        POSTS.store(0, Ordering::SeqCst);
        let dispatcher = Dispatcher::with_post(counting_post);

        dispatcher.wake();
        assert_eq!(POSTS.load(Ordering::SeqCst), 1);
        dispatcher.wake();
        assert_eq!(POSTS.load(Ordering::SeqCst), 1, "未消费前不再 post");

        dispatcher.take_frame_wake();
        dispatcher.wake();
        assert_eq!(POSTS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn failed_post_keeps_the_pending_flag() {
        fn failing_post(_hwnd: isize) -> bool {
            false
        }
        let dispatcher = Dispatcher::with_post(failing_post);

        dispatcher.wake();

        assert!(
            dispatcher.take_frame_wake(),
            "post 失败也必须保留待处理状态（drain-then-check 兜住）"
        );
    }

    #[test]
    fn queued_tasks_are_drained_once() {
        POSTS.store(0, Ordering::SeqCst);
        let dispatcher = Dispatcher::with_post(counting_post);

        dispatcher.push(UserTask::new(|_app| {}));
        dispatcher.push(UserTask::new(|_app| {}));

        assert_eq!(dispatcher.drain_tasks().len(), 2, "两条任务都必须入队");
        assert!(dispatcher.drain_tasks().is_empty(), "drain 只返回一次");
        assert_eq!(POSTS.load(Ordering::SeqCst), 1, "两次 push 合并成一次唤醒");
    }
}

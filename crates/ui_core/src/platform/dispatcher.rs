//! 跨线程唤醒 + 用户任务回投（`design/phase-09-win32-demo.md` §5.1）。
//!
//! 两条规则（都来自本机 probe）：
//! 1. **唤醒必须合并**：`PostMessageW` 队列上限 10,000 条（`0x80070718`），每次状态写入都 post
//!    会打满队列（DEVIATIONS F-87）。这里用 `frame_pending` 做电平信号，只有首次置位才 post。
//! 2. **先置位再 post**：post 失败（配额/窗口已销毁）时主线程的下一次 drain 仍能看到待处理帧。

#[cfg(windows)]
use std::sync::atomic::AtomicU32;
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

/// 唤醒投递：收目标 token（Windows = 当前主窗口句柄，`0` = 没有窗口）与本线程 id，
/// 返回是否成功投递。
type PostFn = fn(isize, u32) -> bool;

/// 线程安全的唤醒队列（`AppHandle` 与平台事件循环共享）。
pub(crate) struct Dispatcher {
    post: PostFn,
    /// 电平信号：`true` = 已有一帧待处理（合并位）。
    frame_pending: AtomicBool,
    /// tokio / a11y 回投的任务（FIFO；不在热路径）。
    tasks: Mutex<VecDeque<UserTask>>,
    /// 唤醒目标：Windows 是窗口句柄；macOS / Linux 用平台自己安装的唤醒句柄（此值忽略）。
    ///
    /// **可更新、可清空**：首个窗口关闭后必须改指存活窗口（或清空），否则 `PostMessageW`
    /// 失败会让 `GetMessageW` 永久阻塞。清空后由 `Dispatcher::bind_thread` 记下的线程 id
    /// 走 `PostThreadMessageW` 兜底。
    hwnd: AtomicIsize,
    /// 主线程 id（Windows：`PostThreadMessageW` 的兜底目标；其它平台没有线程消息，字段不存）。
    #[cfg(windows)]
    thread: AtomicU32,
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
            #[cfg(windows)]
            thread: AtomicU32::new(0),
        })
    }

    /// 绑定唤醒目标（Win32：窗口句柄；macOS / Linux：占位 token，唤醒句柄由平台自己安装）。
    ///
    /// 窗口关闭时调用方必须改指存活窗口或 `Dispatcher::detach`。
    pub(crate) fn attach(&self, hwnd: isize) {
        self.hwnd.store(hwnd, Ordering::Release);
    }

    /// 清空唤醒目标（最后一个窗口关闭）：后续唤醒走线程消息兜底。
    ///
    /// 只有 Windows 的泵需要退订（macOS / Linux 的唤醒句柄由平台自己安装且随泵存活）。
    #[cfg(windows)]
    pub(crate) fn detach(&self) {
        self.attach(0);
    }

    /// 记录主线程 id（`PostThreadMessageW` 的兜底目标）。
    ///
    /// 只有 Windows 有线程消息：macOS / Linux 的 `post_frame_wake` 忽略目标 token。
    #[cfg(windows)]
    pub(crate) fn bind_thread(&self, thread: u32) {
        self.thread.store(thread, Ordering::Release);
    }

    /// 请求一帧。任意线程可调；**幂等**（同一时刻最多一条 `WM_APP` 在队列里）。
    pub(crate) fn wake(&self) {
        if !self.frame_pending.swap(true, Ordering::AcqRel) {
            // 失败不 panic：`frame_pending` 已置位；
            // 窗口目标失效时投递函数会退回线程队列（见 `post_frame_wake`）。
            #[cfg(windows)]
            let thread = self.thread.load(Ordering::Acquire);
            #[cfg(not(windows))]
            let thread = 0;
            let _ = (self.post)(self.hwnd.load(Ordering::Acquire), thread);
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

/// 生产投递（Windows）：优先 `PostMessageW` 到主窗口；窗口已销毁或尚未 attach 时退回
/// `PostThreadMessageW`（线程消息），保证 `GetMessageW` 一定被唤醒。
///
/// 两个目标都不可用时返回 `false`（调用方仍保留 `frame_pending`，主线程下一次 drain 会看到）。
#[cfg(windows)]
fn post_frame_wake(hwnd: isize, thread: u32) -> bool {
    use windows::Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        UI::WindowsAndMessaging::{PostMessageW, PostThreadMessageW},
    };

    if hwnd != 0 {
        // SAFETY: `hwnd` 由本进程的 `NativeWindow` 持有或已销毁；`PostMessageW` 只入队，
        // 不解引用其它指针（句柄失效时返回错误，不会 UB）。
        let posted = unsafe {
            PostMessageW(
                Some(HWND(hwnd as *mut core::ffi::c_void)),
                WM_APP_FRAME_WAKE,
                WPARAM(0),
                LPARAM(0),
            )
        };
        if posted.is_ok() {
            return true;
        }
        // 首个窗口已关闭但目标未更新：落到线程消息，不让 `GetMessageW` 永久阻塞。
    }
    if thread != 0 {
        // SAFETY: `thread` 是主线程 id（`GetCurrentThreadId`）；`PostThreadMessageW` 只入队。
        let posted = unsafe { PostThreadMessageW(thread, WM_APP_FRAME_WAKE, WPARAM(0), LPARAM(0)) };
        return posted.is_ok();
    }
    false
}

/// macOS：`CFRunLoopSourceSignal` + `CFRunLoopWakeUp`（`platform/macos/wake.rs` 安装的句柄）。
#[cfg(target_os = "macos")]
fn post_frame_wake(_token: isize, _thread: u32) -> bool {
    crate::platform::macos::wake::signal()
}

/// Linux：`calloop::ping::Ping::ping`（`platform/linux/event_loop.rs` 安装的句柄）。
#[cfg(target_os = "linux")]
fn post_frame_wake(_token: isize, _thread: u32) -> bool {
    crate::platform::linux::wake::signal()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{Dispatcher, UserTask};

    static POSTS: AtomicUsize = AtomicUsize::new(0);

    /// `POSTS` 是进程级计数器：涉及它的用例必须串行（并行测试会互相把计数改掉）。
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn counting_post(_hwnd: isize, _thread: u32) -> bool {
        POSTS.fetch_add(1, Ordering::SeqCst);
        true
    }

    #[test]
    fn repeated_wakes_coalesce_into_one_post() {
        let _guard = lock();
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
        let _guard = lock();
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
        fn failing_post(_hwnd: isize, _thread: u32) -> bool {
            false
        }
        let dispatcher = Dispatcher::with_post(failing_post);

        dispatcher.wake();

        assert!(
            dispatcher.take_frame_wake(),
            "post 失败也必须保留待处理状态（drain-then-check 兜住）"
        );
    }

    /// 首窗关闭（句柄失效）后唤醒不能丢：`PostMessageW` 失败必须退回线程消息。
    ///
    /// 这是 P14 审查的阻塞项：`attach` 只绑定首个窗口时，窗口一关 `PostMessageW` 就失败，
    /// 主线程随后阻塞在 `GetMessageW`，后台任务再也投不进来。
    #[cfg(windows)]
    #[test]
    fn wake_reaches_the_thread_queue_when_the_window_target_is_gone() {
        use windows::Win32::{System::Threading::GetCurrentThreadId, UI::WindowsAndMessaging::*};

        fn drain_thread_messages() {
            let mut message = MSG::default();
            // SAFETY: `message` 是栈上值；只从本线程队列取消息。
            while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                if message.message == WM_QUIT {
                    return;
                }
            }
        }

        let dispatcher = super::Dispatcher::new();
        // SAFETY: 只查询调用线程的 id。
        dispatcher.bind_thread(unsafe { GetCurrentThreadId() });
        // 一个必定失效的窗口句柄：模拟「首窗已销毁但目标仍是它」。
        dispatcher.attach(0xDEAD_BEEF);
        drain_thread_messages();

        dispatcher.wake();

        let mut message = MSG::default();
        // SAFETY: 同上；`message` 由 `PeekMessageW` 填充后才读。
        let has_message = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) };
        assert!(has_message.as_bool(), "窗口目标失效后必须仍有唤醒消息");
        assert_eq!(message.message, super::WM_APP_FRAME_WAKE);
        assert!(
            message.hwnd.is_invalid(),
            "兜底消息必须走线程队列（hwnd == NULL）"
        );
        assert!(dispatcher.take_frame_wake(), "合并位必须保持待处理");
    }

    /// 正常路径：还活着的目标窗口收到窗口消息（而不是线程消息）。
    #[cfg(windows)]
    #[test]
    fn wake_posts_to_the_attached_window_while_it_lives() {
        use windows::{
            Win32::{
                System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
                UI::WindowsAndMessaging::*,
            },
            core::w,
        };

        let instance = match unsafe { GetModuleHandleW(None) } {
            Ok(instance) => instance,
            Err(_) => return,
        };
        // 消息专用窗口：内置 `STATIC` 类，不需要自定义 WndProc。
        let hwnd = match unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("kytrix wake target"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance.into()),
                None,
            )
        } {
            Ok(hwnd) => hwnd,
            Err(_) => return,
        };

        let dispatcher = super::Dispatcher::new();
        // SAFETY: 只查询调用线程的 id。
        dispatcher.bind_thread(unsafe { GetCurrentThreadId() });
        dispatcher.attach(hwnd.0 as isize);

        dispatcher.wake();

        let mut message = MSG::default();
        // SAFETY: 只取本窗口的队列消息。
        let has_message = unsafe { PeekMessageW(&mut message, Some(hwnd), 0, 0, PM_REMOVE) };
        assert!(has_message.as_bool(), "存活窗口必须收到窗口消息");
        assert_eq!(message.message, super::WM_APP_FRAME_WAKE);

        // SAFETY: 窗口由本测试创建、尚未销毁。
        let _ = unsafe { DestroyWindow(hwnd) };
        dispatcher.detach();
    }

    #[test]
    fn queued_tasks_are_drained_once() {
        let _guard = lock();
        POSTS.store(0, Ordering::SeqCst);
        let dispatcher = Dispatcher::with_post(counting_post);

        dispatcher.push(UserTask::new(|_app| {}));
        dispatcher.push(UserTask::new(|_app| {}));

        assert_eq!(dispatcher.drain_tasks().len(), 2, "两条任务都必须入队");
        assert!(dispatcher.drain_tasks().is_empty(), "drain 只返回一次");
        assert_eq!(POSTS.load(Ordering::SeqCst), 1, "两次 push 合并成一次唤醒");
    }
}

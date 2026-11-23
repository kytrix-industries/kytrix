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

pub(crate) struct UserTask(Box<dyn FnOnce(&mut App) + Send>);

impl UserTask {
    pub(crate) fn new(task: impl FnOnce(&mut App) + Send + 'static) -> Self {
        Self(Box::new(task))
    }

    pub(crate) fn run(self, app: &mut App) {
        (self.0)(app);
    }
}

#[cfg(windows)]
pub(crate) const WM_APP_FRAME_WAKE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;

#[cfg(windows)]
pub(crate) const WM_APP_COMPOSITION_WAKE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 2;

type PostFn = fn(isize, u32) -> bool;

pub(crate) struct Dispatcher {
    post: PostFn,
    frame_pending: AtomicBool,
    tasks: Mutex<VecDeque<UserTask>>,
    hwnd: AtomicIsize,
    #[cfg(windows)]
    thread: AtomicU32,
    #[cfg(windows)]
    modal_loops: AtomicU32,
    #[cfg(windows)]
    wake_later_armed: AtomicBool,
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
            #[cfg(windows)]
            modal_loops: AtomicU32::new(0),
            #[cfg(windows)]
            wake_later_armed: AtomicBool::new(false),
        })
    }

    pub(crate) fn attach(&self, hwnd: isize) {
        self.hwnd.store(hwnd, Ordering::Release);
    }

    #[cfg(windows)]
    pub(crate) fn detach(&self) {
        self.attach(0);
    }

    #[cfg(windows)]
    pub(crate) fn bind_thread(&self, thread: u32) {
        self.thread.store(thread, Ordering::Release);
    }

    pub(crate) fn wake(&self) {
        if !self.frame_pending.swap(true, Ordering::AcqRel) {
            #[cfg(windows)]
            let thread = self.thread.load(Ordering::Acquire);
            #[cfg(not(windows))]
            let thread = 0;
            let _ = (self.post)(self.hwnd.load(Ordering::Acquire), thread);
        }
    }

    pub(crate) fn take_frame_wake(&self) -> bool {
        self.frame_pending.swap(false, Ordering::AcqRel)
    }

    #[cfg(windows)]
    pub(crate) fn wake_later(self: &Arc<Self>, delay: std::time::Duration) {
        if self.wake_later_armed.swap(true, Ordering::AcqRel) {
            return;
        }
        let dispatcher = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("kytrix-wake-later".to_owned())
            .spawn(move || {
                std::thread::sleep(delay);
                dispatcher.wake_later_armed.store(false, Ordering::Release);
                dispatcher.wake();
            });
        if spawned.is_err() {
            self.wake_later_armed.store(false, Ordering::Release);
        }
    }

    #[cfg(windows)]
    pub(crate) fn enter_modal_loop(&self) {
        self.modal_loops.fetch_add(1, Ordering::AcqRel);
    }

    #[cfg(windows)]
    pub(crate) fn exit_modal_loop(&self) {
        let mut loops = self.modal_loops.load(Ordering::Acquire);
        while loops > 0 {
            match self.modal_loops.compare_exchange_weak(
                loops,
                loops - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => loops = current,
            }
        }
    }

    #[cfg(windows)]
    pub(crate) fn in_modal_loop(&self) -> bool {
        self.modal_loops.load(Ordering::Acquire) > 0
    }

    #[cfg(any(target_os = "macos", all(test, windows)))]
    pub(crate) fn has_pending_wake(&self) -> bool {
        self.frame_pending.load(Ordering::Acquire)
    }

    pub(crate) fn push(&self, task: UserTask) {
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.push_back(task);
        }
        self.wake();
    }

    pub(crate) fn drain_tasks(&self) -> Vec<UserTask> {
        match self.tasks.lock() {
            Ok(mut tasks) => tasks.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(windows)]
fn post_frame_wake(hwnd: isize, thread: u32) -> bool {
    use windows::Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        UI::WindowsAndMessaging::{PostMessageW, PostThreadMessageW},
    };

    if hwnd != 0 {
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
    }
    if thread != 0 {
        let posted = unsafe { PostThreadMessageW(thread, WM_APP_FRAME_WAKE, WPARAM(0), LPARAM(0)) };
        return posted.is_ok();
    }
    false
}

#[cfg(target_os = "macos")]
fn post_frame_wake(_token: isize, _thread: u32) -> bool {
    crate::platform::macos::wake::signal()
}

#[cfg(target_os = "linux")]
fn post_frame_wake(_token: isize, _thread: u32) -> bool {
    crate::platform::linux::wake::signal()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{Dispatcher, UserTask};

    static POSTS: AtomicUsize = AtomicUsize::new(0);

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

    #[cfg(windows)]
    #[test]
    fn wake_later_posts_once_after_the_delay_and_coalesces() {
        let _guard = lock();
        POSTS.store(0, Ordering::SeqCst);
        let dispatcher = Dispatcher::with_post(counting_post);

        dispatcher.wake_later(std::time::Duration::from_millis(5));
        dispatcher.wake_later(std::time::Duration::from_millis(5));
        assert_eq!(
            POSTS.load(Ordering::SeqCst),
            0,
            "延迟唤醒不得在延迟到达前 post"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !dispatcher.has_pending_wake() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            dispatcher.has_pending_wake(),
            "延迟唤醒必须在 2 s 内到达（否则被推迟的帧要等松手才上屏）"
        );
        assert_eq!(
            POSTS.load(Ordering::SeqCst),
            1,
            "两次挂起必须合并成一次 post"
        );
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

    #[cfg(windows)]
    #[test]
    fn wake_reaches_the_thread_queue_when_the_window_target_is_gone() {
        use windows::Win32::{System::Threading::GetCurrentThreadId, UI::WindowsAndMessaging::*};

        fn drain_thread_messages() {
            let mut message = MSG::default();
            while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                if message.message == WM_QUIT {
                    return;
                }
            }
        }

        let dispatcher = super::Dispatcher::new();
        dispatcher.bind_thread(unsafe { GetCurrentThreadId() });
        dispatcher.attach(0xDEAD_BEEF);
        drain_thread_messages();

        dispatcher.wake();

        let mut message = MSG::default();
        let has_message = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) };
        assert!(has_message.as_bool(), "窗口目标失效后必须仍有唤醒消息");
        assert_eq!(message.message, super::WM_APP_FRAME_WAKE);
        assert!(
            message.hwnd.is_invalid(),
            "兜底消息必须走线程队列（hwnd == NULL）"
        );
        assert!(dispatcher.take_frame_wake(), "合并位必须保持待处理");
    }

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
        dispatcher.bind_thread(unsafe { GetCurrentThreadId() });
        dispatcher.attach(hwnd.0 as isize);

        dispatcher.wake();

        let mut message = MSG::default();
        let has_message = unsafe { PeekMessageW(&mut message, Some(hwnd), 0, 0, PM_REMOVE) };
        assert!(has_message.as_bool(), "存活窗口必须收到窗口消息");
        assert_eq!(message.message, super::WM_APP_FRAME_WAKE);

        let _ = unsafe { DestroyWindow(hwnd) };
        dispatcher.detach();
    }

    #[cfg(windows)]
    #[test]
    fn the_modal_loop_depth_is_counted_and_never_underflows() {
        let dispatcher = super::Dispatcher::new();
        assert!(!dispatcher.in_modal_loop(), "初始不在模态循环");
        dispatcher.enter_modal_loop();
        assert!(dispatcher.in_modal_loop());
        dispatcher.enter_modal_loop();
        dispatcher.exit_modal_loop();
        assert!(
            dispatcher.in_modal_loop(),
            "嵌套：只退出一层时仍在模态循环里"
        );
        dispatcher.exit_modal_loop();
        assert!(!dispatcher.in_modal_loop(), "两层都退出后回到常规路径");
        dispatcher.exit_modal_loop();
        dispatcher.exit_modal_loop();
        assert!(!dispatcher.in_modal_loop(), "多余的 exit 必须饱和在 0");
        assert!(!dispatcher.take_frame_wake(), "模态记账不得触碰唤醒位");
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

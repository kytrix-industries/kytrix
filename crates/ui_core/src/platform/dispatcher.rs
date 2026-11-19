//! 跨线程唤醒 + 用户任务回投。
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

/// GPU 线程的合成通知（`WM_APP + 2`）：交换链里「刚完成 present 的内容尺寸」变了，UI 线程
/// 可能还按旧内容尺寸设置内容 visual 的裁剪框（`platform/windows/composition.rs`）。
///
/// **不请求新帧**（与 [`WM_APP_FRAME_WAKE`] 无关）：消息本身只负责把 `GetMessageW` 唤醒，
/// 泵在每轮迭代里都会重算一次 DirectComposition 几何（同值 no-op）；若这里置帧唤醒位，每次
/// 完成 present 都会额外产生一次 `App::frame`（HUD 下是真实的 paint + present）。只在 Windows
/// 构建里存在。
#[cfg(windows)]
pub(crate) const WM_APP_COMPOSITION_WAKE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 2;

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
    /// 系统模态循环（Windows：`WM_ENTERSIZEMOVE` 的 move / size 循环）的**全局**深度计数（B1）。
    ///
    /// 为什么必须在 `Dispatcher` 而不是每个 `WndProcState`：`Dispatcher::wake` 的目标是
    /// **attach 的首窗**（`pump::retarget_wake`），而被拖动 / 缩放的窗口不一定是首窗。只看
    /// 「本窗口是否在模态循环」会让拖动非首窗时的 `WM_APP_FRAME_WAKE` 依旧被吞掉。
    ///
    /// 计数而非布尔：系统模态循环可以嵌套（例如 `Alt+Space` 菜单循环里再启动 move 循环），
    /// `WM_EXITSIZEMOVE` 只退出一层。只有 Windows 端有「系统模态循环」这个概念（macOS /
    /// Linux 的模态由应用层 / compositor 表达），其它平台不编译该字段与方法。
    #[cfg(windows)]
    modal_loops: AtomicU32,
    /// `wake_later` 是否已挂起一个延迟唤醒（Windows；RB-14 的推迟帧兜底）。
    ///
    /// 电平信号：已挂起时新的 `wake_later` 直接合并（迟到的唤醒会在主线程重新判定是否仍需
    /// 再挂一次），避免每次模态 tick 都起一个线程。
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

    /// 延迟 `delay` 后再 [`Self::wake`]（Windows 模态 tick 的推迟帧兜底；RB-14）。
    ///
    /// 背景：系统 move / size 模态循环里泵不运行、也不 `sync` 帧节拍线程（`FramePacer`）。
    /// `ModalTick` 用普通预算门禁出帧，若本拍有窗口的帧被推迟（预算 `deferred`），而节拍线程
    /// 恰好未运行，就没有任何后续唤醒——该帧要等松手或下一次外部唤醒才上屏。模态 tick 在
    /// 检测到 `App::has_deferred_frames()` 时挂一次延迟唤醒，让被推迟的帧在下一拍被重新判定。
    ///
    /// 合并：同一时刻最多挂一个线程（`wake_later_armed` 电平信号）；延迟取「远小于一个 refresh
    /// 周期」的值即可（到达后若仍被推迟，模态 tick 会再挂一次），不会自激成忙循环。
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
            // 起线程失败：清掉挂起标记，退化为「不补拍」（下一次外部唤醒仍会出帧）。
            self.wake_later_armed.store(false, Ordering::Release);
        }
    }

    /// 进入一层系统模态循环（Windows `WM_ENTERSIZEMOVE`；B1）。
    ///
    /// 只由主线程的 `WndProc` 调用：模态循环期间的 `WM_APP_FRAME_WAKE` 必须自己驱动帧
    /// （泵的 ④ 被 `DispatchMessageW` 里的系统循环占住），判定见 [`Self::in_modal_loop`]。
    #[cfg(windows)]
    pub(crate) fn enter_modal_loop(&self) {
        self.modal_loops.fetch_add(1, Ordering::AcqRel);
    }

    /// 退出一层系统模态循环（Windows `WM_EXITSIZEMOVE`）。
    ///
    /// 计数**绝不下溢**成 `u32::MAX`（否则「模态外」的所有唤醒都会被误判成模态 tick）；多余
    /// 的 exit 饱和在 0。窗口在模态循环里被销毁时收不到配对的 `WM_EXITSIZEMOVE`，漏退的计数
    /// 由窗口状态的 `Drop` 补齐（见 `WndProcState::modal_entries`）。
    ///
    /// 手写 CAS 循环而不是 `fetch_update`：后者在夜间工具链已被重命名为 `try_update` 并标记
    /// deprecated，而项目测试仍用 stable（还没有新名字）—— 两个工具链都必须零 warning 编译。
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

    /// 当前线程是否处于系统模态循环内（**全局**标记，见 `modal_loops` 字段文档）。
    #[cfg(windows)]
    pub(crate) fn in_modal_loop(&self) -> bool {
        self.modal_loops.load(Ordering::Acquire) > 0
    }

    /// 仅测试（macOS）：当前是否有一个尚未取走的帧唤醒（验证跨线程发布路径真的叫醒了
    /// 主线程）。
    ///
    /// **证据边界**：这里只读 `frame_pending` 电平位，证明「生产者置了位」；真实
    /// `CFRunLoopSourceSignal` / `CFRunLoopWakeUp` 的投递由 mac 真机
    /// `acceptance window --seconds 8 --hz 1`（静止窗口每拍恢复上屏）人工验证。
    #[cfg(all(test, any(target_os = "macos", windows)))]
    pub(crate) fn has_pending_wake(&self) -> bool {
        self.frame_pending.load(Ordering::Acquire)
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

    /// RB-14：模态 tick 的推迟帧兜底 —— `wake_later` 只在延迟后置位一次唤醒；重复挂起合并成
    /// 一个线程/一次 post（不得立即 post，也不得每次模态 tick 都起线程）。
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

    /// B1：系统模态循环的**全局**深度计数 —— 支持嵌套、退出绝不下溢，且不触碰唤醒位。
    ///
    /// 为什么必须全局：`wake` 的目标是 attach 的首窗，而被拖动 / 缩放的窗口不一定是它；
    /// 判定若读窗口自己的 `size_move`，拖动非首窗时模态 tick 永远不触发（报告 §1）。
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
        // 未配对的 exit（窗口在模态循环里被销毁、收不到 WM_EXITSIZEMOVE）不得下溢成 u32::MAX。
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

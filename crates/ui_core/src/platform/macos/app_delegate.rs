//! `NSApplicationDelegate`：协商式退出的唯一入口（R09-16）。
//!
//! 没有 delegate 时 AppKit 收到 `-[NSApplication terminate:]`（Dock 右键退出、`osascript quit`、
//! Cmd+Q、注销）会直接 `replyToApplicationShouldTerminate:YES` → `willTerminate` → `exit()`：
//! `LoopState::drop` / `WindowRenderer::drop` 全部不执行（`Box<App>` 不释放、display link 不
//! invalidate、GPU 线程不 join、设备丢失诊断来不及落盘）。有了 delegate，退出变成一次**握手**：
//!
//! 1. `applicationShouldTerminate:` 返回 `NSTerminateLater` 并唤醒泵（`terminate:` 由此转成引擎的
//!    关闭流程，与点红点/Cmd-W 走同一个 `App::request_close` 裁决点 —— 应用因此可以 **veto**）；
//! 2. 泵关完所有窗口、`App` 与各 `Renderer` 析构完之后，`run()` 调
//!    `replyToApplicationShouldTerminate:`：允许 → AppKit 继续终止（`applicationWillTerminate:`
//!    → `exit`）；被 veto → 取消终止，应用继续跑。
//!
//! `applicationShouldTerminateAfterLastWindowClosed:` 恒 `false`：退出时机由引擎决定
//! （最后一个窗口关闭后泵自己收尾），AppKit 不应抢先终止进程。

use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
    runtime::ProtocolObject,
};
use objc2_app_kit::{NSApplication, NSApplicationDelegate, NSApplicationTerminateReply};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol};

use crate::platform::{dispatcher::Dispatcher, macos::wake};

/// 握手状态：空闲 / 已请求（等泵裁决）/ 已允许（可以回复 AppKit）。
///
/// 三个值都只在主线程读写；`Arc` 只是让 delegate（ObjC 对象）与泵（`LoopState`）共享同一份，
/// 不必在两边各存一个「是否正在退出」的副本。
#[derive(Debug, Default)]
pub(crate) struct QuitHandshake {
    state: AtomicU8,
}

/// `applicationShouldTerminate:` 尚未被调用过。
const IDLE: u8 = 0;
/// 引擎正在跑关闭流程（AppKit 等 `replyToApplicationShouldTerminate:`）。
const PENDING: u8 = 1;
/// 引擎已经完成收尾，可以终止。
const ALLOWED: u8 = 2;

impl QuitHandshake {
    /// 建一份共享状态（`run()` 里持有，delegate 持同一份）。
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 记录退出请求（幂等）。`true` = 这是第一次（泵需要开始关闭流程）。
    pub(crate) fn request(&self) -> bool {
        self.state
            .compare_exchange(IDLE, PENDING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// 是否有待裁决的退出请求（泵每轮检查）。
    pub(crate) fn pending(&self) -> bool {
        self.state.load(Ordering::SeqCst) == PENDING
    }

    /// 是否已允许终止（`run()` 返回后据此回复 AppKit）。
    pub(crate) fn allowed(&self) -> bool {
        self.state.load(Ordering::SeqCst) == ALLOWED
    }

    /// 引擎收尾完成：允许 AppKit 终止。
    pub(crate) fn allow(&self) {
        self.state.store(ALLOWED, Ordering::SeqCst);
    }

    /// 应用拒绝了退出（某个窗口的 `on_close` 返回 `false`）：回到空闲，应用继续运行。
    pub(crate) fn veto(&self) {
        self.state.store(IDLE, Ordering::SeqCst);
    }
}

/// delegate 的 ivar。
struct AppDelegateIvars {
    dispatcher: Arc<Dispatcher>,
    quit: Arc<QuitHandshake>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KytrixAppDelegate"]
    #[ivars = AppDelegateIvars]
    pub(crate) struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            // 注意：`define_class!` 的方法体被塞进 `extern "C-unwind" fn -> <Ret as
            // ConvertReturn<()>>::Inner` 里，方法体里的 `return` 会直接返回**归一化后的类型**。
            // `NSApplicationTerminateReply` 的 `Inner` 是它自己（恒等），`return` 碰巧可用，
            // 但这里统一写成表达式，避免以后换成 `bool`/`Retained` 返回类型时踩同一个坑。
            if self.ivars().quit.allowed() {
                // 泵已经关完窗口、`App`/`Renderer` 也已析构：放行（willTerminate → exit）。
                NSApplicationTerminateReply::TerminateNow
            } else {
                // 首次请求（或正在处理中）：转成引擎的关闭流程并**延迟**退出。协议要求随后调用
                // `replyToApplicationShouldTerminate:`，那一拍在 `run()` 收尾之后。
                self.ivars().quit.request();
                self.ivars().dispatcher.wake();
                NSApplicationTerminateReply::TerminateLater
            }
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _sender: &NSApplication) -> bool {
            // 退出时机由引擎决定（最后一个窗口关闭后泵自己收尾）；AppKit 不得抢先终止。
            false
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            // 走到这里说明 AppKit 已经决定终止（引擎握手已完成，或系统强制退出）。
            // 撤掉 run loop 上的泵：退出过程中不再有回调进 `LoopState`（幂等，`uninstall` 自己
            // 处理「已经卸过」的情况）。
            wake::uninstall();
        }
    }
);

impl AppDelegate {
    pub(crate) fn new(
        mtm: MainThreadMarker,
        dispatcher: Arc<Dispatcher>,
        quit: Arc<QuitHandshake>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AppDelegateIvars { dispatcher, quit });
        // SAFETY: `init` 是 NSObject 的指定初始化器；`this` 由 `alloc` 产生且未初始化。
        unsafe { msg_send![super(this), init] }
    }

    /// 交给 `NSApplication::setDelegate:` 的协议视图。
    pub(crate) fn protocol(&self) -> &ProtocolObject<dyn NSApplicationDelegate> {
        // SAFETY（逻辑）: 上面刚 `unsafe impl NSApplicationDelegate for AppDelegate`。
        ProtocolObject::from_ref(self)
    }
}

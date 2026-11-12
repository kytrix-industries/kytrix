//! 跨线程唤醒（`CFRunLoopSource`）+ 动画定时器（`CFRunLoopTimer`）
//! （`design/phase-12-macos-linux.md` §7.1）。
//!
//! `CFRunLoopSourceSignal` / `CFRunLoopWakeUp` / `CFRunLoopTimerInvalidate` 都是 CoreFoundation
//! 文档保证线程安全（或只从主线程调用）的操作。source 的 `perform` 回调在主线程的 run loop 上
//! 执行泵，因此「唤醒 → 出帧」不依赖 NSEvent 到达；空闲时定时器**被移除**（不轮询，I3）。
//!
//! 句柄以 `Box::into_raw` 泄漏到进程结束（一个进程只有一个主 run loop）。

use std::{
    cell::RefCell,
    ffi::c_void,
    sync::atomic::{AtomicUsize, Ordering},
};

use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFRetained, CFRunLoop, CFRunLoopMode, CFRunLoopSource,
    CFRunLoopSourceContext, CFRunLoopTimer, CFRunLoopTimerContext, kCFRunLoopCommonModes,
    kCFRunLoopDefaultMode,
};

/// 泵回调（`event_loop::pump_callback`）。
pub(crate) type PerformFn = unsafe extern "C-unwind" fn(*mut c_void);
/// 定时器回调（`event_loop::timer_callback`）。
pub(crate) type TimerFn = unsafe extern "C-unwind" fn(*mut CFRunLoopTimer, *mut c_void);

struct Wake {
    run_loop: CFRetained<CFRunLoop>,
    source: CFRetained<CFRunLoopSource>,
    timer_fn: TimerFn,
    state: *mut c_void,
    timer: RefCell<Option<CFRetained<CFRunLoopTimer>>>,
}

/// `0` = 尚未安装（`signal()` 返回 `false`，主线程的 drain-then-check 兜住）。
static WAKE: AtomicUsize = AtomicUsize::new(0);

/// 在主线程安装唤醒源（`state` 原样交给两个回调）。
pub(crate) fn install(state: *mut c_void, perform: PerformFn, timer_fn: TimerFn) -> bool {
    let Some(run_loop) = CFRunLoop::main() else {
        return false;
    };
    let mut context = CFRunLoopSourceContext {
        version: 0,
        info: state,
        retain: None,
        release: None,
        copyDescription: None,
        equal: None,
        hash: None,
        schedule: None,
        cancel: None,
        perform: Some(perform),
    };
    // SAFETY: `context` 是栈上值，`CFRunLoopSourceCreate` 同步拷贝；`info` 在进程生命周期内有效。
    let Some(source) = (unsafe { CFRunLoopSource::new(None, 0, &mut context) }) else {
        return false;
    };
    // CF 要求非空 mode（传 `None` 会被当作未定义行为）：source 挂 default mode。
    run_loop.add_source(Some(&source), Some(default_mode()));
    let wake = Box::into_raw(Box::new(Wake {
        run_loop,
        source,
        timer_fn,
        state,
        timer: RefCell::new(None),
    }));
    WAKE.store(wake as usize, Ordering::Release);
    true
}

fn wake() -> Option<&'static Wake> {
    let pointer = WAKE.load(Ordering::Acquire);
    if pointer == 0 {
        return None;
    }
    // SAFETY: `pointer` 来自 `install` 的 `Box::into_raw`，在 `uninstall` 之前一直有效。
    Some(unsafe { &*(pointer as *const Wake) })
}

/// default run loop mode（CF 的 `kCFRunLoopDefaultMode`；读取 extern static 需要 unsafe）。
fn default_mode() -> &'static CFRunLoopMode {
    // SAFETY: Foundation 保证该 static 在进程生命周期内非空。
    unsafe { kCFRunLoopDefaultMode }.expect("kCFRunLoopDefaultMode is never null")
}

/// 卸载（主线程，`run` 返回之前调用）：移除 source / 定时器，并把 `Wake` 还原成 `Box`。
///
/// 之后再调 [`signal`] 会走「尚未安装」分支，不会碰到已释放的内存。
pub(crate) fn uninstall() {
    let Some(wake) = wake() else {
        return;
    };
    wake.run_loop
        .remove_source(Some(&wake.source), Some(default_mode()));
    if let Some(timer) = wake.timer.borrow_mut().take() {
        wake.run_loop
            .remove_timer(Some(&timer), Some(default_mode()));
        timer.invalidate();
    }
    let pointer = WAKE.swap(0, Ordering::AcqRel);
    if pointer == 0 {
        return;
    }
    // SAFETY: 指针来自 `install` 的 `Box::into_raw`，且 `WAKE` 已换回 0（不会再被 `wake()` 取到）。
    drop(unsafe { Box::from_raw(pointer as *mut Wake) });
}

/// `common modes` 的定时器（滚动/事件跟踪期间也继续 tick）。
fn common_mode() -> Option<&'static CFRunLoopMode> {
    // SAFETY: Foundation 保证该 static 在进程生命周期内非空。
    unsafe { kCFRunLoopCommonModes }
}

/// 任意线程：打断主线程的阻塞等待；`false` = 尚未安装。
pub(crate) fn signal() -> bool {
    let Some(wake) = wake() else {
        return false;
    };
    wake.source.signal();
    wake.run_loop.wake_up();
    true
}

/// 动画定时器开关（主线程；`pump` 每轮按 `app.has_animations()` 同步）。
///
/// 打开：建一个 60 Hz 的 repeating timer 并挂到 default mode；关闭：`invalidate` + 从 run loop 移除。
pub(crate) fn set_animating(active: bool, interval: f64) {
    let Some(wake) = wake() else {
        return;
    };
    let mut timer = wake.timer.borrow_mut();
    match (active, timer.as_ref()) {
        (true, None) => {
            let mut context = CFRunLoopTimerContext {
                version: 0,
                info: wake.state,
                retain: None,
                release: None,
                copyDescription: None,
            };
            // SAFETY: `context` 是栈上值，`CFRunLoopTimerCreate` 同步拷贝；`info` 生命周期同上。
            let created = unsafe {
                // `interval > 0` ⇒ 自动重排（CF 的 repeating 语义）；无 `repeats` 参数。
                CFRunLoopTimer::new(
                    None,
                    CFAbsoluteTimeGetCurrent() + interval,
                    interval,
                    0,
                    0,
                    Some(wake.timer_fn),
                    &mut context,
                )
            };
            if let Some(created) = created {
                wake.run_loop.add_timer(Some(&created), common_mode());
                *timer = Some(created);
            }
        }
        (false, Some(existing)) => {
            wake.run_loop.remove_timer(Some(existing), common_mode());
            existing.invalidate();
            *timer = None;
        }
        _ => {}
    }
}

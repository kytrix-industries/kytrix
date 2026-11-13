//! 跨线程唤醒（`CFRunLoopSource`）+ 连续帧定时器（`CFRunLoopTimer`）
//! （`design/phase-12-macos-linux.md` §7.1）。
//!
//! `CFRunLoopSourceSignal` / `CFRunLoopWakeUp` / `CFRunLoopTimerInvalidate` 都是 CoreFoundation
//! 文档保证线程安全的操作。source 的 `perform` 回调在主线程的 run loop 上执行泵，因此
//! 「唤醒 → 出帧」不依赖 NSEvent 到达；空闲时定时器**被移除**（不轮询，I3）。
//!
//! **生命周期**：句柄表放在 `Mutex<Option<Box<Wake>>>` 里，`signal()` 与 `uninstall()` 都通过
//! 同一把锁串行化 —— `signal` 在临界区内完成 signal/wake，`uninstall` 也只在自己持锁时释放
//! `Wake`，因此不存在「另一线程刚取到指针、这边已经 free」的 UAF（P14 终审项 2）。

use std::{
    cell::RefCell,
    ffi::c_void,
    sync::{Mutex, MutexGuard},
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

/// `CFRunLoopTimerContext.info` 的载荷（只在主线程解引用）。
struct StatePtr(*mut c_void);

// SAFETY: 该指针只作为 `CFRunLoopTimerContext.info` 存进句柄表，解引用只发生在主线程的
// 定时器回调（`event_loop::timer_callback`）里；放进 `Mutex` 只是为了满足 `static` 的 `Send`。
unsafe impl Send for StatePtr {}

/// 句柄表（`run` 的主线程安装，任意线程 `signal`，退出前主线程 `uninstall`）。
struct Wake {
    run_loop: CFRetained<CFRunLoop>,
    source: CFRetained<CFRunLoopSource>,
    timer_fn: TimerFn,
    state: StatePtr,
    timer: RefCell<Option<CFRetained<CFRunLoopTimer>>>,
}

// SAFETY: 所有访问都经由 `WAKE` 的互斥锁；`CFRunLoop` / `CFRunLoopSource` 是 CF 文档保证
// 线程安全的对象（signal / wakeUp 允许跨线程），`timer` 只在持锁的主线程路径上访问。
unsafe impl Send for Wake {}

/// `None` = 尚未安装（`signal()` 返回 `false`，主线程的 drain-then-check 兜住）。
static WAKE: Mutex<Option<Box<Wake>>> = Mutex::new(None);

/// 拿锁；毒化（持有者 panic）时按「尚未安装」处理，不 panic 传播。
fn lock() -> Option<MutexGuard<'static, Option<Box<Wake>>>> {
    match WAKE.lock() {
        Ok(guard) => Some(guard),
        Err(poisoned) => Some(poisoned.into_inner()),
    }
}

/// default run loop mode（CF 的 `kCFRunLoopDefaultMode`；读取 extern static 需要 unsafe）。
fn default_mode() -> &'static CFRunLoopMode {
    // SAFETY: Foundation 保证该 static 在进程生命周期内非空。
    unsafe { kCFRunLoopDefaultMode }.expect("kCFRunLoopDefaultMode is never null")
}

/// `common modes` 的定时器（滚动/事件跟踪期间也继续 tick）。
fn common_mode() -> Option<&'static CFRunLoopMode> {
    // SAFETY: Foundation 保证该 static 在进程生命周期内非空。
    unsafe { kCFRunLoopCommonModes }
}

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
    // SAFETY: `context` 是栈上值，`CFRunLoopSourceCreate` 同步拷贝；`info` 在 `uninstall` 之前有效。
    let Some(source) = (unsafe { CFRunLoopSource::new(None, 0, &mut context) }) else {
        return false;
    };
    // CF 要求非空 mode（传 `None` 会被当作未定义行为）：source 挂 default mode。
    run_loop.add_source(Some(&source), Some(default_mode()));
    let Some(mut guard) = lock() else {
        return false;
    };
    // 重复安装：先卸掉旧的（正常路径只调一次；这样即便重入也不会泄漏 run loop 引用）。
    if let Some(previous) = guard.take() {
        run_loop.remove_source(Some(&previous.source), Some(default_mode()));
    }
    *guard = Some(Box::new(Wake {
        run_loop,
        source,
        timer_fn,
        state: StatePtr(state),
        timer: RefCell::new(None),
    }));
    true
}

/// 卸载（主线程，`run` 返回之前调用）：移除 source / 定时器，并在持锁状态下释放句柄表。
///
/// 之后再调 [`signal`] 只会看到 `None`，不会碰到已释放的内存。
pub(crate) fn uninstall() {
    let Some(mut guard) = lock() else {
        return;
    };
    let Some(wake) = guard.take() else {
        return;
    };
    wake.run_loop
        .remove_source(Some(&wake.source), Some(default_mode()));
    if let Some(timer) = wake.timer.borrow_mut().take() {
        wake.run_loop.remove_timer(Some(&timer), common_mode());
        timer.invalidate();
    }
    // `wake` 在**仍持锁**时 drop：并发的 `signal()` 要么已经完成、要么拿到 `None`。
    drop(wake);
}

/// 任意线程：打断主线程的阻塞等待；`false` = 尚未安装。
pub(crate) fn signal() -> bool {
    let Some(guard) = lock() else {
        return false;
    };
    let Some(wake) = guard.as_ref() else {
        return false;
    };
    wake.source.signal();
    wake.run_loop.wake_up();
    true
}

/// 连续帧定时器开关（主线程；`pump` 每轮按 `app.needs_continuous_frames()` 同步）。
///
/// 打开：建一个 60 Hz 的 repeating timer 并挂到 common modes；关闭：`invalidate` + 从 run loop 移除。
pub(crate) fn set_animating(active: bool, interval: f64) {
    let Some(guard) = lock() else {
        return;
    };
    let Some(wake) = guard.as_ref() else {
        return;
    };
    let mut timer = wake.timer.borrow_mut();
    match (active, timer.as_ref()) {
        (true, None) => {
            let mut context = CFRunLoopTimerContext {
                version: 0,
                info: wake.state.0,
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

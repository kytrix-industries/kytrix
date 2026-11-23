use std::{
    cell::RefCell,
    ffi::c_void,
    sync::{Mutex, MutexGuard},
    time::Instant,
};

use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFRetained, CFRunLoop, CFRunLoopMode, CFRunLoopSource,
    CFRunLoopSourceContext, CFRunLoopTimer, CFRunLoopTimerContext, kCFRunLoopCommonModes,
};

use crate::platform::macos_pump::keep_armed_deadline;

pub(crate) type PerformFn = unsafe extern "C-unwind" fn(*mut c_void);
pub(crate) type TimerFn = unsafe extern "C-unwind" fn(*mut CFRunLoopTimer, *mut c_void);

struct StatePtr(*mut c_void);

unsafe impl Send for StatePtr {}

struct ArmedDeadline {
    timer: CFRetained<CFRunLoopTimer>,
    target: Instant,
    fired: bool,
}

struct Wake {
    run_loop: CFRetained<CFRunLoop>,
    source: CFRetained<CFRunLoopSource>,
    timer_fn: TimerFn,
    state: StatePtr,
    timer: RefCell<Option<CFRetained<CFRunLoopTimer>>>,
    deadline: RefCell<Option<ArmedDeadline>>,
}

unsafe impl Send for Wake {}

static WAKE: Mutex<Option<Box<Wake>>> = Mutex::new(None);

fn lock() -> Option<MutexGuard<'static, Option<Box<Wake>>>> {
    match WAKE.lock() {
        Ok(guard) => Some(guard),
        Err(poisoned) => Some(poisoned.into_inner()),
    }
}

pub(crate) fn common_mode() -> Option<&'static CFRunLoopMode> {
    unsafe { kCFRunLoopCommonModes }
}

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
    let Some(source) = (unsafe { CFRunLoopSource::new(None, 0, &mut context) }) else {
        return false;
    };
    run_loop.add_source(Some(&source), common_mode());
    let Some(mut guard) = lock() else {
        return false;
    };
    if let Some(previous) = guard.take() {
        run_loop.remove_source(Some(&previous.source), common_mode());
    }
    *guard = Some(Box::new(Wake {
        run_loop,
        source,
        timer_fn,
        state: StatePtr(state),
        timer: RefCell::new(None),
        deadline: RefCell::new(None),
    }));
    true
}

pub(crate) fn uninstall() {
    let Some(mut guard) = lock() else {
        return;
    };
    let Some(wake) = guard.take() else {
        return;
    };
    wake.run_loop
        .remove_source(Some(&wake.source), common_mode());
    if let Some(timer) = wake.timer.borrow_mut().take() {
        wake.run_loop.remove_timer(Some(&timer), common_mode());
        timer.invalidate();
    }
    if let Some(armed) = wake.deadline.borrow_mut().take() {
        wake.run_loop
            .remove_timer(Some(&armed.timer), common_mode());
        armed.timer.invalidate();
    }
    drop(wake);
}

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
            let created = unsafe {
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

pub(crate) fn set_deadline(deadline: Option<Instant>) {
    let Some(guard) = lock() else {
        return;
    };
    let Some(wake) = guard.as_ref() else {
        return;
    };
    let mut slot = wake.deadline.borrow_mut();
    let now = Instant::now();
    let armed = slot.as_ref().map(|armed| (armed.target, armed.fired));
    if keep_armed_deadline(deadline, armed, now) {
        return;
    }
    if let Some(armed) = slot.take() {
        wake.run_loop
            .remove_timer(Some(&armed.timer), common_mode());
        armed.timer.invalidate();
    }
    let Some(target) = deadline else {
        return;
    };
    let mut context = CFRunLoopTimerContext {
        version: 0,
        info: wake.state.0,
        retain: None,
        release: None,
        copyDescription: None,
    };
    let created = unsafe {
        CFRunLoopTimer::new(
            None,
            CFAbsoluteTimeGetCurrent() + target.saturating_duration_since(now).as_secs_f64(),
            0.0,
            0,
            0,
            Some(wake.timer_fn),
            &mut context,
        )
    };
    if let Some(created) = created {
        wake.run_loop.add_timer(Some(&created), common_mode());
        *slot = Some(ArmedDeadline {
            timer: created,
            target,
            fired: false,
        });
    }
}

pub(crate) fn note_deadline_fired(fired: *mut CFRunLoopTimer) {
    let Some(guard) = lock() else {
        return;
    };
    let Some(wake) = guard.as_ref() else {
        return;
    };
    let mut slot = wake.deadline.borrow_mut();
    let Some(armed) = slot.as_mut() else {
        return;
    };
    if std::ptr::eq(&*armed.timer, fired.cast_const()) {
        armed.fired = true;
    }
}

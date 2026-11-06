//! The process-global llama backend.

use std::{
    ffi::{c_char, c_void},
    sync::{Mutex, OnceLock, PoisonError},
};

use llama_sys::ffi;

/// The one backend this process initializes.
static BACKEND: OnceLock<()> = OnceLock::new();

/// Guards the first initialization; `llama_backend_init` may run only once per process.
static INIT: Mutex<()> = Mutex::new(());

/// Initializes the process-global backend and decides whether llama.cpp logs.
///
/// `llama_backend_init` returns no error and must run before any other llama.cpp
/// entry point. The first caller alone decides whether llama.cpp logs, because
/// the log callback is global and cannot be changed back once another caller has
/// been served.
pub(super) fn init(log_llama: bool) {
    if BACKEND.get().is_some() {
        return;
    }

    let _guard = INIT.lock().unwrap_or_else(PoisonError::into_inner);
    if BACKEND.get().is_some() {
        return;
    }

    // SAFETY: the once-only gate above guarantees this runs at most once, before
    // any other llama.cpp entry point in this process.
    unsafe {
        ffi::llama_backend_init();
    }
    if !log_llama {
        // SAFETY: `void_log` has the exact `ggml_log_callback` signature and
        // ignores all of its arguments.
        unsafe {
            ffi::llama_log_set(Some(void_log), std::ptr::null_mut());
        }
    }
    let _ = BACKEND.set(());
}

/// Whether this llama.cpp build can offload any layer to a GPU.
pub(super) fn supports_gpu_offload() -> bool {
    // SAFETY: this only reads llama.cpp's compiled backend registry.
    unsafe { ffi::llama_supports_gpu_offload() }
}

/// A log callback that discards every line.
unsafe extern "C" fn void_log(
    _level: ffi::ggml_log_level,
    _text: *const c_char,
    _user_data: *mut c_void,
) {
}

#[cfg(test)]
mod tests {
    use super::{init, supports_gpu_offload};

    #[test]
    fn the_backend_is_initialized_once_and_reused() {
        init(false);
        init(false);
        let _ = supports_gpu_offload();
    }
}

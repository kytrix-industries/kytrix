use std::{
    ffi::{c_char, c_void},
    sync::{Mutex, OnceLock, PoisonError},
};

use llama_sys::ffi;

static BACKEND: OnceLock<()> = OnceLock::new();

static INIT: Mutex<()> = Mutex::new(());

pub(super) fn init(log_llama: bool) {
    if BACKEND.get().is_some() {
        return;
    }

    let _guard = INIT.lock().unwrap_or_else(PoisonError::into_inner);
    if BACKEND.get().is_some() {
        return;
    }

    unsafe {
        ffi::llama_backend_init();
    }
    if !log_llama {
        unsafe {
            ffi::llama_log_set(Some(void_log), std::ptr::null_mut());
        }
    }
    let _ = BACKEND.set(());
}

pub(super) fn supports_gpu_offload() -> bool {
    unsafe { ffi::llama_supports_gpu_offload() }
}

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

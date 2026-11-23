use std::{marker::PhantomData, ptr::NonNull};

use llama_sys::ffi;

use super::{batch::Batch, model::Model};
use crate::error::{ContextCreationError, DecodeError};

pub(super) struct Context<'a> {
    ptr: NonNull<ffi::llama_context>,
    _model: PhantomData<&'a Model>,
}

impl<'a> Context<'a> {
    pub(super) fn new(
        model: *mut ffi::llama_model,
        params: ffi::llama_context_params,
    ) -> Result<Self, ContextCreationError> {
        let context = unsafe { ffi::llama_init_from_model(model, params) };
        let context = NonNull::new(context).ok_or(ContextCreationError::NullContext)?;

        Ok(Self {
            ptr: context,
            _model: PhantomData,
        })
    }

    pub(super) fn decode(&mut self, batch: &mut Batch) -> Result<(), DecodeError> {
        let code = unsafe { ffi::llama_decode(self.ptr.as_ptr(), batch.raw()) };
        if code == 0 {
            Ok(())
        } else {
            Err(DecodeError::Failed { code })
        }
    }

    pub(super) fn clear_kv_cache(&mut self) {
        unsafe {
            let memory = ffi::llama_get_memory(self.ptr.as_ptr());
            ffi::llama_memory_clear(memory, true);
        }
    }

    pub(super) fn raw_ptr(&self) -> *mut ffi::llama_context {
        self.ptr.as_ptr()
    }
}

impl Drop for Context<'_> {
    fn drop(&mut self) {
        unsafe {
            ffi::llama_free(self.ptr.as_ptr());
        }
    }
}

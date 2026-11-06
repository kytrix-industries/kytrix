//! The single llama context the worker owns.

use std::{marker::PhantomData, ptr::NonNull};

use llama_sys::ffi;

use super::{batch::Batch, model::Model};
use crate::error::{ContextCreationError, DecodeError};

/// A context whose lifetime is tied to its model.
pub(super) struct Context<'a> {
    ptr: NonNull<ffi::llama_context>,
    _model: PhantomData<&'a Model>,
}

impl<'a> Context<'a> {
    /// Creates a context for `model`.
    pub(super) fn new(
        model: *mut ffi::llama_model,
        params: ffi::llama_context_params,
    ) -> Result<Self, ContextCreationError> {
        // SAFETY: the model is live for `'a`, and the parameters are a plain
        // by-value C struct.
        let context = unsafe { ffi::llama_init_from_model(model, params) };
        let context = NonNull::new(context).ok_or(ContextCreationError::NullContext)?;

        Ok(Self {
            ptr: context,
            _model: PhantomData,
        })
    }

    /// Evaluates `batch` on this context.
    pub(super) fn decode(&mut self, batch: &mut Batch) -> Result<(), DecodeError> {
        // SAFETY: both handles are live, and `llama_decode` borrows the batch
        // only for the duration of the call.
        let code = unsafe { ffi::llama_decode(self.ptr.as_ptr(), batch.raw()) };
        if code == 0 {
            Ok(())
        } else {
            Err(DecodeError::Failed { code })
        }
    }

    /// Drops every sequence from the KV cache.
    pub(super) fn clear_kv_cache(&mut self) {
        // SAFETY: the context is live, and the memory handle it returns is
        // borrowed from it.
        unsafe {
            let memory = ffi::llama_get_memory(self.ptr.as_ptr());
            ffi::llama_memory_clear(memory, true);
        }
    }

    /// The raw handle, for calls that need the context but do not own it.
    pub(super) fn raw_ptr(&self) -> *mut ffi::llama_context {
        self.ptr.as_ptr()
    }
}

impl Drop for Context<'_> {
    fn drop(&mut self) {
        // SAFETY: this is the only owner of the context handle.
        unsafe {
            ffi::llama_free(self.ptr.as_ptr());
        }
    }
}

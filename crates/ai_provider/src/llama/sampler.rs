//! The sampler chain the worker builds for one request.

use std::ptr::NonNull;

use llama_sys::ffi;

use super::{config::SamplingParams, context::Context};
use crate::error::{ProviderError, SampleError};

/// A llama.cpp sampler chain that owns its child samplers.
pub(super) struct Sampler {
    ptr: NonNull<ffi::llama_sampler>,
}

impl Sampler {
    /// Builds the chain the translation model card specifies.
    pub(super) fn chain(params: &SamplingParams, n_vocab: i32) -> Result<Self, ProviderError> {
        // SAFETY: this only fills a plain C struct with llama.cpp's defaults.
        let mut chain_params = unsafe { ffi::llama_sampler_chain_default_params() };
        chain_params.no_perf = false;
        // SAFETY: the parameters are a plain by-value C struct.
        let chain = NonNull::new(unsafe { ffi::llama_sampler_chain_init(chain_params) })
            .ok_or(ProviderError::SamplerCreation)?;
        let sampler = Self { ptr: chain };
        let chain_ptr = sampler.ptr.as_ptr();

        let add = |child: *mut ffi::llama_sampler| -> Result<(), ProviderError> {
            let child = NonNull::new(child).ok_or(ProviderError::SamplerCreation)?;
            // SAFETY: both the chain and the child are live, and the chain takes
            // ownership of the child.
            unsafe {
                ffi::llama_sampler_chain_add(chain_ptr, child.as_ptr());
            }
            Ok(())
        };

        // SAFETY for every constructor: the arguments are scalars and the
        // returned sampler is owned by the chain once added.
        add(unsafe {
            ffi::llama_sampler_init_penalties(n_vocab, 64, params.repetition_penalty, 0.0, 0.0)
        })?;
        add(unsafe { ffi::llama_sampler_init_top_k(params.top_k) })?;
        add(unsafe { ffi::llama_sampler_init_top_p(params.top_p, 1) })?;
        add(unsafe { ffi::llama_sampler_init_temp(params.temperature) })?;
        add(unsafe { ffi::llama_sampler_init_dist(params.seed) })?;

        Ok(sampler)
    }

    /// Samples the next token from `context`'s logits.
    ///
    /// `llama_sampler_sample` both samples and accepts the token, so the worker
    /// must not call `llama_sampler_accept` afterwards.
    pub(super) fn sample(
        &self,
        context: &Context<'_>,
        index: i32,
    ) -> Result<ffi::llama_token, SampleError> {
        // SAFETY: the sampler and context are live for the duration of the call.
        let token =
            unsafe { ffi::llama_sampler_sample(self.ptr.as_ptr(), context.raw_ptr(), index) };
        if token < 0 {
            Err(SampleError::InvalidToken(token))
        } else {
            Ok(token)
        }
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        // SAFETY: this is the only owner of the chain; it frees its children.
        unsafe {
            ffi::llama_sampler_free(self.ptr.as_ptr());
        }
    }
}

use std::ptr::NonNull;

use llama_sys::ffi;

use super::{config::SamplingParams, context::Context};
use crate::error::{ProviderError, SampleError};

pub(super) struct Sampler {
    ptr: NonNull<ffi::llama_sampler>,
}

impl Sampler {
    pub(super) fn chain(params: &SamplingParams, n_vocab: i32) -> Result<Self, ProviderError> {
        let mut chain_params = unsafe { ffi::llama_sampler_chain_default_params() };
        chain_params.no_perf = false;
        let chain = NonNull::new(unsafe { ffi::llama_sampler_chain_init(chain_params) })
            .ok_or(ProviderError::SamplerCreation)?;
        let sampler = Self { ptr: chain };
        let chain_ptr = sampler.ptr.as_ptr();

        let add = |child: *mut ffi::llama_sampler| -> Result<(), ProviderError> {
            let child = NonNull::new(child).ok_or(ProviderError::SamplerCreation)?;
            unsafe {
                ffi::llama_sampler_chain_add(chain_ptr, child.as_ptr());
            }
            Ok(())
        };

        add(unsafe {
            ffi::llama_sampler_init_penalties(n_vocab, 64, params.repetition_penalty, 0.0, 0.0)
        })?;
        add(unsafe { ffi::llama_sampler_init_top_k(params.top_k) })?;
        add(unsafe { ffi::llama_sampler_init_top_p(params.top_p, 1) })?;
        add(unsafe { ffi::llama_sampler_init_temp(params.temperature) })?;
        add(unsafe { ffi::llama_sampler_init_dist(params.seed) })?;

        Ok(sampler)
    }

    pub(super) fn sample(
        &self,
        context: &Context<'_>,
        index: i32,
    ) -> Result<ffi::llama_token, SampleError> {
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
        unsafe {
            ffi::llama_sampler_free(self.ptr.as_ptr());
        }
    }
}

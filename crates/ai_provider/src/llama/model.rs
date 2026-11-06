//! The model and the C API calls that only need its vocabulary.

use std::{ffi::CString, path::Path, ptr::NonNull};

use llama_sys::ffi;

use super::{backend, context::Context};
use crate::error::{
    ContextCreationError, DetokenizeError, ModelLoadError, ProviderError, TokenizeError,
};

/// A loaded GGUF model.
pub(super) struct Model {
    ptr: NonNull<ffi::llama_model>,
}

impl Model {
    /// Loads `path`, offloading as many layers as the configured limit allows.
    pub(super) fn load(path: &Path, gpu_layers: u32) -> Result<Self, ProviderError> {
        let path = path.to_path_buf();
        let path_str = path
            .to_str()
            .ok_or(ModelLoadError::NonUtf8Path)
            .map_err(|source| ProviderError::ModelLoad {
                path: path.clone(),
                source,
            })?;
        let c_path = CString::new(path_str)
            .map_err(|_| ModelLoadError::InteriorNul)
            .map_err(|source| ProviderError::ModelLoad {
                path: path.clone(),
                source,
            })?;

        let mut params =
            // SAFETY: this only fills a plain C struct with llama.cpp's defaults.
            unsafe { ffi::llama_model_default_params() };
        params.n_gpu_layers = if backend::supports_gpu_offload() {
            i32::try_from(gpu_layers).unwrap_or(i32::MAX)
        } else {
            0
        };

        // SAFETY: the path is a live NUL-terminated C string for the duration of
        // the call, and `params` is a plain by-value C struct.
        let model = unsafe { ffi::llama_model_load_from_file(c_path.as_ptr(), params) };
        let model = NonNull::new(model)
            .ok_or(ModelLoadError::NullModel)
            .map_err(|source| ProviderError::ModelLoad {
                path: path.clone(),
                source,
            })?;

        Ok(Self { ptr: model })
    }

    /// The model's vocabulary.
    fn vocab(&self) -> *const ffi::llama_vocab {
        // SAFETY: the model is live and `llama_model_get_vocab` returns a
        // vocabulary owned by it.
        let vocab = unsafe { ffi::llama_model_get_vocab(self.ptr.as_ptr()) };
        debug_assert!(!vocab.is_null(), "a loaded model has a vocabulary");
        vocab
    }

    /// The number of tokens in the vocabulary.
    pub(super) fn n_vocab(&self) -> i32 {
        // SAFETY: the vocabulary is live for the duration of the borrow.
        unsafe { ffi::llama_vocab_n_tokens(self.vocab()) }
    }

    /// Whether `token` ends generation.
    pub(super) fn is_eog_token(&self, token: ffi::llama_token) -> bool {
        // SAFETY: the vocabulary is live for the duration of the borrow.
        unsafe { ffi::llama_vocab_is_eog(self.vocab(), token) }
    }

    /// The token's attribute bitmask.
    pub(super) fn token_attr(&self, token: ffi::llama_token) -> ffi::llama_token_attr {
        // SAFETY: the vocabulary is live for the duration of the borrow.
        unsafe { ffi::llama_vocab_get_attr(self.vocab(), token) }
    }

    /// Tokenizes `text`.
    ///
    /// `parse_special` is the security boundary: template scaffolding parses
    /// control tokens, user content does not.
    pub(super) fn tokenize(
        &self,
        text: &str,
        parse_special: bool,
    ) -> Result<Vec<ffi::llama_token>, TokenizeError> {
        if text.as_bytes().contains(&0) {
            return Err(TokenizeError::InteriorNul);
        }
        let text_len = i32::try_from(text.len()).map_err(|_| TokenizeError::TooLong)?;
        let mut buffer = vec![0; 8.max(text.len() / 2 + 1)];

        loop {
            let capacity = i32::try_from(buffer.len()).map_err(|_| TokenizeError::TooLong)?;
            // SAFETY: `buffer` is writable for `capacity` tokens, `text` is a
            // live byte slice of exactly `text_len` bytes, and the return value
            // is a count or a negative required size.
            let written = unsafe {
                ffi::llama_tokenize(
                    self.vocab(),
                    text.as_ptr().cast(),
                    text_len,
                    buffer.as_mut_ptr(),
                    capacity,
                    false,
                    parse_special,
                )
            };
            if written >= 0 {
                buffer.truncate(written as usize);
                return Ok(buffer);
            }

            let required = written
                .checked_neg()
                .ok_or(TokenizeError::Failed(written))?;
            let required = usize::try_from(required).map_err(|_| TokenizeError::TooLong)?;
            buffer.resize(required, 0);
        }
    }

    /// Converts one token to its raw piece bytes.
    pub(super) fn token_to_piece(
        &self,
        token: ffi::llama_token,
        special: bool,
    ) -> Result<Vec<u8>, DetokenizeError> {
        let mut buffer = vec![0; 32];

        loop {
            let capacity =
                i32::try_from(buffer.len()).map_err(|_| DetokenizeError::TokenToPiece(i32::MAX))?;
            // SAFETY: `buffer` is writable for `capacity` bytes, and the return
            // value is a byte count or a negative required size.
            let written = unsafe {
                ffi::llama_token_to_piece(
                    self.vocab(),
                    token,
                    buffer.as_mut_ptr().cast(),
                    capacity,
                    0,
                    special,
                )
            };
            if written >= 0 {
                buffer.truncate(written as usize);
                return Ok(buffer);
            }

            let required = written
                .checked_neg()
                .ok_or(DetokenizeError::TokenToPiece(written))?;
            let required =
                usize::try_from(required).map_err(|_| DetokenizeError::TokenToPiece(written))?;
            buffer.resize(required, 0);
        }
    }

    /// Creates the one context the worker uses.
    pub(super) fn new_context(
        &self,
        params: ffi::llama_context_params,
    ) -> Result<Context<'_>, ContextCreationError> {
        Context::new(self.ptr.as_ptr(), params)
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        // SAFETY: this is the only owner of the model handle.
        unsafe {
            ffi::llama_model_free(self.ptr.as_ptr());
        }
    }
}

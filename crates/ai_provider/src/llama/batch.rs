use llama_sys::ffi;

use crate::error::{BatchAddError, ProviderError};

pub(super) struct Batch {
    raw: ffi::llama_batch,
    capacity: usize,
}

impl Batch {
    pub(super) fn new(n_tokens: usize, n_seq_max: i32) -> Result<Self, ProviderError> {
        let n_tokens = i32::try_from(n_tokens).map_err(|_| ProviderError::BatchAllocation)?;
        let raw = unsafe { ffi::llama_batch_init(n_tokens, 0, n_seq_max) };
        if raw.token.is_null()
            || raw.pos.is_null()
            || raw.n_seq_id.is_null()
            || raw.seq_id.is_null()
            || raw.logits.is_null()
        {
            unsafe {
                ffi::llama_batch_free(raw);
            }
            return Err(ProviderError::BatchAllocation);
        }

        Ok(Self {
            raw,
            capacity: n_tokens as usize,
        })
    }

    pub(super) fn clear(&mut self) {
        self.raw.n_tokens = 0;
    }

    pub(super) fn add(
        &mut self,
        token: ffi::llama_token,
        pos: ffi::llama_pos,
        logits: bool,
    ) -> Result<(), BatchAddError> {
        let index =
            usize::try_from(self.raw.n_tokens).map_err(|_| BatchAddError::InsufficientSpace {
                capacity: self.capacity,
            })?;
        if index >= self.capacity {
            return Err(BatchAddError::InsufficientSpace {
                capacity: self.capacity,
            });
        }

        unsafe {
            self.raw.token.add(index).write(token);
            self.raw.pos.add(index).write(pos);
            self.raw.n_seq_id.add(index).write(1);
            (*self.raw.seq_id.add(index)).write(0);
            self.raw.logits.add(index).write(i8::from(logits));
        }
        self.raw.n_tokens += 1;

        Ok(())
    }

    pub(super) fn raw(&self) -> ffi::llama_batch {
        self.raw
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        unsafe {
            ffi::llama_batch_free(self.raw);
        }
    }
}

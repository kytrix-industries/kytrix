//! Incremental detokenization for streamed model output.

use llama_sys::ffi;

use super::model::Model;
use crate::error::DetokenizeError;

/// A stateful detokenizer that buffers an incomplete trailing UTF-8 sequence.
pub(super) struct StreamDetokenizer<'a> {
    model: &'a Model,
    buffer: Vec<u8>,
}

impl<'a> StreamDetokenizer<'a> {
    /// Creates a detokenizer over `model`.
    pub(super) fn new(model: &'a Model) -> Self {
        Self {
            model,
            buffer: Vec::new(),
        }
    }

    /// Feeds one token and returns the text that became complete.
    pub(super) fn push(&mut self, token: ffi::llama_token) -> Result<String, DetokenizeError> {
        let piece = self.model.token_to_piece(token, false)?;
        self.buffer.extend_from_slice(&piece);
        self.drain_complete()
    }

    /// Finishes the stream, returning any remaining complete text.
    pub(super) fn finish(mut self) -> Result<String, DetokenizeError> {
        let text = self.drain_complete()?;
        if self.buffer.is_empty() {
            Ok(text)
        } else {
            Err(DetokenizeError::IncompleteUtf8(self.buffer))
        }
    }

    /// Moves the longest valid UTF-8 prefix out of the buffer.
    fn drain_complete(&mut self) -> Result<String, DetokenizeError> {
        take_valid_utf8(&mut self.buffer)
    }
}

/// Drains the longest valid UTF-8 prefix from `buffer`.
///
/// Returns an error only for genuinely malformed bytes; a merely unfinished
/// trailing multi-byte sequence stays buffered for the next token.
fn take_valid_utf8(buffer: &mut Vec<u8>) -> Result<String, DetokenizeError> {
    let valid = match std::str::from_utf8(buffer) {
        Ok(_) => buffer.len(),
        Err(error) if error.error_len().is_some() => {
            return Err(DetokenizeError::InvalidUtf8(error));
        }
        Err(error) => error.valid_up_to(),
    };
    let complete: Vec<u8> = buffer.drain(..valid).collect();
    Ok(String::from_utf8(complete).expect("the prefix up to valid_up_to is valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::take_valid_utf8;
    use crate::error::DetokenizeError;

    #[test]
    fn complete_utf8_is_fully_drained() {
        let mut buffer = b"hello".to_vec();

        assert_eq!(take_valid_utf8(&mut buffer).expect("valid UTF-8"), "hello");
        assert!(buffer.is_empty());
    }

    #[test]
    fn an_incomplete_character_is_retained() {
        let mut buffer = vec![b'a', 0xE2, 0x82];

        assert_eq!(take_valid_utf8(&mut buffer).expect("valid prefix"), "a");
        assert_eq!(buffer, vec![0xE2, 0x82]);

        buffer.push(0xAC);
        assert_eq!(
            take_valid_utf8(&mut buffer).expect("completed character"),
            "€"
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn malformed_bytes_are_surfaced() {
        let mut buffer = vec![b'a', 0xFF, b'b'];

        assert!(matches!(
            take_valid_utf8(&mut buffer),
            Err(DetokenizeError::InvalidUtf8(_))
        ));
    }
}

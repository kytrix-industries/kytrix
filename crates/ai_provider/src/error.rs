//! Errors produced while loading a model and generating text.

use std::{io, path::PathBuf};

use smol_str::SmolStr;
use thiserror::Error;

/// Typed failures from the llama.cpp backend.
///
/// Every variant is `Send` because failures are produced on the `kytrix-llama`
/// worker thread and sent back over a channel; the worker's own tests keep that
/// true. The source types hold only codes, strings and paths.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The configured model path does not name an existing file.
    #[error("model file `{}` does not exist", path.display())]
    ModelFileMissing { path: PathBuf },
    /// llama.cpp could not load the model.
    #[error("failed to load model `{}`: {source}", path.display())]
    ModelLoad {
        path: PathBuf,
        #[source]
        source: ModelLoadError,
    },
    /// llama.cpp could not create the context the model runs in.
    #[error("failed to create the llama context: {source}")]
    ContextCreation {
        #[source]
        source: ContextCreationError,
    },
    /// The prompt and the requested output do not fit in the context.
    #[error("the prompt needs {tokens} tokens and only {limit} are available for it")]
    PromptTooLong { tokens: usize, limit: usize },
    /// The prompt could not be turned into tokens.
    #[error("failed to tokenize the prompt: {source}")]
    Tokenize {
        #[source]
        source: TokenizeError,
    },
    /// A text span still contains a token that cannot be represented as plain text.
    #[error("the content contains a special token that cannot be represented as plain text")]
    SpecialTokenInContent,
    /// A token could not be added to the batch that will be decoded.
    #[error("failed to add a token to the llama batch: {source}")]
    BatchAdd {
        #[source]
        source: BatchAddError,
    },
    /// llama.cpp could not allocate the batch.
    #[error("llama.cpp returned a null batch")]
    BatchAllocation,
    /// llama.cpp failed to evaluate a batch.
    #[error("llama decode failed: {source}")]
    Decode {
        #[source]
        source: DecodeError,
    },
    /// Generated tokens could not be turned back into text.
    #[error("failed to detokenize model output: {source}")]
    Detokenize {
        #[source]
        source: DetokenizeError,
    },
    /// llama.cpp returned a null sampler while building the sampling chain.
    #[error("llama.cpp returned a null sampler")]
    SamplerCreation,
    /// llama.cpp's sampler could not pick a token.
    #[error("llama sampling failed: {source}")]
    Sample {
        #[source]
        source: SampleError,
    },
    /// The worker thread could not be started.
    #[error("failed to spawn the llama worker thread: {source}")]
    WorkerSpawn {
        #[source]
        source: io::Error,
    },
    /// The worker thread is gone, so no request can be served.
    #[error("the llama worker is no longer running")]
    WorkerUnavailable,
    /// The engine configuration cannot be used.
    #[error("invalid engine configuration: {reason}")]
    InvalidConfig { reason: SmolStr },
}

/// Why the model could not be loaded.
#[derive(Debug, Error)]
pub enum ModelLoadError {
    /// llama.cpp takes a C string, so the path must be valid UTF-8.
    #[error("the model path is not valid UTF-8")]
    NonUtf8Path,
    /// A path cannot contain an interior NUL byte.
    #[error("the model path contains a NUL byte")]
    InteriorNul,
    /// llama.cpp reported failure by returning a null model.
    #[error("llama.cpp returned a null model")]
    NullModel,
}

/// Why the context could not be created.
#[derive(Debug, Error)]
pub enum ContextCreationError {
    /// llama.cpp reported failure by returning a null context.
    #[error("llama.cpp returned a null context")]
    NullContext,
}

/// Why tokenization failed.
#[derive(Debug, Error)]
pub enum TokenizeError {
    /// The text contains a NUL byte, which the C API cannot distinguish from
    /// the end of the string.
    #[error("the text contains a NUL byte")]
    InteriorNul,
    /// The text or its token count does not fit in llama.cpp's `i32`.
    #[error("the text is too long for llama.cpp")]
    TooLong,
    /// `llama_tokenize` returned an unexpected failure code.
    #[error("llama_tokenize returned {0}")]
    Failed(i32),
}

/// Why a token could not be added to a batch.
#[derive(Debug, Error)]
pub enum BatchAddError {
    /// The batch has no room for another token.
    #[error("the batch holds at most {capacity} tokens")]
    InsufficientSpace { capacity: usize },
}

/// Why llama.cpp failed to evaluate a batch.
#[derive(Debug, Error)]
pub enum DecodeError {
    /// `llama_decode` returns a non-zero code on failure.
    #[error("llama_decode returned {code}")]
    Failed { code: i32 },
}

/// Why a generated token could not be turned into text.
#[derive(Debug, Error)]
pub enum DetokenizeError {
    /// `llama_token_to_piece` returned an unexpected failure code.
    #[error("llama_token_to_piece returned {0}")]
    TokenToPiece(i32),
    /// The accumulated token bytes are not valid UTF-8.
    #[error("the token bytes are not valid UTF-8: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    /// The stream ended in the middle of a multi-byte character.
    #[error("stream ended with {} incomplete utf-8 byte(s)", .0.len())]
    IncompleteUtf8(Vec<u8>),
}

/// Why the sampler could not pick a token.
#[derive(Debug, Error)]
pub enum SampleError {
    /// `llama_sampler_sample` returned a negative token id.
    #[error("llama_sampler_sample returned the invalid token {0}")]
    InvalidToken(i32),
}

#[cfg(test)]
mod tests {
    use std::{error::Error as _, path::PathBuf};

    use super::ProviderError;

    /// The worker sends every failure over a channel, so the error must be `Send`.
    #[test]
    fn errors_can_cross_the_worker_channel() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<ProviderError>();
    }

    #[test]
    fn messages_name_the_failing_input_without_a_trailing_period() {
        let missing = ProviderError::ModelFileMissing {
            path: PathBuf::from("models/model.gguf"),
        };
        assert_eq!(
            missing.to_string(),
            "model file `models/model.gguf` does not exist"
        );
        assert!(missing.source().is_none());

        let too_long = ProviderError::PromptTooLong {
            tokens: 9000,
            limit: 4096,
        };
        assert_eq!(
            too_long.to_string(),
            "the prompt needs 9000 tokens and only 4096 are available for it"
        );
        assert!(too_long.source().is_none());
    }
}

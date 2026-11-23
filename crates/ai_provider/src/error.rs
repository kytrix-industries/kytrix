use std::{io, path::PathBuf};

use smol_str::SmolStr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("model file `{}` does not exist", path.display())]
    ModelFileMissing { path: PathBuf },
    #[error("failed to load model `{}`: {source}", path.display())]
    ModelLoad {
        path: PathBuf,
        #[source]
        source: ModelLoadError,
    },
    #[error("failed to create the llama context: {source}")]
    ContextCreation {
        #[source]
        source: ContextCreationError,
    },
    #[error("the prompt needs {tokens} tokens and only {limit} are available for it")]
    PromptTooLong { tokens: usize, limit: usize },
    #[error("failed to tokenize the prompt: {source}")]
    Tokenize {
        #[source]
        source: TokenizeError,
    },
    #[error("the content contains a special token that cannot be represented as plain text")]
    SpecialTokenInContent,
    #[error("failed to add a token to the llama batch: {source}")]
    BatchAdd {
        #[source]
        source: BatchAddError,
    },
    #[error("llama.cpp returned a null batch")]
    BatchAllocation,
    #[error("llama decode failed: {source}")]
    Decode {
        #[source]
        source: DecodeError,
    },
    #[error("failed to detokenize model output: {source}")]
    Detokenize {
        #[source]
        source: DetokenizeError,
    },
    #[error("llama.cpp returned a null sampler")]
    SamplerCreation,
    #[error("llama sampling failed: {source}")]
    Sample {
        #[source]
        source: SampleError,
    },
    #[error("failed to spawn the llama worker thread: {source}")]
    WorkerSpawn {
        #[source]
        source: io::Error,
    },
    #[error("the llama worker is no longer running")]
    WorkerUnavailable,
    #[error("the request was cancelled")]
    Cancelled,
    #[error("invalid engine configuration: {reason}")]
    InvalidConfig { reason: SmolStr },
}

#[derive(Debug, Error)]
pub enum ModelLoadError {
    #[error("the model path is not valid UTF-8")]
    NonUtf8Path,
    #[error("the model path contains a NUL byte")]
    InteriorNul,
    #[error("llama.cpp returned a null model")]
    NullModel,
}

#[derive(Debug, Error)]
pub enum ContextCreationError {
    #[error("llama.cpp returned a null context")]
    NullContext,
}

#[derive(Debug, Error)]
pub enum TokenizeError {
    #[error("the text contains a NUL byte")]
    InteriorNul,
    #[error("the text is too long for llama.cpp")]
    TooLong,
    #[error("llama_tokenize returned {0}")]
    Failed(i32),
}

#[derive(Debug, Error)]
pub enum BatchAddError {
    #[error("the batch holds at most {capacity} tokens")]
    InsufficientSpace { capacity: usize },
}

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("llama_decode returned {code}")]
    Failed { code: i32 },
}

#[derive(Debug, Error)]
pub enum DetokenizeError {
    #[error("llama_token_to_piece returned {0}")]
    TokenToPiece(i32),
    #[error("the token bytes are not valid UTF-8: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    #[error("stream ended with {} incomplete utf-8 byte(s)", .0.len())]
    IncompleteUtf8(Vec<u8>),
}

#[derive(Debug, Error)]
pub enum SampleError {
    #[error("llama_sampler_sample returned the invalid token {0}")]
    InvalidToken(i32),
}

#[cfg(test)]
mod tests {
    use std::{error::Error as _, path::PathBuf};

    use super::ProviderError;

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

        let cancelled = ProviderError::Cancelled;
        assert_eq!(cancelled.to_string(), "the request was cancelled");
        assert!(cancelled.source().is_none());
    }
}

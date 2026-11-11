//! The thread that owns the model and its context.

use std::sync::mpsc::{Receiver, SyncSender};

use llama_sys::ffi;

use super::{
    backend,
    batch::Batch,
    cancel::CancelCheck,
    config::{EngineConfig, SamplingParams},
    context::Context,
    detokenizer::StreamDetokenizer,
    model::Model,
    prompt::PromptSpan,
    sampler::Sampler,
};
use crate::error::{DetokenizeError, ProviderError};

/// One request for the worker thread.
pub(super) enum Command {
    /// Generate text for `prompt` and answer through `reply`.
    Generate {
        prompt: Vec<PromptSpan>,
        params: SamplingParams,
        cancel: Option<CancelCheck>,
        reply: SyncSender<Result<String, ProviderError>>,
    },
    /// Stop serving and let the thread end.
    Shutdown,
}

/// The model owner that runs on the `kytrix-llama` thread.
///
/// The worker is created, used and dropped on one thread. Its context borrows the
/// model, so the two live together in [`Worker::run`] rather than in a struct that
/// could be sent elsewhere.
pub(super) struct Worker {
    model: Model,
    config: EngineConfig,
}

impl Worker {
    /// Loads the model. The caller runs this on the worker thread.
    pub(super) fn load(config: EngineConfig) -> Result<Self, ProviderError> {
        backend::init(config.log_llama);
        let model = Model::load(&config.model_path, config.gpu_layers)?;

        Ok(Self { model, config })
    }

    /// Creates the one context and serves commands until the engine shuts down.
    pub(super) fn run(
        self,
        commands: Receiver<Command>,
        ready: SyncSender<Result<(), ProviderError>>,
    ) {
        let created = self.model.new_context(context_params(&self.config));
        let mut context = match created {
            Ok(context) => context,
            Err(source) => {
                let _ = ready.send(Err(ProviderError::ContextCreation { source }));
                return;
            }
        };
        if ready.send(Ok(())).is_err() {
            return;
        }

        while let Ok(command) = commands.recv() {
            match command {
                Command::Generate {
                    prompt,
                    params,
                    cancel,
                    reply,
                } => {
                    let generated = self.generate(&mut context, &prompt, &params, &cancel);
                    context.clear_kv_cache();
                    let _ = reply.send(generated);
                }
                Command::Shutdown => break,
            }
        }
    }

    /// Tokenizes, decodes and samples one request on the reused context.
    ///
    /// `cancel` is polled before tokenization, before each prefill chunk and before
    /// each sampled token. Returning early is safe: [`Worker::run`] clears the KV
    /// cache after every request, including a cancelled one.
    fn generate(
        &self,
        context: &mut Context<'_>,
        prompt: &[PromptSpan],
        params: &SamplingParams,
        cancel: &Option<CancelCheck>,
    ) -> Result<String, ProviderError> {
        if cancel.as_ref().is_some_and(|check| check()) {
            return Err(ProviderError::Cancelled);
        }

        let mut tokens = Vec::<ffi::llama_token>::new();
        for span in prompt {
            match span {
                PromptSpan::Control(text) => {
                    if text.is_empty() {
                        continue;
                    }
                    tokens.extend(
                        self.model
                            .tokenize(text, true)
                            .map_err(|source| ProviderError::Tokenize { source })?,
                    );
                }
                PromptSpan::Text(text) => {
                    if text.is_empty() {
                        continue;
                    }
                    tokens.extend(tokenize_text_span(&self.model, text)?);
                }
            }
        }
        if tokens.is_empty() {
            // llama.cpp refuses to decode an empty batch, and a prompt that tokenizes to
            // nothing has no text to condition on.
            return Ok(String::new());
        }

        let context_tokens = self.config.context_tokens as usize;
        let max_tokens = params.max_tokens as usize;
        let limit = context_tokens.saturating_sub(max_tokens);
        if tokens.len() + max_tokens > context_tokens {
            return Err(ProviderError::PromptTooLong {
                tokens: tokens.len(),
                limit,
            });
        }

        let batch_tokens = self.config.batch_tokens as usize;
        let mut batch = Batch::new(batch_tokens, 1)?;
        let mut position = 0_i32;
        let chunk_count = tokens.len().div_ceil(batch_tokens);
        for (index, chunk) in tokens.chunks(batch_tokens).enumerate() {
            if cancel.as_ref().is_some_and(|check| check()) {
                return Err(ProviderError::Cancelled);
            }

            let is_last_chunk = index + 1 == chunk_count;
            batch.clear();
            for (offset, token) in chunk.iter().enumerate() {
                let token_position = position + offset as i32;
                let logits = is_last_chunk && offset + 1 == chunk.len();
                batch
                    .add(*token, token_position, logits)
                    .map_err(|source| ProviderError::BatchAdd { source })?;
            }
            context
                .decode(&mut batch)
                .map_err(|source| ProviderError::Decode { source })?;
            position += chunk.len() as i32;
        }

        let sampler = Sampler::chain(params, self.model.n_vocab())?;
        let mut detokenizer = StreamDetokenizer::new(&self.model);
        let mut output = String::new();
        let mut sampled_index = ((tokens.len() - 1) % batch_tokens) as i32;
        let mut truncated = false;

        for generated in 0..params.max_tokens {
            if cancel.as_ref().is_some_and(|check| check()) {
                return Err(ProviderError::Cancelled);
            }

            let token = sampler
                .sample(context, sampled_index)
                .map_err(|source| ProviderError::Sample { source })?;
            if self.model.is_eog_token(token) || params.stop_token_ids.contains(&token) {
                break;
            }

            // A control token that is not in the stop list has no text. Skipping
            // it here keeps one unusual marker from failing the whole request;
            // it is still decoded below so the next position is correct.
            if !is_special_token(&self.model, token) {
                let piece = detokenizer
                    .push(token)
                    .map_err(|source| ProviderError::Detokenize { source })?;
                output.push_str(&piece);
                if let Some(cut) = params
                    .stop_strings
                    .iter()
                    .filter_map(|stop| output.find(stop.as_str()))
                    .min()
                {
                    output.truncate(cut);
                    truncated = true;
                    break;
                }
            }

            let next_position = position + generated as i32;
            batch.clear();
            batch
                .add(token, next_position, true)
                .map_err(|source| ProviderError::BatchAdd { source })?;
            context
                .decode(&mut batch)
                .map_err(|source| ProviderError::Decode { source })?;
            sampled_index = 0;
        }

        if !truncated {
            append_tail(&mut output, detokenizer.finish())?;
        }
        Ok(output)
    }
}

/// Tokenizes user text while preventing any added token from being recognized.
///
/// llama.cpp's `parse_special = false` still pretokenizes USER_DEFINED tokens
/// (`<think>`, `<tool_call>`, ...), so a plain plaintext pass is not enough. If
/// the first pass contains a control/user-defined/unknown token, retry with the
/// text split before every `<`; that breaks the added-token match while keeping
/// the rest of the text intact.
fn tokenize_text_span(model: &Model, text: &str) -> Result<Vec<ffi::llama_token>, ProviderError> {
    let tokens = model
        .tokenize(text, false)
        .map_err(|source| ProviderError::Tokenize { source })?;
    if !tokens.iter().any(|token| is_special_token(model, *token)) {
        return Ok(tokens);
    }

    let mut split = Vec::new();
    for piece in split_before_angle_brackets(text) {
        split.extend(
            model
                .tokenize(piece, false)
                .map_err(|source| ProviderError::Tokenize { source })?,
        );
    }
    if !split.iter().any(|token| is_special_token(model, *token)) {
        return Ok(split);
    }

    // A special token that does not start with `<` would survive the split. A
    // character-by-character pass cannot match any multi-character added token.
    let mut characters = Vec::new();
    for character in text.chars() {
        let mut buffer = [0_u8; 4];
        characters.extend(
            model
                .tokenize(character.encode_utf8(&mut buffer), false)
                .map_err(|source| ProviderError::Tokenize { source })?,
        );
    }
    if characters
        .iter()
        .any(|token| is_special_token(model, *token))
    {
        return Err(ProviderError::SpecialTokenInContent);
    }

    Ok(characters)
}

/// Whether a token must never leak into a prompt text span or the output text.
fn is_special_token(model: &Model, token: ffi::llama_token) -> bool {
    let attributes = model.token_attr(token);
    attributes & ffi::LLAMA_TOKEN_ATTR_CONTROL != 0
        || attributes & ffi::LLAMA_TOKEN_ATTR_USER_DEFINED != 0
        || attributes & ffi::LLAMA_TOKEN_ATTR_UNKNOWN != 0
}

/// Splits text before each `<`, so an added token like `<think>` cannot match.
fn split_before_angle_brackets(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    for (index, character) in text.char_indices() {
        if character == '<' {
            if index > start {
                pieces.push(&text[start..index]);
            }
            pieces.push(&text[index..index + 1]);
            start = index + 1;
        }
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

/// Appends the detokenizer's final complete text.
///
/// Hitting `max_tokens` can stop in the middle of a byte-fallback character. The
/// complete prefix is still a valid translation, so the unfinished tail is
/// dropped rather than turning the whole request into an error.
fn append_tail(
    output: &mut String,
    finished: Result<String, DetokenizeError>,
) -> Result<(), ProviderError> {
    match finished {
        Ok(tail) => output.push_str(&tail),
        Err(DetokenizeError::IncompleteUtf8(_)) => {}
        Err(source) => return Err(ProviderError::Detokenize { source }),
    }
    Ok(())
}

/// Builds the parameters of the one context the worker creates.
fn context_params(config: &EngineConfig) -> ffi::llama_context_params {
    // SAFETY: this only fills a plain C struct with llama.cpp's defaults.
    let mut params = unsafe { ffi::llama_context_default_params() };
    params.n_ctx = config.context_tokens;
    params.n_batch = config.batch_tokens;
    params.flash_attn_type = ffi::LLAMA_FLASH_ATTN_TYPE_AUTO;
    params.offload_kqv = true;
    params.no_perf = true;
    if let Some(threads) = config.threads {
        params.n_threads = threads;
    }
    params
}

#[cfg(test)]
mod tests {
    use llama_sys::ffi;

    use super::{Command, append_tail, context_params, split_before_angle_brackets};
    use crate::{error::DetokenizeError, llama::config::EngineConfig};

    #[test]
    fn text_is_split_before_angle_brackets() {
        assert_eq!(
            split_before_angle_brackets("a<think>b"),
            vec!["a", "<", "think>b"]
        );
        assert_eq!(
            split_before_angle_brackets("<tool_call>x"),
            vec!["<", "tool_call>x"]
        );
        assert_eq!(
            split_before_angle_brackets("plain text"),
            vec!["plain text"]
        );
    }

    #[test]
    fn an_incomplete_tail_keeps_the_complete_translation() {
        let mut output = String::from("你好");
        append_tail(
            &mut output,
            Err(DetokenizeError::IncompleteUtf8(vec![0xE2])),
        )
        .expect("an incomplete tail is not a failure");

        assert_eq!(output, "你好");

        append_tail(&mut output, Ok(String::from("!"))).expect("a complete tail is appended");
        assert_eq!(output, "你好!");
    }

    /// Every field of a queued request crosses the worker channel, so the command and
    /// the result it carries must both be `Send`.
    #[test]
    fn commands_can_cross_the_worker_channel() {
        fn assert_send<T: Send>() {}

        assert_send::<Command>();
    }

    #[test]
    fn the_context_is_created_with_the_configured_limits() {
        let params = context_params(&EngineConfig::new("models/model.gguf"));

        assert_eq!(params.n_ctx, 8192);
        assert_eq!(params.n_batch, 2048);
        assert!(params.offload_kqv);
        assert!(params.no_perf);
        assert_eq!(params.flash_attn_type, ffi::LLAMA_FLASH_ATTN_TYPE_AUTO);
    }

    #[test]
    fn an_explicit_thread_count_reaches_the_context() {
        let mut config = EngineConfig::new("models/model.gguf");
        config.threads = Some(6);

        assert_eq!(context_params(&config).n_threads, 6);
    }
}

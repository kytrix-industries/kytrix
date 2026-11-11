//! The engine handle that owns the worker thread.

use std::{
    sync::mpsc::{self, SyncSender},
    thread::{self, JoinHandle},
};

use super::{
    cancel::CancelCheck,
    config::{EngineConfig, SamplingParams},
    prompt::PromptSpan,
    worker::{Command, Worker},
};
use crate::error::ProviderError;

/// Name of the thread that owns the model and its context.
const WORKER_THREAD_NAME: &str = "kytrix-llama";

/// A loaded model and the worker thread that serves it.
///
/// The handle is `Send + Sync`: each request is handed to the one worker thread, which
/// is the only place the non-`Send` llama context lives. Requests are serialized, which
/// is the only shape a single context supports anyway.
pub struct Engine {
    commands: SyncSender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl Engine {
    /// Loads `config` and blocks until the worker has created its context.
    ///
    /// Loading runs on the worker thread because the model is only ever used there, and
    /// this call waits for the outcome so a bad model path or an unusable configuration
    /// fails here rather than on the first request.
    pub fn load(config: EngineConfig) -> Result<Self, ProviderError> {
        config.validate()?;
        if !config.model_path.is_file() {
            return Err(ProviderError::ModelFileMissing {
                path: config.model_path,
            });
        }

        let (commands, command_receiver) = mpsc::sync_channel(1);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name(WORKER_THREAD_NAME.to_owned())
            .spawn(move || match Worker::load(config) {
                Ok(worker) => worker.run(command_receiver, ready_sender),
                Err(error) => {
                    let _ = ready_sender.send(Err(error));
                }
            })
            .map_err(|source| ProviderError::WorkerSpawn { source })?;

        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                commands,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                if worker.join().is_err() {
                    log::error!("the llama worker panicked while loading the model");
                }
                Err(error)
            }
            Err(_) => {
                if worker.join().is_err() {
                    log::error!("the llama worker panicked before it was ready");
                }
                Err(ProviderError::WorkerUnavailable)
            }
        }
    }

    /// Generates text for `prompt` on the worker thread and returns the raw output.
    ///
    /// Control spans are parsed as special tokens; text spans are tokenized as
    /// plain text, so a document cannot forge a turn marker. The output is not
    /// trimmed; a caller that wraps the text decides what to strip.
    ///
    /// `cancel` is polled by the worker after it receives the request, before each
    /// prompt prefill chunk and before each sampled token. A check that returns
    /// `true` ends the request with [`ProviderError::Cancelled`]; the worker clears
    /// the context and stays available for the next request either way.
    pub fn generate(
        &self,
        prompt: &[PromptSpan],
        params: &SamplingParams,
        cancel: Option<CancelCheck>,
    ) -> Result<String, ProviderError> {
        params.validate()?;

        let (reply, response) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Generate {
                prompt: prompt.to_vec(),
                params: params.clone(),
                cancel,
                reply,
            })
            .map_err(|_| ProviderError::WorkerUnavailable)?;
        response
            .recv()
            .map_err(|_| ProviderError::WorkerUnavailable)?
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        let Some(worker) = self.worker.take() else {
            return;
        };
        if worker.join().is_err() {
            log::error!("the llama worker panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Engine;
    use crate::{error::ProviderError, llama::config::EngineConfig};

    #[test]
    fn the_engine_handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<Engine>();
    }

    #[test]
    fn a_missing_model_file_is_reported_with_its_path() {
        let path = std::path::PathBuf::from("missing-model.gguf");
        let error = Engine::load(EngineConfig::new(path.clone()))
            .err()
            .expect("a missing model must fail");

        assert!(matches!(
            error,
            ProviderError::ModelFileMissing { path: reported } if reported == path
        ));
    }

    #[test]
    fn an_invalid_config_is_rejected_before_the_worker_starts() {
        let mut config = EngineConfig::new("missing-model.gguf");
        config.context_tokens = 0;
        let error = Engine::load(config)
            .err()
            .expect("an invalid config must fail");

        assert!(matches!(error, ProviderError::InvalidConfig { .. }));
    }
}

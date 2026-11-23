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

const WORKER_THREAD_NAME: &str = "kytrix-llama";

pub struct Engine {
    commands: SyncSender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl Engine {
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

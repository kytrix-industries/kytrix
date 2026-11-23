use std::path::PathBuf;

use smol_str::SmolStr;

use crate::error::ProviderError;

#[derive(Debug)]
pub struct EngineConfig {
    pub model_path: PathBuf,
    pub context_tokens: u32,
    pub batch_tokens: u32,
    pub threads: Option<i32>,
    pub gpu_layers: u32,
    pub log_llama: bool,
}

impl EngineConfig {
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
            context_tokens: 8192,
            batch_tokens: 2048,
            threads: None,
            gpu_layers: 999,
            log_llama: false,
        }
    }

    pub(super) fn validate(&self) -> Result<(), ProviderError> {
        if self.context_tokens == 0 {
            return Err(ProviderError::InvalidConfig {
                reason: SmolStr::new_static("context_tokens must be greater than zero"),
            });
        }
        if self.batch_tokens == 0 {
            return Err(ProviderError::InvalidConfig {
                reason: SmolStr::new_static("batch_tokens must be greater than zero"),
            });
        }
        if self.context_tokens > i32::MAX as u32 {
            return Err(ProviderError::InvalidConfig {
                reason: SmolStr::new_static("context_tokens must fit in an i32"),
            });
        }
        if self.batch_tokens > i32::MAX as u32 {
            return Err(ProviderError::InvalidConfig {
                reason: SmolStr::new_static("batch_tokens must fit in an i32"),
            });
        }
        if self.threads.is_some_and(|threads| threads <= 0) {
            return Err(ProviderError::InvalidConfig {
                reason: SmolStr::new_static("threads must be greater than zero when set"),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SamplingParams {
    pub max_tokens: u32,
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: i32,
    pub repetition_penalty: f32,
    pub seed: u32,
    pub stop_token_ids: Vec<i32>,
    pub stop_strings: Vec<String>,
}

impl Default for SamplingParams {
    fn default() -> Self {
        Self {
            max_tokens: 256,
            temperature: 0.8,
            top_p: 0.95,
            top_k: 40,
            repetition_penalty: 1.0,
            seed: 0,
            stop_token_ids: Vec::new(),
            stop_strings: Vec::new(),
        }
    }
}

impl SamplingParams {
    pub(super) fn validate(&self) -> Result<(), ProviderError> {
        if self.max_tokens == 0 {
            return Err(ProviderError::InvalidConfig {
                reason: SmolStr::new_static("max_tokens must be greater than zero"),
            });
        }
        if !self.temperature.is_finite() || self.temperature < 0.0 {
            return Err(ProviderError::InvalidConfig {
                reason: SmolStr::new_static("temperature must be finite and not negative"),
            });
        }
        if !self.top_p.is_finite() || !(0.0..=1.0).contains(&self.top_p) {
            return Err(ProviderError::InvalidConfig {
                reason: SmolStr::new_static("top_p must be finite and between 0.0 and 1.0"),
            });
        }
        if self.top_k < 0 {
            return Err(ProviderError::InvalidConfig {
                reason: SmolStr::new_static("top_k must not be negative"),
            });
        }
        if !self.repetition_penalty.is_finite() || self.repetition_penalty <= 0.0 {
            return Err(ProviderError::InvalidConfig {
                reason: SmolStr::new_static(
                    "repetition_penalty must be finite and greater than zero",
                ),
            });
        }
        if self.stop_strings.iter().any(String::is_empty) {
            return Err(ProviderError::InvalidConfig {
                reason: SmolStr::new_static("stop_strings must not contain an empty string"),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{EngineConfig, SamplingParams};
    use crate::error::ProviderError;

    #[test]
    fn a_new_engine_config_uses_the_translation_defaults() {
        let config = EngineConfig::new("models/model.gguf");

        assert_eq!(config.model_path, PathBuf::from("models/model.gguf"));
        assert_eq!(config.context_tokens, 8192);
        assert_eq!(config.batch_tokens, 2048);
        assert_eq!(config.threads, None);
        assert_eq!(config.gpu_layers, 999);
        assert!(!config.log_llama);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn sampling_defaults_match_the_documented_generation_defaults() {
        let params = SamplingParams::default();

        assert_eq!(params.max_tokens, 256);
        assert_eq!(params.temperature, 0.8);
        assert_eq!(params.top_p, 0.95);
        assert_eq!(params.top_k, 40);
        assert_eq!(params.repetition_penalty, 1.0);
        assert_eq!(params.seed, 0);
        assert!(params.stop_token_ids.is_empty());
        assert!(params.stop_strings.is_empty());
        assert!(params.validate().is_ok());
    }

    #[test]
    fn unusable_sampling_parameters_are_rejected() {
        let invalid: [fn(&mut SamplingParams); 8] = [
            |params| params.max_tokens = 0,
            |params| params.temperature = f32::NAN,
            |params| params.temperature = -0.1,
            |params| params.top_p = 1.5,
            |params| params.top_p = f32::NAN,
            |params| params.top_k = -1,
            |params| params.repetition_penalty = 0.0,
            |params| params.stop_strings.push(String::new()),
        ];

        for mutate in invalid {
            let mut params = SamplingParams::default();
            mutate(&mut params);

            assert!(
                matches!(params.validate(), Err(ProviderError::InvalidConfig { .. })),
                "the sampling parameters must be rejected"
            );
        }

        let mut params = SamplingParams::default();
        params.stop_strings.push(String::from("</s>"));
        assert!(params.validate().is_ok());
    }

    #[test]
    fn unusable_engine_configs_are_rejected_before_the_model_is_touched() {
        let mut config = EngineConfig::new("models/model.gguf");
        config.context_tokens = 0;
        assert!(matches!(
            config.validate(),
            Err(ProviderError::InvalidConfig { .. })
        ));

        let mut config = EngineConfig::new("models/model.gguf");
        config.batch_tokens = 0;
        assert!(matches!(
            config.validate(),
            Err(ProviderError::InvalidConfig { .. })
        ));

        for threads in [0, -1] {
            let mut config = EngineConfig::new("models/model.gguf");
            config.threads = Some(threads);
            assert!(matches!(
                config.validate(),
                Err(ProviderError::InvalidConfig { .. })
            ));
        }

        let mut config = EngineConfig::new("models/model.gguf");
        config.context_tokens = i32::MAX as u32 + 1;
        assert!(matches!(
            config.validate(),
            Err(ProviderError::InvalidConfig { .. })
        ));

        let mut config = EngineConfig::new("models/model.gguf");
        config.batch_tokens = i32::MAX as u32 + 1;
        assert!(matches!(
            config.validate(),
            Err(ProviderError::InvalidConfig { .. })
        ));
    }
}

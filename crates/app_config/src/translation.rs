//! Settings for the local translation model.
//!
//! The model file itself is not named here: the translator loads one fixed GGUF from the
//! directory stored in this configuration. The directory is a setting rather than a build-time
//! location because the models are large, user-provided files that may live on any drive.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where the local translation model is stored.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationConfig {
    /// The directory holding the GGUF model file, or `None` until the user chooses one.
    #[serde(default)]
    pub model_directory: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::TranslationConfig;

    #[test]
    fn a_default_config_names_no_model_directory() {
        assert_eq!(TranslationConfig::default().model_directory, None);
    }

    #[test]
    fn a_model_directory_should_round_trip() {
        let config = TranslationConfig {
            model_directory: Some(PathBuf::from("models")),
        };
        let json = serde_json::to_value(&config).expect("the settings should serialize");

        assert_eq!(json["model_directory"], serde_json::json!("models"));
        assert_eq!(
            serde_json::from_value::<TranslationConfig>(json).expect("the settings should decode"),
            config
        );
    }
}

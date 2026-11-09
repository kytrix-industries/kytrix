//! Settings for the local translation model.
//!
//! The model is chosen from the models this build supports, and loaded from that model's
//! local file in the directory stored here. The directory is a setting rather than a
//! build-time location because the models are large, user-provided files that may live on
//! any drive.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One downloadable Hy-MT2 GGUF model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TranslationModel {
    #[serde(rename = "hy_mt2_1_8b_q4_k_m")]
    HyMt218BQ4Km,
    #[serde(rename = "hy_mt2_1_8b_q6_k")]
    HyMt218BQ6K,
    #[serde(rename = "hy_mt2_1_8b_q8_0")]
    HyMt218BQ8,
    #[serde(rename = "hy_mt2_1_8b_2bit")]
    HyMt218B2Bit,
    #[serde(rename = "hy_mt2_1_8b_1_25bit")]
    HyMt218B125Bit,
    #[serde(rename = "hy_mt2_7b_q4_k_m")]
    HyMt27BQ4Km,
    #[serde(rename = "hy_mt2_7b_q6_k")]
    HyMt27BQ6K,
    #[serde(rename = "hy_mt2_7b_q8_0")]
    HyMt27BQ8,
}

impl TranslationModel {
    /// Every model, in the order the settings page offers them.
    pub const ALL: [Self; 8] = [
        Self::HyMt218BQ4Km,
        Self::HyMt218BQ6K,
        Self::HyMt218BQ8,
        Self::HyMt218B2Bit,
        Self::HyMt218B125Bit,
        Self::HyMt27BQ4Km,
        Self::HyMt27BQ6K,
        Self::HyMt27BQ8,
    ];

    /// The name the settings page shows and [`Self::from_name`] accepts.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::HyMt218BQ4Km => "HY-MT2-1.8B-Q4_K_M",
            Self::HyMt218BQ6K => "HY-MT2-1.8B-Q6_K",
            Self::HyMt218BQ8 => "HY-MT2-1.8B-Q8_0",
            Self::HyMt218B2Bit => "HY-MT2-1.8B-2Bit",
            Self::HyMt218B125Bit => "HY-MT2-1.8B-1.25Bit",
            Self::HyMt27BQ4Km => "HY-MT2-7B-Q4_K_M",
            Self::HyMt27BQ6K => "HY-MT2-7B-Q6_K",
            Self::HyMt27BQ8 => "HY-MT2-7B-Q8_0",
        }
    }

    /// The local file name the model is saved under.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::HyMt218BQ4Km => "HY-MT2-1.8B-Q4_K_M.gguf",
            Self::HyMt218BQ6K => "HY-MT2-1.8B-Q6_K.gguf",
            Self::HyMt218BQ8 => "HY-MT2-1.8B-Q8_0.gguf",
            Self::HyMt218B2Bit => "HY-MT2-1.8B-2Bit.gguf",
            Self::HyMt218B125Bit => "HY-MT2-1.8B-1.25Bit.gguf",
            Self::HyMt27BQ4Km => "HY-MT2-7B-Q4_K_M.gguf",
            Self::HyMt27BQ6K => "HY-MT2-7B-Q6_K.gguf",
            Self::HyMt27BQ8 => "HY-MT2-7B-Q8_0.gguf",
        }
    }

    /// The exact size HuggingFace publishes for the file, in bytes.
    pub fn size_bytes(self) -> u64 {
        match self {
            Self::HyMt218BQ4Km => 1_133_080_448,
            Self::HyMt218BQ6K => 1_474_785_120,
            Self::HyMt218BQ8 => 1_908_528_192,
            Self::HyMt218B2Bit => 600_534_880,
            Self::HyMt218B125Bit => 461_860_800,
            Self::HyMt27BQ4Km => 4_624_648_896,
            Self::HyMt27BQ6K => 6_164_482_720,
            Self::HyMt27BQ8 => 7_981_928_896,
        }
    }

    /// The HuggingFace URL the model is downloaded from.
    pub fn download_url(self) -> &'static str {
        match self {
            Self::HyMt218BQ4Km => {
                "https://huggingface.co/tencent/Hy-MT2-1.8B-GGUF/resolve/main/Hy-MT2-1.8B-Q4_K_M.gguf"
            }
            Self::HyMt218BQ6K => {
                "https://huggingface.co/tencent/Hy-MT2-1.8B-GGUF/resolve/main/Hy-MT2-1.8B-Q6_K.gguf"
            }
            Self::HyMt218BQ8 => {
                "https://huggingface.co/tencent/Hy-MT2-1.8B-GGUF/resolve/main/Hy-MT2-1.8B-Q8_0.gguf"
            }
            Self::HyMt218B2Bit => {
                "https://huggingface.co/tencent/Hy-MT2-1.8B-2Bit-GGUF/resolve/main/Hy-MT2-1.8B-2Bit.gguf"
            }
            Self::HyMt218B125Bit => {
                "https://huggingface.co/tencent/Hy-MT2-1.8B-1.25Bit-GGUF/resolve/main/Hy-MT2-1.8B-1.25Bit.gguf"
            }
            Self::HyMt27BQ4Km => {
                "https://huggingface.co/tencent/Hy-MT2-7B-GGUF/resolve/main/Hy-MT2-7B-Q4_K_M.gguf"
            }
            Self::HyMt27BQ6K => {
                "https://huggingface.co/tencent/Hy-MT2-7B-GGUF/resolve/main/HY-MT2-7B-Q6_K.gguf"
            }
            Self::HyMt27BQ8 => {
                "https://huggingface.co/tencent/Hy-MT2-7B-GGUF/resolve/main/HY-MT2-7B-Q8_0.gguf"
            }
        }
    }

    /// The published size in mebibytes (1 MiB = 1024 * 1024 bytes).
    pub fn size_mb(self) -> f64 {
        self.size_bytes() as f64 / (1024.0 * 1024.0)
    }

    /// Resolves a settings value to the model it names.
    ///
    /// Only the exact display name is accepted: the name is the value the settings
    /// page shows, not a file name or a case-insensitive alias.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|model| model.display_name() == name)
    }
}

/// Where the local translation model is stored and which model it is.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationConfig {
    /// The directory holding the GGUF model files, or `None` until the user chooses one.
    #[serde(default)]
    pub model_directory: Option<PathBuf>,
    /// The selected model, or `None` until the user chooses one.
    #[serde(default)]
    pub selected_model: Option<TranslationModel>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::{TranslationConfig, TranslationModel};

    /// Every published fact, in the order the page offers the models.
    const CATALOG: [(&str, &str, u64); 8] = [
        (
            "HY-MT2-1.8B-Q4_K_M",
            "HY-MT2-1.8B-Q4_K_M.gguf",
            1_133_080_448,
        ),
        ("HY-MT2-1.8B-Q6_K", "HY-MT2-1.8B-Q6_K.gguf", 1_474_785_120),
        ("HY-MT2-1.8B-Q8_0", "HY-MT2-1.8B-Q8_0.gguf", 1_908_528_192),
        ("HY-MT2-1.8B-2Bit", "HY-MT2-1.8B-2Bit.gguf", 600_534_880),
        (
            "HY-MT2-1.8B-1.25Bit",
            "HY-MT2-1.8B-1.25Bit.gguf",
            461_860_800,
        ),
        ("HY-MT2-7B-Q4_K_M", "HY-MT2-7B-Q4_K_M.gguf", 4_624_648_896),
        ("HY-MT2-7B-Q6_K", "HY-MT2-7B-Q6_K.gguf", 6_164_482_720),
        ("HY-MT2-7B-Q8_0", "HY-MT2-7B-Q8_0.gguf", 7_981_928_896),
    ];

    /// Every model's stable wire id and HuggingFace URL, in the same order.
    const URLS: [(&str, &str); 8] = [
        (
            "hy_mt2_1_8b_q4_k_m",
            "https://huggingface.co/tencent/Hy-MT2-1.8B-GGUF/resolve/main/Hy-MT2-1.8B-Q4_K_M.gguf",
        ),
        (
            "hy_mt2_1_8b_q6_k",
            "https://huggingface.co/tencent/Hy-MT2-1.8B-GGUF/resolve/main/Hy-MT2-1.8B-Q6_K.gguf",
        ),
        (
            "hy_mt2_1_8b_q8_0",
            "https://huggingface.co/tencent/Hy-MT2-1.8B-GGUF/resolve/main/Hy-MT2-1.8B-Q8_0.gguf",
        ),
        (
            "hy_mt2_1_8b_2bit",
            "https://huggingface.co/tencent/Hy-MT2-1.8B-2Bit-GGUF/resolve/main/Hy-MT2-1.8B-2Bit.gguf",
        ),
        (
            "hy_mt2_1_8b_1_25bit",
            "https://huggingface.co/tencent/Hy-MT2-1.8B-1.25Bit-GGUF/resolve/main/Hy-MT2-1.8B-1.25Bit.gguf",
        ),
        (
            "hy_mt2_7b_q4_k_m",
            "https://huggingface.co/tencent/Hy-MT2-7B-GGUF/resolve/main/Hy-MT2-7B-Q4_K_M.gguf",
        ),
        (
            "hy_mt2_7b_q6_k",
            "https://huggingface.co/tencent/Hy-MT2-7B-GGUF/resolve/main/HY-MT2-7B-Q6_K.gguf",
        ),
        (
            "hy_mt2_7b_q8_0",
            "https://huggingface.co/tencent/Hy-MT2-7B-GGUF/resolve/main/HY-MT2-7B-Q8_0.gguf",
        ),
    ];

    /// The file name each URL must end with, as the repository publishes it.
    const REMOTE_FILE_NAMES: [&str; 8] = [
        "Hy-MT2-1.8B-Q4_K_M.gguf",
        "Hy-MT2-1.8B-Q6_K.gguf",
        "Hy-MT2-1.8B-Q8_0.gguf",
        "Hy-MT2-1.8B-2Bit.gguf",
        "Hy-MT2-1.8B-1.25Bit.gguf",
        "Hy-MT2-7B-Q4_K_M.gguf",
        "HY-MT2-7B-Q6_K.gguf",
        "HY-MT2-7B-Q8_0.gguf",
    ];

    #[test]
    fn a_default_config_names_no_model_directory_or_selected_model() {
        let config = TranslationConfig::default();

        assert_eq!(config.model_directory, None);
        assert_eq!(config.selected_model, None);
    }

    #[test]
    fn a_model_directory_and_selected_model_should_round_trip() {
        let config = TranslationConfig {
            model_directory: Some(PathBuf::from("models")),
            selected_model: Some(TranslationModel::HyMt27BQ8),
        };
        let json = serde_json::to_value(&config).expect("the settings should serialize");

        assert_eq!(json["model_directory"], json!("models"));
        assert_eq!(json["selected_model"], json!("hy_mt2_7b_q8_0"));
        assert_eq!(
            serde_json::from_value::<TranslationConfig>(json).expect("the settings should decode"),
            config
        );
    }

    #[test]
    fn the_catalog_should_list_the_models_in_page_order() {
        assert_eq!(TranslationModel::ALL.len(), CATALOG.len());
        for (model, (display_name, _, _)) in TranslationModel::ALL.iter().zip(CATALOG) {
            assert_eq!(model.display_name(), display_name);
        }
    }

    #[test]
    fn every_model_should_expose_its_file_size_and_download_url() {
        for (index, model) in TranslationModel::ALL.into_iter().enumerate() {
            let (display_name, file_name, size_bytes) = CATALOG[index];
            let (wire_id, url) = URLS[index];
            let remote_file_name = REMOTE_FILE_NAMES[index];

            assert_eq!(model.display_name(), display_name);
            assert_eq!(model.file_name(), file_name);
            assert_eq!(model.size_bytes(), size_bytes);
            assert_eq!(model.download_url(), url);
            assert_eq!(
                model.download_url().rsplit('/').next(),
                Some(remote_file_name),
                "{} must download the published remote file",
                model.display_name()
            );
            assert!(
                model.download_url().starts_with("https://huggingface.co/"),
                "{} must come from HuggingFace",
                model.display_name()
            );
            assert_eq!(
                serde_json::to_value(model).expect("the model should serialize"),
                json!(wire_id),
                "{} must keep its stable wire id",
                model.display_name()
            );
            assert_eq!(
                serde_json::from_value::<TranslationModel>(json!(wire_id))
                    .expect("the model id should decode"),
                model
            );
        }
    }

    #[test]
    fn the_7b_download_urls_should_name_the_published_files() {
        for (model, expected) in [
            (TranslationModel::HyMt27BQ4Km, "Hy-MT2-7B-Q4_K_M.gguf"),
            (TranslationModel::HyMt27BQ6K, "HY-MT2-7B-Q6_K.gguf"),
            (TranslationModel::HyMt27BQ8, "HY-MT2-7B-Q8_0.gguf"),
        ] {
            assert_eq!(
                model.download_url().rsplit('/').next(),
                Some(expected),
                "{} must name the file the repository publishes",
                model.display_name()
            );
        }
    }

    #[test]
    fn a_size_should_be_reported_in_mebibytes() {
        assert_eq!(
            TranslationModel::HyMt218BQ4Km.size_mb(),
            1_133_080_448.0 / (1024.0 * 1024.0)
        );
    }

    #[test]
    fn from_name_should_match_the_display_name_exactly() {
        for model in TranslationModel::ALL {
            assert_eq!(
                TranslationModel::from_name(model.display_name()),
                Some(model)
            );
        }

        assert_eq!(TranslationModel::from_name("hy-mt2-1.8b-q4_k_m"), None);
        assert_eq!(TranslationModel::from_name("HY-MT2-1.8B-Q4_K_M.gguf"), None);
        assert_eq!(TranslationModel::from_name(""), None);
    }
}

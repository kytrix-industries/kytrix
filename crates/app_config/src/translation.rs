//! Settings for the local translation model.
//!
//! The model is chosen from the models this build supports, and loaded from that model's
//! local file in the directory stored here. The directory is a setting rather than a
//! build-time location because the models are large, user-provided files that may live on
//! any drive.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One site the translation models can be downloaded from.
///
/// It is a stored setting: a download, and a retry of a failed one, starts from the site
/// named here. A completed download is saved under the model's own file name, whichever
/// site it came from, but a retry that finds another site chosen discards the partial file
/// the previous site left rather than continuing it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DownloadSite {
    #[default]
    #[serde(rename = "hugging_face")]
    HuggingFace,
    #[serde(rename = "model_scope")]
    ModelScope,
}

impl DownloadSite {
    /// Every site, in the order the settings page offers them.
    pub const ALL: [Self; 2] = [Self::HuggingFace, Self::ModelScope];
}

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

    /// The parameter count the settings page labels the model with.
    pub fn parameter_size(self) -> &'static str {
        match self {
            Self::HyMt218BQ4Km
            | Self::HyMt218BQ6K
            | Self::HyMt218BQ8
            | Self::HyMt218B2Bit
            | Self::HyMt218B125Bit => "1.8B",
            Self::HyMt27BQ4Km | Self::HyMt27BQ6K | Self::HyMt27BQ8 => "7B",
        }
    }

    /// The quantization width the settings page labels the model with, in bits.
    pub fn quantization_bits(self) -> &'static str {
        match self {
            Self::HyMt218BQ4Km | Self::HyMt27BQ4Km => "4",
            Self::HyMt218BQ6K | Self::HyMt27BQ6K => "6",
            Self::HyMt218BQ8 | Self::HyMt27BQ8 => "8",
            Self::HyMt218B2Bit => "2",
            Self::HyMt218B125Bit => "1.25",
        }
    }

    /// The published size of the model's file, in bytes.
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

    /// The URL the model is downloaded from on `site`.
    pub fn download_url(self, site: DownloadSite) -> &'static str {
        match site {
            DownloadSite::HuggingFace => match self {
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
            },
            DownloadSite::ModelScope => match self {
                Self::HyMt218BQ4Km => {
                    "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-1.8B-GGUF/resolve/master/Hy-MT2-1.8B-Q4_K_M.gguf"
                }
                Self::HyMt218BQ6K => {
                    "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-1.8B-GGUF/resolve/master/Hy-MT2-1.8B-Q6_K.gguf"
                }
                Self::HyMt218BQ8 => {
                    "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-1.8B-GGUF/resolve/master/Hy-MT2-1.8B-Q8_0.gguf"
                }
                Self::HyMt218B2Bit => {
                    "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-1.8B-2bit-GGUF/resolve/master/Hy-MT2-1.8B-2Bit.gguf"
                }
                Self::HyMt218B125Bit => {
                    "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-1.8B-1.25bit-GGUF/resolve/master/Hy-MT2-1.8B-1.25Bit.gguf"
                }
                Self::HyMt27BQ4Km => {
                    "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-7B-GGUF/resolve/master/Hy-MT2-7B-Q4_K_M.gguf"
                }
                Self::HyMt27BQ6K => {
                    "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-7B-GGUF/resolve/master/HY-MT2-7B-Q6_K.gguf"
                }
                Self::HyMt27BQ8 => {
                    "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-7B-GGUF/resolve/master/HY-MT2-7B-Q8_0.gguf"
                }
            },
        }
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
    /// The site the models are downloaded from.
    #[serde(default)]
    pub download_site: DownloadSite,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::{DownloadSite, TranslationConfig, TranslationModel};

    /// Every published fact, in the order the page offers the models.
    const CATALOG: [(&str, &str, u64, &str, &str); 8] = [
        (
            "HY-MT2-1.8B-Q4_K_M",
            "HY-MT2-1.8B-Q4_K_M.gguf",
            1_133_080_448,
            "1.8B",
            "4",
        ),
        (
            "HY-MT2-1.8B-Q6_K",
            "HY-MT2-1.8B-Q6_K.gguf",
            1_474_785_120,
            "1.8B",
            "6",
        ),
        (
            "HY-MT2-1.8B-Q8_0",
            "HY-MT2-1.8B-Q8_0.gguf",
            1_908_528_192,
            "1.8B",
            "8",
        ),
        (
            "HY-MT2-1.8B-2Bit",
            "HY-MT2-1.8B-2Bit.gguf",
            600_534_880,
            "1.8B",
            "2",
        ),
        (
            "HY-MT2-1.8B-1.25Bit",
            "HY-MT2-1.8B-1.25Bit.gguf",
            461_860_800,
            "1.8B",
            "1.25",
        ),
        (
            "HY-MT2-7B-Q4_K_M",
            "HY-MT2-7B-Q4_K_M.gguf",
            4_624_648_896,
            "7B",
            "4",
        ),
        (
            "HY-MT2-7B-Q6_K",
            "HY-MT2-7B-Q6_K.gguf",
            6_164_482_720,
            "7B",
            "6",
        ),
        (
            "HY-MT2-7B-Q8_0",
            "HY-MT2-7B-Q8_0.gguf",
            7_981_928_896,
            "7B",
            "8",
        ),
    ];

    /// Every model's stable wire id and its URL on each site, in the same order.
    const URLS: [(&str, &str, &str); 8] = [
        (
            "hy_mt2_1_8b_q4_k_m",
            "https://huggingface.co/tencent/Hy-MT2-1.8B-GGUF/resolve/main/Hy-MT2-1.8B-Q4_K_M.gguf",
            "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-1.8B-GGUF/resolve/master/Hy-MT2-1.8B-Q4_K_M.gguf",
        ),
        (
            "hy_mt2_1_8b_q6_k",
            "https://huggingface.co/tencent/Hy-MT2-1.8B-GGUF/resolve/main/Hy-MT2-1.8B-Q6_K.gguf",
            "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-1.8B-GGUF/resolve/master/Hy-MT2-1.8B-Q6_K.gguf",
        ),
        (
            "hy_mt2_1_8b_q8_0",
            "https://huggingface.co/tencent/Hy-MT2-1.8B-GGUF/resolve/main/Hy-MT2-1.8B-Q8_0.gguf",
            "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-1.8B-GGUF/resolve/master/Hy-MT2-1.8B-Q8_0.gguf",
        ),
        (
            "hy_mt2_1_8b_2bit",
            "https://huggingface.co/tencent/Hy-MT2-1.8B-2Bit-GGUF/resolve/main/Hy-MT2-1.8B-2Bit.gguf",
            "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-1.8B-2bit-GGUF/resolve/master/Hy-MT2-1.8B-2Bit.gguf",
        ),
        (
            "hy_mt2_1_8b_1_25bit",
            "https://huggingface.co/tencent/Hy-MT2-1.8B-1.25Bit-GGUF/resolve/main/Hy-MT2-1.8B-1.25Bit.gguf",
            "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-1.8B-1.25bit-GGUF/resolve/master/Hy-MT2-1.8B-1.25Bit.gguf",
        ),
        (
            "hy_mt2_7b_q4_k_m",
            "https://huggingface.co/tencent/Hy-MT2-7B-GGUF/resolve/main/Hy-MT2-7B-Q4_K_M.gguf",
            "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-7B-GGUF/resolve/master/Hy-MT2-7B-Q4_K_M.gguf",
        ),
        (
            "hy_mt2_7b_q6_k",
            "https://huggingface.co/tencent/Hy-MT2-7B-GGUF/resolve/main/HY-MT2-7B-Q6_K.gguf",
            "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-7B-GGUF/resolve/master/HY-MT2-7B-Q6_K.gguf",
        ),
        (
            "hy_mt2_7b_q8_0",
            "https://huggingface.co/tencent/Hy-MT2-7B-GGUF/resolve/main/HY-MT2-7B-Q8_0.gguf",
            "https://www.modelscope.cn/models/Tencent-Hunyuan/Hy-MT2-7B-GGUF/resolve/master/HY-MT2-7B-Q8_0.gguf",
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
        assert_eq!(
            config.download_site,
            DownloadSite::HuggingFace,
            "a fresh installation downloads from HuggingFace"
        );
    }

    #[test]
    fn a_model_directory_selected_model_and_download_site_should_round_trip() {
        let config = TranslationConfig {
            model_directory: Some(PathBuf::from("models")),
            selected_model: Some(TranslationModel::HyMt27BQ8),
            download_site: DownloadSite::ModelScope,
        };
        let json = serde_json::to_value(&config).expect("the settings should serialize");

        assert_eq!(json["model_directory"], json!("models"));
        assert_eq!(json["selected_model"], json!("hy_mt2_7b_q8_0"));
        assert_eq!(json["download_site"], json!("model_scope"));
        assert_eq!(
            serde_json::from_value::<TranslationConfig>(json).expect("the settings should decode"),
            config
        );
    }

    /// A file written before the download site was stored has no value of its own, which is
    /// the same state as a fresh installation: downloads come from HuggingFace.
    #[test]
    fn a_config_without_a_download_site_should_download_from_huggingface() {
        let config: TranslationConfig =
            serde_json::from_value(json!({})).expect("the settings should decode");

        assert_eq!(config.download_site, DownloadSite::HuggingFace);
    }

    #[test]
    fn the_catalog_should_list_the_models_in_page_order() {
        assert_eq!(TranslationModel::ALL.len(), CATALOG.len());
        for (model, (display_name, ..)) in TranslationModel::ALL.iter().zip(CATALOG) {
            assert_eq!(model.display_name(), display_name);
        }
    }

    #[test]
    fn every_model_should_expose_its_published_facts_and_download_urls() {
        for (index, model) in TranslationModel::ALL.into_iter().enumerate() {
            let (display_name, file_name, size_bytes, parameter_size, quantization_bits) =
                CATALOG[index];
            let (wire_id, hugging_face_url, model_scope_url) = URLS[index];
            let remote_file_name = REMOTE_FILE_NAMES[index];

            assert_eq!(model.display_name(), display_name);
            assert_eq!(model.file_name(), file_name);
            assert_eq!(model.size_bytes(), size_bytes);
            assert_eq!(model.parameter_size(), parameter_size);
            assert_eq!(model.quantization_bits(), quantization_bits);
            for (site, expected_url) in [
                (DownloadSite::HuggingFace, hugging_face_url),
                (DownloadSite::ModelScope, model_scope_url),
            ] {
                let url = model.download_url(site);
                assert_eq!(
                    url,
                    expected_url,
                    "{site:?} URL of {}",
                    model.display_name()
                );
                assert_eq!(
                    url.rsplit('/').next(),
                    Some(remote_file_name),
                    "{site:?} must download the published remote file for {}",
                    model.display_name()
                );
            }
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
    fn both_download_sites_should_be_offered_in_page_order() {
        assert_eq!(
            DownloadSite::ALL,
            [DownloadSite::HuggingFace, DownloadSite::ModelScope]
        );
    }

    #[test]
    fn every_download_site_should_keep_its_stable_wire_id() {
        for (site, wire_id) in [
            (DownloadSite::HuggingFace, "hugging_face"),
            (DownloadSite::ModelScope, "model_scope"),
        ] {
            assert_eq!(
                serde_json::to_value(site).expect("the site should serialize"),
                json!(wire_id)
            );
            assert_eq!(
                serde_json::from_value::<DownloadSite>(json!(wire_id))
                    .expect("the site id should decode"),
                site
            );
        }
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

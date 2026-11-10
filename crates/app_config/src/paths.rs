//! Where the application keeps its own files.
//!
//! The settings file and the directories whose defaults sit beside it are named here, so the
//! store that writes the file and the settings that name a directory agree on one root.

use std::path::PathBuf;

/// The name of the directory the settings file is stored in.
const SETTINGS_DIRECTORY_NAME: &str = "kytrix";

/// The name of the directory translation models are stored in by default.
const MODELS_DIRECTORY_NAME: &str = "models";

/// The directory the settings file is stored in: `kytrix` under the platform's config directory.
///
/// Nothing here creates it; the store creates what it needs when it writes the file.
pub fn settings_directory() -> Option<PathBuf> {
    dirs::config_dir().map(|directory| directory.join(SETTINGS_DIRECTORY_NAME))
}

/// The directory translation models are stored in unless the settings name another: `models`
/// under [`settings_directory`].
pub fn default_model_directory() -> Option<PathBuf> {
    settings_directory().map(|directory| directory.join(MODELS_DIRECTORY_NAME))
}

#[cfg(test)]
mod tests {
    use super::{default_model_directory, settings_directory};

    #[test]
    fn the_default_model_directory_sits_beside_the_settings_file() {
        assert_eq!(
            default_model_directory(),
            settings_directory().map(|directory| directory.join("models"))
        );
    }

    #[test]
    fn the_settings_directory_is_the_platform_config_directory() {
        assert_eq!(
            settings_directory(),
            dirs::config_dir().map(|directory| directory.join("kytrix"))
        );
    }
}

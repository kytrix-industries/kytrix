use std::path::PathBuf;

const SETTINGS_DIRECTORY_NAME: &str = "kytrix";

const MODELS_DIRECTORY_NAME: &str = "models";

pub fn settings_directory() -> Option<PathBuf> {
    dirs::config_dir().map(|directory| directory.join(SETTINGS_DIRECTORY_NAME))
}

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

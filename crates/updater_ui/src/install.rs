#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;

use bytes::Bytes;
use cargo_packager_updater::{
    Config, Update, UpdateFormat, WindowsConfig, WindowsUpdateInstallMode,
};
use i18n::language::codes::LanguageCode;
use tokio::{runtime::Handle, task::JoinHandle};

use crate::{
    error::InstallError,
    publish,
    release::{ReleaseFormat, ReleaseInfo},
};

pub(crate) fn supported() -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        true
    } else if cfg!(target_os = "linux") {
        appimage().is_some()
    } else {
        false
    }
}

pub(crate) fn install(
    runtime: &Handle,
    release: &ReleaseInfo,
    bytes: Bytes,
    language: LanguageCode,
) -> JoinHandle<Result<(), InstallError>> {
    let release = release.clone();

    runtime.spawn_blocking(move || {
        let update = update_value(&release, language)?;
        update
            .install(bytes.to_vec())
            .map_err(InstallError::Library)
    })
}

fn update_value(release: &ReleaseInfo, language: LanguageCode) -> Result<Update, InstallError> {
    Ok(Update {
        config: Config {
            endpoints: Vec::new(),
            pubkey: String::new(),
            windows: Some(WindowsConfig {
                installer_args: None,
                install_mode: Some(WindowsUpdateInstallMode::Passive),
            }),
        },
        body: release
            .notes
            .as_ref()
            .and_then(|notes| notes.in_language(language))
            .map(ToString::to_string),
        current_version: publish::CURRENT_VERSION.to_owned(),
        version: release.version.to_string(),
        date: release.pub_date,
        target: release.key.wire().to_owned(),
        extract_path: extract_path()?,
        download_url: release.asset.url.clone(),
        signature: String::new(),
        timeout: None,
        headers: http::HeaderMap::new(),
        format: release.asset.format.into(),
    })
}

fn extract_path() -> Result<PathBuf, InstallError> {
    let executable = std::env::current_exe().map_err(|_| InstallError::ExtractPath)?;

    #[cfg(windows)]
    let path = executable
        .parent()
        .map(PathBuf::from)
        .ok_or(InstallError::ExtractPath)?;

    #[cfg(target_os = "macos")]
    let path = macos_bundle(&executable)?;

    #[cfg(target_os = "linux")]
    let path = appimage().unwrap_or(executable);

    Ok(path)
}

#[cfg(target_os = "macos")]
fn macos_bundle(executable: &Path) -> Result<PathBuf, InstallError> {
    let contents = executable.parent().ok_or(InstallError::ExtractPath)?;
    if !contents.to_string_lossy().contains("Contents/MacOS") {
        return Ok(contents.to_path_buf());
    }

    contents
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .ok_or(InstallError::ExtractPath)
}

fn appimage() -> Option<PathBuf> {
    std::env::var_os("APPIMAGE").map(PathBuf::from)
}

impl From<ReleaseFormat> for UpdateFormat {
    fn from(format: ReleaseFormat) -> Self {
        match format {
            ReleaseFormat::Nsis => Self::Nsis,
            ReleaseFormat::App => Self::App,
            ReleaseFormat::AppImage => Self::AppImage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::{PlatformKey, ReleaseNotes};

    #[test]
    fn a_release_format_should_map_to_the_installer_that_takes_it() {
        assert!(matches!(
            UpdateFormat::from(ReleaseFormat::Nsis),
            UpdateFormat::Nsis
        ));
        assert!(matches!(
            UpdateFormat::from(ReleaseFormat::App),
            UpdateFormat::App
        ));
        assert!(matches!(
            UpdateFormat::from(ReleaseFormat::AppImage),
            UpdateFormat::AppImage
        ));
    }

    #[test]
    fn the_extract_path_should_come_from_the_running_executable() {
        let executable = std::env::current_exe().expect("the test binary has a path");
        let path = extract_path().expect("the test binary's path is usable");

        if cfg!(windows) {
            assert_eq!(path, executable.parent().unwrap());
        } else {
            assert!(
                path == executable
                    || path.is_dir()
                    || path
                        .extension()
                        .is_some_and(|extension| extension == "AppImage"),
                "the extracted path is the executable, its directory, or the bundle above it"
            );
        }
    }

    #[test]
    fn only_a_platform_that_can_replace_itself_should_be_installable() {
        if cfg!(any(windows, target_os = "macos")) {
            assert!(supported());
        } else if cfg!(target_os = "linux") {
            assert_eq!(supported(), std::env::var_os("APPIMAGE").is_some());
        } else {
            assert!(!supported());
        }
    }

    #[test]
    fn an_update_should_be_built_from_the_release_it_came_from() {
        let release = ReleaseInfo {
            key: PlatformKey::current().expect("the tests run on a packaged platform"),
            version: semver::Version::new(1, 2, 3),
            notes: Some(ReleaseNotes::for_test(&[
                ("en", "what changed"),
                ("zh-CN", "更新内容"),
            ])),
            pub_date: None,
            asset: crate::release::PlatformRelease {
                url: url::Url::parse("https://example.invalid/kytrix-1.2.3-setup.exe").unwrap(),
                format: ReleaseFormat::Nsis,
            },
        };

        let update =
            update_value(&release, LanguageCode::ZhCn).expect("the running executable has a path");

        assert_eq!(update.version, "1.2.3");
        assert_eq!(update.current_version, publish::CURRENT_VERSION);
        assert_eq!(update.target, release.key.wire());
        assert_eq!(update.body.as_deref(), Some("更新内容"));
        assert_eq!(update.download_url, release.asset.url);
        assert_eq!(update.date, None);
        assert!(update.headers.is_empty());
        assert!(matches!(
            update
                .config
                .windows
                .as_ref()
                .and_then(|w| w.install_mode.clone()),
            Some(WindowsUpdateInstallMode::Passive)
        ));
    }
}

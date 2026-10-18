//! The platform install, through the updater library's own installer.
//!
//! Installing is not downloaded-and-replaced generically: each platform has its
//! own shape — Windows hands the NSIS installer to a detached PowerShell process
//! and exits, macOS unpacks a `.tar.gz` over the application bundle and escalates
//! through `osascript` when the bundle is not writable, Linux writes the AppImage
//! over the one the application is running from. All three are the library's, so
//! what this module owns is the `Update` value they are told to install and the
//! path they replace, which the library computes privately from its own builder.

#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;

use bytes::Bytes;
use cargo_packager_updater::{
    Config, Update, UpdateFormat, WindowsConfig, WindowsUpdateInstallMode,
};
use i18n::language::codes::LanguageCode;
use tokio::task::JoinHandle;
use widget_tokio_runtime::runtime::Runtime;

use crate::{
    error::InstallError,
    publish,
    release::{ReleaseFormat, ReleaseInfo},
};

/// Whether the running application can install an update over itself.
///
/// Windows and macOS install in place. On Linux only an AppImage can be: anything
/// else was put there by the distribution's package manager, which owns the files
/// and would overwrite them again on its next upgrade, so there the release page is
/// the only way forward.
pub(crate) fn supported() -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        true
    } else if cfg!(target_os = "linux") {
        appimage().is_some()
    } else {
        false
    }
}

/// Installs `bytes` as `release`, off the interface thread.
///
/// The blocking work is handed to the application runtime: it writes the package,
/// relocates or replaces the running application and, on macOS, may run `osascript`
/// for an authorization prompt, none of which may happen on the interface thread.
///
/// On Windows the call does not come back at all: the library starts the installer
/// and exits the process from inside it, so the returned handle never resolves and
/// nothing reports an outcome. On macOS and Linux it resolves once the files are in
/// place, and the application is what leaves afterwards.
pub(crate) fn install(
    runtime: &Runtime,
    release: &ReleaseInfo,
    bytes: Bytes,
    language: LanguageCode,
) -> JoinHandle<Result<(), InstallError>> {
    let release = release.clone();

    runtime.blocking(move || {
        let update = update_value(&release, language)?;
        update
            .install(bytes.to_vec())
            .map_err(InstallError::Library)
    })
}

/// The library's `Update` value for `release`, which is what its installer takes.
///
/// Every field is the library's own, checked against `cargo-packager-updater`
/// 0.2.3's `Update`: `date` is `Option<OffsetDateTime>`, `download_url` a `Url`,
/// `target` the wire key, `format` the installer kind, and `headers` an
/// `http::HeaderMap`. `body` is the release notes in `language`, which the
/// installer reads none of. Nothing here downloads: the payload was fetched by
/// this crate's own request, through the route the user configured.
fn update_value(release: &ReleaseInfo, language: LanguageCode) -> Result<Update, InstallError> {
    Ok(Update {
        // The installer reads the Windows install mode and nothing else of the
        // configuration, so the endpoints are left empty rather than pointing a
        // second copy of the same URL at a request nobody makes. The key is the
        // published one: it is what a future re-verification would check against.
        config: Config {
            endpoints: Vec::new(),
            pubkey: publish::PUBKEY.to_owned(),
            windows: Some(WindowsConfig {
                installer_args: None,
                // Passive shows the installer's progress window and asks nothing.
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
        signature: release.asset.signature.to_string(),
        timeout: None,
        headers: http::HeaderMap::new(),
        format: release.asset.format.into(),
    })
}

/// The path the platform's installer replaces, from the running executable.
///
/// This is the library's own rule (`extract_path_from_executable`, private to it):
/// the executable's directory on Windows, the bundle two levels above the binary on
/// macOS, and the AppImage the application was started from on Linux.
fn extract_path() -> Result<PathBuf, InstallError> {
    let executable = std::env::current_exe().map_err(|_| InstallError::ExtractPath)?;

    // Windows installs into the directory the executable is in.
    #[cfg(windows)]
    let path = executable
        .parent()
        .map(PathBuf::from)
        .ok_or(InstallError::ExtractPath)?;

    // A macOS binary is `<app>.app/Contents/MacOS/<binary>`, so the bundle is two
    // levels above it.
    #[cfg(target_os = "macos")]
    let path = macos_bundle(&executable)?;

    // An AppImage is the application: it is what gets replaced.
    #[cfg(target_os = "linux")]
    let path = appimage().unwrap_or(executable);

    Ok(path)
}

/// The application bundle a macOS binary belongs to.
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

/// The AppImage the application is running from, when it is one.
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

    /// The path the installer is pointed at is the running executable's own place,
    /// which is what the library would have computed for the same build.
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

    /// A build that cannot be installed over is one the dialog offers the release
    /// page for, so what `supported` says has to match the platform's own rule.
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
                signature: smol_str::SmolStr::new_static("signature"),
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
        assert_eq!(update.signature, "signature");
        assert_eq!(update.date, None);
        assert_eq!(update.config.pubkey, publish::PUBKEY);
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

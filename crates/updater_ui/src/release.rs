//! `latest.json` as the release workflow publishes it, and what one build takes from it.

use ahash::HashMap;
use i18n::language::codes::LanguageCode;
use semver::Version;
use serde::Deserialize;
use smol_str::SmolStr;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use url::Url;

use crate::error::{MetadataError, UpdaterError};

/// How one platform's update package is installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum ReleaseFormat {
    /// The Windows NSIS installer.
    #[serde(rename = "nsis")]
    Nsis,
    /// The macOS application bundle, as a `.tar.gz`.
    #[serde(rename = "app")]
    App,
    /// The Linux AppImage.
    #[serde(rename = "appimage")]
    AppImage,
}

/// One platform of a release: what to download, and how it is installed.
#[derive(Clone, Debug, Deserialize)]
pub struct PlatformRelease {
    /// Where the payload is downloaded from.
    pub url: Url,
    /// How the payload is installed.
    pub format: ReleaseFormat,
}

/// The platform keys a release is published under.
///
/// The spelling is the updater library's own, which is what
/// [`PlatformKey::wire`] returns and what `cargo_packager_updater::target()`
/// produces for the running platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformKey {
    /// `windows-x86_64`.
    WindowsX64,
    /// `windows-aarch64`.
    WindowsArm64,
    /// `macos-x86_64`.
    MacosX64,
    /// `macos-aarch64`.
    MacosArm64,
    /// `linux-x86_64`.
    LinuxX64,
    /// `linux-aarch64`.
    LinuxArm64,
}

impl PlatformKey {
    /// The key of the platform this build runs on, when it is one the application is packaged for.
    pub fn current() -> Option<Self> {
        if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            Some(Self::WindowsX64)
        } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
            Some(Self::WindowsArm64)
        } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            Some(Self::MacosX64)
        } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Some(Self::MacosArm64)
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some(Self::LinuxX64)
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            Some(Self::LinuxArm64)
        } else {
            None
        }
    }

    /// The key as a release spells it.
    pub const fn wire(self) -> &'static str {
        match self {
            Self::WindowsX64 => "windows-x86_64",
            Self::WindowsArm64 => "windows-aarch64",
            Self::MacosX64 => "macos-x86_64",
            Self::MacosArm64 => "macos-aarch64",
            Self::LinuxX64 => "linux-x86_64",
            Self::LinuxArm64 => "linux-aarch64",
        }
    }
}

/// The language every release publishes notes in, and the fallback for the rest.
const FALLBACK_LANGUAGE: &str = "en";

/// The release notes, keyed by the language code each translation is published under.
///
/// `latest.json` publishes one entry per language, e.g.
/// `{"en": "...", "zh-CN": "..."}`, and the dialog draws the one for the language
/// the interface is in.
#[derive(Clone, Debug, Deserialize)]
#[serde(transparent)]
pub struct ReleaseNotes(HashMap<SmolStr, SmolStr>);

impl ReleaseNotes {
    /// The notes in `language`, or English when that language was not published.
    pub fn in_language(&self, language: LanguageCode) -> Option<&SmolStr> {
        self.0
            .get(language.as_str())
            .or_else(|| self.0.get(FALLBACK_LANGUAGE))
    }

    /// Notes built from `entries`, for a test that draws one.
    #[cfg(test)]
    pub(crate) fn for_test(entries: &[(&'static str, &'static str)]) -> Self {
        Self(
            entries
                .iter()
                .map(|(language, text)| (SmolStr::new_static(language), SmolStr::new_static(text)))
                .collect(),
        )
    }
}

/// The release the metadata endpoint announces.
#[derive(Clone, Debug, Deserialize)]
pub struct ReleaseManifest {
    /// The version the release is of.
    pub version: Version,
    /// The release notes, by language, which the dialog draws as markdown.
    pub notes: Option<ReleaseNotes>,
    /// When the release was published, as RFC 3339 text.
    pub pub_date: Option<SmolStr>,
    /// One entry per platform the release is published for.
    pub platforms: HashMap<SmolStr, PlatformRelease>,
}

impl ReleaseManifest {
    /// Reads a manifest out of the response body.
    pub fn parse(body: &[u8]) -> Result<Self, UpdaterError> {
        serde_json::from_slice(body)
            .map_err(|source| UpdaterError::Metadata(MetadataError::Json(source)))
    }

    /// The update one platform would install from this release, when it is newer than `current`.
    ///
    /// `Ok(None)` is a release that is not newer than the running build, which is
    /// most checks. A release that is newer but names no entry for `key` is not an
    /// update this build can install at all, which is a failure rather than silence.
    pub fn select(
        &self,
        key: PlatformKey,
        current: &Version,
    ) -> Result<Option<ReleaseInfo>, UpdaterError> {
        if self.version <= *current {
            return Ok(None);
        }
        let Some(asset) = self.platforms.get(key.wire()) else {
            return Err(UpdaterError::UnsupportedPlatform);
        };
        let pub_date = self
            .pub_date
            .as_deref()
            .map(|value| OffsetDateTime::parse(value, &Rfc3339))
            .transpose()
            .map_err(|source| UpdaterError::Metadata(MetadataError::PublishDate(source)))?;

        Ok(Some(ReleaseInfo {
            key,
            version: self.version.clone(),
            notes: self.notes.clone(),
            pub_date,
            asset: asset.clone(),
        }))
    }
}

/// One release reduced to what downloading and installing it needs.
#[derive(Clone, Debug)]
pub struct ReleaseInfo {
    /// The platform key the payload was published under.
    pub key: PlatformKey,
    /// The version the release is of.
    pub version: Version,
    /// The release notes, by language, when the release published any.
    pub notes: Option<ReleaseNotes>,
    /// When the release was published, when it says.
    pub pub_date: Option<OffsetDateTime>,
    /// The payload for this platform.
    pub asset: PlatformRelease,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest that names every platform the release workflow publishes.
    const COMPLETE: &str = r####"{
        "version": "9.9.9",
        "notes": {"en": "## Notes\n\nWhat changed.", "zh-CN": "## 说明\n\n更新内容。"},
        "pub_date": "2026-10-20T09:00:00Z",
        "platforms": {
            "windows-x86_64":  { "url": "https://example.invalid/kytrix-x64-setup.exe",  "format": "nsis" },
            "windows-aarch64": { "url": "https://example.invalid/kytrix-arm64-setup.exe", "format": "nsis" },
            "macos-x86_64":    { "url": "https://example.invalid/kytrix-x64.app.tar.gz",  "format": "app" },
            "macos-aarch64":   { "url": "https://example.invalid/kytrix-arm64.app.tar.gz", "format": "app" },
            "linux-x86_64":    { "url": "https://example.invalid/kytrix-x86_64.AppImage", "format": "appimage" },
            "linux-aarch64":   { "url": "https://example.invalid/kytrix-aarch64.AppImage", "format": "appimage" }
        }
    }"####;

    fn current() -> Version {
        Version::parse("1.0.0").unwrap()
    }

    #[test]
    fn a_release_should_be_read_with_every_platform_it_publishes() {
        let manifest = ReleaseManifest::parse(COMPLETE.as_bytes()).expect("the manifest parses");

        assert_eq!(manifest.version, Version::new(9, 9, 9));
        let notes = manifest
            .notes
            .as_ref()
            .expect("the release publishes notes");
        assert_eq!(
            notes.in_language(LanguageCode::En).map(SmolStr::as_str),
            Some("## Notes\n\nWhat changed.")
        );
        assert_eq!(
            notes.in_language(LanguageCode::ZhCn).map(SmolStr::as_str),
            Some("## 说明\n\n更新内容。")
        );
        assert_eq!(manifest.platforms.len(), 6, "every platform is an entry");

        for key in [
            PlatformKey::WindowsX64,
            PlatformKey::WindowsArm64,
            PlatformKey::MacosX64,
            PlatformKey::MacosArm64,
            PlatformKey::LinuxX64,
            PlatformKey::LinuxArm64,
        ] {
            let release = manifest
                .select(key, &current())
                .unwrap_or_else(|error| panic!("{key:?} is published: {error}"))
                .unwrap_or_else(|| panic!("{key:?} is newer than the running build"));
            assert_eq!(release.key, key);
            assert_eq!(release.asset.url.host_str(), Some("example.invalid"));
        }
        assert_eq!(
            manifest
                .select(PlatformKey::WindowsX64, &current())
                .unwrap()
                .unwrap()
                .asset
                .format,
            ReleaseFormat::Nsis
        );
        assert_eq!(
            manifest
                .select(PlatformKey::LinuxArm64, &current())
                .unwrap()
                .unwrap()
                .asset
                .format,
            ReleaseFormat::AppImage
        );
    }

    #[test]
    fn a_release_without_notes_or_a_publish_date_should_still_be_read() {
        let manifest = ReleaseManifest::parse(
            br#"{"version": "9.9.9", "platforms": {"windows-x86_64": {"url": "https://example.invalid/a.exe", "format": "nsis"}}}"#,
        )
        .expect("the optional fields are optional");

        assert!(manifest.notes.is_none());
        assert_eq!(manifest.pub_date, None);
        let release = manifest
            .select(PlatformKey::WindowsX64, &current())
            .expect("the platform is published")
            .expect("the release is newer");
        assert!(release.notes.is_none());
        assert_eq!(release.pub_date, None);
    }

    #[test]
    fn notes_for_a_language_that_was_not_published_should_fall_back_to_english() {
        let notes = ReleaseNotes::for_test(&[("en", "English notes")]);

        assert_eq!(
            notes.in_language(LanguageCode::ZhCn).map(SmolStr::as_str),
            Some("English notes")
        );
        assert_eq!(
            notes.in_language(LanguageCode::En).map(SmolStr::as_str),
            Some("English notes")
        );
    }

    #[test]
    fn a_publish_date_that_is_not_rfc_3339_should_be_refused() {
        let manifest = ReleaseManifest::parse(
            br#"{"version": "9.9.9", "pub_date": "20 October 2026", "platforms": {"windows-x86_64": {"url": "https://example.invalid/a.exe", "format": "nsis"}}}"#,
        )
        .expect("the text is a string like any other");

        assert!(matches!(
            manifest.select(PlatformKey::WindowsX64, &current()),
            Err(UpdaterError::Metadata(MetadataError::PublishDate(_)))
        ));
    }

    #[test]
    fn a_format_the_updater_does_not_know_should_be_refused() {
        assert!(
            ReleaseManifest::parse(
                br#"{"version": "9.9.9", "platforms": {"linux-x86_64": {"url": "https://example.invalid/a.deb", "format": "deb"}}}"#
            )
            .is_err(),
            "a format no installer can take is not a release this build reads"
        );
    }

    #[test]
    fn only_a_newer_version_should_be_an_update() {
        let manifest = ReleaseManifest::parse(COMPLETE.as_bytes()).unwrap();

        assert!(
            manifest
                .select(PlatformKey::WindowsX64, &Version::new(9, 9, 9))
                .unwrap()
                .is_none(),
            "the same version is not an update"
        );
        assert!(
            manifest
                .select(PlatformKey::WindowsX64, &Version::new(10, 0, 0))
                .unwrap()
                .is_none(),
            "a newer running build is not an update either"
        );
        assert!(
            manifest
                .select(PlatformKey::WindowsX64, &Version::new(9, 9, 8))
                .unwrap()
                .is_some(),
            "a newer release is an update"
        );
    }

    #[test]
    fn a_newer_release_without_this_platform_should_be_refused() {
        let manifest = ReleaseManifest::parse(
            br#"{"version": "9.9.9", "platforms": {"linux-x86_64": {"url": "https://example.invalid/a.AppImage", "format": "appimage"}}}"#,
        )
        .unwrap();

        assert!(matches!(
            manifest.select(PlatformKey::WindowsX64, &current()),
            Err(UpdaterError::UnsupportedPlatform)
        ));
        assert!(
            manifest
                .select(PlatformKey::LinuxX64, &Version::new(10, 0, 0))
                .unwrap()
                .is_none(),
            "a release that is not newer is not an update even where it is published"
        );
    }

    #[test]
    fn the_platform_key_should_be_the_updater_librarys_own() {
        match cargo_packager_updater::target() {
            Some(target) => assert_eq!(
                PlatformKey::current().map(PlatformKey::wire),
                Some(target.as_str()),
                "the key a release is published under is the key the library looks for"
            ),
            None => assert_eq!(
                PlatformKey::current(),
                None,
                "a platform the library cannot name is one this build has no key for"
            ),
        }
    }
}

use app_config::network::ConfigError;
use cargo_packager_updater as updater;
use download_provider::error::DownloadFailure;
use i18n::{t_smol, translate::Translate};
use network::transport::error::TransportError;
use smol_str::SmolStr;
use thiserror::Error;
use widget::error::TranslatedError;

#[derive(Debug, Error)]
pub enum UpdaterError {
    #[error("the update route could not be resolved")]
    Route(#[source] ConfigError),
    #[error("the update HTTP client could not be created")]
    Client(#[source] TransportError),
    #[error("the update request failed")]
    Transport(#[source] UpdateRequestError),
    #[error("the update download failed")]
    Download(#[source] DownloadFailure),
    #[error("the update metadata could not be read")]
    Metadata(#[source] MetadataError),
    #[error("this platform has no in-app update")]
    UnsupportedPlatform,
    #[error("the update could not be installed")]
    Install(#[source] InstallError),
}

impl UpdaterError {
    pub fn message_key(&self) -> &'static str {
        match self {
            Self::Route(_) => "error.updater.route",
            Self::Client(_) => "error.updater.client",
            Self::Transport(_) => "error.updater.transport",
            Self::Download(_) => "error.updater.download",
            Self::Metadata(_) => "error.updater.metadata",
            Self::UnsupportedPlatform => "error.updater.unsupported_platform",
            Self::Install(_) => "error.updater.install",
        }
    }
}

impl TranslatedError for UpdaterError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        t_smol!(translate, self.message_key())
    }
}

#[derive(Debug, Error)]
pub enum UpdateRequestError {
    #[error("the update request could not be sent")]
    Send(#[source] reqwest::Error),
    #[error("the update request was cancelled")]
    Cancelled,
    #[error("the update response could not be read")]
    Read(#[source] TransportError),
    #[error("the update request was answered with HTTP {status}")]
    Status { status: http::StatusCode },
    #[error("the update response redirected somewhere unusable")]
    Redirect(#[source] RedirectRefusal),
    #[error("the update request task did not finish")]
    Task(#[source] tokio::task::JoinError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum RedirectRefusal {
    #[error("the redirect budget is spent")]
    HopLimit,
    #[error("the redirect would leave https")]
    Downgrade,
    #[error("the redirect location is not an HTTP or HTTPS URL")]
    Scheme,
    #[error("the redirect named no location")]
    MissingLocation,
    #[error("the redirect location is not a URL")]
    MalformedLocation,
}

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("the update metadata endpoint is not a URL")]
    Endpoint(#[source] url::ParseError),
    #[error("the update metadata is not the JSON the updater expects")]
    Json(#[source] serde_json::Error),
    #[error("the version this build reports is not a semantic version")]
    Version(#[source] semver::Error),
    #[error("the update publish date is not an RFC 3339 timestamp")]
    PublishDate(#[source] time::error::Parse),
}

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("the update package was refused by the installer")]
    Library(#[source] updater::Error),
    #[error("the location of the running application could not be determined")]
    ExtractPath,
    #[error("the update install task did not finish")]
    Task(#[source] tokio::task::JoinError),
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use download_provider::error::DownloadFailure;
    use i18n::{embedded, language::codes::LanguageCode, translate::Translate};
    use url::Url;

    use super::*;

    fn message_keys() -> Vec<&'static str> {
        const TEST_MODULE: &str = concat!("#[cfg", "(test)]");
        let file = include_str!("./error.rs");
        let production = file.split_once(TEST_MODULE).map_or(file, |(half, _)| half);
        let mut keys = Vec::new();
        let mut rest = production;

        while let Some(found) = rest.find("\"error.updater.") {
            let key = &rest[found + 1..];
            let Some(closing) = key.find('"') else {
                break;
            };
            keys.push(&key[..closing]);
            rest = &key[closing..];
        }

        keys
    }

    #[test]
    fn every_message_key_should_be_defined_in_every_locale() {
        let i18n = embedded::load().expect("the embedded locales load");
        let keys = message_keys();

        assert_eq!(
            keys.len(),
            7,
            "every failure the updater reports names its message, and {} was read",
            keys.len()
        );

        for language in i18n.languages() {
            let defined = i18n
                .keys(language)
                .expect("an embedded language exposes its keys")
                .collect::<Vec<_>>();

            for key in &keys {
                assert!(
                    defined.contains(key),
                    "{key} is named by a failure but {language} does not define it"
                );
                assert!(
                    i18n.try_translate(language, key)
                        .is_some_and(|text| !text.is_empty()),
                    "{key} is named by a failure but renders empty in {language}"
                );
            }
        }
    }

    #[test]
    fn every_failure_should_be_stated_in_the_language_it_is_drawn_in() {
        let en = Translate::new(
            Rc::new(embedded::load().expect("the embedded locales load")),
            LanguageCode::En,
        );
        let zh = Translate::new(
            Rc::new(embedded::load().expect("the embedded locales load")),
            LanguageCode::ZhCn,
        );
        let failures = [
            UpdaterError::Transport(UpdateRequestError::Cancelled),
            UpdaterError::Download(DownloadFailure::Cancelled),
            UpdaterError::Metadata(MetadataError::Endpoint(
                Url::parse("not a url").expect_err("the literal is not a URL"),
            )),
            UpdaterError::UnsupportedPlatform,
            UpdaterError::Install(InstallError::ExtractPath),
        ];

        for failure in &failures {
            let english = failure.translated_message(&en);
            let chinese = failure.translated_message(&zh);
            assert!(
                !english.is_empty() && !chinese.is_empty(),
                "{failure:?} states nothing in one language"
            );
            assert_ne!(
                english, chinese,
                "{failure:?} is the same sentence in both languages"
            );
            assert!(
                !english.contains("error.updater."),
                "{failure:?} draws its catalogue key rather than the sentence"
            );
        }
    }
}

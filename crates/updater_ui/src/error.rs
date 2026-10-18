//! Every way the update path can fail, as values the interface can name.
//!
//! [`UpdaterError`] is the one type the state machine carries: each of its variants
//! is a thing the dialog says differently about, through the catalogue key
//! [`UpdaterError::message_key`] names. The types it wraps are per-stage, so a log
//! line still says which step failed and why.

use app_config::network::ConfigError;
use cargo_packager_updater as updater;
use network::transport::error::TransportError;
use thiserror::Error;

/// Everything the update path can fail with.
#[derive(Debug, Error)]
pub enum UpdaterError {
    /// The outbound route for an update request could not be resolved.
    ///
    /// A cancelled resolution reports itself as a route failure, because that is
    /// what the transport's own error type says; the caller tells a cancellation
    /// from a failure by the token it cancelled with, not by this.
    #[error("the update route could not be resolved")]
    Route(#[source] ConfigError),
    /// The HTTP client for the resolved route could not be built.
    #[error("the update HTTP client could not be created")]
    Client(#[source] TransportError),
    /// The request or its response failed on the wire.
    #[error("the update request failed")]
    Transport(#[source] UpdateRequestError),
    /// The published metadata could not be read as the release it announces.
    #[error("the update metadata could not be read")]
    Metadata(#[source] MetadataError),
    /// This build has no update it could install for the platform it runs on.
    #[error("this platform has no in-app update")]
    UnsupportedPlatform,
    /// The platform's installer did not take the payload.
    #[error("the update could not be installed")]
    Install(#[source] InstallError),
}

impl UpdaterError {
    /// The catalogue key the interface speaks this failure with.
    pub fn message_key(&self) -> &'static str {
        match self {
            Self::Route(_) => "error.updater.route",
            Self::Client(_) => "error.updater.client",
            Self::Transport(_) => "error.updater.transport",
            Self::Metadata(_) => "error.updater.metadata",
            Self::UnsupportedPlatform => "error.updater.unsupported_platform",
            Self::Install(_) => "error.updater.install",
        }
    }
}

/// Why an update request did not produce a response.
#[derive(Debug, Error)]
pub enum UpdateRequestError {
    /// The request could not be sent at all.
    #[error("the update request could not be sent")]
    Send(#[source] reqwest::Error),
    /// The user cancelled the request while it was in flight.
    #[error("the update request was cancelled")]
    Cancelled,
    /// The response could not be read.
    #[error("the update response could not be read")]
    Read(#[source] TransportError),
    /// The server answered with a status that carries no payload.
    #[error("the update request was answered with HTTP {status}")]
    Status { status: http::StatusCode },
    /// The response redirected somewhere this client does not follow.
    #[error("the update response redirected somewhere unusable")]
    Redirect(#[source] RedirectRefusal),
    /// The task that was running the request did not finish.
    #[error("the update request task did not finish")]
    Task(#[source] tokio::task::JoinError),
}

/// Why a redirect response was not followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum RedirectRefusal {
    /// The request's hop budget is spent.
    #[error("the redirect budget is spent")]
    HopLimit,
    /// Following it would leave encrypted transport.
    #[error("the redirect would leave https")]
    Downgrade,
    /// The location is not an HTTP or HTTPS URL.
    #[error("the redirect location is not an HTTP or HTTPS URL")]
    Scheme,
    /// The response did not name where it goes.
    #[error("the redirect named no location")]
    MissingLocation,
    /// The location is not a URL this request can be sent to.
    #[error("the redirect location is not a URL")]
    MalformedLocation,
}

/// Why `latest.json` could not be read as the release it announces.
#[derive(Debug, Error)]
pub enum MetadataError {
    /// The compiled-in endpoint is not a URL.
    ///
    /// It is a constant, so this is a build error rather than something a user
    /// can act on; it is a variant rather than a panic because a check that
    /// cannot run is a check that failed, and the updater already says so.
    #[error("the update metadata endpoint is not a URL")]
    Endpoint(#[source] url::ParseError),
    /// The response is not the JSON the release workflow publishes.
    #[error("the update metadata is not the JSON the updater expects")]
    Json(#[source] serde_json::Error),
    /// The version this build reports is not a semantic version.
    #[error("the version this build reports is not a semantic version")]
    Version(#[source] semver::Error),
    /// The publish date is not an RFC 3339 timestamp.
    #[error("the update publish date is not an RFC 3339 timestamp")]
    PublishDate(#[source] time::error::Parse),
}

/// Why the platform's installer did not take the payload.
#[derive(Debug, Error)]
pub enum InstallError {
    /// The library's installer refused the package.
    #[error("the update package was refused by the installer")]
    Library(#[source] updater::Error),
    /// The running executable's location could not be determined.
    #[error("the location of the running application could not be determined")]
    ExtractPath,
    /// The task that was installing the payload did not finish.
    #[error("the update install task did not finish")]
    Task(#[source] tokio::task::JoinError),
}

#[cfg(test)]
mod tests {
    use i18n::global::embedded;

    /// The catalogue keys this file's arms name, read out of the arms themselves so
    /// that an arm naming a new key is covered by the arm that names it.
    fn message_keys() -> Vec<&'static str> {
        // The test module is not production code, and a scan that ran over it would
        // read the literal below rather than the mappings.
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

    /// A key no locale defines translates to an empty string rather than failing, so
    /// a failure whose key left the catalogue is a dialog with nothing written where
    /// it says what went wrong. Both languages carry every one of them, and none of
    /// them renders empty.
    #[test]
    fn every_message_key_should_be_defined_in_every_locale() {
        let i18n = embedded::load().expect("the embedded locales load");
        let keys = message_keys();

        assert_eq!(
            keys.len(),
            6,
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
}

//! What a failed download is, and the full error behind it.
//!
//! [`DownloadFailure`] is the whole vocabulary a caller reacts to: it is `Copy`,
//! carries no allocation and names the operation that failed rather than the
//! library that reported it. [`DownloadError`] is what the download layer itself
//! returns, so the source is available where the failure is logged;
//! [`DownloadError::failure`] reduces it to that vocabulary.

use std::{io, path::PathBuf};

use app_config::network::ConfigError;
use network::transport::error::TransportError;
use reqwest::StatusCode;
use thiserror::Error;

/// Why a download could not be completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum DownloadFailure {
    /// The download URL is not a URL this client can request.
    #[error("the download URL is invalid")]
    InvalidUrl,
    /// The outbound route for the download could not be resolved.
    #[error("the download route could not be resolved")]
    Route,
    /// The transport could not start the operation or build its client.
    #[error("the download transport could not be started")]
    Transport,
    /// The request could not be sent at all.
    #[error("the download request could not be sent")]
    Request,
    /// The response body could not be read.
    #[error("the download response could not be read")]
    Read,
    /// The server answered with a status that carries no payload.
    #[error("the download was answered with HTTP {status}")]
    Status { status: u16 },
    /// The redirect chain could not be followed to a final response.
    #[error("the download redirect was unusable")]
    Redirect,
    /// The server's partial response did not describe the bytes that were asked for.
    #[error("the download range response was unusable")]
    Range,
    /// A file could not be read, written, replaced or removed.
    #[error("the download file could not be read or written")]
    Io { kind: io::ErrorKind },
    /// The destination has no file name, so no partial file can sit beside it.
    #[error("the download destination has no file name")]
    InvalidDestination,
    /// The bytes that arrived do not match the size that was known or announced.
    #[error("the download was {actual} bytes but {expected} were expected")]
    SizeMismatch { expected: u64, actual: u64 },
    /// The entity is larger than its operation class may download.
    #[error("the download exceeds its {limit} byte budget")]
    TooLarge { limit: u64 },
    /// The download was cancelled.
    #[error("the download was cancelled")]
    Cancelled,
    /// The task that was running the download failed.
    #[error("the download task failed")]
    Task,
}

/// The full download error, with the source that produced it.
///
/// Only the download layer sees this; the interface draws [`DownloadFailure`].
#[derive(Debug, Error)]
pub(crate) enum DownloadError {
    /// The destination has no file name, so there is no partial file to stream into.
    #[error("the download destination {path:?} has no file name")]
    InvalidDestination { path: PathBuf },
    /// The redirect chain could not be followed to a final response.
    #[error("the download redirect was unusable")]
    Redirect(#[source] RedirectFailure),
    /// The server's partial response did not describe the bytes that were asked for.
    #[error("the download range response was unusable")]
    Range(#[source] RangeFailure),
    /// The outbound route for the download could not be resolved.
    #[error("the download route could not be resolved")]
    Route(#[source] ConfigError),
    /// The transport could not start the operation or build its client.
    #[error("the download transport could not be started")]
    Transport(#[source] TransportError),
    /// The request could not be sent at all.
    #[error("the download request could not be sent")]
    Request(#[source] reqwest::Error),
    /// The response body could not be read.
    #[error("the download response could not be read")]
    Read(#[source] reqwest::Error),
    /// The server answered with a status that carries no payload.
    #[error("the download was answered with HTTP {status}")]
    Status { status: StatusCode },
    /// A file could not be read, written, replaced or removed.
    #[error("the download file could not be read or written")]
    Io(#[source] io::Error),
    /// The bytes that arrived do not match the size that was known or announced.
    #[error("the download was {actual} bytes but {expected} were expected")]
    SizeMismatch { expected: u64, actual: u64 },
    /// The entity is larger than its operation class may download.
    #[error("the download exceeds its {limit} byte budget")]
    TooLarge { limit: u64 },
    /// The download was cancelled.
    #[error("the download was cancelled")]
    Cancelled,
    /// The task that was running the download failed.
    #[error("the download task failed")]
    Task(#[source] tokio::task::JoinError),
}

impl DownloadError {
    /// Reduces the full error to the vocabulary the interface draws.
    pub(crate) fn failure(&self) -> DownloadFailure {
        match self {
            Self::InvalidDestination { .. } => DownloadFailure::InvalidDestination,
            Self::Redirect(_) => DownloadFailure::Redirect,
            Self::Range(_) => DownloadFailure::Range,
            Self::Route(_) => DownloadFailure::Route,
            Self::Transport(_) => DownloadFailure::Transport,
            Self::Request(_) => DownloadFailure::Request,
            Self::Read(_) => DownloadFailure::Read,
            Self::Status { status } => DownloadFailure::Status {
                status: status.as_u16(),
            },
            Self::Io(error) => DownloadFailure::Io { kind: error.kind() },
            Self::SizeMismatch { expected, actual } => DownloadFailure::SizeMismatch {
                expected: *expected,
                actual: *actual,
            },
            Self::TooLarge { limit } => DownloadFailure::TooLarge { limit: *limit },
            Self::Cancelled => DownloadFailure::Cancelled,
            Self::Task(_) => DownloadFailure::Task,
        }
    }
}

/// Why a redirect response could not be followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub(crate) enum RedirectFailure {
    /// The redirect did not say where it goes.
    #[error("the redirect named no location")]
    MissingLocation,
    /// The redirect chain is longer than the download class allows.
    #[error("the redirect chain exceeded its hop limit")]
    HopLimit,
    /// The location is not a URL this client can request.
    #[error("the redirect location could not be resolved")]
    MalformedLocation,
    /// The location is not an HTTP or HTTPS URL.
    #[error("the redirect is not an http or https URL")]
    Scheme,
    /// Following the redirect would leave encrypted transport.
    #[error("the redirect would leave https")]
    Downgrade,
}

/// Why a partial response could not be used for the requested range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub(crate) enum RangeFailure {
    /// The server sent `206 Partial Content` without a `Content-Range`.
    #[error("the range response named no content range")]
    Missing,
    /// The `Content-Range` is not a complete byte range.
    #[error("the range response content range is malformed")]
    Malformed,
    /// The server sent a partial response that no range requested.
    #[error("the range response was not requested")]
    Unsolicited,
    /// The partial response starts somewhere other than the requested offset.
    #[error("the range response starts at {actual} but {expected} was requested")]
    StartMismatch { expected: u64, actual: u64 },
    /// The partial response carries a different number of bytes than the range it names.
    #[error("the range response announced {announced} bytes for a {range} byte range")]
    LengthMismatch { range: u64, announced: u64 },
    /// The partial response body was not the length of the range it names.
    #[error("the range response carried {actual} bytes for a {expected} byte range")]
    BodyMismatch { expected: u64, actual: u64 },
    /// The server cannot satisfy the requested range.
    #[error("the range response could not satisfy the requested range")]
    Unsatisfiable,
}

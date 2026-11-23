use std::{io, path::PathBuf};

use app_config::network::ConfigError;
use network::transport::error::TransportError;
use reqwest::StatusCode;
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum DownloadFailure {
    #[error("the download URL is invalid")]
    InvalidUrl,
    #[error("the download route could not be resolved")]
    Route,
    #[error("the download transport could not be started")]
    Transport,
    #[error("the download request could not be sent")]
    Request,
    #[error("the download response could not be read")]
    Read,
    #[error("the download was answered with HTTP {status}")]
    Status { status: u16 },
    #[error("the download redirect was unusable")]
    Redirect,
    #[error("the download range response was unusable")]
    Range,
    #[error("the download file could not be read or written")]
    Io { kind: io::ErrorKind },
    #[error("the download destination has no file name")]
    InvalidDestination,
    #[error("the download was {actual} bytes but {expected} were expected")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("the download exceeds its {limit} byte budget")]
    TooLarge { limit: u64 },
    #[error("the download was cancelled")]
    Cancelled,
    #[error("the download task failed")]
    Task,
}

#[derive(Debug, Error)]
pub(crate) enum DownloadError {
    #[error("the download destination {path:?} has no file name")]
    InvalidDestination { path: PathBuf },
    #[error("the download redirect was unusable")]
    Redirect(#[source] RedirectFailure),
    #[error("the download range response was unusable")]
    Range(#[source] RangeFailure),
    #[error("the download route could not be resolved")]
    Route(#[source] ConfigError),
    #[error("the download transport could not be started")]
    Transport(#[source] TransportError),
    #[error("the download request could not be sent")]
    Request(#[source] reqwest::Error),
    #[error("the download response could not be read")]
    Read(#[source] reqwest::Error),
    #[error("the download was answered with HTTP {status}")]
    Status { status: StatusCode },
    #[error("the download file could not be read or written")]
    Io(#[source] io::Error),
    #[error("the download was {actual} bytes but {expected} were expected")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("the download exceeds its {limit} byte budget")]
    TooLarge { limit: u64 },
    #[error("the download was cancelled")]
    Cancelled,
    #[error("the download task failed")]
    Task(#[source] tokio::task::JoinError),
}

impl DownloadError {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub(crate) enum RedirectFailure {
    #[error("the redirect named no location")]
    MissingLocation,
    #[error("the redirect chain exceeded its hop limit")]
    HopLimit,
    #[error("the redirect location could not be resolved")]
    MalformedLocation,
    #[error("the redirect is not an http or https URL")]
    Scheme,
    #[error("the redirect would leave https")]
    Downgrade,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub(crate) enum RangeFailure {
    #[error("the range response named no content range")]
    Missing,
    #[error("the range response content range is malformed")]
    Malformed,
    #[error("the range response was not requested")]
    Unsolicited,
    #[error("the range response starts at {actual} but {expected} was requested")]
    StartMismatch { expected: u64, actual: u64 },
    #[error("the range response announced {announced} bytes for a {range} byte range")]
    LengthMismatch { range: u64, announced: u64 },
    #[error("the range response carried {actual} bytes for a {expected} byte range")]
    BodyMismatch { expected: u64, actual: u64 },
    #[error("the range response could not satisfy the requested range")]
    Unsatisfiable,
}

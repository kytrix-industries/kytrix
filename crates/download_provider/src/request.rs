//! What a download is asked for.

use std::path::PathBuf;

use app_config::network::RouteTarget;
use network::transport::http::HttpClientClass;
use url::Url;

/// One download request, as the caller that adds it knows it.
#[derive(Clone, Debug)]
pub struct DownloadRequest {
    /// Where the bytes come from.
    pub url: Url,
    /// The file they are assembled into.
    pub destination: PathBuf,
    /// Which configuration route the download takes.
    pub target: RouteTarget,
    /// Which HTTP client policy the download uses.
    pub class: HttpClientClass,
    /// The final size when the caller knows it; a model publishes its own size.
    pub expected_size: Option<u64>,
}

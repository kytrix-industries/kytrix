use std::path::PathBuf;

use app_config::network::RouteTarget;
use network::transport::http::HttpClientClass;
use url::Url;

#[derive(Clone, Debug)]
pub struct DownloadRequest {
    pub url: Url,
    pub destination: PathBuf,
    pub target: RouteTarget,
    pub class: HttpClientClass,
    pub expected_size: Option<u64>,
}

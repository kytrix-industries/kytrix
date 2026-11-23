use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use app_config::network::{Config, KytrixServiceCapability, RouteTarget, ServiceTarget};
use network::transport::{http::HttpClientClass, hub::TransportHub};
use reqwest::{Client, Response, StatusCode, header::LOCATION};
use sha2::{Digest, Sha256};
use smol_str::{SmolStr, format_smolstr};
use thiserror::Error;
use tokio::{fs::File, io::AsyncWriteExt, sync::OnceCell, time::Instant};
use tokio_util::sync::CancellationToken;
use url::Url;

pub(super) const MEDIA_MAX_BYTES: u64 = 32 * 1_024 * 1_024;

const MEDIA_URL_MAX_BYTES: usize = 2_048;

pub(super) const MEDIA_OPERATION_TIMEOUT: Duration = Duration::from_secs(15);

const CACHE_FILE_LIMIT: usize = 512;
const CACHE_BYTE_LIMIT: u64 = 512 * 1_024 * 1_024;

pub(super) fn plausible_media_url(url: &str) -> bool {
    url.len() <= MEDIA_URL_MAX_BYTES
        && ["https://pbs.twimg.com", "https://video.twimg.com"]
            .iter()
            .any(|origin| {
                url.strip_prefix(origin).is_some_and(|rest| {
                    rest.is_empty() || rest.starts_with('/') || rest.starts_with(':')
                })
            })
}

fn valid_x_media_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none_or(|port| port == 443)
        && matches!(url.host_str(), Some("pbs.twimg.com" | "video.twimg.com"))
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum MediaError {
    #[error("the X media URL is invalid")]
    InvalidUrl,
    #[error("the X media proxy route is unavailable: {0}")]
    Route(SmolStr),
    #[error("the X media request failed: {0}")]
    Network(SmolStr),
    #[error("the X media response is too large")]
    TooLarge,
    #[error("the X media response was incomplete")]
    Incomplete,
    #[error("the X media redirect target is invalid")]
    Redirect,
    #[error("X API returned HTTP {0}")]
    Server(StatusCode),
    #[error("the X media request timed out")]
    Timeout,
    #[error("the X media request was cancelled")]
    Cancelled,
    #[error("the X media cache is unavailable: {0}")]
    Cache(SmolStr),
}

impl From<MediaCacheError> for MediaError {
    fn from(error: MediaCacheError) -> Self {
        match error {
            MediaCacheError::Cancelled => Self::Cancelled,
            error => Self::Cache(format_smolstr!("{error}")),
        }
    }
}

pub(super) trait MediaTransport: Clone + Send + Sync + 'static {
    fn client(
        &self,
        destination: &Url,
        cancellation: &CancellationToken,
    ) -> impl Future<Output = Result<Client, MediaError>> + Send;
}

#[derive(Clone)]
pub(super) struct SharedTransport {
    hub: Arc<TransportHub>,
    config: Config,
}

impl SharedTransport {
    pub(super) fn new(hub: Arc<TransportHub>, config: Config) -> Self {
        Self { hub, config }
    }
}

impl MediaTransport for SharedTransport {
    async fn client(
        &self,
        destination: &Url,
        _cancellation: &CancellationToken,
    ) -> Result<Client, MediaError> {
        let route = network::proxy::resolve(
            &self.config,
            &RouteTarget::Service(ServiceTarget::KytrixService {
                capability: KytrixServiceCapability::XMedia,
            }),
            destination,
        )
        .map_err(|error| MediaError::Route(format_smolstr!("{error}")))?;
        self.hub
            .http()
            .client(&route, HttpClientClass::XMedia)
            .map_err(|error| MediaError::Route(format_smolstr!("{error}")))
    }
}

#[derive(Clone)]
pub(super) struct MediaDownloader<T = SharedTransport> {
    transport: T,
    cache: MediaCache,
}

impl<T> MediaDownloader<T> {
    pub(super) fn new(transport: T) -> Self {
        Self {
            transport,
            cache: MediaCache::new(),
        }
    }

    #[cfg(test)]
    pub(super) const fn with_cache(transport: T, cache: MediaCache) -> Self {
        Self { transport, cache }
    }
}

impl<T> MediaDownloader<T>
where
    T: MediaTransport,
{
    pub(super) async fn fetch(
        &self,
        url: &str,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<PathBuf, MediaError> {
        let url = self.parse(url)?;
        if let Some(path) = self
            .cache
            .lookup(&url, MEDIA_MAX_BYTES, cancellation)
            .await?
        {
            return Ok(path);
        }
        let response = self.request(&url, deadline, cancellation).await?;
        self.store(response, deadline, &url, cancellation).await
    }

    fn parse(&self, url: &str) -> Result<Url, MediaError> {
        if url.len() > MEDIA_URL_MAX_BYTES {
            return Err(MediaError::InvalidUrl);
        }
        let url = Url::parse(url).map_err(|_| MediaError::InvalidUrl)?;
        if !valid_x_media_url(&url) {
            return Err(MediaError::InvalidUrl);
        }
        Ok(url)
    }

    async fn request(
        &self,
        initial_url: &Url,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<Response, MediaError> {
        let mut current = initial_url.clone();
        let limit = HttpClientClass::XMedia.redirect_limit();
        for hop in 0..limit {
            let client = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(MediaError::Cancelled),
                result = tokio::time::timeout_at(deadline, self.transport.client(&current, cancellation)) => {
                    result.map_err(|_| MediaError::Timeout)??
                }
            };
            let response = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(MediaError::Cancelled),
                result = tokio::time::timeout_at(
                    deadline,
                    client
                        .get(current.clone())
                        .header(reqwest::header::ACCEPT_ENCODING, "identity")
                        .send(),
                ) => result.map_err(|_| MediaError::Timeout)?.map_err(network_error)?,
            };
            if !response.status().is_redirection() {
                return Ok(response);
            }
            if hop + 1 == limit {
                return Err(MediaError::Redirect);
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or(MediaError::Redirect)?;
            let next = current.join(location).map_err(|_| MediaError::Redirect)?;
            if !valid_x_media_url(&next) {
                return Err(MediaError::Redirect);
            }
            current = next;
        }
        Err(MediaError::Redirect)
    }

    async fn store(
        &self,
        response: Response,
        deadline: Instant,
        url: &Url,
        cancellation: &CancellationToken,
    ) -> Result<PathBuf, MediaError> {
        if !response.status().is_success() {
            return Err(MediaError::Server(response.status()));
        }
        let total = response.content_length();
        if total.is_some_and(|total| total > MEDIA_MAX_BYTES) {
            return Err(MediaError::TooLarge);
        }

        let mut write = self.cache.begin(url, cancellation).await?;
        let result = async {
            let mut body = response;
            let mut received = 0u64;
            loop {
                let chunk = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(MediaError::Cancelled),
                    result = tokio::time::timeout_at(deadline, body.chunk()) => {
                        result.map_err(|_| MediaError::Timeout)?
                    }
                };
                let Some(chunk) = chunk.map_err(network_error)? else {
                    break;
                };
                received = received
                    .checked_add(chunk.len() as u64)
                    .ok_or(MediaError::TooLarge)?;
                if received > MEDIA_MAX_BYTES || total.is_some_and(|total| received > total) {
                    return Err(MediaError::TooLarge);
                }
                write.write_all(&chunk, cancellation).await?;
            }
            if total.is_some_and(|total| total != received) {
                return Err(MediaError::Incomplete);
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            write.abort().await?;
            return Err(error);
        }
        Ok(write.commit(cancellation).await?)
    }
}

fn network_error(error: reqwest::Error) -> MediaError {
    if error.is_timeout() {
        MediaError::Timeout
    } else {
        MediaError::Network(format_smolstr!("{error}"))
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum MediaCacheError {
    #[error("the X media cache directory is unavailable")]
    DirectoryUnavailable,
    #[error("X media cache I/O failed: {0}")]
    Io(SmolStr),
    #[error("the X media cache operation was cancelled")]
    Cancelled,
}

#[derive(Clone)]
pub(super) struct MediaCache {
    inner: Arc<MediaCacheInner>,
}

struct MediaCacheInner {
    root: Option<PathBuf>,
    initialized: OnceCell<Result<(), MediaCacheError>>,
    file_limit: usize,
    byte_limit: u64,
}

impl MediaCache {
    pub(super) fn new() -> Self {
        let root = dirs::cache_dir().map(|root| root.join("kytrix").join("x"));
        Self::with_options(root, CACHE_FILE_LIMIT, CACHE_BYTE_LIMIT)
    }

    pub(super) fn with_options(root: Option<PathBuf>, file_limit: usize, byte_limit: u64) -> Self {
        Self {
            inner: Arc::new(MediaCacheInner {
                root,
                initialized: OnceCell::new(),
                file_limit,
                byte_limit,
            }),
        }
    }

    async fn initialize(&self, cancellation: &CancellationToken) -> Result<&Path, MediaCacheError> {
        let root = self
            .inner
            .root
            .as_deref()
            .ok_or(MediaCacheError::DirectoryUnavailable)?;
        let initialization = self.inner.initialized.get_or_init(|| async {
            tokio::fs::create_dir_all(root).await.map_err(io_error)?;
            let mut entries = tokio::fs::read_dir(root).await.map_err(io_error)?;
            while let Some(entry) = entries.next_entry().await.map_err(io_error)? {
                if entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "tmp")
                {
                    match tokio::fs::remove_file(entry.path()).await {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(io_error(error)),
                    }
                }
            }
            Ok(())
        });
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(MediaCacheError::Cancelled),
            result = initialization => result.clone()?,
        }
        Ok(root)
    }

    pub(super) async fn lookup(
        &self,
        url: &Url,
        max_entry_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<Option<PathBuf>, MediaCacheError> {
        let root = self.initialize(cancellation).await?.to_path_buf();
        let path = root.join(cache_key(url));
        lookup_file(path, max_entry_bytes, cancellation).await
    }

    pub(super) async fn begin(
        &self,
        url: &Url,
        cancellation: &CancellationToken,
    ) -> Result<MediaCacheWrite, MediaCacheError> {
        let root = self.initialize(cancellation).await?;
        let final_path = root.join(cache_key(url));
        let temp_path = root.join(format_smolstr!("{}.tmp", uuid::Uuid::new_v4()).as_str());
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        let open = options.open(temp_path.clone());
        tokio::pin!(open);
        let file = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                if let Ok(file) = open.await {
                    cleanup_temporary_file(Some(file), Some(temp_path)).await?;
                }
                return Err(MediaCacheError::Cancelled);
            }
            result = &mut open => result.map_err(io_error)?,
        };
        Ok(MediaCacheWrite {
            cache: self.clone(),
            file: Some(file),
            temp_path: Some(temp_path),
            final_path,
        })
    }

    async fn enforce_limits(
        &self,
        keep: PathBuf,
        cancellation: &CancellationToken,
    ) -> Result<(), MediaCacheError> {
        let root = self.initialize(cancellation).await?.to_path_buf();
        let file_limit = self.inner.file_limit;
        let byte_limit = self.inner.byte_limit;
        enforce_limits(&root, &keep, file_limit, byte_limit, cancellation).await
    }

    #[cfg(test)]
    pub(super) async fn seed(&self, url: &Url, bytes: &[u8]) -> PathBuf {
        let cancellation = CancellationToken::new();
        let root = self
            .initialize(&cancellation)
            .await
            .expect("the test cache should have a directory");
        let path = root.join(cache_key(url));
        tokio::fs::write(&path, bytes)
            .await
            .expect("the test picture should be written");
        path
    }
}

pub(super) struct MediaCacheWrite {
    cache: MediaCache,
    file: Option<File>,
    temp_path: Option<PathBuf>,
    final_path: PathBuf,
}

impl MediaCacheWrite {
    pub(super) async fn write_all(
        &mut self,
        bytes: &[u8],
        cancellation: &CancellationToken,
    ) -> Result<(), MediaCacheError> {
        if cancellation.is_cancelled() {
            return Err(MediaCacheError::Cancelled);
        }
        let write = self
            .file
            .as_mut()
            .ok_or_else(|| {
                MediaCacheError::Io(SmolStr::new_static("X media cache file is closed"))
            })?
            .write_all(bytes);
        tokio::pin!(write);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                let _ = write.await;
                Err(MediaCacheError::Cancelled)
            }
            result = &mut write => result.map_err(io_error),
        }
    }

    pub(super) async fn commit(
        mut self,
        cancellation: &CancellationToken,
    ) -> Result<PathBuf, MediaCacheError> {
        let result = self.commit_inner(cancellation).await;
        if result.is_err() {
            self.cleanup().await?;
        }
        result
    }

    pub(super) async fn abort(mut self) -> Result<(), MediaCacheError> {
        self.cleanup().await
    }

    async fn commit_inner(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<PathBuf, MediaCacheError> {
        if cancellation.is_cancelled() {
            return Err(MediaCacheError::Cancelled);
        }
        let Some(file) = self.file.as_mut() else {
            return Err(MediaCacheError::Io(SmolStr::new_static(
                "X media cache file is closed",
            )));
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(MediaCacheError::Cancelled),
            result = file.sync_all() => result.map_err(io_error)?,
        }
        drop(self.file.take());
        let temp_path = self.temp_path.as_ref().cloned().ok_or_else(|| {
            MediaCacheError::Io(SmolStr::new_static("X media cache file is missing"))
        })?;
        let final_path = self.final_path.clone();
        let committed = commit_file(&temp_path, &final_path, cancellation).await?;
        self.temp_path.take();
        self.cache
            .enforce_limits(committed.clone(), cancellation)
            .await?;
        Ok(committed)
    }

    async fn cleanup(&mut self) -> Result<(), MediaCacheError> {
        cleanup_temporary_file(self.file.take(), self.temp_path.take()).await
    }
}

impl Drop for MediaCacheWrite {
    fn drop(&mut self) {
        drop(self.file.take());
        if let Some(path) = self.temp_path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

async fn cleanup_temporary_file(
    file: Option<File>,
    path: Option<PathBuf>,
) -> Result<(), MediaCacheError> {
    let close_error = if let Some(mut file) = file {
        let result = file.shutdown().await.map_err(io_error);
        drop(file);
        result.err()
    } else {
        None
    };
    let remove_error = if let Some(path) = path {
        match tokio::fs::remove_file(path).await {
            Ok(()) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some(io_error(error)),
        }
    } else {
        None
    };
    match remove_error.or(close_error) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn cache_key(url: &Url) -> String {
    const_hex::encode(Sha256::digest(url.as_str().as_bytes()))
}

async fn lookup_file(
    path: PathBuf,
    max_entry_bytes: u64,
    cancellation: &CancellationToken,
) -> Result<Option<PathBuf>, MediaCacheError> {
    let metadata = match tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(MediaCacheError::Cancelled),
        result = tokio::fs::metadata(&path) => result,
    } {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > max_entry_bytes {
        if metadata.is_file() {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(MediaCacheError::Cancelled),
                result = tokio::fs::remove_file(&path) => result.map_err(io_error)?,
            }
        }
        return Ok(None);
    }
    Ok(Some(path))
}

async fn commit_file(
    temp_path: &Path,
    final_path: &Path,
    cancellation: &CancellationToken,
) -> Result<PathBuf, MediaCacheError> {
    let rename = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(MediaCacheError::Cancelled),
        result = tokio::fs::rename(temp_path, final_path) => result,
    };
    match rename {
        Ok(()) => return Ok(final_path.to_path_buf()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io_error(error)),
    }
    let metadata = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(MediaCacheError::Cancelled),
        result = tokio::fs::metadata(final_path) => result.map_err(io_error)?,
    };
    if !metadata.is_file() || metadata.len() == 0 {
        if metadata.is_file() {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(MediaCacheError::Cancelled),
                result = tokio::fs::remove_file(final_path) => result.map_err(io_error)?,
            }
        }
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(MediaCacheError::Cancelled),
            result = tokio::fs::rename(temp_path, final_path) => result.map_err(io_error)?,
        }
    } else {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(MediaCacheError::Cancelled),
            result = tokio::fs::remove_file(temp_path) => result.map_err(io_error)?,
        }
    }
    Ok(final_path.to_path_buf())
}

struct CacheEntry {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

async fn enforce_limits(
    root: &Path,
    keep: &Path,
    file_limit: usize,
    byte_limit: u64,
    cancellation: &CancellationToken,
) -> Result<(), MediaCacheError> {
    let mut entries = Vec::with_capacity(file_limit);
    let mut total_bytes = 0u64;
    let mut directory = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(MediaCacheError::Cancelled),
        result = tokio::fs::read_dir(root) => result.map_err(io_error)?,
    };
    loop {
        let entry = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(MediaCacheError::Cancelled),
            result = directory.next_entry() => result.map_err(io_error)?,
        };
        let Some(entry) = entry else {
            break;
        };
        let path = entry.path();
        if !is_cache_file(&path) {
            continue;
        }
        let metadata = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(MediaCacheError::Cancelled),
            result = entry.metadata() => result.map_err(io_error)?,
        };
        if !metadata.is_file() {
            continue;
        }
        total_bytes = total_bytes.checked_add(metadata.len()).ok_or_else(|| {
            MediaCacheError::Io(SmolStr::new_static("X media cache size overflowed"))
        })?;
        entries.push(CacheEntry {
            path,
            bytes: metadata.len(),
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    entries.sort_by(|left, right| {
        let left_keep = left.path == keep;
        let right_keep = right.path == keep;
        left_keep
            .cmp(&right_keep)
            .then_with(|| left.modified.cmp(&right.modified))
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut count = entries.len();
    for entry in entries {
        if count <= file_limit && total_bytes <= byte_limit {
            break;
        }
        if entry.path == keep {
            continue;
        }
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(MediaCacheError::Cancelled),
            result = tokio::fs::remove_file(&entry.path) => result.map_err(io_error)?,
        }
        count -= 1;
        total_bytes -= entry.bytes;
    }
    Ok(())
}

fn is_cache_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn io_error(error: std::io::Error) -> MediaCacheError {
    MediaCacheError::Io(format_smolstr!("{error}"))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    use super::*;
    use crate::test_support::test_temp;

    #[derive(Clone, Copy)]
    struct LoopbackTransport;

    impl MediaTransport for LoopbackTransport {
        async fn client(
            &self,
            _destination: &Url,
            _cancellation: &CancellationToken,
        ) -> Result<Client, MediaError> {
            Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(network_error)
        }
    }

    struct TestServer {
        address: SocketAddr,
        task: JoinHandle<Vec<u8>>,
    }

    impl TestServer {
        async fn spawn(head: &'static str, body: &'static [u8]) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut chunk = [0; 256];
                    let count = stream.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&chunk[..count]);
                }
                stream.write_all(head.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
                stream.flush().await.unwrap();
                request
            });
            Self { address, task }
        }

        fn url(&self, path: &str) -> Url {
            Url::parse(&format!("http://{}{path}", self.address)).unwrap()
        }

        async fn request(self) -> Vec<u8> {
            self.task.await.unwrap()
        }
    }

    fn cache(root: &Path, file_limit: usize, byte_limit: u64) -> MediaCache {
        MediaCache::with_options(Some(root.to_path_buf()), file_limit, byte_limit)
    }

    fn downloader(directory: &tempfile::TempDir) -> MediaDownloader<LoopbackTransport> {
        MediaDownloader::with_cache(
            LoopbackTransport,
            cache(directory.path(), 10, 1_024 * 1_024),
        )
    }

    fn deadline() -> Instant {
        Instant::now() + MEDIA_OPERATION_TIMEOUT
    }

    impl MediaCache {
        async fn temporary_file_count(&self) -> usize {
            let Some(root) = self.inner.root.as_deref() else {
                return 0;
            };
            let Ok(mut entries) = tokio::fs::read_dir(root).await else {
                return 0;
            };
            let mut count = 0;
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "tmp")
                {
                    count += 1;
                }
            }
            count
        }
    }

    #[test]
    fn media_urls_should_only_allow_the_x_image_origins() {
        for allowed in [
            "https://pbs.twimg.com/media/image.jpg",
            "https://video.twimg.com/tweet_video/image.gif",
            "https://pbs.twimg.com:443/media/image.jpg?format=png",
        ] {
            assert!(valid_x_media_url(&Url::parse(allowed).unwrap()));
            assert!(plausible_media_url(allowed));
        }
        for rejected in [
            "http://pbs.twimg.com/media/image.jpg",
            "https://pbs.twimg.com:444/media/image.jpg",
            "https://user@pbs.twimg.com/media/image.jpg",
            "https://127.0.0.1/media/image.jpg",
            "https://localhost/media/image.jpg",
            "https://example.com/media/image.jpg",
        ] {
            assert!(!valid_x_media_url(&Url::parse(rejected).unwrap()));
        }
        for untracked in [
            "http://pbs.twimg.com/media/image.jpg",
            "https://example.com/media/image.jpg",
            "not a url",
        ] {
            assert!(!plausible_media_url(untracked));
        }
        assert!(!plausible_media_url(&format!(
            "https://pbs.twimg.com/{}",
            "a".repeat(MEDIA_URL_MAX_BYTES)
        )));
    }

    #[tokio::test]
    async fn a_url_the_policy_refuses_should_never_be_requested() {
        let directory = test_temp("media");
        let downloader = downloader(&directory);

        for refused in [
            "https://example.com/media/image.jpg",
            "http://pbs.twimg.com/media/image.jpg",
            "not a url",
        ] {
            assert_eq!(
                downloader
                    .fetch(refused, deadline(), &CancellationToken::new())
                    .await,
                Err(MediaError::InvalidUrl)
            );
        }
    }

    #[tokio::test]
    async fn a_cached_picture_should_be_answered_without_a_request() {
        let directory = test_temp("media");
        let cache = cache(directory.path(), 10, 1_024);
        let downloader = MediaDownloader::with_cache(
            LoopbackTransport,
            MediaCache::with_options(Some(directory.path().to_path_buf()), 10, 1_024 * 1_024),
        );
        let cancellation = CancellationToken::new();
        let url = Url::parse("https://pbs.twimg.com/media/cached.jpg").unwrap();
        let mut pending = cache.begin(&url, &cancellation).await.unwrap();
        pending.write_all(b"cached", &cancellation).await.unwrap();
        pending.commit(&cancellation).await.unwrap();

        let path = downloader
            .fetch(url.as_str(), deadline(), &cancellation)
            .await
            .unwrap();

        assert_eq!(tokio::fs::read(path).await.unwrap(), b"cached");
    }

    #[tokio::test]
    async fn a_download_should_store_the_whole_body_under_the_url() {
        let server = TestServer::spawn(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n",
            b"image",
        )
        .await;
        let directory = test_temp("media");
        let downloader = downloader(&directory);
        let cancellation = CancellationToken::new();
        let url = server.url("/media");

        let response = downloader
            .request(&url, deadline(), &cancellation)
            .await
            .unwrap();
        let path = downloader
            .store(response, deadline(), &url, &cancellation)
            .await
            .unwrap();

        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"image");
        assert_eq!(downloader.cache.temporary_file_count().await, 0);
        assert!(
            downloader
                .cache
                .lookup(&url, MEDIA_MAX_BYTES, &cancellation)
                .await
                .unwrap()
                .is_some()
        );
        assert!(!server.request().await.is_empty());
    }

    #[tokio::test]
    async fn a_short_body_should_leave_the_cache_as_it_was() {
        let server = TestServer::spawn(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n",
            b"im",
        )
        .await;
        let directory = test_temp("media");
        let downloader = downloader(&directory);
        let cancellation = CancellationToken::new();
        let url = server.url("/media");

        let response = downloader
            .request(&url, deadline(), &cancellation)
            .await
            .unwrap();
        assert!(
            downloader
                .store(response, deadline(), &url, &cancellation)
                .await
                .is_err()
        );

        assert_eq!(downloader.cache.temporary_file_count().await, 0);
        assert!(
            downloader
                .cache
                .lookup(&url, MEDIA_MAX_BYTES, &cancellation)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_declared_length_over_the_bound_should_be_refused_before_the_body() {
        let server = TestServer::spawn(
            "HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\n",
            b"",
        )
        .await;
        let directory = test_temp("media");
        let downloader = downloader(&directory);
        let cancellation = CancellationToken::new();
        let url = server.url("/media");

        let response = downloader
            .request(&url, deadline(), &cancellation)
            .await
            .unwrap();
        assert_eq!(
            downloader
                .store(response, deadline(), &url, &cancellation)
                .await,
            Err(MediaError::TooLarge)
        );
        assert_eq!(downloader.cache.temporary_file_count().await, 0);
    }

    #[tokio::test]
    async fn an_error_status_should_be_reported_rather_than_stored() {
        let server = TestServer::spawn(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            b"",
        )
        .await;
        let directory = test_temp("media");
        let downloader = downloader(&directory);
        let cancellation = CancellationToken::new();
        let url = server.url("/media");

        let response = downloader
            .request(&url, deadline(), &cancellation)
            .await
            .unwrap();
        assert_eq!(
            downloader
                .store(response, deadline(), &url, &cancellation)
                .await,
            Err(MediaError::Server(StatusCode::NOT_FOUND))
        );
    }

    #[tokio::test]
    async fn a_redirect_to_another_origin_should_not_be_followed() {
        let private = TestServer::spawn(
            "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nsecret",
            b"",
        )
        .await;
        let address = private.address;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = listener.local_addr().unwrap();
        let redirect = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut chunk = [0; 256];
                let count = stream.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&chunk[..count]);
            }
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{address}/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let directory = test_temp("media");
        let downloader = downloader(&directory);
        let cancellation = CancellationToken::new();
        let url = Url::parse(&format!("http://{origin}/media")).unwrap();

        let refused = downloader.request(&url, deadline(), &cancellation).await;
        assert_eq!(
            refused.expect_err("a redirect off the image hosts is not followed"),
            MediaError::Redirect
        );

        redirect.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), private.task)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unfinished_downloads_should_miss_until_the_commit() {
        let directory = test_temp("media");
        let cache = cache(directory.path(), 10, 1_024);
        let cancellation = CancellationToken::new();
        let url = Url::parse("https://pbs.twimg.com/profile_images/avatar.jpg").unwrap();
        let mut pending = cache.begin(&url, &cancellation).await.unwrap();
        pending.write_all(b"image", &cancellation).await.unwrap();

        assert_eq!(cache.lookup(&url, 32, &cancellation).await.unwrap(), None);

        let path = pending.commit(&cancellation).await.unwrap();
        assert_eq!(
            cache.lookup(&url, 32, &cancellation).await.unwrap(),
            Some(path)
        );
    }

    #[tokio::test]
    async fn the_cache_should_evict_oldest_files_by_count_and_total_size() {
        let directory = test_temp("media");
        let cache = cache(directory.path(), 2, 8);
        let cancellation = CancellationToken::new();
        let first = Url::parse("https://pbs.twimg.com/media/first.jpg").unwrap();
        let second = Url::parse("https://pbs.twimg.com/media/second.jpg").unwrap();
        let third = Url::parse("https://pbs.twimg.com/media/third.jpg").unwrap();

        for url in [&first, &second, &third] {
            let mut pending = cache.begin(url, &cancellation).await.unwrap();
            pending.write_all(b"data", &cancellation).await.unwrap();
            pending.commit(&cancellation).await.unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        assert_eq!(cache.lookup(&first, 32, &cancellation).await.unwrap(), None);
        assert!(
            cache
                .lookup(&second, 32, &cancellation)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            cache
                .lookup(&third, 32, &cancellation)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn empty_and_oversized_files_should_be_cache_misses() {
        let directory = test_temp("media");
        let cache = cache(directory.path(), 10, 1_024);
        let cancellation = CancellationToken::new();
        let empty = Url::parse("https://pbs.twimg.com/media/empty.jpg").unwrap();
        let large = Url::parse("https://pbs.twimg.com/media/large.jpg").unwrap();
        tokio::fs::create_dir_all(directory.path()).await.unwrap();
        tokio::fs::write(directory.path().join(cache_key(&empty)), [])
            .await
            .unwrap();
        tokio::fs::write(directory.path().join(cache_key(&large)), b"12345")
            .await
            .unwrap();

        assert_eq!(cache.lookup(&empty, 4, &cancellation).await.unwrap(), None);
        assert_eq!(cache.lookup(&large, 4, &cancellation).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_failed_commit_should_remove_the_temporary_file() {
        let directory = test_temp("media");
        let cache = cache(directory.path(), 10, 1_024);
        let cancellation = CancellationToken::new();
        let url = Url::parse("https://pbs.twimg.com/media/commit-error.jpg").unwrap();
        tokio::fs::create_dir(directory.path().join(cache_key(&url)))
            .await
            .unwrap();
        let mut pending = cache.begin(&url, &cancellation).await.unwrap();
        pending.write_all(b"image", &cancellation).await.unwrap();
        assert!(pending.commit(&cancellation).await.is_err());

        assert_eq!(cache.temporary_file_count().await, 0);
    }

    #[tokio::test]
    async fn an_explicit_abort_should_close_and_remove_the_temporary_file() {
        let directory = test_temp("media");
        let cache = cache(directory.path(), 10, 1_024);
        let cancellation = CancellationToken::new();
        let url = Url::parse("https://pbs.twimg.com/media/abort.jpg").unwrap();
        let mut pending = cache.begin(&url, &cancellation).await.unwrap();
        pending.write_all(b"partial", &cancellation).await.unwrap();

        pending.abort().await.unwrap();

        assert_eq!(cache.temporary_file_count().await, 0);
    }

    #[tokio::test]
    async fn a_cancelled_commit_should_await_cleanup_and_remove_the_temporary_file() {
        let directory = test_temp("media");
        let cache = cache(directory.path(), 10, 1_024);
        let cancellation = CancellationToken::new();
        let url = Url::parse("https://pbs.twimg.com/media/cancelled-commit.jpg").unwrap();
        let mut pending = cache.begin(&url, &cancellation).await.unwrap();
        pending.write_all(b"partial", &cancellation).await.unwrap();
        cancellation.cancel();

        assert_eq!(
            pending.commit(&cancellation).await,
            Err(MediaCacheError::Cancelled)
        );
        assert_eq!(cache.temporary_file_count().await, 0);
    }

    #[tokio::test]
    async fn a_cache_without_a_directory_should_refuse_rather_than_panic() {
        let cache = MediaCache::with_options(None, 10, 1_024);
        let cancellation = CancellationToken::new();
        let url = Url::parse("https://pbs.twimg.com/media/image.jpg").unwrap();

        assert_eq!(
            cache.lookup(&url, 32, &cancellation).await,
            Err(MediaCacheError::DirectoryUnavailable)
        );
    }
}

use std::{fmt, sync::Arc};

use download_provider::{
    request::DownloadRequest,
    task::{DownloadEvent, DownloadId, DownloadTask},
};

pub trait DownloadService {
    fn add(&self, request: DownloadRequest) -> DownloadId;

    fn pause(&self, id: DownloadId) -> bool;

    fn resume(&self, id: DownloadId) -> bool;

    fn delete(&self, id: DownloadId) -> bool;

    fn task(&self, id: DownloadId) -> Option<DownloadTask>;

    fn subscribe(&self, sink: Arc<dyn Fn(DownloadEvent) + Send + Sync>) -> DownloadSubscription;
}

pub struct DownloadSubscription {
    unsubscribe: Option<Box<dyn FnOnce()>>,
}

impl DownloadSubscription {
    pub fn new(unsubscribe: impl FnOnce() + 'static) -> Self {
        Self {
            unsubscribe: Some(Box::new(unsubscribe)),
        }
    }

    #[must_use]
    pub fn none() -> Self {
        Self { unsubscribe: None }
    }
}

impl Drop for DownloadSubscription {
    fn drop(&mut self) {
        if let Some(unsubscribe) = self.unsubscribe.take() {
            unsubscribe();
        }
    }
}

impl fmt::Debug for DownloadSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DownloadSubscription")
            .field("active", &self.unsubscribe.is_some())
            .finish()
    }
}

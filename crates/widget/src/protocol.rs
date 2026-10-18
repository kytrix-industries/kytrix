use std::{error::Error, fmt, hash::Hash, sync::Arc};

/// Defines the business messages exchanged by one widget and its background service.
pub trait WidgetProtocol: Send + Sync + 'static {
    type Request: Send + 'static;
    type Response: Send + 'static;
    type StreamKey: Clone + Eq + Hash + Send + Sync + 'static;
    type StreamMessage: Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;
}

/// Monotonically increasing identifier assigned to an RPC call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(u64);

impl RequestId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Value-semantic generation assigned whenever a keyed stream is replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamGeneration(u64);

impl StreamGeneration {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// One background stream message with enough metadata to reject stale updates.
#[derive(Debug)]
pub struct StreamEnvelope<K, M> {
    key: K,
    generation: StreamGeneration,
    sequence: u64,
    message: Arc<M>,
}

impl<K, M> StreamEnvelope<K, M> {
    pub fn new(key: K, generation: StreamGeneration, sequence: u64, message: Arc<M>) -> Self {
        Self {
            key,
            generation,
            sequence,
            message,
        }
    }

    pub fn key(&self) -> &K {
        &self.key
    }

    pub const fn generation(&self) -> StreamGeneration {
        self.generation
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn message(&self) -> &Arc<M> {
        &self.message
    }

    pub fn into_message(self) -> Arc<M> {
        self.message
    }
}

impl<K: Clone, M> Clone for StreamEnvelope<K, M> {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            generation: self.generation,
            sequence: self.sequence,
            message: self.message.clone(),
        }
    }
}

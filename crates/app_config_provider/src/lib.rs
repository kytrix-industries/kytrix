//! Durable settings persistence, the GPUI global that owns the snapshot, and
//! the per-domain events its changes are emitted from.

pub mod events;
pub mod manager;
pub mod notification;
pub mod store;

#[cfg(test)]
pub(crate) mod test_support;

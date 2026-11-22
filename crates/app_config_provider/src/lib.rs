//! Durable settings persistence, the plain-value manager that owns the snapshot,
//! and the per-domain change channel its writes publish on.

pub mod events;
pub mod manager;
pub mod notification;
pub mod store;

#[cfg(test)]
pub(crate) mod test_support;

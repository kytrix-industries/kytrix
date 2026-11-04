//! The Polymarket adapter and its typed wire shapes.

pub(crate) mod adapter;
mod heartbeat;
pub(crate) mod types;

pub(crate) use adapter::PolymarketAdapter;

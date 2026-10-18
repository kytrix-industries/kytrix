//! Hyperliquid adapter implementation and typed wire codec.
//!
//! Besides the adapter itself this module publishes the entry points the
//! composition root needs for the scopes this venue deploys rather than declares:
//! `adapter::implementation::HyperliquidAdapter::read_venue` for the one reading of
//! the venue's own tables a load makes — what it deploys and the metadata of every
//! scope it publishes, in one conversation — `catalog::perp_dex_descriptor` for the
//! descriptor of a builder-deployed perpetual DEX,
//! `catalog::deployed_perp_dexes` and `implementation::is_verified_perp_dex` for
//! deciding whether one may be registered, and `implementation::post_hyperliquid_info`
//! for the `info` request that answers them. Registering is the composition root's
//! decision: this crate only says which scopes it can serve and why.
//!
//! The request path reads the metadata that reading published, never the venue:
//! a ticker, a catalog or a wallet read is resolved against the execution
//! context's venue metadata, and no method here holds a table of its own between
//! requests.

pub(crate) mod adapter;
mod budget;
pub(crate) mod catalog;
mod codec;
mod heartbeat;
mod signing;
mod types;

pub(crate) use codec::HyperliquidCodec;
pub(crate) use types::*;

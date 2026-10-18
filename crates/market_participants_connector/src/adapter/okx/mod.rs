//! OKX adapter implementation and v5 wire codec.
//!
//! A regression defence stands here for the metadata model this build commits:
//! OKX resolves every request from its own catalog and the published directory, so
//! **no request path of this adapter carries a metadata fetch**. There is no venue
//! table, no token table and no per-route metadata read behind it — what a request
//! encodes comes from the resolved listing, the credential and the execution
//! context's endpoint, and the adapter holds no cache of its own. Reading the
//! venue at request time would be a second source of route knowledge beside
//! `LoadedVenues` and a metadata conversation on a request path; a
//! change that adds one is a defect, not an optimization.

mod account;
mod adapter;
mod auth;
mod codec;
mod heartbeat;
mod market_data;
mod types;
pub(crate) use adapter::OkxAdapter;
pub(crate) use auth::OkxSigner;
pub(crate) use codec::{CODEC, OkxCodec};
pub(crate) use types::*;

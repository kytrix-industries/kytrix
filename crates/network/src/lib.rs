//! Generic network infrastructure for Kytrix.
//!
//! This crate owns HTTP and WebSocket transport, outbound proxy route
//! resolution, and the operating-system proxy discovery those routes depend
//! on. It deliberately has no market-participant adapter, credential-signing, or
//! account-domain dependencies.

pub mod error;
pub mod proxy;
mod system_proxy;
#[cfg(test)]
mod test_support;
pub mod transport;

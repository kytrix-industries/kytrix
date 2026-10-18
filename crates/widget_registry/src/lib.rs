//! The statically linked widget catalogue and its registry.
//!
//! Every widget is compiled into the binary, so the complete catalogue is known
//! at build time and the application never registers widgets dynamically.

pub mod catalogue;
pub mod registry;

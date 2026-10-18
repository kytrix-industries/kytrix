//! The symbols the market-data cases read.
//!
//! The suite reads one liquid US listing by default and lets a re-run name another, which
//! is what a check against a symbol with a corporate action in the window needs. The
//! choice is one value for the whole run, so every case of one run measures the same
//! security.

use std::env;

/// The environment variable that overrides the symbol this suite reads.
pub const SYMBOL_ENV: &str = "KYTRIX_LONGBRIDGE_TEST_SYMBOL";
/// The listing the market-data cases read when nothing overrides it.
pub const DEFAULT_SYMBOL: &str = "AAPL.US";

/// The symbol this run reads: the environment's own, or the default.
pub fn symbol() -> String {
    env::var(SYMBOL_ENV).unwrap_or_else(|_| DEFAULT_SYMBOL.to_owned())
}

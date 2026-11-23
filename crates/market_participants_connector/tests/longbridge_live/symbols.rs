use std::env;

pub const SYMBOL_ENV: &str = "KYTRIX_LONGBRIDGE_TEST_SYMBOL";
pub const DEFAULT_SYMBOL: &str = "AAPL.US";

pub fn symbol() -> String {
    env::var(SYMBOL_ENV).unwrap_or_else(|_| DEFAULT_SYMBOL.to_owned())
}

#[cfg(test)]
mod tests;
mod wire;

pub(crate) use wire::{fetch_ticker, subscribe_ticker};

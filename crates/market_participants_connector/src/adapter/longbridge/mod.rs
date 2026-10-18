//! The Longbridge adapter: the venue's authenticated quote session, and the
//! market data read through it.
//!
//! The venue speaks one custom-framed WebSocket for everything it publishes, so
//! the modules here are the layers of that one conversation. [`codec`] is the
//! framing and the venue's own enumerations, [`signature`] is the HTTP signature the
//! token endpoint demands, [`socket`] the transport — the endpoint a region resolves
//! to, the two upgrade headers the venue routes by, the connection key built from the
//! same protocol that dials — and [`auth`] the session: the OTP handshake, the
//! market-data profile the venue answers with, and the two bounds that profile gives
//! every later command. [`symbols`] and [`catalog`] are the venue's listings, which
//! are read from the market participant's own published directory rather than from the venue,
//! because the directory is public and the venue's own listing table is not.
//! [`adapter`] is the one `MarketParticipantAdapter` the gateway registers, and the two
//! market-data paths it dispatches to are [`candles`] — the history paging, the opening
//! screen a subscription starts from, and the push aggregation that builds every candle
//! after it — and [`tickers`], which reads the same socket's quotes as whole snapshots.
//! [`wire`] is the one reading of an inbound frame both of those paths share.

mod adapter;
mod auth;
#[cfg(test)]
mod auth_tests;
mod candles;
mod catalog;
mod codec;
mod quota;
mod signature;
mod socket;
#[cfg(test)]
mod socket_tests;
mod symbols;
mod tickers;
mod wire;

#[cfg(test)]
mod test_support;

pub(crate) use adapter::LongbridgeAdapter;
/// The names the venue's directory files are published under.
///
/// A fixture that had to spell the names itself would be answering nothing at all when
/// one of them drifted, and a download over it would wait for an answer instead of
/// failing on the name. Nothing outside this module reads them today: the venue's own
/// tests and the symbol-directory fixture both name the files through the adapter that
/// fetches them, which is the one place the two spellings are held together.
#[cfg(test)]
pub(crate) use symbols::{NASDAQ_LISTED_FILE, OTHER_LISTED_FILE};
pub(crate) use symbols::{SymbolRow, fetch_symbol_rows};

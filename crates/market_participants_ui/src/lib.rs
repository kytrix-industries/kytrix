//! The market participants entry in the window header: the one place a venue is loaded.
//!
//! [`menu::MarketParticipants`] lists every venue this build declares and offers each of them
//! the action that loads it — [`market_participants_connector::connector::MarketParticipantsConnector::load_venue`],
//! whose only production caller this is. What a venue *is* (the venue list, its scopes,
//! catalog) belongs to the market participants root; what this crate owns is the picture of it: which
//! rows exist, what each of them is doing, and what the user is told when one fails.

pub mod menu;

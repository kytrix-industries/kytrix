//! The configuration model the rest of the application is written against.
//!
//! [`settings::Config`] is the persisted application snapshot. This crate owns
//! the [`network`] proxy and outbound-route model, the [`market_participants`] credential
//! settings, the [`widget`] config envelope, and the [`workspace`] pane layouts,
//! so every consumer reads one configuration vocabulary.
//!
//! Loading, atomic persistence, the settings manager and its per-domain events live
//! in `app_config_provider`, which also installs the manager in GPUI global state.
//! Runtime credential resolution, market-participants routing, adapters, and
//! connection pools remain outside this crate in `market_participants_connector`;
//! `app_config` never creates or owns those runtime resources.

pub mod change;
pub mod market_participants;
pub mod network;
pub mod settings;
pub mod time_zone;
pub mod widget;
pub mod workspace;

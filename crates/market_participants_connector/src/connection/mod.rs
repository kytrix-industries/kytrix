//! Shared WebSocket connection state and pooling.

mod heartbeat;
#[cfg(test)]
mod integration_tests;
pub(crate) mod key;
pub(crate) mod physical;
mod pool;
mod reconnect;
mod state;

pub(crate) use heartbeat::Keepalive;
pub(crate) use key::ConnectionKey;
#[cfg(test)]
pub(crate) use physical::PendingRequest;
#[cfg(test)]
pub(crate) use physical::no_response_id;
pub(crate) use physical::{
    Connection, ConnectionSubscription, Reauthentication, ReauthenticationRequest,
    SubscriptionTopic,
};
pub(crate) use pool::ConnectionPool;
pub(crate) use reconnect::{ExponentialBackoff, ReconnectPolicy};
pub(crate) use state::ConnectionState;

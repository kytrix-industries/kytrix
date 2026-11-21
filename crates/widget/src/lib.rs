//! Statically registered widgets backed by typed RPC and supervised streams.
//!
//! The contract layer the twenty widget crates implement: [`view::WidgetView`] is
//! the widget itself, [`any::AnyWidget`] is its type-erased handle, [`cx::WidgetCx`]
//! is the one context it is built and driven through, and [`frame::FrameRouter`] is
//! the single owner of the engine's frame hook. [`status`] is the state machine,
//! [`surface`] the presentation rule, [`binding`] the executor bridge, and
//! [`services`] the injected collaborators.
//!
//! Nothing in this crate depends on a windowing toolkit: a widget is built from engine nodes
//! and `base_ui::kit` components, so the same contract is what the widget crates
//! migrate to.

pub mod account;
pub mod any;
pub mod binding;
pub mod category;
pub mod config;
pub mod cx;
pub mod descriptor;
pub mod error;
pub mod frame;
pub mod identity;
pub mod instrument_link;
pub mod protocol;
pub mod rpc;
pub mod runtime;
pub mod services;
pub mod status;
pub mod stream;
pub mod surface;
pub mod time_zone;
pub mod ui;
pub mod util;
pub mod venue;
pub mod view;

/// The framework's test host and assertions. Available to this crate's tests and,
/// through the `test-support` feature, to the widget crates that implement the
/// contract.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

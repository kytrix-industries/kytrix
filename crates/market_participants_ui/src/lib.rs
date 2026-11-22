//! The market participants entry in the title bar: the one place a venue is loaded.
//!
//! [`menu::MarketParticipants`] lists every venue this build declares and offers each of them
//! the action that loads it — [`market_participants_connector::connector::MarketParticipantsConnector::load_venue`],
//! whose only production caller this is. What a venue *is* (the venue list, its scopes,
//! catalog) belongs to the market participants root; what this crate owns is the picture of it: which
//! rows exist, what each of them is doing, and what the user is told when one fails.
//!
//! # Two collaborators, injected
//!
//! The menu is an application-layer control, not a widget, so nothing hands it a
//! `WidgetCx` and it reads no process global. [`host::MarketParticipantsHost`] carries the
//! two things it renders with:
//!
//! - the frozen [`widget::services::WidgetServices`] — translation, the language signal,
//!   the theme, the three composition-root registries (layer stack, frame router,
//!   animation router) and the application runtime;
//! - the [`host::VenueDirectory`] it lists and loads through. Production is the
//!   [`market_participants_connector::connector::MarketParticipantsConnector`] the
//!   composition root installed; a headless test hands in its own directory, because the
//!   connector's constructor still needs the old toolkit's runtime type (P5 removes that).
//!
//! # Frames and async
//!
//! The menu is synchronised by the caller's frame hook ([`menu::MarketParticipants::sync`]):
//! it drains what the loads it started reported, follows the language, keeps the panel, the
//! trigger and any alert the loads raised in step, and rebuilds the panel's rows only when
//! what they are drawn from changed. A load runs on the engine's own runtime
//! ([`ui_core::app::AppHandle::spawn`]); its result comes back through a channel and a frame
//! request ([`ui_core::app::AppHandle::send`]), so no `tokio::spawn` runs from the frame
//! thread and nothing polls a task from inside a frame.
//!
//! The panel is `base_ui::kit`'s anchored overlay — one overlay in the popup band, dismissed
//! by an outside click or `Esc` — and the alert a failed load raises is the kit dialog's alert
//! shape. Both are released where they are unmounted, never left to the process.

pub mod error;
pub mod host;
pub mod menu;

#[cfg(test)]
pub(crate) mod test_support;

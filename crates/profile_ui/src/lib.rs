//! The account control: who is signed in, and how that changes.
//!
//! [`profile::ProfileMenu`] is the title bar's account control: the account, the flows that
//! change it, and the face that opens the account menu below it — or, while nobody is signed
//! in, the button whose press starts the sign-in.
//!
//! This is an **application-layer control**, not a widget: it is mounted by the shell's title
//! bar rather than by a pane, so it implements the engine's five stages itself
//! (`new` → `build` → `sync` → `release`) instead of the `WidgetView` contract. It therefore
//! owns no process global either: everything it needs the composition root injects through
//! [`host::ProfileHost`] — the frozen [`widget::services::WidgetServices`] (translation, theme,
//! language, settings snapshot, the three component registries and the tokio runtime), the
//! account service seam, and the account settings seam. See the [`host`] module for why each is
//! a field and how the root assembles one.
//!
//! # What it owns
//!
//! One account, and every flow that can change it: starting a sign-in, waiting for the
//! browser, storing what comes back, asking the account service what tier that account is on,
//! opening the account dashboard, signing out, and noticing when the credential behind the
//! account stops being valid. The avatar is generated here too, because it is a property of the
//! account and has to be dropped with it.
//!
//! # What it does not own
//!
//! The account itself. That lives in the application settings, which arrive as the
//! [`host::AccountSettings`] seam: this crate **reads** them and **writes** them, but the store
//! is the composition root's (P4-3). Every screen that cares who is signed in follows the same
//! record, so signing in and signing out cannot leave the title bar and the panes disagreeing.
//!
//! # What it does not do
//!
//! It never retries. A failed sign-in reports itself and stops; the user asks again. A tier
//! that could not be read leaves the menu saying it is unknown rather than asking a second
//! time, because the tiers of an account do not change while it is signed in.
//!
//! # Module layout
//!
//! - [`host`] — the injected collaborators and the two seams (account service, account
//!   settings), plus the `AccountService` implementation the production root wires.
//! - [`account`] — the signed-in account as the menu draws it (pure derivation).
//! - [`profile`] — the control itself: the five stages, the flows they drive, the signals the
//!   triggers read.
//! - `menu` / `dialogs` — the content of the account menu and of the alerts a flow reports in.
//! - `avatar` / `oauth` / `session` — generated faces, the browser half of a sign-in, and
//!   whether the stored credential is still good.

pub mod account;
pub mod host;
pub mod profile;

mod avatar;
mod dialogs;
mod menu;
mod oauth;
mod session;

#[cfg(test)]
pub(crate) mod test_support;

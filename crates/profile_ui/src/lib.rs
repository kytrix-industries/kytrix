//! The account entry: who is signed in, and how that changes.
//!
//! [`profile::ProfileMenu`] is the stateful host: the account, the flows that change it, and
//! the content of the menu behind the entry. The entry that opens the menu belongs to the
//! window that draws the account — the sidebar's foot — and opens
//! [`profile::ProfileMenu::show_context_menu`] as a window-level menu.
//!
//! The sign-in itself is the account service's OAuth authorization, driven from
//! `oauth`: the browser authorizes, a loopback listener in this process receives the
//! code, and the account that comes back is what the settings store.

pub mod account;
pub mod profile;

mod avatar;
mod dialogs;
mod menu;
mod oauth;
mod session;

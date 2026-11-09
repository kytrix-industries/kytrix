//! The account control: who is signed in, and how that changes.
//!
//! [`profile::ProfileMenu`] is the title bar's account control: the account, the flows that
//! change it, and the face that opens the account menu below it — or, while nobody is signed
//! in, the button whose press starts the sign-in.
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

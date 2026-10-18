//! The account entry: who is signed in, and how that changes.
//!
//! [`profile::ProfileMenu`] is the stateful host a window mounts;
//! [`avatar_dropdown::AvatarDropDown`] is the stateless control it draws; and
//! [`account::SignedInAccount`] is the only piece of account state that control needs.
//!
//! The sign-in itself is the account service's OAuth authorization, driven from
//! `oauth`: the browser authorizes, a loopback listener in this process receives the
//! code, and the account that comes back is what the settings store.

pub mod account;
pub mod avatar_dropdown;
pub mod profile;

mod avatar;
mod dialogs;
mod oauth;
mod session;

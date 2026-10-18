//! Chromium `proxy_config_service_linux` discovery plus PAC/WPAD evaluation.

mod command;
mod endpoint;
mod gnome;
mod kde;
mod settings;
mod wpad;

#[cfg(test)]
mod linux_tests;

pub(crate) use settings::{fingerprint, read, snapshot};

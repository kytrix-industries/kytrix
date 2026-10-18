//! Operating-system proxy discovery and PAC evaluation.

#[cfg(any(windows, target_os = "linux"))]
pub(super) mod bypass;
mod core;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod pac;
pub(super) mod parse;
#[cfg(windows)]
mod windows;

#[cfg(all(test, any(windows, target_os = "linux")))]
mod bypass_tests;
#[cfg(test)]
mod core_tests;
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod pac_tests;
#[cfg(test)]
mod parse_tests;
#[cfg(all(test, windows))]
mod windows_tests;

pub(crate) use core::{cached_os_proxy_fingerprint, read_os_proxy, refresh_os_proxy_fingerprint};

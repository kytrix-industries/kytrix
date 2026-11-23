pub(crate) mod dispatcher;
pub(crate) mod ime;

#[cfg(any(target_os = "macos", test))]
pub(crate) mod menu;

#[cfg(any(target_os = "macos", test))]
pub(crate) mod macos_placement;

#[cfg(any(target_os = "macos", test))]
pub(crate) mod macos_close;

#[cfg(any(target_os = "macos", test))]
pub(crate) mod macos_owner;

#[cfg(any(target_os = "macos", test))]
pub(crate) mod macos_drag;

#[cfg(any(target_os = "macos", test))]
pub(crate) mod macos_chrome;

#[cfg(any(target_os = "macos", test))]
pub(crate) mod macos_pump;

#[cfg(any(target_os = "linux", test))]
pub(crate) mod wayland;

#[cfg(windows)]
pub(crate) mod event;
#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(target_os = "linux")]
pub(crate) use linux as native;
#[cfg(target_os = "macos")]
pub(crate) use macos as native;
#[cfg(windows)]
pub(crate) use windows as native;

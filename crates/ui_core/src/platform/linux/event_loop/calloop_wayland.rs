//! 名称别名：`calloop-wayland-source` 的 `WaylandSource`。
//!
//! 单独成文件（而非 `mod calloop_wayland { ... }` 内联模块）：workspace 的 `inline_modules` lint
//! 禁止内联模块，且独立文件在导航与 `cargo doc` 里都有明确位置。

pub(crate) use calloop_wayland_source::WaylandSource;

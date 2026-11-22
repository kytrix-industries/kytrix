//! The application's window: the title bar, the sidebar, the page area and the frame route.
//!
//! The shell is a plain value, not an engine view: the engine has no entity system, so the
//! shell owns its regions (title bar, sidebar, pages) and drives them from its one frame
//! route. That route is the only place that mutates the shell — a control's click handler
//! reports an [`Intent`] into the queue the route drains, and a dialog's confirmation does
//! the same — which is what makes "apply the user's command, lay the window out, then draw
//! it" a property of the code.
//!
//! # Ownership and shutdown
//!
//! The route registered with the composition root's [`FrameRouter`] holds the shell; the
//! window's close path releases the shell's components and unregisters the route, so a
//! closed window leaves no panes, dialogs or frame routes behind. The route never
//! re-registers itself, and the shell never touches `App::on_frame` or
//! `App::on_animation_complete`: the router owns the first (kit's animation router owns the
//! second).
//!
//! [`FrameRouter`]: widget::frame::FrameRouter

pub(crate) mod dialogs;
mod intent;
pub(crate) mod metrics;
pub(crate) mod pages;
mod sidebar;
mod title_bar;
mod ui;
mod workspace;
mod workspaces;

pub(crate) mod root;

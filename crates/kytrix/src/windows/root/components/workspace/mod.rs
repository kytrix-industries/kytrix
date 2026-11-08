mod controls;
mod drag;
mod layout;
mod pages;
mod pane;
mod placeholder;
mod stage;
mod store;
mod tree;

#[cfg(test)]
pub(crate) use pages::PAGE_TRANSITION_DURATION;
pub(crate) use stage::Stage;
pub(crate) use store::{Workspaces, WorkspacesEvent};

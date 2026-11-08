//! The widget gallery: the page in which a pane's widget is chosen.
//!
//! The page is embedded by the workspace overlay rather than opened as a window dialog:
//! it reports a choice through [`gallery::WidgetGalleryEvent`] and leaves what happens
//! to it, and whether the page goes away, to the workspace.

pub mod gallery;
mod widget_icon;

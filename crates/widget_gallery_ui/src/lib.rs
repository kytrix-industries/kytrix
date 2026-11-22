//! The widget gallery: the page in which a pane's widget is chosen.
//!
//! The page is embedded by the workspace overlay rather than opened as a window dialog:
//! it reports a choice through [`gallery::WidgetGalleryEvent`] and leaves what happens to
//! it, and whether the page goes away, to the workspace.
//!
//! The page is built on the engine and on `base_ui::kit`: it owns a node subtree, receives
//! the application's one [`WidgetServices`](widget::services::WidgetServices) composition
//! root, lists the catalogue through [`catalogue::GalleryCatalogue`], and mounts the
//! selected widget as a live preview over that seam. Switching the selection releases the
//! previous preview, and releasing the page releases the preview and every kit component
//! it built — which is what keeps opening and closing the page, or moving through its
//! catalogue, from accumulating widget instances.
//!
//! Nothing here depends on a windowing toolkit: the page is engine nodes, kit components,
//! and the widget contract.

pub mod catalogue;
pub mod gallery;
pub mod widget_icon;

//! The gallery page a pane opens to choose its widget, drawn over the workspace.
//!
//! The page itself is [`widget_gallery_ui`]'s: it builds its own subtree, takes the
//! keyboard, previews the selected widget and reports a confirmation or a cancellation
//! once. What belongs to the workspace is the overlay around it (it covers the panes while
//! it is open), which pane it was opened for, and what a choice applies to.

use std::rc::Rc;

use base_ui::theme::role::Role;
use ui_core::{app::App, error::EngineError, node::id::NodeId, window::WindowId};
use widget::identity::WidgetId;
use widget_gallery_ui::gallery::{WidgetGallery, WidgetGalleryEvent};

use super::{super::ui, gallery_catalogue::RegistryCatalogue, tree::PaneId};
use crate::composition::Composition;

/// The overlay's debug id.
pub(crate) const WORKSPACE_PAGES: &str = "WORKSPACE_PAGES";

/// The gallery page as the workspace draws it.
pub(crate) struct GalleryPage {
    window: WindowId,
    pane: PaneId,
    gallery: WidgetGallery,
    root: NodeId,
    /// Whether the page has reported its decision and is on its way out.
    closing: bool,
}

impl GalleryPage {
    /// Opens the page over the workspace for `pane`.
    ///
    /// A second page over the first would have nothing to choose for, so the caller opens
    /// one only when none is up.
    pub(crate) fn open(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        composition: &Composition,
        pane: PaneId,
        current: Option<WidgetId>,
    ) -> Result<Self, EngineError> {
        let root = ui::named(
            app,
            window,
            parent,
            WORKSPACE_PAGES,
            base_ui::style_builder::StyleBuilder::new()
                .absolute()
                .top(0.0)
                .left(0.0)
                .right(0.0)
                .bottom(0.0)
                .flex_col()
                .overflow_hidden()
                .build(),
        )?;
        // The page covers the workspace rather than floating over visible panes: the
        // backdrop is painted below the page's own content.
        ui::themed_box(
            app,
            window,
            root,
            base_ui::style_builder::StyleBuilder::new()
                .absolute()
                .top(0.0)
                .left(0.0)
                .right(0.0)
                .bottom(0.0)
                .build(),
            Role::SurfaceCanvas,
            &composition.theme,
        )?;
        let catalogue: Rc<dyn widget_gallery_ui::catalogue::GalleryCatalogue> =
            Rc::new(RegistryCatalogue::new(Rc::clone(&composition.catalogue)));
        let gallery = WidgetGallery::mount(
            app,
            window,
            root,
            Rc::clone(composition.services()),
            catalogue,
            current,
        )?;

        Ok(Self {
            window,
            pane,
            gallery,
            root,
            closing: false,
        })
    }

    /// The pane this page chooses a widget for.
    pub(crate) const fn pane(&self) -> PaneId {
        self.pane
    }

    /// The page's one report, if it has made one.
    pub(crate) fn take_event(&self) -> Option<WidgetGalleryEvent> {
        self.gallery.take_event()
    }

    /// Whether the page is leaving (a second report changes nothing).
    pub(crate) const fn is_closing(&self) -> bool {
        self.closing
    }

    /// Marks the page as leaving: a report behind the one already taken is ignored.
    pub(crate) fn begin_closing(&mut self) {
        self.closing = true;
    }

    /// Releases the page, its preview and its nodes.
    pub(crate) fn release(self, app: &mut App) {
        self.gallery.release(app);
        let _ = app.remove_node(self.window, self.root);
    }
}

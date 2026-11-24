use std::rc::Rc;

use ui_core::{app::App, error::EngineError, math::px, node::id::NodeId, window::WindowId};
use ui_kit::theme::role::Role;
use widget::identity::WidgetId;
use widget_gallery_ui::gallery::{WidgetGallery, WidgetGalleryEvent};

use super::{super::ui, gallery_catalogue::RegistryCatalogue, tree::PaneId};
use crate::composition::Composition;

pub(crate) const WORKSPACE_PAGES: &str = "WORKSPACE_PAGES";

pub(crate) struct GalleryPage {
    window: WindowId,
    pane: PaneId,
    gallery: WidgetGallery,
    root: NodeId,
    closing: bool,
}

impl GalleryPage {
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
            ui_core::paint::style_builder::StyleBuilder::new()
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .right(px(0.0))
                .bottom(px(0.0))
                .flex_col()
                .overflow_hidden()
                .build(),
        )?;
        ui::themed_box(
            app,
            window,
            root,
            ui_core::paint::style_builder::StyleBuilder::new()
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .right(px(0.0))
                .bottom(px(0.0))
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

    pub(crate) const fn pane(&self) -> PaneId {
        self.pane
    }

    pub(crate) fn take_event(&self) -> Option<WidgetGalleryEvent> {
        self.gallery.take_event()
    }

    pub(crate) const fn is_closing(&self) -> bool {
        self.closing
    }

    pub(crate) fn begin_closing(&mut self) {
        self.closing = true;
    }

    pub(crate) fn release(self, app: &mut App) {
        self.gallery.release(app);
        let _ = app.remove_node(self.window, self.root);
    }
}

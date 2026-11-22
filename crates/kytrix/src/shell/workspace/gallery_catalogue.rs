//! The adapter between the widget gallery page and the widget registry.
//!
//! The page (`widget_gallery_ui`) and the catalogue (`widget_registry`) do not know each
//! other: the page lists what it is told and mounts what it is told to through
//! [`GalleryCatalogue`], and the composition root is what has both. The adapter is that
//! join: the registry's list becomes the page's list, and the registry's one mount path
//! becomes the page's mount path.
//!
//! A preview is not persisted: it is a widget shown inside the page, released when the page
//! moves to another row or leaves. Its config-change report therefore goes nowhere
//! ([`WidgetConfigChange::default`]) — the pane's own widget reports through the pane.

use std::rc::Rc;

use ui_core::{app::App, error::EngineError, node::id::NodeId, window::WindowId};
use widget::{
    any::AnyWidget, config::WidgetConfigChange, descriptor::WidgetDescriptor, identity::WidgetId,
    services::WidgetServices,
};
use widget_gallery_ui::catalogue::{GalleryCatalogue, GalleryError};
use widget_registry::registry::{WidgetMountError, WidgetRegistry};

/// The registry seen through the gallery's seam.
pub(crate) struct RegistryCatalogue {
    registry: Rc<WidgetRegistry>,
}

impl RegistryCatalogue {
    /// Wraps the application's one registry.
    pub(crate) const fn new(registry: Rc<WidgetRegistry>) -> Self {
        Self { registry }
    }
}

impl GalleryCatalogue for RegistryCatalogue {
    fn descriptors(&self) -> Vec<WidgetDescriptor> {
        self.registry
            .iter()
            .map(|registration| registration.descriptor().clone())
            .collect()
    }

    fn mount(
        &self,
        id: &WidgetId,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        services: &Rc<WidgetServices>,
    ) -> Result<AnyWidget, GalleryError> {
        self.registry
            .mount_default(
                id.as_str(),
                app,
                window,
                parent,
                services,
                WidgetConfigChange::default(),
            )
            .map_err(|error| match error {
                WidgetMountError::NotRegistered(id) => GalleryError::NotRegistered(id),
                WidgetMountError::Mount(error) => GalleryError::Engine(error),
            })
    }
}

/// The adapter's own engine error surface, kept out of the page's trait.
#[allow(dead_code)]
fn engine_error(error: EngineError) -> GalleryError {
    GalleryError::Engine(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The adapter answers the registry's own list: the page draws exactly what the build
    /// registers, including the default widget (the page filters what it may offer).
    #[test]
    fn the_adapter_should_list_every_registered_widget() {
        let registry =
            Rc::new(widget_registry::catalogue::registry().expect("the catalogue should assemble"));
        let catalogue = RegistryCatalogue::new(Rc::clone(&registry));
        assert_eq!(catalogue.descriptors().len(), registry.iter().len());
        assert!(catalogue.descriptors().len() > 1);
    }
}

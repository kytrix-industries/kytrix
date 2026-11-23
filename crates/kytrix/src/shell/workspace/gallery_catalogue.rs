use std::rc::Rc;

use ui_core::{app::App, node::id::NodeId, window::WindowId};
use widget::{
    any::AnyWidget, config::WidgetConfigChange, descriptor::WidgetDescriptor, identity::WidgetId,
    services::WidgetServices,
};
use widget_gallery_ui::catalogue::{GalleryCatalogue, GalleryError};
use widget_registry::registry::{WidgetMountError, WidgetRegistry};

pub(crate) struct RegistryCatalogue {
    registry: Rc<WidgetRegistry>,
}

impl RegistryCatalogue {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_adapter_should_list_every_registered_widget() {
        let registry =
            Rc::new(widget_registry::catalogue::registry().expect("the catalogue should assemble"));
        let catalogue = RegistryCatalogue::new(Rc::clone(&registry));
        assert_eq!(catalogue.descriptors().len(), registry.iter().len());
        assert!(catalogue.descriptors().len() > 1);
    }
}

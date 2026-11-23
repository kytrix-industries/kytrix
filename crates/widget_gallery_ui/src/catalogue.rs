use std::rc::Rc;

use smol_str::SmolStr;
use ui_core::{app::App, error::EngineError, node::id::NodeId, window::WindowId};
use widget::{
    any::AnyWidget, descriptor::WidgetDescriptor, identity::WidgetId, services::WidgetServices,
};

pub trait GalleryCatalogue {
    fn descriptors(&self) -> Vec<WidgetDescriptor>;

    fn mount(
        &self,
        id: &WidgetId,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        services: &Rc<WidgetServices>,
    ) -> Result<AnyWidget, GalleryError>;
}

#[derive(Debug)]
pub enum GalleryError {
    NotRegistered(SmolStr),
    Engine(EngineError),
}

impl std::fmt::Display for GalleryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRegistered(id) => write!(formatter, "widget `{id}` is not registered"),
            Self::Engine(error) => write!(formatter, "mounting the widget failed: {error}"),
        }
    }
}

impl std::error::Error for GalleryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotRegistered(_) => None,
            Self::Engine(error) => Some(error),
        }
    }
}

impl From<EngineError> for GalleryError {
    fn from(error: EngineError) -> Self {
        Self::Engine(error)
    }
}

#[cfg(test)]
mod tests {
    use super::GalleryError;

    #[test]
    fn an_engine_failure_should_carry_its_source() {
        let error =
            GalleryError::Engine(ui_core::error::EngineError::InvalidDpiScale { factor: 0.0 });

        assert!(
            std::error::Error::source(&error).is_some(),
            "the engine error stays reachable: {error}"
        );
        assert!(
            error.to_string().contains("0"),
            "the gallery's sentence names the engine's failure: {error}"
        );
    }

    #[test]
    fn a_missing_registration_should_name_the_widget() {
        let error = GalleryError::NotRegistered(smol_str::SmolStr::new_static("kytrix.missing"));

        assert_eq!(
            error.to_string(),
            "widget `kytrix.missing` is not registered"
        );
        assert!(std::error::Error::source(&error).is_none());
    }
}

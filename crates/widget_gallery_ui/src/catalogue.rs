//! What the gallery needs from the widget registry.
//!
//! The registry owns the catalogue: which widgets are compiled into the application, the
//! metadata each one declares, and how one is mounted. This crate owns the page, so the
//! two meet at the narrow seam below rather than at a type from either crate.
//!
//! # Why this is a trait rather than the registry type
//!
//! The gallery is a page an application embeds; the registry is the application's
//! catalogue. Neither crate may depend on the other: the registry is the last crate that
//! compiles (it imports every widget), and the gallery is a leaf an application root wires
//! both into. A trait the page consumes lets the host adapt the registry it has — the
//! composition root (P4 `kytrix`) implements it over the registry, and the tests here
//! implement it over their own catalogue.
//!
//! # What the registry has to offer
//!
//! [`GalleryCatalogue::descriptors`] is the registry's list, in registry order, with the
//! metadata the page draws (identity, category, name key, description key, icon).
//! [`GalleryCatalogue::mount`] is the registry's one mount path: create the widget, build
//! its surface under the parent the page hands over, and start its first load. Both are
//! exactly `WidgetRegistry::iter()` + `WidgetRegistration::mount(..)`; the page never
//! sees a `WidgetView` type, so a new widget appears on the page without the page
//! changing.
//!
//! The returned [`AnyWidget`] is the page's to release: the preview owns at most one, and
//! releasing it is what keeps switching rows from accumulating widget instances.

use std::rc::Rc;

use smol_str::SmolStr;
use ui_core::{app::App, error::EngineError, node::id::NodeId, window::WindowId};
use widget::{
    any::AnyWidget, descriptor::WidgetDescriptor, identity::WidgetId, services::WidgetServices,
};

/// The catalogue operations the gallery page needs.
///
/// The host implements this over its registry; the page calls nothing else to learn what
/// may be chosen or to show what was chosen.
pub trait GalleryCatalogue {
    /// Every registered widget, in registry order.
    ///
    /// The page keeps the selectable ones and ignores the rest, so the implementation may
    /// return the registry's whole list unchanged.
    fn descriptors(&self) -> Vec<WidgetDescriptor>;

    /// Mounts the widget `id` as a child of `parent` and starts its first load.
    ///
    /// The page owns the returned instance and releases it when it replaces the preview
    /// or is itself released.
    ///
    /// # Errors
    ///
    /// [`GalleryError::NotRegistered`] when the catalogue does not know `id` (the page
    /// only asks for ids it listed, so this is a catalogue that changed underneath the
    /// page), or [`GalleryError::Engine`] when the engine refused the mount.
    fn mount(
        &self,
        id: &WidgetId,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        services: &Rc<WidgetServices>,
    ) -> Result<AnyWidget, GalleryError>;
}

/// Why the page could not mount the widget it selected.
///
/// It is the seam's error rather than a user-facing one: the gallery reports the failure
/// to the application log and shows the preview's empty slot, because a widget that
/// cannot be built is a catalogue/integration fault, not copy for the reader.
#[derive(Debug)]
pub enum GalleryError {
    /// The catalogue has no widget with this identity.
    NotRegistered(SmolStr),
    /// The engine refused to build the widget's surface or body.
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

    /// The engine's failure keeps its source and its sentence, so a host that logs the
    /// gallery's error can still tell an engine refusal from a missing registration.
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

    /// A missing registration says which widget the catalogue could not answer for.
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

//! Immutable metadata that identifies one widget type.

use app_config::widget::WidgetConfigSchemaVersion;

use super::{category::WidgetCategory, identity::WidgetId};

/// Immutable metadata used to identify, display, and persist a widget type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WidgetDescriptor {
    id: WidgetId,
    display_name: &'static str,
    category: WidgetCategory,
    description: &'static str,
    icon: Option<&'static str>,
    selectable: bool,
    config_schema_version: WidgetConfigSchemaVersion,
}

impl WidgetDescriptor {
    /// Creates a descriptor with no optional description or icon, selectable.
    pub const fn new(
        id: WidgetId,
        display_name: &'static str,
        category: WidgetCategory,
        config_schema_version: WidgetConfigSchemaVersion,
    ) -> Self {
        Self {
            id,
            display_name,
            category,
            description: "",
            icon: None,
            selectable: true,
            config_schema_version,
        }
    }

    /// Sets human-readable detail for widget selection surfaces.
    pub const fn with_description(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }

    /// Sets an application asset path or icon key.
    pub const fn with_icon(mut self, icon: &'static str) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Sets whether a selection surface may offer this widget.
    pub const fn with_selectable(mut self, selectable: bool) -> Self {
        self.selectable = selectable;
        self
    }

    /// Returns the stable type ID.
    pub const fn id(&self) -> &WidgetId {
        &self.id
    }

    /// Returns the display name message key.
    pub const fn display_name(&self) -> &'static str {
        self.display_name
    }

    /// Returns the gallery category this widget is listed under.
    pub const fn category(&self) -> WidgetCategory {
        self.category
    }

    /// Returns the optional descriptive text, or an empty string when omitted.
    pub const fn description(&self) -> &'static str {
        self.description
    }

    /// Returns the optional application asset path or icon key.
    pub const fn icon(&self) -> Option<&'static str> {
        self.icon
    }

    /// Returns whether a selection surface may offer this widget.
    pub const fn selectable(&self) -> bool {
        self.selectable
    }

    /// Returns the persisted config schema version.
    pub const fn config_schema_version(&self) -> WidgetConfigSchemaVersion {
        self.config_schema_version
    }
}

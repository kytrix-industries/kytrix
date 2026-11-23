use app_config::widget::WidgetConfigSchemaVersion;

use super::{category::WidgetCategory, identity::WidgetId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WidgetDescriptor {
    id: WidgetId,
    display_name: &'static str,
    category: WidgetCategory,
    description: Option<&'static str>,
    icon: Option<&'static str>,
    selectable: bool,
    config_schema_version: WidgetConfigSchemaVersion,
}

impl WidgetDescriptor {
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
            description: None,
            icon: None,
            selectable: true,
            config_schema_version,
        }
    }

    pub const fn with_description(mut self, description: &'static str) -> Self {
        self.description = Some(description);
        self
    }

    pub const fn with_icon(mut self, icon: &'static str) -> Self {
        self.icon = Some(icon);
        self
    }

    pub const fn with_selectable(mut self, selectable: bool) -> Self {
        self.selectable = selectable;
        self
    }

    pub const fn id(&self) -> &WidgetId {
        &self.id
    }

    pub const fn display_name(&self) -> &'static str {
        self.display_name
    }

    pub const fn category(&self) -> WidgetCategory {
        self.category
    }

    pub const fn description(&self) -> Option<&'static str> {
        self.description
    }

    pub const fn icon(&self) -> Option<&'static str> {
        self.icon
    }

    pub const fn selectable(&self) -> bool {
        self.selectable
    }

    pub const fn config_schema_version(&self) -> WidgetConfigSchemaVersion {
        self.config_schema_version
    }
}

//! Preferences about the interface itself.
//!
//! These values describe how a window draws the application rather than what the
//! application does, so they are stored beside the domain settings but take no part in
//! the configuration change events: the window that draws them reads them at startup and
//! writes them back when the user changes them, and nothing else subscribes to them.

use serde::{Deserialize, Serialize};

/// How the main window draws its left sidebar.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum SidebarMode {
    /// The sidebar rests at no width: the title bar's toggle floats a preview of it.
    #[serde(rename = "hidden")]
    Hidden,
    /// The sidebar is drawn as icons without labels.
    #[serde(rename = "icon_only")]
    IconOnly,
    /// The sidebar is drawn with icons and labels.
    #[default]
    #[serde(rename = "full")]
    Full,
}

/// The persisted interface preferences.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiConfig {
    /// How the main window draws its left sidebar.
    pub sidebar: SidebarMode,
}

#[cfg(test)]
mod tests {
    use super::{SidebarMode, UiConfig};

    /// A fresh installation shows the full sidebar, which is the state a file written
    /// before this setting existed decodes as well.
    #[test]
    fn the_default_sidebar_mode_should_be_full() {
        assert_eq!(SidebarMode::default(), SidebarMode::Full);
        assert_eq!(UiConfig::default().sidebar, SidebarMode::Full);
    }

    /// Each mode is one stable wire value, and the value is what a stored file carries.
    #[test]
    fn every_sidebar_mode_should_round_trip_through_its_wire_value() {
        for (mode, value) in [
            (SidebarMode::Hidden, "hidden"),
            (SidebarMode::IconOnly, "icon_only"),
            (SidebarMode::Full, "full"),
        ] {
            let json = serde_json::to_value(mode).expect("a sidebar mode should serialize");
            let decoded: SidebarMode =
                serde_json::from_value(json.clone()).expect("a sidebar mode should deserialize");

            assert_eq!((json, decoded), (serde_json::json!(value), mode));
        }
    }

    /// The mode is a field of the interface section, under the name the file stores it as.
    #[test]
    fn the_sidebar_should_be_a_named_field_of_the_ui_section() {
        let config = UiConfig {
            sidebar: SidebarMode::IconOnly,
        };
        let json = serde_json::to_value(&config).expect("the interface settings should serialize");

        assert_eq!(json, serde_json::json!({ "sidebar": "icon_only" }));
        assert_eq!(
            serde_json::from_value::<UiConfig>(json).expect("the interface settings should decode"),
            config
        );
    }
}

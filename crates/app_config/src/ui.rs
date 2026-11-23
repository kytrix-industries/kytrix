use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum SidebarMode {
    #[serde(rename = "hidden")]
    Hidden,
    #[serde(rename = "icon_only")]
    IconOnly,
    #[default]
    #[serde(rename = "full")]
    Full,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiConfig {
    pub sidebar: SidebarMode,
}

#[cfg(test)]
mod tests {
    use super::{SidebarMode, UiConfig};

    #[test]
    fn the_default_sidebar_mode_should_be_full() {
        assert_eq!(SidebarMode::default(), SidebarMode::Full);
        assert_eq!(UiConfig::default().sidebar, SidebarMode::Full);
    }

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

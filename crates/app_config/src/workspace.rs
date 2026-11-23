use ahash::AHashSet;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use smol_str::SmolStr;
use thiserror::Error;

use crate::widget::VersionedWidgetConfig;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    #[serde(deserialize_with = "deserialize_workspace_name")]
    pub name: SmolStr,
    pub layout: WorkspaceLayout,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum WorkspaceLayout {
    #[serde(rename = "split")]
    Split {
        orientation: WorkspaceOrientation,
        #[serde(
            serialize_with = "serialize_head_fraction",
            deserialize_with = "deserialize_head_fraction"
        )]
        head_fraction: f32,
        head: Box<Self>,
        tail: Box<Self>,
    },
    #[serde(rename = "pane")]
    Pane { widget: WorkspaceWidget },
}

pub const DEFAULT_WIDGET_ID: &str = "kytrix.empty";

impl WorkspaceLayout {
    pub fn single_pane() -> Self {
        Self::Pane {
            widget: WorkspaceWidget {
                id: SmolStr::new_static(DEFAULT_WIDGET_ID),
                config: None,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceOrientation {
    #[serde(rename = "horizontal")]
    Horizontal,
    #[serde(rename = "vertical")]
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceWidget {
    #[serde(deserialize_with = "deserialize_widget_id")]
    pub id: SmolStr,
    #[serde(default)]
    pub config: Option<VersionedWidgetConfig>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct WorkspacesConfig {
    pub entries: Vec<WorkspaceConfig>,
    pub active: Option<SmolStr>,
}

#[derive(Deserialize)]
struct WorkspacesConfigFields {
    #[serde(default)]
    entries: Vec<WorkspaceConfig>,
    #[serde(default)]
    active: Option<SmolStr>,
}

impl<'de> Deserialize<'de> for WorkspacesConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let WorkspacesConfigFields { entries, active } =
            WorkspacesConfigFields::deserialize(deserializer)?;

        validate_workspaces(&entries, active.as_ref()).map_err(D::Error::custom)?;

        Ok(Self { entries, active })
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
enum WorkspacesConfigError {
    #[error("an empty workspace list must not name an active workspace")]
    ActiveWithoutEntries,
    #[error("a workspace list must name its active workspace")]
    MissingActiveWorkspace,
    #[error("workspace {name:?} is named as active but does not exist")]
    UnknownActiveWorkspace { name: SmolStr },
    #[error("workspace {name:?} is listed more than once")]
    DuplicateName { name: SmolStr },
}

fn validate_workspaces(
    entries: &[WorkspaceConfig],
    active: Option<&SmolStr>,
) -> Result<(), WorkspacesConfigError> {
    if entries.is_empty() {
        return match active {
            None => Ok(()),
            Some(_) => Err(WorkspacesConfigError::ActiveWithoutEntries),
        };
    }

    let Some(active) = active else {
        return Err(WorkspacesConfigError::MissingActiveWorkspace);
    };

    let mut names = AHashSet::with_capacity(entries.len());
    for entry in entries {
        if !names.insert(&entry.name) {
            return Err(WorkspacesConfigError::DuplicateName {
                name: entry.name.clone(),
            });
        }
    }

    if !names.contains(active) {
        return Err(WorkspacesConfigError::UnknownActiveWorkspace {
            name: active.clone(),
        });
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum WorkspaceNameError {
    #[error("a workspace name must not be empty")]
    Empty,
    #[error("another workspace already uses this name")]
    Taken,
}

pub fn validate_workspace_name(name: &str) -> Result<SmolStr, WorkspaceNameError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(WorkspaceNameError::Empty);
    }

    Ok(SmolStr::new(name))
}

fn deserialize_workspace_name<'de, D>(deserializer: D) -> Result<SmolStr, D::Error>
where
    D: Deserializer<'de>,
{
    let stored = SmolStr::deserialize(deserializer)?;
    let name = validate_workspace_name(stored.as_str()).map_err(D::Error::custom)?;
    if name != stored {
        return Err(D::Error::custom(format_args!(
            "workspace name {stored:?} must not have surrounding whitespace"
        )));
    }

    Ok(name)
}

fn deserialize_head_fraction<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    let fraction = f32::deserialize(deserializer)?;
    if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
        return Err(D::Error::custom(format_args!(
            "a split fraction must be finite and within 0.0 and 1.0; found {fraction}"
        )));
    }

    Ok(fraction)
}

fn serialize_head_fraction<S>(fraction: &f32, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_f64((f64::from(*fraction) * 1_000.0).round() / 1_000.0)
}

fn deserialize_widget_id<'de, D>(deserializer: D) -> Result<SmolStr, D::Error>
where
    D: Deserializer<'de>,
{
    let id = SmolStr::deserialize(deserializer)?;
    if id.is_empty() {
        return Err(D::Error::custom("a pane widget id must not be empty"));
    }

    Ok(id)
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::{
            route::{VenueListingKey, VenueRoute},
            venue::VenueListingId,
        },
        market::selection::InstrumentSelection,
    };
    use serde::{
        Deserialize, Serialize,
        de::value::{Error as ValueError, F32Deserializer},
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::widget::{WidgetConfig, WidgetConfigSchemaVersion, decode_or_default};

    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    struct TestConfig {
        interval: u32,
    }

    impl WidgetConfig for TestConfig {
        const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
    }

    fn pane(widget: WorkspaceWidget) -> WorkspaceLayout {
        WorkspaceLayout::Pane { widget }
    }

    fn widget(id: &str, config: Option<VersionedWidgetConfig>) -> WorkspaceWidget {
        WorkspaceWidget {
            id: SmolStr::new(id),
            config,
        }
    }

    fn default_pane() -> WorkspaceLayout {
        WorkspaceLayout::single_pane()
    }

    fn saved_workspaces() -> WorkspacesConfig {
        WorkspacesConfig {
            entries: vec![
                WorkspaceConfig {
                    name: SmolStr::new_static("工作区 1"),
                    layout: WorkspaceLayout::Split {
                        orientation: WorkspaceOrientation::Horizontal,
                        head_fraction: 0.35,
                        head: Box::new(pane(widget(
                            "kytrix.candlestick",
                            Some(
                                VersionedWidgetConfig::encode(&TestConfig { interval: 1 })
                                    .expect("a test config should encode"),
                            ),
                        ))),
                        tail: Box::new(WorkspaceLayout::Split {
                            orientation: WorkspaceOrientation::Vertical,
                            head_fraction: 0.75,
                            head: Box::new(default_pane()),
                            tail: Box::new(pane(widget("kytrix.order-book", None))),
                        }),
                    },
                },
                WorkspaceConfig {
                    name: SmolStr::new_static("Workspace 2"),
                    layout: WorkspaceLayout::single_pane(),
                },
            ],
            active: Some(SmolStr::new_static("Workspace 2")),
        }
    }

    #[test]
    fn workspaces_should_round_trip_through_json() {
        let saved = saved_workspaces();

        let json = serde_json::to_string(&saved).expect("workspaces should encode");
        let decoded = serde_json::from_str::<WorkspacesConfig>(&json)
            .expect("encoded workspaces should decode");

        assert_eq!(decoded, saved);
    }

    #[test]
    fn a_workspace_layout_should_use_the_documented_wire_shape() {
        let json = serde_json::to_value(saved_workspaces()).expect("workspaces should encode");

        let first = &json["entries"][0];
        assert_eq!(first["name"], "工作区 1");
        assert_eq!(first["layout"]["kind"], "split");
        assert_eq!(first["layout"]["orientation"], "horizontal");
        assert_eq!(first["layout"]["head_fraction"], 0.35);
        assert_eq!(first["layout"]["tail"]["kind"], "split");
        assert_eq!(first["layout"]["tail"]["orientation"], "vertical");
        assert_eq!(
            first["layout"]["head"]["widget"]["id"],
            "kytrix.candlestick"
        );
        assert_eq!(
            first["layout"]["head"]["widget"]["config"]["schema_version"],
            1
        );
        assert_eq!(
            first["layout"]["tail"]["head"]["widget"]["id"], DEFAULT_WIDGET_ID,
            "a pane nobody has chosen for shows the widget a pane falls back to"
        );
        assert_eq!(
            first["layout"]["tail"]["head"]["widget"]["config"],
            Value::Null
        );
        assert_eq!(
            first["layout"]["tail"]["tail"]["widget"]["config"],
            Value::Null
        );
        assert_eq!(json["entries"][1]["layout"]["kind"], "pane");
        assert_eq!(
            json["entries"][1]["layout"]["widget"]["id"], DEFAULT_WIDGET_ID,
            "a new workspace starts as a pane showing that widget"
        );
        assert_eq!(json["active"], "Workspace 2");
    }

    fn default_pane_json() -> Value {
        json!({ "kind": "pane", "widget": { "id": DEFAULT_WIDGET_ID, "config": null } })
    }

    #[test]
    fn a_duplicate_workspace_name_should_be_rejected() {
        let duplicate = json!({
            "entries": [
                { "name": "Workspace 1", "layout": default_pane_json() },
                { "name": "Workspace 1", "layout": default_pane_json() },
            ],
            "active": "Workspace 1",
        });

        let error = serde_json::from_value::<WorkspacesConfig>(duplicate)
            .expect_err("two workspaces must not share a name");

        assert!(error.to_string().contains("more than once"));
    }

    #[test]
    fn an_empty_or_padded_workspace_name_should_be_rejected() {
        for name in ["", "   ", " Workspace 1", "Workspace 1 "] {
            let saved = json!({
                "entries": [{ "name": name, "layout": default_pane_json() }],
                "active": name,
            });

            assert!(
                serde_json::from_value::<WorkspacesConfig>(saved).is_err(),
                "workspace name {name:?} must be rejected"
            );
        }
    }

    #[test]
    fn an_active_workspace_that_is_missing_or_unknown_should_be_rejected() {
        let entries = json!([
            { "name": "Workspace 1", "layout": default_pane_json() },
        ]);

        assert!(
            serde_json::from_value::<WorkspacesConfig>(json!({ "entries": entries })).is_err(),
            "a workspace list must name its active workspace"
        );
        assert!(
            serde_json::from_value::<WorkspacesConfig>(json!({
                "entries": entries,
                "active": "Workspace 9",
            }))
            .is_err(),
            "the active workspace must exist"
        );
    }

    #[test]
    fn an_empty_workspace_list_should_require_no_active_workspace() {
        let decoded = serde_json::from_value::<WorkspacesConfig>(json!({
            "entries": [],
            "active": null,
        }))
        .expect("an installation without workspaces is valid");

        assert_eq!(decoded, WorkspacesConfig::default());
        assert!(
            serde_json::from_value::<WorkspacesConfig>(json!({
                "entries": [],
                "active": "Workspace 1",
            }))
            .is_err(),
            "an empty list cannot have an active workspace"
        );
    }

    #[test]
    fn a_split_fraction_outside_zero_to_one_should_be_rejected() {
        for fraction in [json!(-0.1), json!(1.1), json!(1.0e40), json!(null)] {
            let saved = json!({
                "entries": [{
                    "name": "Workspace 1",
                    "layout": {
                        "kind": "split",
                        "orientation": "horizontal",
                        "head_fraction": fraction,
                        "head": default_pane_json(),
                        "tail": default_pane_json(),
                    },
                }],
                "active": "Workspace 1",
            });

            assert!(
                serde_json::from_value::<WorkspacesConfig>(saved).is_err(),
                "fraction {fraction} must be rejected"
            );
        }
    }

    #[test]
    fn a_nan_split_fraction_should_be_rejected() {
        let error = deserialize_head_fraction(F32Deserializer::<ValueError>::new(f32::NAN))
            .expect_err("NaN is not a usable split fraction");

        assert!(error.to_string().contains("finite"));
    }

    #[test]
    fn a_fraction_at_either_end_should_be_accepted() {
        for fraction in [0.0_f32, 1.0] {
            let saved = json!({
                "entries": [{
                    "name": "Workspace 1",
                    "layout": {
                        "kind": "split",
                        "orientation": "vertical",
                        "head_fraction": fraction,
                        "head": default_pane_json(),
                        "tail": default_pane_json(),
                    },
                }],
                "active": "Workspace 1",
            });

            assert!(
                serde_json::from_value::<WorkspacesConfig>(saved).is_ok(),
                "fraction {fraction} is within range"
            );
        }
    }

    #[test]
    fn an_empty_widget_id_should_be_rejected() {
        let saved = json!({
            "entries": [{
                "name": "Workspace 1",
                "layout": {
                    "kind": "pane",
                    "widget": { "id": "", "config": null },
                },
            }],
            "active": "Workspace 1",
        });

        assert!(serde_json::from_value::<WorkspacesConfig>(saved).is_err());
    }

    #[test]
    fn a_pane_without_a_widget_should_be_rejected() {
        for missing in [json!(null), json!("kytrix.empty")] {
            let saved = json!({
                "entries": [{
                    "name": "Workspace 1",
                    "layout": { "kind": "pane", "widget": missing },
                }],
                "active": "Workspace 1",
            });

            assert!(
                serde_json::from_value::<WorkspacesConfig>(saved).is_err(),
                "a pane naming {missing} is not a pane"
            );
        }
    }

    #[test]
    fn a_new_workspace_should_start_as_a_pane_showing_the_default_widget() {
        let layout = WorkspaceLayout::single_pane();
        let WorkspaceLayout::Pane { widget } = &layout else {
            panic!("a new workspace starts as a single pane");
        };

        assert_eq!(widget.id, DEFAULT_WIDGET_ID);
        assert_eq!(widget.config, None);
        assert_eq!(
            serde_json::to_value(&layout).expect("a new pane should encode"),
            json!({ "kind": "pane", "widget": { "id": DEFAULT_WIDGET_ID, "config": null } })
        );
    }

    #[test]
    fn unknown_fields_in_a_workspace_should_be_ignored() {
        let decoded = serde_json::from_value::<WorkspacesConfig>(json!({
            "entries": [{
                "name": "Workspace 1",
                "extra": 7,
                "layout": { "kind": "pane", "widget": { "id": DEFAULT_WIDGET_ID, "config": null }, "extra": 7 },
            }],
            "active": "Workspace 1",
            "extra": 7,
        }))
        .expect("unknown workspace fields should be ignored");

        assert_eq!(decoded.active.as_deref(), Some("Workspace 1"));
    }

    #[test]
    fn an_unknown_layout_kind_should_be_rejected() {
        let saved = json!({
            "entries": [{
                "name": "Workspace 1",
                "layout": { "kind": "grid", "columns": 3 },
            }],
            "active": "Workspace 1",
        });

        assert!(serde_json::from_value::<WorkspacesConfig>(saved).is_err());
    }

    #[test]
    fn validate_workspace_name_should_trim_and_reject_empty() {
        assert_eq!(
            validate_workspace_name("  Workspace 1\t"),
            Ok(SmolStr::new_static("Workspace 1"))
        );
        assert_eq!(
            validate_workspace_name("   "),
            Err(WorkspaceNameError::Empty)
        );
        assert_eq!(validate_workspace_name(""), Err(WorkspaceNameError::Empty));
    }

    #[test]
    fn a_schema_version_of_zero_should_be_rejected_in_a_pane() {
        let saved = json!({
            "entries": [{
                "name": "Workspace 1",
                "layout": {
                    "kind": "pane",
                    "widget": {
                        "id": "kytrix.candlestick",
                        "config": { "schema_version": 0, "payload": "{}" },
                    },
                },
            }],
            "active": "Workspace 1",
        });

        assert!(serde_json::from_value::<WorkspacesConfig>(saved).is_err());
    }

    #[test]
    fn a_widget_config_schema_version_should_survive_a_round_trip() {
        let saved = json!({
            "entries": [{
                "name": "Workspace 1",
                "layout": {
                    "kind": "pane",
                    "widget": {
                        "id": "kytrix.candlestick",
                        "config": { "schema_version": 2, "payload": "{}" },
                    },
                },
            }],
            "active": "Workspace 1",
        });

        let decoded = serde_json::from_value::<WorkspacesConfig>(saved)
            .expect("a versioned pane config should decode");
        let WorkspaceLayout::Pane { widget } = &decoded.entries[0].layout else {
            panic!("the saved layout is a pane");
        };
        let config = widget.config.as_ref().expect("the pane keeps its envelope");

        assert_eq!(
            config.schema_version(),
            WidgetConfigSchemaVersion::new_static(2)
        );
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct ListingConfig {
        selection: InstrumentSelection,
    }

    impl Default for ListingConfig {
        fn default() -> Self {
            Self {
                selection: listing_selection("core/spot", "00"),
            }
        }
    }

    impl WidgetConfig for ListingConfig {
        const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
    }

    fn listing_selection(scope: &str, identity: &str) -> InstrumentSelection {
        InstrumentSelection {
            listing: VenueListingKey::new(
                VenueRoute::new(
                    VenueId::Binance,
                    VenueScopeId::new(scope).expect("the scope should be canonical"),
                ),
                VenueListingId::from_identity_value(&format!(
                    "listing-id/v1/{}",
                    identity.repeat(32)
                ))
                .expect("the listing id should decode"),
            ),
        }
    }

    fn stored_route_and_instrument_payload() -> &'static str {
        concat!(
            r#"{"route":{"venue":"binance","scope":"spot"},"#,
            r#""instrument":{"symbol":"BTCUSDT"}}"#,
        )
    }

    fn stored_route_and_instrument_config() -> VersionedWidgetConfig {
        serde_json::from_value(json!({
            "schema_version": 1,
            "payload": stored_route_and_instrument_payload(),
        }))
        .expect("the removed envelope should decode")
    }

    #[test]
    fn a_stored_route_and_instrument_payload_should_be_replaced_by_the_default() {
        assert!(
            serde_json::from_str::<ListingConfig>(stored_route_and_instrument_payload()).is_err(),
            "the removed payload does not carry the listing this config requires"
        );

        let stored = stored_route_and_instrument_config();
        let saved = WorkspacesConfig {
            entries: vec![WorkspaceConfig {
                name: SmolStr::new_static("Workspace 1"),
                layout: pane(widget("kytrix.candlestick", Some(stored))),
            }],
            active: Some(SmolStr::new_static("Workspace 1")),
        };
        let decoded = serde_json::from_str::<WorkspacesConfig>(
            &serde_json::to_string(&saved).expect("the workspace should encode"),
        )
        .expect("the saved workspace should decode");
        let WorkspaceLayout::Pane { widget } = &decoded.entries[0].layout else {
            panic!("the saved layout is a pane");
        };
        let stored = widget.config.clone().expect("the pane keeps its envelope");

        let config = decode_or_default::<ListingConfig>(Some(stored.clone()));
        assert_eq!(
            config,
            ListingConfig::default(),
            "a payload of the removed shape is dropped whole"
        );

        let written_back =
            VersionedWidgetConfig::encode(&config).expect("the default config should encode");
        assert_ne!(
            written_back, stored,
            "the pane writes the replacement back because the envelopes differ"
        );
        assert_eq!(
            decode_or_default::<ListingConfig>(Some(written_back)),
            ListingConfig::default()
        );
    }

    #[test]
    fn a_listing_payload_should_round_trip_and_require_its_listing_key() {
        let config = ListingConfig {
            selection: listing_selection("core/perp", "ab"),
        };
        let stored = VersionedWidgetConfig::encode(&config).expect("the config should encode");

        assert_eq!(decode_or_default::<ListingConfig>(Some(stored)), config);
        assert_ne!(
            config,
            ListingConfig::default(),
            "a stored selection is not the selection the widget falls back to"
        );
        assert_eq!(
            serde_json::to_value(&config).expect("the config should encode"),
            json!({
                "selection": {
                    "listing": {
                        "route": { "venue": "binance", "scope": "core/perp" },
                        "listing_id": format!("listing-id/v1/{}", "ab".repeat(32)),
                    },
                },
            })
        );

        let written_back = VersionedWidgetConfig::encode(&ListingConfig::default())
            .expect("the default config should encode");
        for removed in [
            stored_route_and_instrument_payload(),
            r#"{"selection":{"route":{"venue":"binance","scope":"spot"}}}"#,
            r#"{"selection":{}}"#,
            "{}",
        ] {
            assert!(
                serde_json::from_str::<ListingConfig>(removed).is_err(),
                "the listing is required to decode this config: {removed}"
            );

            let stored = serde_json::from_value::<VersionedWidgetConfig>(json!({
                "schema_version": 1,
                "payload": removed,
            }))
            .expect("the envelope should decode");

            assert_eq!(
                decode_or_default::<ListingConfig>(Some(stored.clone())),
                ListingConfig::default(),
                "{removed}"
            );
            assert_ne!(
                written_back, stored,
                "a payload this config cannot read is replaced on write back: {removed}"
            );
        }
    }
}

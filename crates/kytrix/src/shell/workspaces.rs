use app_config::workspace::{
    WorkspaceConfig, WorkspaceLayout, WorkspaceNameError, WorkspacesConfig, validate_workspace_name,
};
use i18n::{t_smol, translate::Translate};
use smol_str::SmolStr;

pub(crate) struct Workspaces {
    entries: Vec<WorkspaceConfig>,
    active: usize,
}

impl Workspaces {
    pub(crate) fn load(stored: &WorkspacesConfig, translate: &Translate) -> (Self, bool) {
        let mut entries = stored.entries.clone();
        let created = entries.is_empty();
        if created {
            entries.push(WorkspaceConfig {
                name: default_name(1, translate),
                layout: WorkspaceLayout::single_pane(),
            });
        }
        let active = stored
            .active
            .as_ref()
            .and_then(|name| entries.iter().position(|entry| &entry.name == name))
            .unwrap_or(0);

        (Self { entries, active }, created)
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &SmolStr> {
        self.entries.iter().map(|entry| &entry.name)
    }

    pub(crate) fn active_name(&self) -> &SmolStr {
        &self.entries[self.active].name
    }

    pub(crate) fn active_layout(&self) -> &WorkspaceLayout {
        &self.entries[self.active].layout
    }

    pub(crate) fn set_active_layout(&mut self, layout: WorkspaceLayout) {
        self.entries[self.active].layout = layout;
    }

    pub(crate) fn can_delete(&self) -> bool {
        self.entries.len() > 1
    }

    pub(crate) fn suggested_name(&self, translate: &Translate) -> SmolStr {
        (1..=self.entries.len() + 1)
            .map(|index| default_name(index, translate))
            .find(|name| !self.name_is_taken(name.as_str(), None))
            .expect("more candidates than workspaces always leaves a free name")
    }

    pub(crate) fn validate_name(
        &self,
        name: &str,
        except: Option<&str>,
    ) -> Result<SmolStr, WorkspaceNameError> {
        let name = validate_workspace_name(name)?;
        if self.name_is_taken(name.as_str(), except) {
            return Err(WorkspaceNameError::Taken);
        }

        Ok(name)
    }

    pub(crate) fn select(&mut self, name: &str) -> bool {
        let Some(target) = self.entries.iter().position(|entry| entry.name == name) else {
            log::error!("workspace `{name}` does not exist");
            return false;
        };
        if target == self.active {
            return false;
        }
        self.active = target;
        true
    }

    pub(crate) fn add(&mut self, name: &str) -> Result<(), WorkspaceNameError> {
        let name = self.validate_name(name, None)?;
        self.entries.push(WorkspaceConfig {
            name,
            layout: WorkspaceLayout::single_pane(),
        });
        self.active = self.entries.len() - 1;
        Ok(())
    }

    pub(crate) fn rename_entry(
        &mut self,
        name: &str,
        new_name: &str,
    ) -> Result<bool, WorkspaceNameError> {
        let Some(index) = self.entries.iter().position(|entry| entry.name == name) else {
            log::error!("workspace `{name}` does not exist");
            return Ok(false);
        };
        let new_name = self.validate_name(new_name, Some(self.entries[index].name.as_str()))?;
        if new_name == self.entries[index].name {
            return Ok(false);
        }
        self.entries[index].name = new_name;
        Ok(true)
    }

    pub(crate) fn delete_entry(&mut self, name: &str) -> bool {
        if !self.can_delete() {
            log::error!("the last workspace cannot be deleted");
            return false;
        }
        let Some(index) = self.entries.iter().position(|entry| entry.name == name) else {
            log::error!("workspace `{name}` does not exist");
            return false;
        };

        self.entries.remove(index);
        if index == self.active {
            self.active = self.active.saturating_sub(1);
            true
        } else {
            if index < self.active {
                self.active -= 1;
            }
            false
        }
    }

    pub(crate) fn config(&self) -> WorkspacesConfig {
        WorkspacesConfig {
            entries: self.entries.clone(),
            active: Some(self.active_name().clone()),
        }
    }

    fn name_is_taken(&self, name: &str, except: Option<&str>) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.name == name && Some(entry.name.as_str()) != except)
    }
}

fn default_name(index: usize, translate: &Translate) -> SmolStr {
    t_smol!(translate, "window.workspace.default_name", index = index)
}

#[cfg(test)]
mod tests {
    use app_config::workspace::{WorkspaceLayout, WorkspacesConfig};
    use i18n::language::codes::LanguageCode;

    use super::*;
    use crate::test_support::translate;
    fn workspaces(names: &[&str]) -> Workspaces {
        let translate = translate(LanguageCode::En);
        let stored = WorkspacesConfig {
            entries: names
                .iter()
                .map(|name| WorkspaceConfig {
                    name: SmolStr::new(*name),
                    layout: WorkspaceLayout::single_pane(),
                })
                .collect(),
            active: names.first().map(|name| SmolStr::new(*name)),
        };
        let (workspaces, created) = Workspaces::load(&stored, &translate);
        assert!(!created);
        workspaces
    }

    #[test]
    fn a_first_start_should_create_one_workspace() {
        let translate = translate(LanguageCode::En);
        let (workspaces, created) = Workspaces::load(&WorkspacesConfig::default(), &translate);
        assert!(created);
        assert_eq!(workspaces.names().count(), 1);
        assert_eq!(
            workspaces.active_name().as_str(),
            "Workspace 1",
            "the name comes from the catalogue"
        );
        assert!(!workspaces.can_delete());
    }

    #[test]
    fn a_stored_active_workspace_should_be_resolved_or_fall_back() {
        let translate = translate(LanguageCode::En);
        let stored = WorkspacesConfig {
            entries: vec![WorkspaceConfig {
                name: SmolStr::new_static("one"),
                layout: WorkspaceLayout::single_pane(),
            }],
            active: Some(SmolStr::new_static("gone")),
        };
        let (workspaces, _) = Workspaces::load(&stored, &translate);
        assert_eq!(workspaces.active_name().as_str(), "one");
    }

    #[test]
    fn the_suggested_name_should_skip_the_names_in_use() {
        let translate = translate(LanguageCode::En);
        let workspaces = workspaces(&["Workspace 1", "Workspace 3"]);
        assert_eq!(
            workspaces.suggested_name(&translate).as_str(),
            "Workspace 2"
        );
    }

    #[test]
    fn a_name_should_be_checked_against_the_list() {
        let workspaces = workspaces(&["one", "two"]);
        assert_eq!(
            workspaces.validate_name("three", None),
            Ok(SmolStr::new_static("three"))
        );
        assert_eq!(
            workspaces.validate_name("two", None),
            Err(WorkspaceNameError::Taken)
        );
        assert_eq!(
            workspaces.validate_name("two", Some("two")),
            Ok(SmolStr::new_static("two"))
        );
        assert!(workspaces.validate_name("  ", None).is_err());
    }

    #[test]
    fn adding_should_switch_to_the_new_workspace() {
        let mut workspaces = workspaces(&["one"]);
        workspaces.add("two").expect("a free name");
        assert_eq!(workspaces.names().count(), 2);
        assert_eq!(workspaces.active_name().as_str(), "two");
        assert!(workspaces.can_delete());
    }

    #[test]
    fn selecting_should_move_the_marker_once() {
        let mut workspaces = workspaces(&["one", "two"]);
        assert!(workspaces.select("two"));
        assert_eq!(workspaces.active_name().as_str(), "two");
        assert!(!workspaces.select("two"));
        assert!(!workspaces.select("gone"));
    }

    #[test]
    fn deleting_should_choose_the_previous_workspace() {
        let mut workspaces = workspaces(&["one", "two", "three"]);
        workspaces.select("three");
        assert!(workspaces.delete_entry("three"));
        assert_eq!(workspaces.active_name().as_str(), "two");
        assert!(!workspaces.delete_entry("one"));
        assert_eq!(workspaces.active_name().as_str(), "two");
    }

    #[test]
    fn the_last_workspace_should_stay() {
        let mut workspaces = workspaces(&["one"]);
        assert!(!workspaces.delete_entry("one"));
        assert_eq!(workspaces.names().count(), 1);
    }

    #[test]
    fn renaming_should_keep_the_shown_workspace() {
        let mut workspaces = workspaces(&["one", "two"]);
        workspaces.select("two");
        assert_eq!(workspaces.rename_entry("one", "renamed"), Ok(true));
        assert_eq!(workspaces.active_name().as_str(), "two");
        assert_eq!(workspaces.rename_entry("two", "two"), Ok(false));
        assert_eq!(
            workspaces.rename_entry("two", "renamed"),
            Err(WorkspaceNameError::Taken)
        );
    }

    #[test]
    fn the_stored_settings_should_name_the_active_workspace() {
        let mut workspaces = workspaces(&["one", "two"]);
        workspaces.select("two");
        workspaces.set_active_layout(WorkspaceLayout::single_pane());

        let config = workspaces.config();
        assert_eq!(config.entries.len(), 2);
        assert_eq!(config.active, Some(SmolStr::new_static("two")));
    }
}

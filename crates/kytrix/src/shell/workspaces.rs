//! The workspaces of this installation: their names, their layouts, and which one is shown.
//!
//! Only the active workspace is materialised: the stage is rebuilt from the active entry's
//! layout when the user switches, so an inactive workspace costs nothing but the layout it
//! is stored as. The list itself is pure data — it validates names and moves the active
//! marker; the shell is what persists a change (through the settings commit seam) and what
//! rebuilds the stage when the active workspace changes.

use app_config::workspace::{
    WorkspaceConfig, WorkspaceLayout, WorkspaceNameError, WorkspacesConfig, validate_workspace_name,
};
use i18n::{t_smol, translate::Translate};
use smol_str::SmolStr;

/// The workspaces of this installation, in the order the sidebar lists them.
pub(crate) struct Workspaces {
    entries: Vec<WorkspaceConfig>,
    active: usize,
}

impl Workspaces {
    /// Loads the stored workspaces, creating the first one when there is none.
    ///
    /// A first start has nothing to restore, so the default workspace is written out by the
    /// caller (the store reports that it had to create one): the next start reads it back
    /// rather than creating a second one.
    pub(crate) fn load(stored: &WorkspacesConfig, translate: &Translate) -> (Self, bool) {
        let mut entries = stored.entries.clone();
        let created = entries.is_empty();
        if created {
            entries.push(WorkspaceConfig {
                name: default_name(1, translate),
                layout: WorkspaceLayout::single_pane(),
            });
        }
        // A stored file cannot name an active workspace that is not in the list, so this
        // only has to answer for a list that was built in memory.
        let active = stored
            .active
            .as_ref()
            .and_then(|name| entries.iter().position(|entry| &entry.name == name))
            .unwrap_or(0);

        (Self { entries, active }, created)
    }

    /// The workspace names, in the order the sidebar lists them.
    pub(crate) fn names(&self) -> impl Iterator<Item = &SmolStr> {
        self.entries.iter().map(|entry| &entry.name)
    }

    /// The name of the workspace being shown.
    pub(crate) fn active_name(&self) -> &SmolStr {
        &self.entries[self.active].name
    }

    /// The layout the active workspace is stored as.
    pub(crate) fn active_layout(&self) -> &WorkspaceLayout {
        &self.entries[self.active].layout
    }

    /// Stores the layout the stage is showing into the active workspace.
    pub(crate) fn set_active_layout(&mut self, layout: WorkspaceLayout) {
        self.entries[self.active].layout = layout;
    }

    /// Whether a workspace may be deleted: the last one cannot, because the application
    /// always shows exactly one.
    pub(crate) fn can_delete(&self) -> bool {
        self.entries.len() > 1
    }

    /// The next unused default name in the language showing now.
    pub(crate) fn suggested_name(&self, translate: &Translate) -> SmolStr {
        // One more candidate than there are workspaces leaves at least one free name, so
        // this always finds one.
        (1..=self.entries.len() + 1)
            .map(|index| default_name(index, translate))
            .find(|name| !self.name_is_taken(name.as_str(), None))
            .expect("more candidates than workspaces always leaves a free name")
    }

    /// Checks a candidate name against the workspaces it must not collide with.
    ///
    /// `except` is the workspace being renamed, which may keep its own name.
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

    /// Switches to the workspace named `name`, reporting whether the shown workspace
    /// changed. A name the list does not know is not a switch (the caller's rows come from
    /// the same list, so an unknown name can only be stale).
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

    /// Adds a workspace and switches to it.
    pub(crate) fn add(&mut self, name: &str) -> Result<(), WorkspaceNameError> {
        let name = self.validate_name(name, None)?;
        self.entries.push(WorkspaceConfig {
            name,
            layout: WorkspaceLayout::single_pane(),
        });
        self.active = self.entries.len() - 1;
        Ok(())
    }

    /// Renames the workspace called `name`, reporting whether anything changed.
    ///
    /// The name is the workspace's identity, so renaming the active one keeps the active
    /// marker with it.
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

    /// Deletes the workspace called `name`, reporting whether the shown workspace changed.
    ///
    /// Deleting the active workspace activates the one before it; deleting another one only
    /// shifts the active index when the removed entry was before it. The last workspace
    /// cannot be deleted.
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

    /// The settings this list is stored as.
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

/// The default name of a workspace that does not have one yet.
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

    /// A first start creates the workspace it shows, and the next start reads it back.
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

    /// A stored workspace that does not exist cannot be shown, so a list built in memory
    /// falls back to its first entry.
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

    /// The suggested name is the first default that is not taken.
    #[test]
    fn the_suggested_name_should_skip_the_names_in_use() {
        let translate = translate(LanguageCode::En);
        let workspaces = workspaces(&["Workspace 1", "Workspace 3"]);
        assert_eq!(
            workspaces.suggested_name(&translate).as_str(),
            "Workspace 2"
        );
    }

    /// A name may not be empty or duplicate another workspace's, but a workspace may keep
    /// the name it already has.
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

    /// Adding a workspace appends it and switches to it.
    #[test]
    fn adding_should_switch_to_the_new_workspace() {
        let mut workspaces = workspaces(&["one"]);
        workspaces.add("two").expect("a free name");
        assert_eq!(workspaces.names().count(), 2);
        assert_eq!(workspaces.active_name().as_str(), "two");
        assert!(workspaces.can_delete());
    }

    /// Selecting a workspace moves the marker; selecting a missing or already-shown one
    /// changes nothing.
    #[test]
    fn selecting_should_move_the_marker_once() {
        let mut workspaces = workspaces(&["one", "two"]);
        assert!(workspaces.select("two"));
        assert_eq!(workspaces.active_name().as_str(), "two");
        assert!(!workspaces.select("two"));
        assert!(!workspaces.select("gone"));
    }

    /// Deleting the active workspace shows the one before it; deleting an earlier one
    /// shifts the marker without changing what is shown.
    #[test]
    fn deleting_should_choose_the_previous_workspace() {
        let mut workspaces = workspaces(&["one", "two", "three"]);
        workspaces.select("three");
        assert!(workspaces.delete_entry("three"));
        assert_eq!(workspaces.active_name().as_str(), "two");
        assert!(!workspaces.delete_entry("one"));
        assert_eq!(workspaces.active_name().as_str(), "two");
    }

    /// The last workspace cannot be deleted: the application always shows exactly one.
    #[test]
    fn the_last_workspace_should_stay() {
        let mut workspaces = workspaces(&["one"]);
        assert!(!workspaces.delete_entry("one"));
        assert_eq!(workspaces.names().count(), 1);
    }

    /// Renaming the active workspace keeps it active; renaming another one leaves the
    /// shown workspace alone. A rename to the name it already has is no change.
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

    /// The stored settings are the list as it stands, with the active workspace named.
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

//! The workspaces of this installation, and the stage mirroring the active one.
//!
//! Only the active workspace is materialised. Switching snapshots the stage that
//! is going away into its persisted layout and rebuilds the incoming one from
//! its own, so an inactive workspace costs nothing but the layout it is stored
//! as — and a widget the user returns to is built again from its configuration,
//! exactly as it is after a restart.
//!
//! Every change is written to the settings. A one-shot change is written at once
//! because the result is final; dragging a split bar is written once the drag
//! pauses, because each write replaces and fsyncs the whole configuration file.

use std::time::Duration;

use app_config::workspace::{
    WorkspaceConfig, WorkspaceLayout, WorkspaceNameError, WorkspacesConfig, validate_workspace_name,
};
use app_config_provider::manager::ConfigManager;
use gpui_kit::{
    App, AppContext, BorrowAppContext, Context, Entity, IntoElement, ParentElement, Render,
    SharedString, Styled, Subscription, Task, Window, div,
};
use i18n::t;
use smol_str::SmolStr;

use super::{Stage, stage::StageChange};

/// How long a split bar has to stay still before the drag is written out.
const PERSIST_SETTLE_DELAY: Duration = Duration::from_millis(400);

/// The element id the first stage of a process gets. Every rebuilt stage takes
/// the next one, so a new stage never inherits a dead stage's keyed state.
const FIRST_STAGE_ID: u64 = 1;

pub(crate) struct Workspaces {
    /// In the order the sidebar lists them; the active one is the only entry
    /// with a runtime stage.
    entries: Vec<WorkspaceConfig>,
    active: usize,
    stage: Entity<Stage>,
    /// Monotonic for the life of the process, so no two stages share an element id.
    next_stage_id: u64,
    /// The pending debounced write. Replacing it cancels the previous one, and
    /// dropping it cancels the write it was waiting on.
    pending_persist: Option<Task<()>>,
    /// Follows the current stage's layout changes; replaced with the stage.
    stage_layout: Subscription,
}

impl Workspaces {
    /// Loads the stored workspaces, creating the first one when there is none.
    ///
    /// A first start has nothing to restore, so the default workspace is written
    /// out immediately: the next start reads it back rather than creating a
    /// second one.
    pub(crate) fn load(cx: &mut Context<Self>) -> Self {
        let stored = cx.global::<ConfigManager>().read().workspaces.clone();
        let mut entries = stored.entries;
        let created = entries.is_empty();
        if created {
            entries.push(WorkspaceConfig {
                name: default_workspace_name(1, cx),
                layout: WorkspaceLayout::single_pane(),
            });
        }
        // A stored file cannot name an active workspace that is not in the list,
        // so this only has to answer for a list that was built in memory.
        let active = stored
            .active
            .as_ref()
            .and_then(|name| entries.iter().position(|entry| &entry.name == name))
            .unwrap_or(0);

        let (stage, stage_layout) = Self::build_stage(FIRST_STAGE_ID, &entries[active].layout, cx);
        let mut workspaces = Self {
            entries,
            active,
            stage,
            next_stage_id: FIRST_STAGE_ID + 1,
            pending_persist: None,
            stage_layout,
        };
        // A widget config this build could not read was rebuilt from its defaults
        // while the stage was being built, so what is on screen is no longer what
        // the settings file says. Writing it out here is what stops the same repair
        // from happening again on every start.
        let repaired = workspaces.stage.read(cx).has_repaired_widget_config();
        if created || repaired {
            workspaces.persist(cx);
        }

        workspaces
    }

    /// The workspaces the sidebar lists, in order.
    pub(crate) fn names(&self) -> impl Iterator<Item = &SmolStr> {
        self.entries.iter().map(|entry| &entry.name)
    }

    pub(crate) fn active_name(&self) -> &SmolStr {
        &self.entries[self.active].name
    }

    /// Whether a workspace may be deleted.
    ///
    /// The last one cannot: the application always shows exactly one workspace,
    /// so an empty list is not a state it has.
    pub(crate) fn can_delete(&self) -> bool {
        self.entries.len() > 1
    }

    /// The next unused default name in the current language.
    pub(crate) fn suggested_name(&self, cx: &App) -> SharedString {
        // One more candidate than there are workspaces leaves at least one free
        // name, so this always finds one.
        let name = (1..=self.entries.len() + 1)
            .map(|index| default_workspace_name(index, cx))
            .find(|name| !self.name_is_taken(name.as_str(), None))
            .expect("more candidates than workspaces always leaves a free name");

        SharedString::from(name.as_str())
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

    /// Switches to a workspace by name.
    pub(crate) fn select(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(target) = self.entries.iter().position(|entry| entry.name == name) else {
            log::error!("workspace `{name}` does not exist");

            return;
        };
        if target == self.active {
            return;
        }

        self.snapshot_active(cx);
        self.pending_persist = None;
        self.active = target;
        self.replace_stage(cx);
        self.persist(cx);
        cx.notify();
    }

    /// Adds a workspace and switches to it.
    pub(crate) fn add(
        &mut self,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), WorkspaceNameError> {
        let name = self.validate_name(name, None)?;
        self.snapshot_active(cx);
        self.pending_persist = None;
        self.entries.push(WorkspaceConfig {
            name,
            layout: WorkspaceLayout::single_pane(),
        });
        self.active = self.entries.len() - 1;
        self.replace_stage(cx);
        self.persist(cx);
        cx.notify();

        Ok(())
    }

    /// Renames the workspace called `name`.
    ///
    /// The name is the workspace's identity, so renaming the active one also
    /// moves the active marker. Renaming another one leaves the active
    /// workspace, the stage mirroring it, and the marker alone.
    ///
    /// A name that is not a workspace's is a no-op, like selecting one: the
    /// caller reads it from the same list, so an unknown name can only be stale.
    pub(crate) fn rename_entry(
        &mut self,
        name: &str,
        new_name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), WorkspaceNameError> {
        let Some(index) = self.entries.iter().position(|entry| entry.name == name) else {
            log::error!("workspace `{name}` does not exist");

            return Ok(());
        };
        // The renamed workspace may keep its own name, so it is the one name the
        // new name does not have to differ from.
        let new_name = self.validate_name(new_name, Some(self.entries[index].name.as_str()))?;
        if new_name == self.entries[index].name {
            return Ok(());
        }

        self.entries[index].name = new_name;
        self.persist(cx);
        cx.notify();

        Ok(())
    }

    /// Deletes the workspace called `name`.
    ///
    /// The last workspace cannot be deleted, because the application always shows
    /// exactly one. Deleting the active workspace activates the one before it and
    /// rebuilds the stage; deleting another one only shifts the active index when
    /// the removed entry was before it, and the stage is left in place.
    ///
    /// A name that is not a workspace's is a no-op, like renaming one.
    pub(crate) fn delete_entry(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self.can_delete() {
            log::error!("the last workspace cannot be deleted");

            return;
        }
        let Some(index) = self.entries.iter().position(|entry| entry.name == name) else {
            log::error!("workspace `{name}` does not exist");

            return;
        };

        self.pending_persist = None;
        self.entries.remove(index);
        if index == self.active {
            self.active = self.active.saturating_sub(1);
            self.replace_stage(cx);
        } else if index < self.active {
            self.active -= 1;
        }
        self.persist(cx);
        cx.notify();
    }

    fn name_is_taken(&self, name: &str, except: Option<&str>) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.name == name && Some(entry.name.as_str()) != except)
    }

    fn stage_layout_changed(&mut self, change: StageChange, cx: &mut Context<Self>) {
        self.snapshot_active(cx);
        match change {
            StageChange::Committed => self.persist(cx),
            StageChange::Adjusting => self.schedule_persist(cx),
        }
    }

    /// Copies the stage's layout back into the workspace it belongs to.
    fn snapshot_active(&mut self, cx: &App) {
        self.entries[self.active].layout = self.stage.read(cx).layout(cx);
    }

    /// Builds the stage for `layout`, and follows the layout changes it reports.
    fn build_stage(
        stage_id: u64,
        layout: &WorkspaceLayout,
        cx: &mut Context<Self>,
    ) -> (Entity<Stage>, Subscription) {
        let stage = cx.new(|cx| Stage::from_layout(stage_id, layout, cx));
        let subscription = cx.subscribe(&stage, |this, _, change, cx| {
            this.stage_layout_changed(*change, cx)
        });

        (stage, subscription)
    }

    /// Replaces the mirrored stage with one built from the active workspace.
    fn replace_stage(&mut self, cx: &mut Context<Self>) {
        let stage_id = self.next_stage_id;
        self.next_stage_id += 1;
        let (stage, stage_layout) =
            Self::build_stage(stage_id, &self.entries[self.active].layout, cx);

        self.stage = stage;
        self.stage_layout = stage_layout;
    }

    fn workspaces_config(&self) -> WorkspacesConfig {
        WorkspacesConfig {
            entries: self.entries.clone(),
            active: Some(self.active_name().clone()),
        }
    }

    /// Writes the workspaces out immediately.
    fn persist(&mut self, cx: &mut App) {
        let workspaces = self.workspaces_config();
        cx.update_global::<ConfigManager, _>(|manager, cx| {
            manager.update(|config| config.workspaces = workspaces, cx);
        });
    }

    /// Writes the workspaces out once the changes stop arriving.
    fn schedule_persist(&mut self, cx: &mut Context<Self>) {
        self.pending_persist = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PERSIST_SETTLE_DELAY).await;
            let _ = this.update(cx, |this, cx| this.persist(cx));
        }));
    }
}

impl Render for Workspaces {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.stage.clone())
    }
}

/// The default name of a workspace that does not have one yet.
fn default_workspace_name(index: usize, cx: &App) -> SmolStr {
    SmolStr::new(t!(cx, "window.workspace.default_name", index = index).as_str())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use app_config::{settings::Config, workspace::WorkspaceNameError};
    use base_ui::style::theme::Theme;
    use gpui_kit::{Bounds, ElementId, Entity, TestAppContext, point, px, size};
    use i18n::language::codes::LanguageCode;

    use super::*;
    use crate::windows::root::components::workspace::tree::{PaneNode, PaneOrientation};

    const CONFIG_FILE_NAME: &str = "config.json";

    /// A temporary directory of the test's own, under the workspace's build output.
    ///
    /// The system temporary directory is not usable here: a test that runs behind a restricted
    /// integrity token cannot create directories in `%TEMP%`, which leaves every test about a
    /// file on disk failing for a reason that has nothing to do with the code under test. The
    /// build output is the workspace's own writable scratch space, so the directory goes there.
    fn test_temp(name: &str) -> tempfile::TempDir {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("this crate lives two directories below the workspace root");
        let root = workspace.join("target").join("test-temp").join("kytrix");
        std::fs::create_dir_all(&root).expect("the test temporary directory should be created");
        tempfile::Builder::new()
            .prefix(name)
            .tempdir_in(&root)
            .expect("a temporary directory should be created")
    }

    /// A running application whose settings file lives in a temporary directory,
    /// so what the workspaces wrote can be read back the way a restart would.
    ///
    /// The entity handle is declared before the app context on purpose: fields
    /// drop in declaration order, and the context asserts that no entity is still
    /// held when it shuts down.
    struct Harness {
        _directory: tempfile::TempDir,
        path: PathBuf,
        workspaces: Entity<Workspaces>,
        cx: TestAppContext,
    }

    impl Harness {
        /// Starts with the default display language, which is the application's.
        fn start(config: &Config) -> Self {
            Self::start_in(config, LanguageCode::ZhCn)
        }

        /// Starts with `language` installed, the state `main` leaves behind after
        /// applying the persisted display language.
        fn start_in(config: &Config, language: LanguageCode) -> Self {
            let directory = test_temp("workspaces");
            let path = directory.path().join(CONFIG_FILE_NAME);
            let cx = TestAppContext::single();
            let workspaces = cx.update(|cx| {
                i18n::global::embedded::install(cx).expect("embedded locales should load");
                widget_registry::catalogue::install(cx).expect("the catalogue should install");
                cx.set_global(Theme::default());
                cx.set_global(language);

                let mut manager = ConfigManager::from_path(&path, cx)
                    .expect("a missing settings file starts from defaults");
                manager
                    .write(config.clone(), cx)
                    .expect("the injected settings should be stored");
                cx.set_global(manager);

                cx.new(Workspaces::load)
            });

            Self {
                _directory: directory,
                path,
                workspaces,
                cx,
            }
        }

        fn update<R>(
            &mut self,
            update: impl FnOnce(&mut Workspaces, &mut Context<Workspaces>) -> R,
        ) -> R {
            let workspaces = self.workspaces.clone();
            self.cx.update(move |cx| workspaces.update(cx, update))
        }

        fn names(&mut self) -> Vec<SmolStr> {
            self.update(|workspaces, _| workspaces.names().cloned().collect())
        }

        fn active_name(&mut self) -> SmolStr {
            self.update(|workspaces, _| workspaces.active_name().clone())
        }

        fn active_index(&mut self) -> usize {
            self.update(|workspaces, _| workspaces.active)
        }

        fn stage(&mut self) -> Entity<Stage> {
            self.update(|workspaces, _| workspaces.stage.clone())
        }

        fn pane_count(&mut self) -> usize {
            let stage = self.stage();
            pane_ids(&stage, &mut self.cx).len()
        }

        /// Reads the settings the way a restart would.
        fn stored(&self) -> WorkspacesConfig {
            self.cx
                .update(|cx| ConfigManager::from_path(&self.path, cx))
                .expect("the settings file should load")
                .read()
                .workspaces
                .clone()
        }
    }

    fn pane_ids(stage: &Entity<Stage>, cx: &mut TestAppContext) -> Vec<ElementId> {
        cx.update(|cx| {
            let mut ids = Vec::new();
            stage
                .read(cx)
                .pane_tree()
                .root
                .for_each_pane(&mut |pane| ids.push(pane.id.clone()));
            ids
        })
    }

    fn only_pane(stage: &Entity<Stage>, cx: &mut TestAppContext) -> ElementId {
        let mut ids = pane_ids(stage, cx);
        assert_eq!(ids.len(), 1, "the stage under test is a single pane");

        ids.remove(0)
    }

    /// The widget type the `pane_id` pane of `stage` is showing.
    fn shown_widget(stage: &Entity<Stage>, pane_id: &ElementId, cx: &mut TestAppContext) -> String {
        let pane_id = pane_id.clone();
        cx.update(|cx| {
            stage
                .read(cx)
                .pane_widget_id(&pane_id, cx)
                .expect("every pane shows a widget")
                .as_str()
                .to_owned()
        })
    }

    /// Splits a pane, which is a settled structural change.
    fn split(stage: &Entity<Stage>, pane_id: &ElementId, cx: &mut TestAppContext) {
        let pane_id = pane_id.clone();
        cx.update(|cx| {
            stage.update(cx, |stage, cx| {
                stage.split_pane(&pane_id, PaneOrientation::Horizontal, true, cx);
            });
        });
    }

    /// Gives the split's children the bounds a rendered frame would measure.
    ///
    /// Measurement is the only thing that produces bounds, so a test that resizes
    /// a split without opening a window supplies them directly.
    fn measure_split(stage: &Entity<Stage>, cx: &mut TestAppContext) {
        let measurements = cx.update(|cx| {
            let pane_tree = stage.read(cx).pane_tree();
            let PaneNode::Split {
                id: split_id,
                head,
                tail,
                ..
            } = &pane_tree.root
            else {
                panic!("the test splits a pane before measuring");
            };
            let (PaneNode::Leaf { id: head_id, .. }, PaneNode::Leaf { id: tail_id, .. }) =
                (head.as_ref(), tail.as_ref())
            else {
                panic!("the test split holds two panes");
            };

            vec![
                (
                    *split_id,
                    Bounds::new(point(px(0.0), px(0.0)), size(px(640.0), px(480.0))),
                ),
                (
                    *head_id,
                    Bounds::new(point(px(0.0), px(0.0)), size(px(318.0), px(480.0))),
                ),
                (
                    *tail_id,
                    Bounds::new(point(px(322.0), px(0.0)), size(px(318.0), px(480.0))),
                ),
            ]
        });
        cx.update(|cx| {
            stage.update(cx, |stage, _| {
                stage.pane_tree_mut().update_layout_bounds(&measurements);
            });
        });
    }

    /// Drags the split bar to `x`, which is a change that is still in progress.
    fn drag_split_bar(stage: &Entity<Stage>, cx: &mut TestAppContext, x: f32) {
        let split_id = cx.update(|cx| match &stage.read(cx).pane_tree().root {
            PaneNode::Split { id, .. } => *id,
            PaneNode::Leaf { .. } => panic!("the test splits a pane before dragging"),
        });
        cx.update(|cx| {
            stage.update(cx, |stage, cx| {
                stage.resize_split(split_id, point(px(0.0), px(0.0)), point(px(x), px(0.0)), cx);
            });
        });
    }

    #[test]
    fn a_fresh_install_should_start_with_one_workspace_named_for_the_language() {
        let mut harness = Harness::start(&Config::default());

        assert_eq!(harness.names(), [SmolStr::new_static("工作区 1")]);
        assert_eq!(
            harness.stored().active,
            Some(SmolStr::new_static("工作区 1"))
        );
        assert!(
            !harness.update(|workspaces, _| workspaces.can_delete()),
            "the only workspace cannot be deleted"
        );

        let stage = harness.stage();
        let pane = only_pane(&stage, &mut harness.cx);
        assert_eq!(
            shown_widget(&stage, &pane, &mut harness.cx),
            app_config::workspace::DEFAULT_WIDGET_ID,
            "a pane nobody has chosen a widget for shows the widget a pane falls back to"
        );
    }

    #[test]
    fn the_default_workspace_should_be_named_in_the_installed_language() {
        let mut harness = Harness::start_in(&Config::default(), LanguageCode::En);

        assert_eq!(harness.names(), [SmolStr::new_static("Workspace 1")]);
        assert_eq!(
            harness.stored().active,
            Some(SmolStr::new_static("Workspace 1"))
        );
    }

    #[test]
    fn switching_should_rebuild_the_stage_and_restore_that_workspace_layout() {
        let mut harness = Harness::start(&Config::default());
        harness
            .update(|workspaces, cx| workspaces.add("Second", cx))
            .expect("the name should be accepted");
        let second = harness.stage();
        split(
            &second,
            &only_pane(&second, &mut harness.cx),
            &mut harness.cx,
        );
        assert_eq!(harness.pane_count(), 2);

        harness.update(|workspaces, cx| workspaces.select("工作区 1", cx));

        assert_eq!(harness.active_name(), SmolStr::new_static("工作区 1"));
        assert_eq!(
            harness.pane_count(),
            1,
            "switching rebuilds the stage from the layout that workspace was left with"
        );

        harness.update(|workspaces, cx| workspaces.select("Second", cx));

        assert_eq!(
            harness.pane_count(),
            2,
            "the split workspace comes back the way it was left"
        );
    }

    #[test]
    fn each_workspace_should_keep_its_own_layout() {
        let mut harness = Harness::start(&Config::default());
        let first = harness.stage();
        split(&first, &only_pane(&first, &mut harness.cx), &mut harness.cx);
        harness
            .update(|workspaces, cx| workspaces.add("Second", cx))
            .expect("the name should be accepted");
        let second = harness.stage();
        for index in 0..2 {
            let pane = pane_ids(&second, &mut harness.cx)[index].clone();
            split(&second, &pane, &mut harness.cx);
        }
        assert_eq!(harness.pane_count(), 3);

        harness.update(|workspaces, cx| workspaces.select("工作区 1", cx));
        assert_eq!(harness.pane_count(), 2);

        harness.update(|workspaces, cx| workspaces.select("Second", cx));
        assert_eq!(harness.pane_count(), 3);
    }

    #[test]
    fn a_structural_change_should_be_persisted_immediately() {
        let mut harness = Harness::start(&Config::default());
        let before = harness.stored().entries[0].layout.clone();

        let stage = harness.stage();
        split(&stage, &only_pane(&stage, &mut harness.cx), &mut harness.cx);

        assert_ne!(
            harness.stored().entries[0].layout,
            before,
            "a split is settled, so it is written without waiting for the clock"
        );
    }

    #[test]
    fn a_resize_should_be_persisted_once_the_drag_pauses() {
        let mut harness = Harness::start(&Config::default());
        let stage = harness.stage();
        split(&stage, &only_pane(&stage, &mut harness.cx), &mut harness.cx);
        measure_split(&stage, &mut harness.cx);
        let before = harness.stored().entries[0].layout.clone();

        drag_split_bar(&stage, &mut harness.cx, 400.0);
        // Let the debounced write start waiting for the drag to pause.
        harness.cx.run_until_parked();

        assert_eq!(
            harness.stored().entries[0].layout,
            before,
            "a drag in progress is not written out"
        );

        harness.cx.executor().advance_clock(PERSIST_SETTLE_DELAY);
        harness.cx.run_until_parked();

        assert_ne!(
            harness.stored().entries[0].layout,
            before,
            "the dragged proportions are written once the drag pauses"
        );
    }

    #[test]
    fn adding_should_reject_a_taken_or_empty_name() {
        let mut harness = Harness::start(&Config::default());

        assert_eq!(
            harness.update(|workspaces, cx| workspaces.add("   ", cx)),
            Err(WorkspaceNameError::Empty)
        );
        assert_eq!(
            harness.update(|workspaces, cx| workspaces.add("工作区 1", cx)),
            Err(WorkspaceNameError::Taken)
        );
        assert_eq!(harness.names().len(), 1);
        assert_eq!(
            harness.stored().entries.len(),
            1,
            "a rejected name is never written"
        );
    }

    #[test]
    fn adding_should_switch_to_the_new_workspace() {
        let mut harness = Harness::start(&Config::default());

        harness
            .update(|workspaces, cx| workspaces.add("  Research ", cx))
            .expect("the name should be accepted");

        assert_eq!(
            harness.active_name(),
            SmolStr::new_static("Research"),
            "a stored name is trimmed"
        );
        assert_eq!(
            harness.names(),
            [
                SmolStr::new_static("工作区 1"),
                SmolStr::new_static("Research")
            ]
        );
        assert_eq!(
            harness.stored().active,
            Some(SmolStr::new_static("Research"))
        );
        assert!(harness.update(|workspaces, _| workspaces.can_delete()));
        assert_eq!(
            harness.pane_count(),
            1,
            "a new workspace starts as one pane"
        );
    }

    #[test]
    fn renaming_to_its_own_name_should_be_accepted() {
        let mut harness = Harness::start(&Config::default());
        let before = harness.stored();

        harness
            .update(|workspaces, cx| workspaces.rename_entry("工作区 1", "工作区 1", cx))
            .expect("a workspace may keep the name it has");

        assert_eq!(harness.active_name(), SmolStr::new_static("工作区 1"));
        assert_eq!(harness.stored(), before, "keeping the name writes nothing");
    }

    #[test]
    fn renaming_an_inactive_workspace_should_leave_the_active_one_alone() {
        let mut harness = Harness::start(&Config::default());
        harness
            .update(|workspaces, cx| workspaces.add("Research", cx))
            .expect("the name should be accepted");
        let stage = harness.stage();
        let pane = only_pane(&stage, &mut harness.cx);

        harness
            .update(|workspaces, cx| workspaces.rename_entry("工作区 1", "Notes", cx))
            .expect("an inactive workspace can be renamed");

        assert_eq!(
            harness.names(),
            [
                SmolStr::new_static("Notes"),
                SmolStr::new_static("Research")
            ],
            "the renamed entry keeps its place in the list"
        );
        assert_eq!(
            harness.active_name(),
            SmolStr::new_static("Research"),
            "renaming another workspace does not switch to it"
        );
        assert_eq!(
            harness.stage(),
            stage,
            "the stage mirrors the active workspace, so it is not rebuilt"
        );
        assert_eq!(
            only_pane(&harness.stage(), &mut harness.cx),
            pane,
            "the active workspace keeps the pane it was showing"
        );
        let stored = harness.stored();
        assert_eq!(
            stored
                .entries
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>(),
            [
                SmolStr::new_static("Notes"),
                SmolStr::new_static("Research")
            ],
            "the renamed workspace is written out"
        );
        assert_eq!(
            stored.active,
            Some(SmolStr::new_static("Research")),
            "the active marker still names the active workspace"
        );
    }

    #[test]
    fn renaming_the_active_workspace_should_move_the_active_marker() {
        let mut harness = Harness::start(&Config::default());

        harness
            .update(|workspaces, cx| workspaces.rename_entry("工作区 1", "Renamed", cx))
            .expect("the name should be accepted");

        assert_eq!(harness.active_name(), SmolStr::new_static("Renamed"));
        assert_eq!(harness.names(), [SmolStr::new_static("Renamed")]);
        assert_eq!(
            harness.stored(),
            WorkspacesConfig {
                entries: vec![WorkspaceConfig {
                    name: SmolStr::new_static("Renamed"),
                    layout: WorkspaceLayout::single_pane(),
                }],
                active: Some(SmolStr::new_static("Renamed")),
            },
            "the settings file reads back as the renamed workspace, with the marker on it"
        );
    }

    #[test]
    fn renaming_to_a_taken_or_empty_name_should_change_nothing() {
        let mut harness = Harness::start(&Config::default());
        harness
            .update(|workspaces, cx| workspaces.add("Second", cx))
            .expect("the name should be accepted");
        let before = harness.stored();

        assert_eq!(
            harness
                .update(|workspaces, cx| { workspaces.rename_entry("工作区 1", "Second", cx) }),
            Err(WorkspaceNameError::Taken),
            "an inactive workspace cannot take another workspace's name"
        );
        assert_eq!(
            harness.update(|workspaces, cx| { workspaces.rename_entry("Second", "   ", cx) }),
            Err(WorkspaceNameError::Empty),
            "the active workspace cannot take an empty name"
        );
        assert_eq!(harness.stored(), before, "a rejected name writes nothing");
        assert_eq!(
            harness.names(),
            [
                SmolStr::new_static("工作区 1"),
                SmolStr::new_static("Second")
            ]
        );
        assert_eq!(harness.active_name(), SmolStr::new_static("Second"));
    }

    #[test]
    fn renaming_an_unknown_workspace_should_change_nothing() {
        let mut harness = Harness::start(&Config::default());
        let before = harness.stored();

        assert_eq!(
            harness.update(|workspaces, cx| { workspaces.rename_entry("Missing", "Other", cx) }),
            Ok(()),
            "an unknown name is a no-op rather than a panic"
        );
        assert_eq!(harness.stored(), before, "an unknown name writes nothing");
        assert_eq!(harness.names(), [SmolStr::new_static("工作区 1")]);
    }

    #[test]
    fn deleting_the_only_workspace_should_be_refused() {
        let mut harness = Harness::start(&Config::default());
        let stage = harness.stage();
        let before = harness.stored();

        harness.update(|workspaces, cx| workspaces.delete_entry("工作区 1", cx));

        assert_eq!(harness.names(), [SmolStr::new_static("工作区 1")]);
        assert_eq!(harness.stage(), stage);
        assert_eq!(harness.pane_count(), 1);
        assert_eq!(
            harness.stored(),
            before,
            "a refused deletion writes nothing"
        );
    }

    #[test]
    fn deleting_an_inactive_workspace_should_keep_the_active_one_and_its_stage() {
        let mut harness = Harness::start(&Config::default());
        harness
            .update(|workspaces, cx| workspaces.add("Second", cx))
            .expect("the name should be accepted");
        harness
            .update(|workspaces, cx| workspaces.add("Third", cx))
            .expect("the name should be accepted");
        harness.update(|workspaces, cx| workspaces.select("Second", cx));
        let stage = harness.stage();
        let pane = only_pane(&stage, &mut harness.cx);
        split(&stage, &pane, &mut harness.cx);
        let panes = pane_ids(&stage, &mut harness.cx);

        harness.update(|workspaces, cx| workspaces.delete_entry("Third", cx));

        assert_eq!(
            harness.names(),
            [
                SmolStr::new_static("工作区 1"),
                SmolStr::new_static("Second")
            ]
        );
        assert_eq!(harness.active_name(), SmolStr::new_static("Second"));
        assert_eq!(
            harness.active_index(),
            1,
            "the removed entry was after the active one, so the index does not move"
        );
        assert_eq!(
            harness.stage(),
            stage,
            "the stage mirrors the active workspace, which was not deleted"
        );
        assert_eq!(
            pane_ids(&harness.stage(), &mut harness.cx),
            panes,
            "the layout the active workspace was showing is still on screen"
        );
        let stored = harness.stored();
        assert_eq!(stored.entries.len(), 2);
        assert_eq!(stored.active, Some(SmolStr::new_static("Second")));
    }

    #[test]
    fn deleting_a_workspace_before_the_active_one_should_shift_the_active_index() {
        let mut harness = Harness::start(&Config::default());
        harness
            .update(|workspaces, cx| workspaces.add("Second", cx))
            .expect("the name should be accepted");
        harness
            .update(|workspaces, cx| workspaces.add("Third", cx))
            .expect("the name should be accepted");
        let stage = harness.stage();

        harness.update(|workspaces, cx| workspaces.delete_entry("工作区 1", cx));

        assert_eq!(
            harness.names(),
            [SmolStr::new_static("Second"), SmolStr::new_static("Third")]
        );
        assert_eq!(
            harness.active_index(),
            1,
            "the entries after the removed one move down, so the index follows"
        );
        assert_eq!(
            harness.active_name(),
            SmolStr::new_static("Third"),
            "the active workspace is the same one, now at another index"
        );
        assert_eq!(harness.stage(), stage, "the active stage is not rebuilt");
        assert_eq!(harness.stored().active, Some(SmolStr::new_static("Third")));
    }

    #[test]
    fn deleting_the_active_workspace_should_activate_the_previous_one() {
        let mut harness = Harness::start(&Config::default());
        harness
            .update(|workspaces, cx| workspaces.add("Second", cx))
            .expect("the name should be accepted");
        harness
            .update(|workspaces, cx| workspaces.add("Third", cx))
            .expect("the name should be accepted");
        let deleted = harness.stage();
        split(
            &deleted,
            &only_pane(&deleted, &mut harness.cx),
            &mut harness.cx,
        );

        harness.update(|workspaces, cx| workspaces.delete_entry("Third", cx));

        assert_eq!(
            harness.names(),
            [
                SmolStr::new_static("工作区 1"),
                SmolStr::new_static("Second")
            ]
        );
        assert_eq!(harness.active_name(), SmolStr::new_static("Second"));
        assert_eq!(harness.active_index(), 1);
        assert_ne!(
            harness.stage(),
            deleted,
            "the deleted workspace's stage is replaced by one built for the previous workspace"
        );
        assert_eq!(
            harness.pane_count(),
            1,
            "Second is restored from its own single-pane layout, not the deleted split"
        );
        assert_eq!(harness.stored().active, Some(SmolStr::new_static("Second")));

        harness.update(|workspaces, cx| workspaces.delete_entry("Second", cx));

        assert_eq!(
            harness.active_name(),
            SmolStr::new_static("工作区 1"),
            "deleting the first workspace activates the one that takes its place"
        );
    }

    #[test]
    fn the_deleted_workspace_should_be_dropped() {
        let mut harness = Harness::start(&Config::default());
        harness
            .update(|workspaces, cx| workspaces.add("Second", cx))
            .expect("the name should be accepted");
        let doomed = harness.stage().downgrade();

        harness.update(|workspaces, cx| workspaces.delete_entry("Second", cx));
        harness.cx.run_until_parked();

        assert!(
            doomed.upgrade().is_none(),
            "the deleted workspace's stage, its panes and its widgets are released"
        );
    }

    #[test]
    fn deleting_an_unknown_workspace_should_change_nothing() {
        let mut harness = Harness::start(&Config::default());
        harness
            .update(|workspaces, cx| workspaces.add("Second", cx))
            .expect("the name should be accepted");
        let stage = harness.stage();
        let before = harness.stored();

        harness.update(|workspaces, cx| workspaces.delete_entry("Missing", cx));

        assert_eq!(
            harness.names(),
            [
                SmolStr::new_static("工作区 1"),
                SmolStr::new_static("Second")
            ]
        );
        assert_eq!(harness.active_name(), SmolStr::new_static("Second"));
        assert_eq!(harness.stage(), stage);
        assert_eq!(harness.stored(), before, "an unknown name writes nothing");
    }

    #[test]
    fn the_suggested_name_should_be_the_first_unused_default_name() {
        let mut harness = Harness::start(&Config::default());

        assert_eq!(
            harness.update(|workspaces, cx| workspaces.suggested_name(cx)),
            "工作区 2"
        );

        harness
            .update(|workspaces, cx| workspaces.add("工作区 2", cx))
            .expect("the name should be accepted");
        assert_eq!(
            harness.update(|workspaces, cx| workspaces.suggested_name(cx)),
            "工作区 3"
        );

        harness
            .update(|workspaces, cx| workspaces.add("Research", cx))
            .expect("the name should be accepted");
        assert_eq!(
            harness.update(|workspaces, cx| workspaces.suggested_name(cx)),
            "工作区 3",
            "a workspace that is not named by default does not take a default name"
        );
    }
}

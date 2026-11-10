//! The page area of the main window: the workspace and the settings page.
//!
//! The two are peers. The sidebar's settings entry switches to the settings page at once, and the
//! workspace list asks for the workspace again — a press on any of its rows, and an add or a
//! delete that leaves another workspace active. Neither switch is animated: they are two pages of
//! the window, not a surface moving over one.
//!
//! The workspace keeps what it was showing while the settings page is drawn. Nothing is rebuilt
//! and no widget is reloaded when the user comes back; only the workspace's own page — the
//! widget gallery — is taken away, because it belongs to a workspace that is no longer being
//! shown.
//!
//! The settings page has no cancel and no confirm button: every control writes the change it makes
//! at once, so leaving the page keeps everything that was stored. What is left is a text field the
//! user typed into without leaving it, and that is written before the page goes; a write that
//! failed does not keep the user on a page they asked to leave, so the page goes and the window
//! reports what could not be stored.

use gpui_kit::{AppContext as _, Context, Entity, IntoElement, Render, Subscription, Window};
use settings_ui::settings::{SettingsPage, show_write_failure};

use super::workspace::{Workspaces, WorkspacesEvent};

/// The page the window is showing in its page area.
enum MainPage {
    /// The active workspace, with its panes and its own page.
    Workspace,
    /// The settings of the application.
    Settings(Entity<SettingsPage>),
}

/// The workspace and the settings page, one of which fills the window's page area.
pub(crate) struct MainPages {
    /// The workspace the window shows, kept for as long as the window lives.
    workspaces: Entity<Workspaces>,
    page: MainPage,
    /// Why the settings page could not store what was still typed into it when it was left.
    ///
    /// The page is left regardless, so the reason is carried here and reported over whatever is
    /// drawn next: the field it came from is what the user lost by leaving.
    pending_write_failure: Option<String>,
    /// Follows the workspace list, so a selected workspace takes the page area back from the
    /// settings page.
    _workspaces: Subscription,
}

impl MainPages {
    pub(crate) fn new(workspaces: Entity<Workspaces>, cx: &mut Context<Self>) -> Self {
        let workspaces_events = cx.subscribe(&workspaces, |pages, _, _: &WorkspacesEvent, cx| {
            pages.show_workspace(cx)
        });

        Self {
            workspaces,
            page: MainPage::Workspace,
            pending_write_failure: None,
            _workspaces: workspaces_events,
        }
    }

    /// The workspace list the sidebar draws its rows from.
    pub(crate) fn workspaces(&self) -> &Entity<Workspaces> {
        &self.workspaces
    }

    /// Shows the settings page, opened over the settings currently stored.
    ///
    /// The workspace's own open page is taken away first: the gallery belongs to a workspace
    /// the user is leaving, and a choice in it has no pane to land on while the settings are
    /// shown. A press while the settings are already shown does nothing, so the page the
    /// user is working in is not replaced by a fresh one.
    pub(crate) fn show_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.page, MainPage::Settings(_)) {
            return;
        }

        self.workspaces
            .update(cx, |workspaces, cx| workspaces.close_page(cx));
        // The workspace is no longer drawn, but its entities are still alive and could still
        // hold the caret — a search field in a widget, for instance. The settings page does not
        // take the caret for itself, so the caret of the page being covered is dropped here;
        // otherwise the next keystroke would reach a field the user cannot see.
        window.blur(cx);
        let page = cx.new(SettingsPage::load);
        self.page = MainPage::Settings(page);
        cx.notify();
    }

    /// Shows the workspace again, ending the settings page if one was open.
    ///
    /// What is still typed into a text field of the settings page is written before it goes. The
    /// page is left whether or not that write landed: a user who asked for the workspace is not
    /// held on the settings page, and what could not be stored is reported over the workspace. A
    /// reason that was already waiting to be reported is not dropped by a leave that stored
    /// everything: the frame that reports it may not have been drawn yet.
    fn show_workspace(&mut self, cx: &mut Context<Self>) {
        let MainPage::Settings(page) = &self.page else {
            return;
        };
        let page = page.clone();
        if let Some(reason) = page.update(cx, |page, cx| page.commit_pending(cx)) {
            self.pending_write_failure = Some(reason);
        }
        self.page = MainPage::Workspace;
        cx.notify();
    }
}

impl Render for MainPages {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A page that was left with a write that failed reports it over what is drawn now: the
        // settings page it came from is gone, and the input it came from with it.
        if let Some(reason) = self.pending_write_failure.take() {
            show_write_failure(window, cx, reason);
        }

        match &self.page {
            MainPage::Workspace => self.workspaces.clone().into_any_element(),
            MainPage::Settings(page) => page.clone().into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, time::Duration};

    use app_config::settings::{Appearance, Config};
    use app_config_provider::manager::ConfigManager;
    use base_ui::{layer::Layer, style};
    use gpui_kit::{
        App, AppContext as _, Context, ElementId, Entity, FocusHandle, IntoElement, Modifiers,
        ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext, Window, div,
        px, size,
    };
    use i18n::language::codes::LanguageCode;
    use tempfile::TempDir;

    use super::MainPages;
    use crate::windows::root::components::workspace::{
        PAGE_TRANSITION_DURATION, Stage, Workspaces,
    };

    const PANE_BODY: &str = "PANE_BODY";
    const SETTINGS_PAGE: &str = "SETTINGS_PAGE";
    const GALLERY: &str = "WIDGET_GALLERY";
    /// The appearance page's select, and the entry of it that pins the dark appearance. Both are
    /// the page's own selectors, so what the test changes is a control, not the settings.
    const APPEARANCE: &str = "SETTINGS_CONTROL_appearance";
    const APPEARANCE_DARK: &str = "TEST_MENU_ITEM_Dark";
    /// The OKX page and its API key, a text field: what is typed into it is only written when the
    /// caret leaves it or the page is left.
    const OKX_ENTRY: &str = "SETTINGS_ENTRY_market_participants.okx";
    const OKX_API_KEY: &str = "SETTINGS_CONTROL_market_participants.okx.api_key";
    const ALERT: &str = "BASE_UI_ALERT";
    const CONFIG_FILE_NAME: &str = "config.json";

    /// A window root drawing the page area inside the window's layer, which is how the root window
    /// mounts it.
    ///
    /// The layer is the window's rather than the page area's: a write that could not be stored is
    /// reported by an alert, and an alert is opened on the layer the window was built with.
    struct Host {
        pages: Entity<MainPages>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.pages.clone())
        }
    }

    /// A temporary directory of the test's own, under the workspace's build output.
    ///
    /// The system temporary directory is not usable here: a test that runs behind a restricted
    /// integrity token cannot create directories in `%TEMP%`, which leaves every test about a
    /// file on disk failing for a reason that has nothing to do with the code under test. The
    /// build output is the workspace's own writable scratch space, so the directory goes there.
    fn test_temp(name: &str) -> TempDir {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("this crate lives two directories below the workspace root");
        let root = workspace.join("target").join("test-temp").join("kytrix");
        fs::create_dir_all(&root).expect("the test temporary directory should be created");
        tempfile::Builder::new()
            .prefix(name)
            .tempdir_in(&root)
            .expect("a temporary directory should be created")
    }

    /// Opens a window over a fresh workspace and the page area that shows it.
    fn harness(
        cx: &mut TestAppContext,
    ) -> (
        Entity<MainPages>,
        Entity<Workspaces>,
        &mut VisualTestContext,
    ) {
        harness_over(cx, |cx| ConfigManager::in_memory(Config::default(), cx))
    }

    /// Opens a window whose settings are the ones `manager` stores.
    fn harness_over(
        cx: &mut TestAppContext,
        build_manager: impl FnOnce(&mut App) -> ConfigManager,
    ) -> (
        Entity<MainPages>,
        Entity<Workspaces>,
        &mut VisualTestContext,
    ) {
        cx.update(|cx| {
            gpui_kit::base::init(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            widget_registry::catalogue::install(cx).expect("the catalogue should install");
            style::theme::install(cx);
            cx.set_global(LanguageCode::En);
            let manager = build_manager(cx);
            cx.set_global(manager);
        });
        let workspaces = cx.update(|cx| cx.new(Workspaces::load));
        let pages = cx.update(|cx| cx.new(|cx| MainPages::new(workspaces.clone(), cx)));
        let (_, cx) = cx.add_window_view({
            let pages = pages.clone();
            move |_, cx| Layer::new(cx.new(|_| Host { pages }))
        });
        cx.simulate_resize(size(px(1280.0), px(960.0)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        (pages, workspaces, cx)
    }

    /// Lets a page transition finish and repaints, so what is asserted is the settled frame.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(PAGE_TRANSITION_DURATION + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the window should draw {selector}"))
            .center();
        cx.simulate_click(target, Modifiers::none());
        settle(cx);
    }

    /// Opens the settings page over the workspace, the way the sidebar's settings entry does.
    fn open_settings(pages: &Entity<MainPages>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            pages.update(cx, |pages, cx| pages.show_settings(window, cx));
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// Presses the row of the workspace that is active, which is what asks for the workspace
    /// again. This harness mounts the page area alone, so the press the row makes is made here.
    fn press_the_active_workspace_row(workspaces: &Entity<Workspaces>, cx: &mut VisualTestContext) {
        cx.update(|_, cx| {
            let active = workspaces.read(cx).active_name().clone();
            workspaces.update(cx, |workspaces, cx| workspaces.select(&active, cx));
        });
        settle(cx);
    }

    /// The appearance the settings hold, which is what every frame after the change reads.
    fn stored_appearance(cx: &VisualTestContext) -> Appearance {
        cx.read(|cx| cx.global::<ConfigManager>().read().appearance)
    }

    /// A window whose settings cannot be stored: the config directory is a file, so every write
    /// fails the way an unwritable config directory does.
    fn harness_behind_a_blocking_file(
        cx: &mut TestAppContext,
    ) -> (
        TempDir,
        Entity<MainPages>,
        Entity<Workspaces>,
        &mut VisualTestContext,
    ) {
        let directory = test_temp("pages-behind-a-blocking-file");
        let blocking = directory.path().join("settings");
        fs::write(&blocking, b"block the config directory")
            .expect("the blocking file should be written");
        let path = blocking.join(CONFIG_FILE_NAME);
        let (pages, workspaces, cx) = harness_over(cx, move |cx| {
            ConfigManager::from_path(path, cx)
                .expect("a missing settings file starts from defaults")
        });

        (directory, pages, workspaces, cx)
    }

    /// Opens the OKX page and types into its API key without leaving the field: what was typed is
    /// only in the field, which is what leaving the page has to carry.
    fn type_into_the_okx_api_key_without_leaving(cx: &mut VisualTestContext) {
        click(cx, OKX_ENTRY);
        click(cx, OKX_API_KEY);
        cx.simulate_input("x");
        cx.run_until_parked();
    }

    /// A focus handle a test can put the window's caret on, standing in for a field in the
    /// workspace.
    struct FocusProbe {
        handle: FocusHandle,
    }

    impl Render for FocusProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn pane_id(stage: &Entity<Stage>, cx: &mut VisualTestContext) -> ElementId {
        cx.update(|_, cx| {
            let mut ids = Vec::with_capacity(1);
            stage
                .read(cx)
                .pane_tree()
                .root()
                .for_each_pane(&mut |pane| ids.push(pane.id.clone()));
            ids.pop().expect("a single pane workspace holds one pane")
        })
    }

    /// The settings page takes the page area at once, and a workspace row gives it back to the
    /// workspace the list names. The stage is the same one throughout: showing the settings is
    /// not rebuilding the workspace.
    #[gpui_kit::test]
    fn showing_the_settings_should_replace_the_workspace_and_give_it_back(cx: &mut TestAppContext) {
        let (pages, workspaces, cx) = harness(cx);
        let stage = cx.update(|_, cx| workspaces.read(cx).stage().clone());

        assert!(
            cx.debug_bounds(PANE_BODY).is_some(),
            "the workspace is the page the window opens on"
        );
        assert!(cx.debug_bounds(SETTINGS_PAGE).is_none());

        open_settings(&pages, cx);

        assert!(
            cx.debug_bounds(SETTINGS_PAGE).is_some(),
            "the settings page should take the page area"
        );
        assert!(
            cx.debug_bounds(PANE_BODY).is_none(),
            "the workspace should not be drawn behind the settings page"
        );

        press_the_active_workspace_row(&workspaces, cx);

        assert!(
            cx.debug_bounds(SETTINGS_PAGE).is_none(),
            "a workspace row should give the page area back to the workspace"
        );
        assert!(
            cx.debug_bounds(PANE_BODY).is_some(),
            "the workspace should be drawn again"
        );
        assert_eq!(
            cx.update(|_, cx| workspaces.read(cx).stage().clone()),
            stage,
            "showing the settings and coming back must not rebuild the stage"
        );
    }

    /// The workspace is not drawn behind the settings page, so its focus goes with it: a field
    /// the user left the caret in must not take the keystrokes typed on the settings page.
    #[gpui_kit::test]
    fn showing_the_settings_should_drop_the_workspaces_focus(cx: &mut TestAppContext) {
        let (pages, _, cx) = harness(cx);
        let probe = cx.update(|_, cx| {
            cx.new(|cx| FocusProbe {
                handle: cx.focus_handle(),
            })
        });
        let handle = cx.update(|_, cx| probe.read(cx).handle.clone());
        cx.update(|window, cx| handle.focus(window, cx));
        assert!(
            cx.update(|window, _| handle.is_focused(window)),
            "the test should start with the workspace holding the caret"
        );

        cx.update(|window, cx| {
            pages.update(cx, |pages, cx| pages.show_settings(window, cx));
        });

        assert!(
            cx.update(|window, _| !handle.is_focused(window)),
            "switching to the settings page should take the caret out of the hidden workspace"
        );
    }

    /// Leaving the settings page ends it: coming back opens a page over the settings stored now
    /// rather than reviving the page that was left.
    #[gpui_kit::test]
    fn leaving_the_settings_should_open_a_fresh_page(cx: &mut TestAppContext) {
        let (pages, _, cx) = harness(cx);
        open_settings(&pages, cx);
        let first = cx.update(|_, cx| match &pages.read(cx).page {
            super::MainPage::Settings(page) => page.entity_id(),
            super::MainPage::Workspace => panic!("the settings page should be open"),
        });

        cx.update(|_, cx| pages.update(cx, |pages, cx| pages.show_workspace(cx)));
        open_settings(&pages, cx);
        let second = cx.update(|_, cx| match &pages.read(cx).page {
            super::MainPage::Settings(page) => page.entity_id(),
            super::MainPage::Workspace => panic!("the settings page should be open again"),
        });

        assert_ne!(
            first, second,
            "the settings page opened after leaving must be a new one"
        );
    }

    /// A press on the settings entry while the settings are already shown does nothing: the page
    /// the user is working in is not replaced by a fresh one.
    #[gpui_kit::test]
    fn asking_for_the_settings_twice_should_keep_the_same_page(cx: &mut TestAppContext) {
        let (pages, _, cx) = harness(cx);

        open_settings(&pages, cx);
        let first = cx.update(|_, cx| match &pages.read(cx).page {
            super::MainPage::Settings(page) => page.entity_id(),
            super::MainPage::Workspace => panic!("the settings page should be open"),
        });
        open_settings(&pages, cx);
        let second = cx.update(|_, cx| match &pages.read(cx).page {
            super::MainPage::Settings(page) => page.entity_id(),
            super::MainPage::Workspace => panic!("the settings page should be open"),
        });

        assert_eq!(
            first, second,
            "a second press on the settings entry must not replace the page that is open"
        );
    }

    /// The workspace's own page belongs to the workspace that is no longer shown: switching to
    /// the settings closes the gallery, and coming back does not bring it back.
    #[gpui_kit::test]
    fn switching_to_the_settings_should_close_the_workspace_gallery(cx: &mut TestAppContext) {
        let (pages, workspaces, cx) = harness(cx);
        let stage = cx.update(|_, cx| workspaces.read(cx).stage().clone());
        let pane = pane_id(&stage, cx);
        cx.update(|window, cx| {
            stage.update(cx, |stage, cx| {
                stage.open_widget_gallery(&pane, window, cx);
            });
        });
        settle(cx);
        assert!(
            cx.debug_bounds(GALLERY).is_some(),
            "the gallery should be open over the workspace"
        );

        open_settings(&pages, cx);
        assert!(
            cx.debug_bounds(SETTINGS_PAGE).is_some(),
            "the settings page should take the page area"
        );
        assert!(
            cx.debug_bounds(GALLERY).is_none(),
            "the gallery belongs to the workspace that is no longer shown"
        );

        press_the_active_workspace_row(&workspaces, cx);
        assert!(
            cx.debug_bounds(GALLERY).is_none(),
            "coming back to the workspace must not restore a gallery that was closed"
        );
    }

    /// A control writes what it is changed to at once, and leaving the page keeps it: the
    /// workspace comes back drawn over the settings the choice stored.
    #[gpui_kit::test]
    fn leaving_the_settings_should_keep_what_was_changed(cx: &mut TestAppContext) {
        let (pages, workspaces, cx) = harness(cx);

        open_settings(&pages, cx);
        click(cx, APPEARANCE);
        click(cx, APPEARANCE_DARK);
        assert_eq!(
            stored_appearance(cx),
            Appearance::Dark,
            "choosing an appearance should store it at once"
        );

        press_the_active_workspace_row(&workspaces, cx);

        assert!(
            cx.debug_bounds(SETTINGS_PAGE).is_none(),
            "the workspace row should take the settings page away"
        );
        assert!(
            cx.debug_bounds(PANE_BODY).is_some(),
            "the workspace should be drawn again"
        );
        assert_eq!(
            stored_appearance(cx),
            Appearance::Dark,
            "leaving the settings must keep what a control stored"
        );
    }

    /// A text field that could not be stored when the page was left does not keep the user on the
    /// page: the workspace comes back, the field goes with the page, and the reason is reported
    /// over the workspace.
    #[gpui_kit::test]
    fn a_failed_pending_write_should_leave_and_report_on_the_workspace(cx: &mut TestAppContext) {
        let (_directory, pages, workspaces, cx) = harness_behind_a_blocking_file(cx);

        open_settings(&pages, cx);
        type_into_the_okx_api_key_without_leaving(cx);
        press_the_active_workspace_row(&workspaces, cx);
        settle(cx);

        assert!(
            cx.debug_bounds(SETTINGS_PAGE).is_none(),
            "a write that could not be stored must not keep the settings page open"
        );
        assert!(
            cx.debug_bounds(PANE_BODY).is_some(),
            "the workspace the row asked for should be shown"
        );
        assert!(
            cx.debug_bounds(ALERT).is_some(),
            "what could not be stored should be reported over the workspace"
        );
    }

    /// A reason waiting to be reported is not lost when the settings are opened and left again
    /// before the frame that would report it: what is reported is the write that failed.
    #[gpui_kit::test]
    fn a_failure_waiting_to_be_reported_should_survive_leaving_the_settings_again(
        cx: &mut TestAppContext,
    ) {
        let (_directory, pages, _workspaces, cx) = harness_behind_a_blocking_file(cx);

        open_settings(&pages, cx);
        type_into_the_okx_api_key_without_leaving(cx);

        // One frame's worth of state: the first leave fails to store the field, and the settings
        // are opened and left again before anything was drawn.
        cx.update(|window, cx| {
            pages.update(cx, |pages, cx| pages.show_workspace(cx));
            assert!(
                pages.read(cx).pending_write_failure.is_some(),
                "the write that failed should be waiting to be reported"
            );

            pages.update(cx, |pages, cx| pages.show_settings(window, cx));
            pages.update(cx, |pages, cx| pages.show_workspace(cx));
            assert!(
                pages.read(cx).pending_write_failure.is_some(),
                "leaving a page with nothing to store must not drop the reason that was waiting"
            );
        });
        settle(cx);

        assert!(
            cx.debug_bounds(SETTINGS_PAGE).is_none(),
            "the settings page should be gone"
        );
        assert!(
            cx.debug_bounds(PANE_BODY).is_some(),
            "the workspace should be shown"
        );
        assert!(
            cx.debug_bounds(ALERT).is_some(),
            "the reason the first leave carried should still be reported"
        );
    }

    /// The workspace list is the navigation signal, not just a row press: an add and a delete
    /// that leaves another workspace active both activate a workspace, and the page area follows
    /// them off the settings page the way it follows a row.
    #[gpui_kit::test]
    fn add_or_delete_of_the_active_workspace_should_leave_the_settings_page(
        cx: &mut TestAppContext,
    ) {
        let (pages, workspaces, cx) = harness(cx);
        let name = cx.update(|_, cx| workspaces.read(cx).suggested_name(cx));

        open_settings(&pages, cx);
        assert!(
            cx.debug_bounds(SETTINGS_PAGE).is_some(),
            "the settings page should be open before the workspace is added"
        );
        cx.update(|_, cx| {
            workspaces
                .update(cx, |workspaces, cx| workspaces.add(name.as_str(), cx))
                .expect("the suggested name should be a free one");
        });
        settle(cx);
        assert!(
            cx.debug_bounds(SETTINGS_PAGE).is_none(),
            "adding a workspace should show the workspace it activates"
        );
        assert!(
            cx.debug_bounds(PANE_BODY).is_some(),
            "the added workspace should be drawn"
        );

        open_settings(&pages, cx);
        assert!(
            cx.debug_bounds(SETTINGS_PAGE).is_some(),
            "the settings page should be open before the workspace is deleted"
        );
        cx.update(|_, cx| {
            let active = workspaces.read(cx).active_name().clone();
            workspaces.update(cx, |workspaces, cx| {
                workspaces.delete_entry(active.as_str(), cx);
            });
        });
        settle(cx);
        assert!(
            cx.debug_bounds(SETTINGS_PAGE).is_none(),
            "deleting the active workspace should show the workspace that follows it"
        );
        assert!(
            cx.debug_bounds(PANE_BODY).is_some(),
            "the workspace that follows the deleted one should be drawn"
        );
    }
}

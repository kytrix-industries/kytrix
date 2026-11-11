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
//! at once, and a dialog writes its draft only when it is confirmed, so leaving the page keeps
//! everything that was stored. A write that failed does not keep the user on a page they asked to
//! leave: the page goes and the window reports what could not be stored.

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
    /// Why a write the settings page started could not be stored, when the page was left
    /// before its own `Render` reported the failure.
    ///
    /// The producer is the settings page's write path: a write it made and storage refused, on
    /// a page the user left before the frame that would have shown the failure. The page is
    /// left regardless, so the reason is carried here and reported over whatever is drawn next:
    /// the settings page that could have reported it is gone.
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
    /// This is where a write the settings page started, and storage refused before the page's
    /// own `Render` reported it, is carried out of the page. The page is left either way: a user
    /// who asked for the workspace is not held on the settings page, and what could not be
    /// stored is reported over the workspace. A reason that was already waiting to be reported
    /// is not dropped by a leave that stored everything: the frame that reports it may not have
    /// been drawn yet.
    fn show_workspace(&mut self, cx: &mut Context<Self>) {
        let MainPage::Settings(page) = &self.page else {
            return;
        };
        let page = page.clone();
        if let Some(reason) = page.update(cx, |page, cx| page.take_write_failure(cx)) {
            self.pending_write_failure = Some(reason);
        }
        self.page = MainPage::Workspace;
        cx.notify();
    }
}

impl Render for MainPages {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A page that was left with a write that failed reports it over what is drawn now: the
        // settings page it came from is gone.
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
    use std::time::Duration;

    use app_config::settings::{Appearance, Config};
    use app_config_provider::manager::ConfigManager;
    use base_ui::{layer::Layer, style};
    use gpui_kit::{
        AppContext as _, Context, ElementId, Entity, FocusHandle, IntoElement, Modifiers,
        ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext, Window, div,
        px, size,
    };
    use i18n::language::codes::LanguageCode;

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

    /// Opens a window over a fresh workspace and the page area that shows it.
    fn harness(
        cx: &mut TestAppContext,
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
            let manager = ConfigManager::in_memory(Config::default(), cx);
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

    /// A write that failed on the settings page but was never reported by its own frame is not
    /// dropped when the page is left: the window reports it over the workspace.
    ///
    /// The handover is covered in two halves: settings_ui's own test covers what
    /// `take_write_failure` hands over, and this one covers what the window does with it after
    /// `show_workspace` — reports it through Render, and consumes it once.
    #[gpui_kit::test]
    fn a_write_failure_carried_out_of_the_settings_page_is_reported_over_the_workspace(
        cx: &mut TestAppContext,
    ) {
        let (pages, _, cx) = harness(cx);
        open_settings(&pages, cx);
        // What `show_workspace` carries out of a page whose render never reported the failure:
        // the page is left before the frame that would have shown it, so nothing else knows.
        cx.update(|_, cx| {
            pages.update(cx, |pages, cx| {
                pages.pending_write_failure = Some("the settings could not be stored".to_owned());
                pages.show_workspace(cx);
            });
        });

        settle(cx);

        assert!(
            cx.debug_bounds("BASE_UI_ALERT").is_some(),
            "the failure carried out of the settings page should be reported"
        );
        assert!(
            cx.debug_bounds(PANE_BODY).is_some(),
            "the workspace is what is drawn while the failure is reported"
        );
        assert!(
            cx.update(|_, cx| pages.read(cx).pending_write_failure.is_none()),
            "the reason is reported once rather than carried into every frame"
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

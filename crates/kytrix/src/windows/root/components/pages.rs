//! The page area of the main window: the workspace and the settings page.
//!
//! The two are peers. The sidebar's settings entry switches to the settings page at once, and a
//! press on any workspace row switches back to the workspace the list names. Neither switch is
//! animated: they are two pages of the window, not a surface moving over one.
//!
//! The workspace keeps what it was showing while the settings page is drawn. Nothing is rebuilt
//! and no widget is reloaded when the user comes back; only the workspace's own page — the
//! widget gallery — is taken away, because it belongs to a workspace that is no longer being
//! shown.
//!
//! The settings page is a session of its own: it starts from the stored settings when it opens,
//! and leaving it — by either of its buttons or by choosing a workspace — ends that session.

use gpui_kit::{
    AppContext as _, Context, DismissEvent, Entity, IntoElement, Render, Subscription, Window,
};
use settings_ui::settings::SettingsPage;

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
    /// Follows the settings page while it is the page shown: either of its buttons ends the
    /// session by asking to be dismissed.
    settings_events: Option<Subscription>,
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
            settings_events: None,
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
    /// shown. A press while the settings are already shown does nothing, so the session the
    /// user is editing is not replaced by a fresh one.
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
        self.settings_events = Some(cx.subscribe(&page, |pages, _, _: &DismissEvent, cx| {
            pages.show_workspace(cx)
        }));
        self.page = MainPage::Settings(page);
        cx.notify();
    }

    /// Shows the workspace again, ending the settings session if one was open.
    ///
    /// Leaving the settings page discards what was not confirmed: the next time the page is
    /// opened it starts from the settings that were stored, not from the session that was
    /// abandoned.
    fn show_workspace(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.page, MainPage::Settings(_)) {
            return;
        }

        self.settings_events = None;
        self.page = MainPage::Workspace;
        cx.notify();
    }
}

impl Render for MainPages {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        match &self.page {
            MainPage::Workspace => self.workspaces.clone().into_any_element(),
            MainPage::Settings(page) => page.clone().into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use app_config::settings::Config;
    use app_config_provider::manager::ConfigManager;
    use base_ui::style;
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
    const SETTINGS_CONFIRM: &str = "SETTINGS_CONFIRM";
    const GALLERY: &str = "WIDGET_GALLERY";

    /// A window root drawing the page area, which is what the root window gives it.
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
            move |_, _| Host { pages }
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
        let active = cx.update(|_, cx| workspaces.read(cx).active_name().clone());

        assert!(
            cx.debug_bounds(PANE_BODY).is_some(),
            "the workspace is the page the window opens on"
        );
        assert!(cx.debug_bounds(SETTINGS_PAGE).is_none());

        cx.update(|window, cx| {
            pages.update(cx, |pages, cx| pages.show_settings(window, cx));
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(SETTINGS_PAGE).is_some(),
            "the settings page should take the page area"
        );
        assert!(
            cx.debug_bounds(PANE_BODY).is_none(),
            "the workspace should not be drawn behind the settings page"
        );

        cx.update(|_, cx| workspaces.update(cx, |workspaces, cx| workspaces.select(&active, cx)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

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

    /// Leaving the settings page ends its session: coming back builds a session from the stored
    /// settings rather than reviving the changes that were abandoned.
    #[gpui_kit::test]
    fn leaving_the_settings_should_start_a_new_session(cx: &mut TestAppContext) {
        let (pages, _, cx) = harness(cx);
        cx.update(|window, cx| {
            pages.update(cx, |pages, cx| pages.show_settings(window, cx));
        });
        let first = cx.update(|_, cx| match &pages.read(cx).page {
            super::MainPage::Settings(page) => page.entity_id(),
            super::MainPage::Workspace => panic!("the settings page should be open"),
        });

        cx.update(|_, cx| pages.update(cx, |pages, cx| pages.show_workspace(cx)));
        cx.update(|window, cx| {
            pages.update(cx, |pages, cx| pages.show_settings(window, cx));
        });
        let second = cx.update(|_, cx| match &pages.read(cx).page {
            super::MainPage::Settings(page) => page.entity_id(),
            super::MainPage::Workspace => panic!("the settings page should be open again"),
        });

        assert_ne!(
            first, second,
            "the settings page opened after leaving must be a new session"
        );
    }

    /// A press on the settings entry while the settings are already shown does nothing: the
    /// session the user is editing is not replaced by a fresh one.
    #[gpui_kit::test]
    fn asking_for_the_settings_twice_should_keep_the_same_session(cx: &mut TestAppContext) {
        let (pages, _, cx) = harness(cx);

        cx.update(|window, cx| {
            pages.update(cx, |pages, cx| pages.show_settings(window, cx));
        });
        let first = cx.update(|_, cx| match &pages.read(cx).page {
            super::MainPage::Settings(page) => page.entity_id(),
            super::MainPage::Workspace => panic!("the settings page should be open"),
        });
        cx.update(|window, cx| {
            pages.update(cx, |pages, cx| pages.show_settings(window, cx));
        });
        let second = cx.update(|_, cx| match &pages.read(cx).page {
            super::MainPage::Settings(page) => page.entity_id(),
            super::MainPage::Workspace => panic!("the settings page should be open"),
        });

        assert_eq!(
            first, second,
            "a second press on the settings entry must not replace the open session"
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

        cx.update(|window, cx| {
            pages.update(cx, |pages, cx| pages.show_settings(window, cx));
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds(SETTINGS_PAGE).is_some(),
            "the settings page should take the page area"
        );
        assert!(
            cx.debug_bounds(GALLERY).is_none(),
            "the gallery belongs to the workspace that is no longer shown"
        );

        cx.update(|_, cx| {
            workspaces.update(cx, |workspaces, cx| {
                let active = workspaces.active_name().clone();
                workspaces.select(&active, cx);
            })
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds(GALLERY).is_none(),
            "coming back to the workspace must not restore a gallery that was closed"
        );
    }

    /// Confirming the settings gives the page area back to the workspace, and so does
    /// cancelling: what was changed and what was not is the page's own session, and either
    /// button ends it.
    #[gpui_kit::test]
    fn the_settings_footer_should_return_to_the_workspace(cx: &mut TestAppContext) {
        let (pages, _, cx) = harness(cx);

        cx.update(|window, cx| {
            pages.update(cx, |pages, cx| pages.show_settings(window, cx));
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        click(cx, SETTINGS_CONFIRM);

        assert!(
            cx.debug_bounds(SETTINGS_PAGE).is_none(),
            "confirming should take the settings page away"
        );
        assert!(
            cx.debug_bounds(PANE_BODY).is_some(),
            "confirming should show the workspace again"
        );

        cx.update(|window, cx| {
            pages.update(cx, |pages, cx| pages.show_settings(window, cx));
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        click(cx, "SETTINGS_CANCEL");

        assert!(
            cx.debug_bounds(SETTINGS_PAGE).is_none(),
            "cancelling should take the settings page away"
        );
        assert!(
            cx.debug_bounds(PANE_BODY).is_some(),
            "cancelling should show the workspace again"
        );
    }
}

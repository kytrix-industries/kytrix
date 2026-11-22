//! The page area of the main window: the active workspace, or the settings page.
//!
//! The two are peers. The sidebar's settings entry switches to the settings page at once,
//! and a press on a workspace row gives the page area back to the workspace. Neither switch
//! is animated: they are two pages of the window, not a surface moving over one.
//!
//! # The workspace keeps what it was showing
//!
//! Switching to the settings page hides the stage rather than releasing it: the panes, their
//! widgets and everything they have collected are still there when the user comes back. What
//! is released is the workspace's own page (the widget gallery): it belongs to a workspace
//! the user is leaving, and a choice made in it would have no pane to land on.
//!
//! # Write failures
//!
//! The settings page reports what it could not store on the frame it draws. A page the user
//! left before that frame never reported it, so the failure is carried out of the page here
//! and reported over whatever is drawn next.

use base_ui::style_builder::StyleBuilder;
use settings_ui::settings::SettingsPage;
use smol_str::SmolStr;
use ui_core::{
    app::App,
    error::EngineError,
    event::Event,
    node::{id::NodeId, signal::Signal},
    window::WindowId,
};

use super::{
    intent::Intent,
    metrics,
    ui::{self, Intents},
    workspace::Stage,
    workspaces::Workspaces,
};
use crate::composition::Composition;

/// The page the window is showing in its page area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    /// The active workspace, with its panes and its own page.
    Workspace,
    /// The settings of the application.
    Settings,
}

/// The workspace and the settings page, one of which fills the page area.
pub(crate) struct Pages {
    window: WindowId,
    /// The page area's own node: both pages are its children.
    pub root: NodeId,
    stage: Stage,
    settings: Option<SettingsPage>,
    page: Page,
    /// A write the settings page started and the user left before it was reported.
    pending_write_failure: Option<SmolStr>,
    /// The stage's visibility (the settings page covers it while it is open).
    stage_visible: Signal<bool>,
}

impl Pages {
    /// Builds the page area with the workspace showing.
    pub(crate) fn build(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        composition: &Composition,
        intents: Intents,
        workspaces: &Workspaces,
    ) -> Result<Self, EngineError> {
        let root = ui::named(
            app,
            window,
            parent,
            "ROOT_PAGE",
            StyleBuilder::new()
                .relative()
                .flex_1()
                .min_height_0()
                .min_width_0()
                .width_full()
                .padding(metrics::PAGE_PADDING)
                .build(),
        )?;
        let stage_visible = app.signal(true);
        let stage = Stage::build(
            app,
            window,
            root,
            composition,
            intents,
            stage_visible,
            workspaces.active_layout(),
        )?;

        Ok(Self {
            window,
            root,
            stage,
            settings: None,
            page: Page::Workspace,
            pending_write_failure: None,
            stage_visible,
        })
    }

    /// The page the window is showing (the window's tests assert the switch).
    #[cfg(test)]
    pub(crate) const fn page(&self) -> Page {
        self.page
    }

    /// The workspace's stage.
    pub(crate) fn stage(&self) -> &Stage {
        &self.stage
    }

    /// Applies one workspace request to the stage, reporting whether the stored layout must
    /// follow.
    pub(crate) fn apply(
        &mut self,
        app: &mut App,
        composition: &Composition,
        intent: &Intent,
    ) -> Result<bool, EngineError> {
        if self.page != Page::Workspace {
            return Ok(false);
        }
        self.stage.apply(app, composition, intent)
    }

    /// Releases the stage and rebuilds it from `workspaces`' active layout.
    pub(crate) fn rebuild_stage(
        &mut self,
        app: &mut App,
        composition: &Composition,
        intents: Intents,
        workspaces: &Workspaces,
    ) -> Result<(), EngineError> {
        self.stage.release(app);
        self.stage = Stage::build(
            app,
            self.window,
            self.root,
            composition,
            intents,
            self.stage_visible,
            workspaces.active_layout(),
        )?;
        if self.page == Page::Workspace {
            app.write(&self.stage_visible, true);
        }

        Ok(())
    }

    /// Shows the settings page, opened over the settings currently stored.
    pub(crate) fn show_settings(
        &mut self,
        app: &mut App,
        composition: &Composition,
    ) -> Result<(), EngineError> {
        if self.page == Page::Settings {
            return Ok(());
        }
        // The workspace's own page belongs to a workspace the user is leaving.
        self.stage.close_gallery(app);
        app.write(&self.stage_visible, false);
        let host = settings_ui::host::SettingsHost::new(
            std::rc::Rc::clone(composition.services()),
            composition.settings_store.clone(),
        );
        let page = SettingsPage::mount(host, "kytrix.settings", app, self.window, self.root)?;
        self.settings = Some(page);
        self.page = Page::Settings;

        Ok(())
    }

    /// Shows the workspace again, ending the settings page if one was open.
    pub(crate) fn show_workspace(&mut self, app: &mut App) {
        let Some(mut page) = self.settings.take() else {
            return;
        };
        if let Some(reason) = page.take_write_failure() {
            self.pending_write_failure = Some(reason);
        }
        page.release(app);
        self.page = Page::Workspace;
        app.write(&self.stage_visible, true);
    }

    /// Runs one frame of the page that is showing.
    pub(crate) fn sync(
        &mut self,
        app: &mut App,
        composition: &Composition,
    ) -> Result<(), EngineError> {
        if app.window(self.window).is_none() {
            return Ok(());
        }
        match self.page {
            Page::Workspace => self.stage.sync(app, composition),
            Page::Settings => {
                if let Some(page) = &mut self.settings {
                    page.sync(app)?;
                    if let Some(reason) = page.take_write_failure() {
                        self.pending_write_failure = Some(reason);
                    }
                }

                Ok(())
            }
        }
    }

    /// Takes the write failure the window has to report over what is drawn now.
    pub(crate) fn take_write_failure(&mut self) -> Option<SmolStr> {
        self.pending_write_failure.take()
    }

    /// Forwards one platform event to the page that is showing.
    pub(crate) fn dispatch(&mut self, app: &mut App, event: &Event) {
        match self.page {
            Page::Workspace => self.stage.dispatch(app, event),
            Page::Settings => {
                if let Some(page) = &mut self.settings {
                    let _ = page.dispatch(app, event);
                }
            }
        }
    }

    /// Releases everything the page area owns.
    pub(crate) fn release(&mut self, app: &mut App) {
        if let Some(mut page) = self.settings.take() {
            page.release(app);
        }
        self.stage.release(app);
        let _ = app.remove_node(self.window, self.root);
    }
}

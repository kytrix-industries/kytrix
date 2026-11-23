use settings_ui::settings::SettingsPage;
use smol_str::SmolStr;
use ui_core::{
    app::App,
    error::EngineError,
    event::Event,
    node::{id::NodeId, signal::Signal},
    window::WindowId,
};
use ui_kit::style_builder::StyleBuilder;

use super::{
    intent::Intent,
    metrics,
    ui::{self, Intents},
    workspace::Stage,
    workspaces::Workspaces,
};
use crate::composition::Composition;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    Workspace,
    Settings,
}

pub(crate) struct Pages {
    window: WindowId,
    pub root: NodeId,
    stage: Stage,
    settings: Option<SettingsPage>,
    page: Page,
    pending_write_failure: Option<SmolStr>,
    stage_visible: Signal<bool>,
}

impl Pages {
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

    #[cfg(test)]
    pub(crate) const fn page(&self) -> Page {
        self.page
    }

    pub(crate) fn stage(&self) -> &Stage {
        &self.stage
    }

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

    pub(crate) fn show_settings(
        &mut self,
        app: &mut App,
        composition: &Composition,
    ) -> Result<(), EngineError> {
        if self.page == Page::Settings {
            return Ok(());
        }
        self.stage.close_gallery(app);
        app.write(&self.stage_visible, false);
        let host = settings_ui::host::SettingsHost::new(
            std::rc::Rc::clone(composition.services()),
            composition.settings_store.clone(),
        )
        .with_downloads(std::rc::Rc::clone(composition.translation_downloads()));
        let page = SettingsPage::mount(host, "kytrix.settings", app, self.window, self.root)?;
        self.settings = Some(page);
        self.page = Page::Settings;

        Ok(())
    }

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

    pub(crate) fn take_write_failure(&mut self) -> Option<SmolStr> {
        self.pending_write_failure.take()
    }

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

    pub(crate) fn release(&mut self, app: &mut App) {
        if let Some(mut page) = self.settings.take() {
            page.release(app);
        }
        self.stage.release(app);
        let _ = app.remove_node(self.window, self.root);
    }
}

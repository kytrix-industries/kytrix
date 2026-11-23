use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use base_ui::theme::role::Role;
use i18n::t_smol;
use smol_str::SmolStr;
use ui_core::{
    app::App,
    error::EngineError,
    event::Event,
    math::Size,
    window::{Decorations, WindowId, WindowOptions},
};
use widget::frame::RouteId;

use super::{
    dialogs::{AlertCopy, Dialogs, NameAlert},
    intent::{Intent, IntentQueue},
    metrics,
    pages::Pages,
    sidebar::Sidebar,
    title_bar::TitleBar,
    ui,
    workspaces::Workspaces,
};
use crate::composition::Composition;

pub(crate) struct Shell {
    composition: Rc<Composition>,
    window: WindowId,
    title_bar: TitleBar,
    sidebar: Sidebar,
    pages: Pages,
    workspaces: Workspaces,
    dialogs: Dialogs,
    intents: Rc<RefCell<IntentQueue>>,
    stage_dirty: Rc<Cell<bool>>,
    route: Option<RouteId>,
    closed: bool,
}

impl Shell {
    pub(crate) fn open(app: &mut App, composition: Rc<Composition>) -> Result<Self, EngineError> {
        let theme = composition.theme.clone();
        let appearance = theme.current(app);
        let background = theme.theme_for(appearance).role(Role::SurfaceCanvas);
        let window = app.create_window(main_window_options(background));
        Self::open_in(app, composition, window)
    }

    pub(crate) fn open_in(
        app: &mut App,
        composition: Rc<Composition>,
        window: WindowId,
    ) -> Result<Self, EngineError> {
        let theme = composition.theme.clone();

        let root = ui::tree(app, window)?.root();
        ui::window_backdrop(app, window, root, &theme)?;

        let stored = composition.hub.read(|config| config.workspaces.clone());
        let (workspaces, created) = Workspaces::load(&stored, composition.hub.translate());
        if created {
            let _ = composition.write_workspaces(app, workspaces.config());
        }
        let intents = ui::intents();
        let sidebar = Sidebar::build(
            app,
            window,
            root,
            &composition,
            composition.hub.read(|config| config.ui.sidebar),
            Rc::clone(&intents),
            &workspaces,
        )?;
        let page_column = ui::container(
            app,
            window,
            root,
            base_ui::style_builder::StyleBuilder::new()
                .flex_col()
                .flex_1()
                .min_width_0()
                .height_full()
                .build(),
        )?;
        ui::container(
            app,
            window,
            page_column,
            base_ui::style_builder::StyleBuilder::new()
                .width_full()
                .height(metrics::TITLE_BAR_HEIGHT)
                .flex_none()
                .build(),
        )?;
        let pages = Pages::build(
            app,
            window,
            page_column,
            &composition,
            Rc::clone(&intents),
            &workspaces,
        )?;
        let stage_dirty = pages.stage().dirty_flag();
        let title_bar = TitleBar::build(app, window, root, &composition, Rc::clone(&intents))?;
        let dialogs = Dialogs::new(window, &composition);

        Ok(Self {
            composition,
            window,
            title_bar,
            sidebar,
            pages,
            workspaces,
            dialogs,
            intents,
            stage_dirty,
            route: None,
            closed: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn workspaces(&self) -> &Workspaces {
        &self.workspaces
    }

    #[cfg(feature = "acceptance")]
    pub(crate) const fn window_id(&self) -> WindowId {
        self.window
    }

    #[cfg(test)]
    pub(crate) fn pages(&self) -> &Pages {
        &self.pages
    }

    pub(crate) fn install(
        app: &mut App,
        mut shell: Shell,
    ) -> Result<Rc<RefCell<Shell>>, EngineError> {
        let composition = Rc::clone(&shell.composition);
        if let Some(recovery) = &composition.recovery {
            let translate = composition.translate.clone();
            let title = t_smol!(translate, "startup.config_reset_title");
            let message = match &recovery.backup {
                Some(backup) => t_smol!(
                    translate,
                    "startup.config_reset_message",
                    file = backup.display()
                ),
                None => t_smol!(translate, "startup.config_reset_message_no_backup"),
            };
            let confirm = t_smol!(translate, "common.confirm");
            shell
                .dialogs
                .alert(app, &composition, title, message, confirm, |_| {});
        }

        let shell = Rc::new(RefCell::new(shell));
        let route = {
            let shell = Rc::clone(&shell);
            composition
                .frame_router
                .register(move |app| shell.borrow_mut().tick(app))
        };
        shell.borrow_mut().route = Some(route);

        Ok(shell)
    }

    fn tick(&mut self, app: &mut App) -> Result<(), EngineError> {
        if app.window(self.window).is_none() || self.closed {
            return Ok(());
        }
        let events: Vec<Event> = app
            .drain_events()
            .into_iter()
            .filter_map(|(window, event)| (window == self.window).then_some(event))
            .collect();
        for event in &events {
            self.handle_event(app, event);
        }
        if self.closed {
            return Ok(());
        }

        let intents = self.intents.borrow_mut().drain();
        for intent in intents {
            self.apply_intent(app, intent)?;
            if self.closed {
                return Ok(());
            }
        }

        self.composition.sync_catalog_generation(app);
        self.composition.sync_downloads(app);
        self.title_bar.sync(app)?;
        self.sidebar.sync(app, &self.workspaces)?;
        self.pages.sync(app, &self.composition)?;
        self.dialogs.sync(app);
        if let Some(reason) = self.pages.take_write_failure() {
            self.report_write_failure(app, reason);
        }
        self.persist_stage_if_dirty(app)?;

        Ok(())
    }

    fn handle_event(&mut self, app: &mut App, event: &Event) {
        match event {
            Event::CloseRequested => {
                self.close(app);
                return;
            }
            Event::AppearanceChanged { dark } => {
                self.composition.hub.apply_system_appearance(app, *dark);
            }
            _ => {}
        }
        self.title_bar.dispatch(app, event);
        self.sidebar.dispatch(app, event);
        self.pages.dispatch(app, event);
        self.dialogs.dispatch(app, event);
    }

    fn apply_intent(&mut self, app: &mut App, intent: Intent) -> Result<(), EngineError> {
        if let Some(control) = intent.window_control() {
            let _ = app.request_window_control(self.window, control);
            let _ = app.request_repaint(self.window);
            return Ok(());
        }
        match intent {
            Intent::CloseWindow => self.close(app),
            Intent::ToggleSidebar => {
                let mode = metrics::next_mode(self.sidebar.mode());
                self.composition.write_sidebar_mode(app, mode);
                self.sidebar.set_mode(app, mode, &self.workspaces)?;
            }
            Intent::ShowSettings => self.pages.show_settings(app, &self.composition)?,
            Intent::SelectWorkspace(name) => self.select_workspace(app, &name)?,
            Intent::AddWorkspace => self.ask_add_workspace(app),
            Intent::RenameWorkspace(name) => self.ask_rename_workspace(app, &name),
            Intent::DeleteWorkspace(name) => self.ask_delete_workspace(app, &name),
            Intent::CreateWorkspace(name) => self.create_workspace(app, name)?,
            Intent::ApplyRename { from, to } => self.rename_workspace(app, &from, &to)?,
            Intent::ConfirmDelete(name) => self.delete_workspace(app, &name)?,
            other => {
                let changed = self.pages.apply(app, &self.composition, &other)?;
                if changed {
                    self.persist_stage(app)?;
                }
            }
        }

        Ok(())
    }

    fn select_workspace(&mut self, app: &mut App, name: &SmolStr) -> Result<(), EngineError> {
        self.persist_stage(app)?;
        if !self.workspaces.select(name) {
            self.pages.show_workspace(app);
            return Ok(());
        }
        self.pages.show_workspace(app);
        self.rebuild_stage(app)?;
        let _ = self
            .composition
            .write_workspaces(app, self.workspaces.config());
        self.sidebar.rebuild(app, &self.workspaces)?;

        Ok(())
    }

    fn create_workspace(&mut self, app: &mut App, name: SmolStr) -> Result<(), EngineError> {
        self.persist_stage(app)?;
        if let Err(error) = self.workspaces.add(&name) {
            log::error!("failed to add the workspace `{name}`: {error}");
            return Ok(());
        }
        self.pages.show_workspace(app);
        self.rebuild_stage(app)?;
        let _ = self
            .composition
            .write_workspaces(app, self.workspaces.config());
        self.sidebar.rebuild(app, &self.workspaces)?;

        Ok(())
    }

    fn rename_workspace(
        &mut self,
        app: &mut App,
        from: &SmolStr,
        to: &SmolStr,
    ) -> Result<(), EngineError> {
        match self.workspaces.rename_entry(from, to) {
            Ok(true) => {
                let _ = self
                    .composition
                    .write_workspaces(app, self.workspaces.config());
                self.sidebar.rebuild(app, &self.workspaces)?;
            }
            Ok(false) => {}
            Err(error) => log::error!("failed to rename the workspace `{from}`: {error}"),
        }

        Ok(())
    }

    fn delete_workspace(&mut self, app: &mut App, name: &SmolStr) -> Result<(), EngineError> {
        self.persist_stage(app)?;
        let changed = self.workspaces.delete_entry(name);
        let _ = self
            .composition
            .write_workspaces(app, self.workspaces.config());
        if changed {
            self.rebuild_stage(app)?;
        }
        self.sidebar.rebuild(app, &self.workspaces)?;

        Ok(())
    }

    fn rebuild_stage(&mut self, app: &mut App) -> Result<(), EngineError> {
        self.pages.rebuild_stage(
            app,
            &self.composition,
            Rc::clone(&self.intents),
            &self.workspaces,
        )?;
        self.stage_dirty = self.pages.stage().dirty_flag();

        Ok(())
    }

    fn ask_add_workspace(&mut self, app: &mut App) {
        let translate = self.composition.translate.clone();
        let suggested = self.workspaces.suggested_name(&translate);
        let taken: Vec<SmolStr> = self.workspaces.names().cloned().collect();
        let alert = NameAlert {
            title: t_smol!(translate, "window.workspace.add_title"),
            message: t_smol!(translate, "window.workspace.add_message"),
            placeholder: t_smol!(translate, "window.workspace.name_placeholder"),
            initial: suggested,
            confirm: t_smol!(translate, "common.confirm"),
            cancel: t_smol!(translate, "common.cancel"),
        };
        let validate_translate = translate;
        let validate =
            move |value: &str| validate_workspace_name(value, &taken, &validate_translate);
        let intents = Rc::clone(&self.intents);
        self.dialogs.name_alert(
            app,
            &self.composition,
            alert,
            validate,
            move |value, app| {
                let Some(name) = value else {
                    return;
                };
                intents.borrow_mut().push(Intent::CreateWorkspace(name));
                let _ = app;
            },
        );
    }

    fn ask_rename_workspace(&mut self, app: &mut App, name: &SmolStr) {
        let translate = self.composition.translate.clone();
        let taken: Vec<SmolStr> = self
            .workspaces
            .names()
            .filter(|taken| *taken != name)
            .cloned()
            .collect();
        let alert = NameAlert {
            title: t_smol!(translate, "window.workspace.rename_title"),
            message: t_smol!(
                translate,
                "window.workspace.rename_message",
                name = name.as_str()
            ),
            placeholder: t_smol!(translate, "window.workspace.name_placeholder"),
            initial: name.clone(),
            confirm: t_smol!(translate, "common.confirm"),
            cancel: t_smol!(translate, "common.cancel"),
        };
        let validate_translate = translate;
        let validate =
            move |value: &str| validate_workspace_name(value, &taken, &validate_translate);
        let intents = Rc::clone(&self.intents);
        let from = name.clone();
        self.dialogs.name_alert(
            app,
            &self.composition,
            alert,
            validate,
            move |value, _app| {
                let Some(to) = value else {
                    return;
                };
                intents.borrow_mut().push(Intent::ApplyRename {
                    from: from.clone(),
                    to,
                });
            },
        );
    }

    fn ask_delete_workspace(&mut self, app: &mut App, name: &SmolStr) {
        let translate = self.composition.translate.clone();
        let title = t_smol!(translate, "window.workspace.delete_title");
        let message = t_smol!(
            translate,
            "window.workspace.delete_message",
            name = name.as_str()
        );
        let confirm = t_smol!(translate, "common.confirm");
        let cancel = t_smol!(translate, "common.cancel");
        let intents = Rc::clone(&self.intents);
        let target = name.clone();
        let on_confirm = move |_app: &mut App| {
            intents
                .borrow_mut()
                .push(Intent::ConfirmDelete(target.clone()));
        };
        self.dialogs.alert_with_cancel(
            app,
            &self.composition,
            AlertCopy {
                title,
                message,
                confirm,
                cancel,
            },
            on_confirm,
        );
    }

    fn report_write_failure(&mut self, app: &mut App, reason: SmolStr) {
        let translate = self.composition.translate.clone();
        let title = t_smol!(translate, "settings_ui.save_failed.title");
        let confirm = t_smol!(translate, "common.confirm");
        self.dialogs
            .alert(app, &self.composition, title, reason, confirm, |_| {});
    }

    fn persist_stage_if_dirty(&mut self, app: &mut App) -> Result<(), EngineError> {
        if !self.stage_dirty.get() {
            return Ok(());
        }
        self.stage_dirty.set(false);
        self.persist_stage(app)
    }

    fn persist_stage(&mut self, app: &mut App) -> Result<(), EngineError> {
        let layout = self.pages.stage().snapshot();
        self.workspaces.set_active_layout(layout);
        let _ = self
            .composition
            .write_workspaces(app, self.workspaces.config());

        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn title_bar(&self) -> &TitleBar {
        &self.title_bar
    }

    #[cfg(test)]
    pub(crate) fn close_for_test(&mut self, app: &mut App) {
        self.close(app);
    }

    fn close(&mut self, app: &mut App) {
        if self.closed {
            return;
        }
        self.closed = true;
        if let Some(route) = self.route.take() {
            self.composition.frame_router.unregister(route);
        }
        self.persist_stage(app).ok();
        self.dialogs.release(app);
        self.title_bar.release(app);
        self.sidebar.release(app);
        self.pages.release(app);
        app.close_window(self.window);
    }
}

pub(crate) fn main_window_options(background: ui_core::paint::color::Color) -> WindowOptions {
    WindowOptions {
        title: SmolStr::new_static("Kytrix"),
        size: Size::new(1280.0, 960.0),
        min_size: Some(Size::new(800.0, 600.0)),
        decorations: if cfg!(target_os = "windows") {
            Decorations::None
        } else {
            Decorations::Native
        },
        background,
        ..WindowOptions::default()
    }
}

fn validate_workspace_name(
    value: &str,
    taken: &[SmolStr],
    translate: &i18n::translate::Translate,
) -> Result<(), SmolStr> {
    let required = t_smol!(translate, "window.workspace.name_required");
    let exists = t_smol!(translate, "window.workspace.name_exists");
    let name = app_config::workspace::validate_workspace_name(value).map_err(|_| required)?;
    if taken.iter().any(|taken| taken.as_str() == name.as_str()) {
        return Err(exists);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_validator_should_report_what_is_wrong() {
        let translate = crate::test_support::translate(i18n::language::codes::LanguageCode::En);
        let taken = vec![SmolStr::new_static("one")];
        assert!(validate_workspace_name("two", &taken, &translate).is_ok());
        let empty = validate_workspace_name("   ", &taken, &translate).unwrap_err();
        let duplicate = validate_workspace_name("one", &taken, &translate).unwrap_err();
        assert_ne!(empty, duplicate);
    }
}

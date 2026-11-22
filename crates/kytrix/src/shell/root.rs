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

/// The window the shell draws.
pub(crate) struct Shell {
    composition: Rc<Composition>,
    window: WindowId,
    title_bar: TitleBar,
    sidebar: Sidebar,
    pages: Pages,
    workspaces: Workspaces,
    dialogs: Dialogs,
    /// The queue every control and dialog reports into.
    intents: Rc<RefCell<IntentQueue>>,
    /// The stage's widget-config dirty flag, checked once per frame.
    stage_dirty: Rc<Cell<bool>>,
    /// The frame route this shell registered (removed when the window closes).
    route: Option<RouteId>,
    /// Whether the close path already ran.
    closed: bool,
}

impl Shell {
    /// Opens the application's main window and builds the shell into it.
    pub(crate) fn open(app: &mut App, composition: Rc<Composition>) -> Result<Self, EngineError> {
        let theme = composition.theme.clone();
        let appearance = theme.current(app);
        let background = theme.theme_for(appearance).role(Role::SurfaceCanvas);
        let window = app.create_window(main_window_options(background));
        Self::open_in(app, composition, window)
    }

    /// Builds the shell into a window the caller already created (the headless host's own).
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
        // The page column leaves room for the title bar the shell draws over it, so the
        // window's chrome never covers the page's own controls.
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

    /// The workspace list the shell shows.
    #[cfg(test)]
    pub(crate) fn workspaces(&self) -> &Workspaces {
        &self.workspaces
    }

    /// The page area.
    #[cfg(test)]
    pub(crate) fn pages(&self) -> &Pages {
        &self.pages
    }

    /// Installs the shell as the application's one frame route.
    ///
    /// The route owns the shell from here on; a recovered settings file is announced over
    /// the window that just opened.
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

    /// One frame: drain the events and requests, apply them, then synchronize every region.
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

    /// Feeds one platform event to the regions that understand it.
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

    /// Applies one request from a control or a dialog.
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

    /// Shows the workspace called `name`, rebuilding the stage when it changed.
    fn select_workspace(&mut self, app: &mut App, name: &SmolStr) -> Result<(), EngineError> {
        self.persist_stage(app)?;
        if !self.workspaces.select(name) {
            // A press on the workspace that is already active still asks for the workspace to
            // be shown: that is how the user comes back from the settings page.
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

    /// Adds a workspace and switches to it.
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

    /// Renames a workspace.
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

    /// Deletes a workspace; the active one's deletion shows the one that follows it.
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

    /// Rebuilds the stage from the active workspace's stored layout.
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

    /// Asks for the name of a new workspace.
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

    /// Asks for the new name of the named workspace.
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

    /// Asks whether the named workspace should be deleted.
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

    /// Reports a settings write the page could not store.
    fn report_write_failure(&mut self, app: &mut App, reason: SmolStr) {
        let translate = self.composition.translate.clone();
        let title = t_smol!(translate, "settings_ui.save_failed.title");
        let confirm = t_smol!(translate, "common.confirm");
        self.dialogs
            .alert(app, &self.composition, title, reason, confirm, |_| {});
    }

    /// Persists the stage's layout when a widget reported a config change.
    fn persist_stage_if_dirty(&mut self, app: &mut App) -> Result<(), EngineError> {
        if !self.stage_dirty.get() {
            return Ok(());
        }
        self.stage_dirty.set(false);
        self.persist_stage(app)
    }

    /// Writes the stage's layout into the active workspace and the settings.
    fn persist_stage(&mut self, app: &mut App) -> Result<(), EngineError> {
        let layout = self.pages.stage().snapshot();
        self.workspaces.set_active_layout(layout);
        let _ = self
            .composition
            .write_workspaces(app, self.workspaces.config());

        Ok(())
    }

    /// The title bar (the window's tests assert its drag region).
    #[cfg(test)]
    pub(crate) fn title_bar(&self) -> &TitleBar {
        &self.title_bar
    }

    /// Closes the window the way the close button does (the tests' shutdown).
    #[cfg(test)]
    pub(crate) fn close_for_test(&mut self, app: &mut App) {
        self.close(app);
    }

    /// Releases the shell's regions and closes the window.
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

/// The main window's options: the application's one window, frameless on Windows (E6).
pub(crate) fn main_window_options(background: ui_core::paint::color::Color) -> WindowOptions {
    WindowOptions {
        title: SmolStr::new_static("Kytrix"),
        size: Size::new(1280.0, 960.0),
        min_size: Some(Size::new(800.0, 600.0)),
        // Windows draws no system title bar: this shell's title bar is the chrome (E6).
        // The other platforms keep their native decoration: macOS's traffic lights are the
        // native chrome the shell makes room for, and Linux has no equivalent frameless
        // path this batch supports.
        decorations: if cfg!(target_os = "windows") {
            Decorations::None
        } else {
            Decorations::Native
        },
        background,
        ..WindowOptions::default()
    }
}

/// Checks a candidate workspace name against the names in use.
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

    /// An empty name is refused with the catalogue's own sentence, and a name in use with a
    /// different one: the validator is what the alert shows.
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

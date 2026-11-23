use app_config::ui::SidebarMode;
use base_ui::{
    kit::{button::Button, icon::IconKind, size::UnifiedSizable},
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
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
    workspaces::Workspaces,
};
use crate::composition::Composition;

pub(crate) const SIDEBAR: &str = "SIDEBAR";

pub(crate) const SIDEBAR_DIVIDER: &str = "SIDEBAR_DIVIDER";

pub(crate) const SIDEBAR_NEW_WORKSPACE: &str = "SIDEBAR_NEW_WORKSPACE";
pub(crate) const SIDEBAR_NEW_WORKSPACE_LABEL: &str = "SIDEBAR_NEW_WORKSPACE_LABEL";

pub(crate) const SIDEBAR_WORKSPACE_SEPARATOR: &str = "SIDEBAR_WORKSPACE_SEPARATOR";

pub(crate) const SIDEBAR_WORKSPACE_NAME: &str = "SIDEBAR_WORKSPACE_NAME";

const SIDEBAR_ITEM: &str = "SIDEBAR_ITEM";
const SIDEBAR_WORKSPACE_CLOSE: &str = "SIDEBAR_WORKSPACE_CLOSE";
const SIDEBAR_WORKSPACE_RENAME: &str = "SIDEBAR_WORKSPACE_RENAME";

pub(crate) const SIDEBAR_FOOTER_SETTINGS: &str = "SIDEBAR_FOOTER_SETTINGS";
pub(crate) const SIDEBAR_FOOTER_SETTINGS_LABEL: &str = "SIDEBAR_FOOTER_SETTINGS_LABEL";

pub(crate) struct Sidebar {
    window: WindowId,
    parent: NodeId,
    node: Option<NodeId>,
    mode: SidebarMode,
    theme: ThemeHandle,
    translate: Translate,
    intents: Intents,
    language_signal: Signal<LanguageCode>,
    language: LanguageCode,
    buttons: Vec<Button>,
}

impl Sidebar {
    pub(crate) fn build(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        composition: &Composition,
        mode: SidebarMode,
        intents: Intents,
        workspaces: &Workspaces,
    ) -> Result<Self, EngineError> {
        let mut sidebar = Self {
            window,
            parent,
            node: None,
            mode,
            theme: composition.theme.clone(),
            translate: composition.translate.clone(),
            intents,
            language_signal: composition.services().language,
            language: composition
                .hub
                .read(|config| crate::language::code(config.language)),
            buttons: Vec::new(),
        };
        sidebar.build_panel(app, mode, workspaces)?;

        Ok(sidebar)
    }

    pub(crate) fn mode(&self) -> SidebarMode {
        self.mode
    }

    fn build_panel(
        &mut self,
        app: &mut App,
        mode: SidebarMode,
        workspaces: &Workspaces,
    ) -> Result<(), EngineError> {
        self.release_controls(app);
        let style = StyleBuilder::new()
            .relative()
            .flex_none()
            .width(metrics::width(mode))
            .height_full()
            .flex_col()
            .overflow_hidden()
            .build();
        let node = match self.node {
            Some(node) => {
                let children = ui::tree(app, self.window)?.get(node)?.children().to_vec();
                for child in children {
                    app.remove_node(self.window, child)?;
                }
                ui::tree(app, self.window)?.set_style(node, style)?;
                node
            }
            None => ui::named(app, self.window, self.parent, SIDEBAR, style)?,
        };
        self.node = Some(node);
        self.mode = mode;
        if mode == SidebarMode::Hidden {
            return Ok(());
        }

        ui::themed_box(
            app,
            self.window,
            node,
            StyleBuilder::new()
                .absolute()
                .top(0.0)
                .left(0.0)
                .right(0.0)
                .bottom(0.0)
                .build(),
            Role::SurfaceSunken,
            &self.theme,
        )?;
        ui::named(
            app,
            self.window,
            node,
            SIDEBAR_DIVIDER,
            StyleBuilder::new()
                .absolute()
                .right(0.0)
                .top(0.0)
                .bottom(0.0)
                .width(1.0)
                .build(),
        )?;
        ui::container(
            app,
            self.window,
            node,
            StyleBuilder::new()
                .height(metrics::TITLE_BAR_HEIGHT)
                .flex_none()
                .build(),
        )?;

        let full = metrics::draws_labels(mode);
        let content = ui::container(
            app,
            self.window,
            node,
            StyleBuilder::new()
                .flex_col()
                .flex_1()
                .min_height_0()
                .width_full()
                .gap(metrics::SIDEBAR_ITEM_GAP)
                .padding(metrics::SIDEBAR_PADDING)
                .build(),
        )?;
        self.build_add_button(app, content, full)?;
        ui::named(
            app,
            self.window,
            content,
            SIDEBAR_WORKSPACE_SEPARATOR,
            StyleBuilder::new().width_full().build(),
        )?;
        ui::separator_horizontal(app, self.window, content, &self.theme)?;

        let list = ui::container(
            app,
            self.window,
            content,
            StyleBuilder::new()
                .flex_col()
                .flex_1()
                .min_height_0()
                .width_full()
                .gap(metrics::SIDEBAR_ITEM_GAP)
                .build(),
        )?;
        for name in workspaces.names() {
            let active = name == workspaces.active_name();
            self.build_row(app, list, full, name, active, workspaces.can_delete())?;
        }

        ui::container(
            app,
            self.window,
            content,
            StyleBuilder::new().flex_1().min_height_0().build(),
        )?;
        ui::separator_horizontal(app, self.window, content, &self.theme)?;
        self.build_settings_button(app, content, full)?;

        Ok(())
    }

    fn build_add_button(
        &mut self,
        app: &mut App,
        parent: NodeId,
        full: bool,
    ) -> Result<(), EngineError> {
        let label = t_smol!(self.translate, "window.workspace.add");
        let intents = self.intents.clone();
        let button = Button::new(
            app,
            self.theme.clone(),
            SmolStr::new_static("kytrix.sidebar.add-workspace"),
        )
        .ghost()
        .medium()
        .icon(IconKind::Plus)
        .a11y_label(label.clone())
        .tooltip_text(label.clone())
        .on_click(move |ctx| ui::report(ctx, &intents, Intent::AddWorkspace));
        let row = ui::named(
            app,
            self.window,
            parent,
            SIDEBAR_NEW_WORKSPACE,
            StyleBuilder::new().width_full().build(),
        )?;
        let node = button.build(app, self.window, row)?;
        if full {
            ui::label_named(
                app,
                self.window,
                node,
                SIDEBAR_NEW_WORKSPACE_LABEL,
                label,
                12.0,
                500,
                Role::FgDefault,
                &self.theme,
            )?;
        }
        self.buttons.push(button);

        Ok(())
    }

    fn build_row(
        &mut self,
        app: &mut App,
        parent: NodeId,
        full: bool,
        name: &SmolStr,
        active: bool,
        can_delete: bool,
    ) -> Result<(), EngineError> {
        let selector = format_smolstr!("{SIDEBAR_ITEM}_{name}");
        let row = ui::named_owned(
            app,
            self.window,
            parent,
            selector,
            StyleBuilder::new().relative().width_full().build(),
        )?;
        let intents = self.intents.clone();
        let target = name.clone();
        let button = Button::new(
            app,
            self.theme.clone(),
            format_smolstr!("kytrix.sidebar.row.{name}"),
        )
        .ghost()
        .medium()
        .icon(IconKind::SquaresFour)
        .selected(active)
        .a11y_label(name.clone())
        .on_click(move |ctx| ui::report(ctx, &intents, Intent::SelectWorkspace(target.clone())));
        let node = button.build(app, self.window, row)?;
        if full {
            ui::label_named(
                app,
                self.window,
                node,
                SIDEBAR_WORKSPACE_NAME,
                name.clone(),
                12.0,
                500,
                Role::FgDefault,
                &self.theme,
            )?;
        }
        self.buttons.push(button);

        if full && active && can_delete {
            self.build_row_actions(app, row, name)?;
        }

        Ok(())
    }

    fn build_row_actions(
        &mut self,
        app: &mut App,
        row: NodeId,
        name: &SmolStr,
    ) -> Result<(), EngineError> {
        let rename_label = t_smol!(self.translate, "window.sidebar.rename");
        let intents = self.intents.clone();
        let target = name.clone();
        let rename = Button::new(
            app,
            self.theme.clone(),
            format_smolstr!("kytrix.sidebar.rename.{name}"),
        )
        .ghost()
        .small()
        .icon(IconKind::NotePencil)
        .a11y_label(rename_label.clone())
        .tooltip_text(rename_label)
        .on_click(move |ctx| ui::report(ctx, &intents, Intent::RenameWorkspace(target.clone())));
        let selector = format_smolstr!("{SIDEBAR_WORKSPACE_RENAME}_{name}");
        let anchor = ui::named_owned(
            app,
            self.window,
            row,
            selector,
            StyleBuilder::new()
                .absolute()
                .right(metrics::SIDEBAR_PADDING + 26.0)
                .top(metrics::SIDEBAR_PADDING)
                .build(),
        )?;
        rename.build(app, self.window, anchor)?;
        self.buttons.push(rename);

        let close_label = t_smol!(self.translate, "window.workspace.close");
        let intents = self.intents.clone();
        let target = name.clone();
        let close = Button::new(
            app,
            self.theme.clone(),
            format_smolstr!("kytrix.sidebar.delete.{name}"),
        )
        .ghost()
        .small()
        .icon(IconKind::Trash)
        .a11y_label(close_label.clone())
        .tooltip_text(close_label)
        .on_click(move |ctx| ui::report(ctx, &intents, Intent::DeleteWorkspace(target.clone())));
        let selector = format_smolstr!("{SIDEBAR_WORKSPACE_CLOSE}_{name}");
        let anchor = ui::named_owned(
            app,
            self.window,
            row,
            selector,
            StyleBuilder::new()
                .absolute()
                .right(metrics::SIDEBAR_PADDING)
                .top(metrics::SIDEBAR_PADDING)
                .build(),
        )?;
        close.build(app, self.window, anchor)?;
        self.buttons.push(close);

        Ok(())
    }

    fn build_settings_button(
        &mut self,
        app: &mut App,
        parent: NodeId,
        full: bool,
    ) -> Result<(), EngineError> {
        let label = t_smol!(self.translate, "window.sidebar.settings");
        let intents = self.intents.clone();
        let button = Button::new(
            app,
            self.theme.clone(),
            SmolStr::new_static("kytrix.sidebar.settings"),
        )
        .ghost()
        .medium()
        .icon(IconKind::GearSix)
        .a11y_label(label.clone())
        .tooltip_text(label.clone())
        .on_click(move |ctx| ui::report(ctx, &intents, Intent::ShowSettings));
        let row = ui::named(
            app,
            self.window,
            parent,
            SIDEBAR_FOOTER_SETTINGS,
            StyleBuilder::new().width_full().build(),
        )?;
        let node = button.build(app, self.window, row)?;
        if full {
            ui::label_named(
                app,
                self.window,
                node,
                SIDEBAR_FOOTER_SETTINGS_LABEL,
                label,
                12.0,
                500,
                Role::FgDefault,
                &self.theme,
            )?;
        }
        self.buttons.push(button);

        Ok(())
    }

    pub(crate) fn sync(
        &mut self,
        app: &mut App,
        workspaces: &Workspaces,
    ) -> Result<(), EngineError> {
        if app.window(self.window).is_none() {
            return Ok(());
        }
        let language = app.read_with(&self.language_signal, |language| *language);
        if language != self.language {
            self.language = language;
            let mode = self.mode;
            self.build_panel(app, mode, workspaces)?;
        }

        Ok(())
    }

    pub(crate) fn rebuild(
        &mut self,
        app: &mut App,
        workspaces: &Workspaces,
    ) -> Result<(), EngineError> {
        let mode = self.mode;
        self.build_panel(app, mode, workspaces)
    }

    pub(crate) fn set_mode(
        &mut self,
        app: &mut App,
        mode: SidebarMode,
        workspaces: &Workspaces,
    ) -> Result<(), EngineError> {
        if mode == self.mode {
            return Ok(());
        }
        self.build_panel(app, mode, workspaces)
    }

    fn release_controls(&mut self, app: &mut App) {
        for button in std::mem::take(&mut self.buttons) {
            button.release(app);
        }
    }

    pub(crate) fn dispatch(&mut self, app: &mut App, event: &Event) {
        for button in &self.buttons {
            let _ = button.dispatch(app, event);
        }
    }

    pub(crate) fn release(&mut self, app: &mut App) {
        self.release_controls(app);
        if let Some(node) = self.node.take() {
            let _ = app.remove_node(self.window, node);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_full_panel_should_draw_labels() {
        assert!(metrics::draws_labels(SidebarMode::Full));
        assert!(!metrics::draws_labels(SidebarMode::IconOnly));
        assert!(!metrics::draws_labels(SidebarMode::Hidden));
    }
}

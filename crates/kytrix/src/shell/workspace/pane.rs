use app_config::{widget::VersionedWidgetConfig, workspace::WorkspaceWidget};
use i18n::{t_smol, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    event::Event,
    math::Rect,
    node::{builder::NodeBuilder, id::NodeId},
    paint::{primitive::Border, style::StylePatch},
    window::WindowId,
};
use ui_kit::{
    kit::{button::Button, icon::IconKind, size::UnifiedSizable},
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{handle::ThemeHandle, role::Role},
};
use widget::{
    any::AnyWidget, config::WidgetConfigChange, services::WidgetServices, view::HeaderGroup,
};
use widget_registry::registry::{WidgetMountError, WidgetRegistry};

use super::{
    super::{
        intent::Intent,
        ui::{self, Intents},
    },
    tree::{PaneId, PaneOrientation},
};

pub(crate) const PANE_BODY: &str = "PANE_BODY";

pub(crate) const PANE_HEADER: &str = "PANE_HEADER";

pub(crate) const PANE_WIDGET_BUTTON: &str = "PANE_WIDGET_BUTTON";
pub(crate) const PANE_SPLIT_RIGHT: &str = "PANE_SPLIT_RIGHT";
pub(crate) const PANE_SPLIT_DOWN: &str = "PANE_SPLIT_DOWN";
pub(crate) const PANE_CLOSE_BUTTON: &str = "PANE_CLOSE_BUTTON";

pub(crate) struct Pane {
    pub id: PaneId,
    pub window: WindowId,
    pub container: NodeId,
    header_slot: NodeId,
    body: NodeId,
    header: Option<NodeId>,
    widget: Option<AnyWidget>,
    widget_id: SmolStr,
    config: Option<VersionedWidgetConfig>,
    intents: Intents,
    theme: ThemeHandle,
    translate: Translate,
    window_buttons: Vec<Button>,
    close_button_visible: bool,
}

pub(crate) struct PaneParts<'a> {
    pub registry: &'a WidgetRegistry,
    pub services: &'a std::rc::Rc<WidgetServices>,
    pub config_change: WidgetConfigChange,
    pub intents: Intents,
    pub theme: &'a ThemeHandle,
    pub translate: &'a Translate,
}

impl Pane {
    pub(crate) fn mount(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        id: PaneId,
        stored: &WorkspaceWidget,
        parts: PaneParts<'_>,
    ) -> Result<Self, EngineError> {
        let PaneParts {
            registry,
            services,
            config_change,
            intents,
            theme,
            translate,
        } = parts;
        let container = build_frame(app, window, parent, theme)?;
        let header_slot = ui::container(
            app,
            window,
            container,
            StyleBuilder::new().width_full().flex_none().build(),
        )?;
        let body = ui::named(
            app,
            window,
            container,
            PANE_BODY,
            StyleBuilder::new()
                .flex_col()
                .flex_1()
                .min_width_0()
                .min_height_0()
                .width_full()
                .build(),
        )?;

        let mut pane = Self {
            id,
            window,
            container,
            header_slot,
            body,
            header: None,
            widget: None,
            widget_id: stored.id.clone(),
            config: stored.config.clone(),
            intents,
            theme: theme.clone(),
            translate: translate.clone(),
            window_buttons: Vec::new(),
            close_button_visible: false,
        };
        pane.mount_widget(app, window, stored, registry, services, config_change)?;
        pane.rebuild_header(app, window)?;

        Ok(pane)
    }

    fn mount_widget(
        &mut self,
        app: &mut App,
        window: WindowId,
        stored: &WorkspaceWidget,
        registry: &WidgetRegistry,
        services: &std::rc::Rc<WidgetServices>,
        config_change: WidgetConfigChange,
    ) -> Result<(), EngineError> {
        let mounted = match registry.mount(
            stored.id.as_str(),
            app,
            window,
            self.body,
            services,
            stored.config.clone(),
            config_change.clone(),
        ) {
            Ok(mounted) => mounted,
            Err(WidgetMountError::NotRegistered(id)) => {
                log::warn!(
                    "pane {} stored widget `{id}`, which this build does not register; \
                     falling back to the default widget",
                    self.id.index()
                );
                registry
                    .mount_default(
                        app_config::workspace::DEFAULT_WIDGET_ID,
                        app,
                        window,
                        self.body,
                        services,
                        config_change,
                    )
                    .map_err(|error| match error {
                        WidgetMountError::Mount(error) => error,
                        WidgetMountError::NotRegistered(id) => {
                            log::error!("the default widget `{id}` is not registered");
                            EngineError::StaleNode { node: self.body }
                        }
                    })?
            }
            Err(WidgetMountError::Mount(error)) => return Err(error),
        };

        self.widget_id = SmolStr::new(mounted.widget_id().as_str());
        self.widget = Some(mounted);

        Ok(())
    }

    pub(crate) fn snapshot(&self) -> WorkspaceWidget {
        WorkspaceWidget {
            id: self.widget_id.clone(),
            config: match &self.widget {
                Some(widget) => widget.config(),
                None => self.config.clone(),
            },
        }
    }

    pub(crate) fn widget_id(&self) -> &str {
        &self.widget_id
    }

    pub(crate) fn set_widget(
        &mut self,
        app: &mut App,
        window: WindowId,
        stored: &WorkspaceWidget,
        registry: &WidgetRegistry,
        services: &std::rc::Rc<WidgetServices>,
        config_change: WidgetConfigChange,
    ) -> Result<(), EngineError> {
        self.release_header(app);
        self.release_widget(app);
        self.mount_widget(app, window, stored, registry, services, config_change)?;
        self.rebuild_header(app, window)?;

        Ok(())
    }

    pub(crate) fn set_rect(&mut self, app: &mut App, rect: Rect) -> Result<(), EngineError> {
        let tree = ui::tree(app, self.window)?;
        tree.set_style(self.container, frame_style(rect))
    }

    pub(crate) fn rebuild_header(
        &mut self,
        app: &mut App,
        window: WindowId,
    ) -> Result<(), EngineError> {
        self.release_header(app);
        let header = build_header_row(app, window, self.header_slot, &self.theme)?;
        let mut buttons = Vec::with_capacity(4);
        let left = ui::container(
            app,
            window,
            header,
            StyleBuilder::new().flex_row().items_center().build(),
        )?;
        ui::container(
            app,
            window,
            header,
            StyleBuilder::new()
                .flex_1()
                .min_width_0()
                .height_full()
                .build(),
        )?;
        let right = ui::container(
            app,
            window,
            header,
            StyleBuilder::new().flex_row().items_center().build(),
        )?;
        let chrome = HeaderChrome {
            theme: &self.theme,
            translate: &self.translate,
            intents: &self.intents,
        };
        for action in [
            HeaderAction::Widget,
            HeaderAction::SplitRight,
            HeaderAction::SplitDown,
        ] {
            buttons.push(build_header_control(
                app, window, header, self.id, action, &chrome,
            )?);
        }
        if self.close_button_visible {
            buttons.push(build_header_control(
                app,
                window,
                header,
                self.id,
                HeaderAction::Close,
                &chrome,
            )?);
        }
        if let Some(widget) = &self.widget {
            for (group, parent) in [(HeaderGroup::Left, left), (HeaderGroup::Right, right)] {
                if let Err(error) = widget.header(app, group, parent) {
                    log::error!("pane {} header group failed: {error}", self.id.index());
                }
            }
        }
        self.header = Some(header);
        self.window_buttons = buttons;

        Ok(())
    }

    pub(crate) fn set_close_button_visible(
        &mut self,
        app: &mut App,
        window: WindowId,
        visible: bool,
    ) -> Result<(), EngineError> {
        if visible == self.close_button_visible {
            return Ok(());
        }
        self.close_button_visible = visible;
        self.rebuild_header(app, window)
    }

    fn release_header(&mut self, app: &mut App) {
        for button in std::mem::take(&mut self.window_buttons) {
            button.release(app);
        }
        if let Some(header) = self.header.take() {
            let _ = app.remove_node(self.window, header);
        }
    }

    fn release_widget(&mut self, app: &mut App) {
        if let Some(widget) = self.widget.take() {
            let _ = widget.release(app);
        }
    }

    pub(crate) fn sync(&self, app: &mut App) {
        for button in &self.window_buttons {
            let _ = button.sync(app);
        }
    }

    pub(crate) fn dispatch(&mut self, app: &mut App, event: &Event) {
        for button in &self.window_buttons {
            let _ = button.dispatch(app, event);
        }
    }

    pub(crate) fn release(&mut self, app: &mut App) {
        self.release_header(app);
        self.release_widget(app);
        let _ = app.remove_node(self.window, self.container);
    }
}

fn frame_style(rect: Rect) -> ui_core::paint::style::Style {
    StyleBuilder::new()
        .absolute()
        .left(rect.origin.x)
        .top(rect.origin.y)
        .width(rect.size.width)
        .height(rect.size.height)
        .flex_col()
        .gap(4.0)
        .padding(4.0)
        .overflow_hidden()
        .min_width_0()
        .min_height_0()
        .build()
}

fn build_frame(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    theme: &ThemeHandle,
) -> Result<NodeId, EngineError> {
    let background = ui::color(theme, app, Role::SurfaceCanvas);
    let themed = theme.clone();
    NodeBuilder::boxed()
        .id(SmolStr::new_static("PANE_FRAME"))
        .style(frame_style(Rect::default()))
        .paint(
            PaintBuilder::new()
                .background(background)
                .rounded(4.0)
                .build(),
        )
        .bind_style(move |cx| StylePatch {
            background: Some(themed.role(cx, Role::SurfaceCanvas)),
            border: Some(Border {
                width: 1.0,
                color: themed.role(cx, Role::LineDivider),
            }),
            ..StylePatch::default()
        })
        .build(ui::tree(app, window)?, parent)
}

fn build_header_row(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    theme: &ThemeHandle,
) -> Result<NodeId, EngineError> {
    let background = ui::color(theme, app, Role::SurfaceRaised);
    let themed = theme.clone();
    NodeBuilder::boxed()
        .id(SmolStr::new_static(PANE_HEADER))
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(4.0)
                .padding(4.0)
                .width_full()
                .height(34.0)
                .flex_none()
                .overflow_hidden()
                .build(),
        )
        .paint(
            PaintBuilder::new()
                .background(background)
                .rounded(4.0)
                .build(),
        )
        .bind_style(move |cx| StylePatch {
            background: Some(themed.role(cx, Role::SurfaceRaised)),
            border: Some(Border {
                width: 1.0,
                color: themed.role(cx, Role::LineDivider),
            }),
            ..StylePatch::default()
        })
        .build(ui::tree(app, window)?, parent)
}

struct HeaderChrome<'a> {
    theme: &'a ThemeHandle,
    translate: &'a Translate,
    intents: &'a Intents,
}

fn build_header_control(
    app: &mut App,
    window: WindowId,
    header: NodeId,
    pane: PaneId,
    action: HeaderAction,
    chrome: &HeaderChrome<'_>,
) -> Result<Button, EngineError> {
    let HeaderChrome {
        theme,
        translate,
        intents,
    } = *chrome;
    let label = t_smol!(translate, action.label_key());
    let intents = intents.clone();
    let button = Button::new(app, theme.clone(), header_action_key(pane, action))
        .ghost()
        .small()
        .icon(action.icon())
        .a11y_label(label.clone())
        .tooltip_text(label)
        .on_click(move |ctx| {
            if let Some(intent) = action.intent(pane) {
                ui::report(ctx, &intents, intent);
            }
        });
    let wrapper = ui::named(
        app,
        window,
        header,
        action.selector(),
        StyleBuilder::new().build(),
    )?;
    button.build(app, window, wrapper)?;

    Ok(button)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeaderAction {
    Widget,
    SplitRight,
    SplitDown,
    Close,
}

impl HeaderAction {
    pub(crate) const fn intent(self, pane: PaneId) -> Option<Intent> {
        match self {
            Self::Widget => Some(Intent::OpenGallery(pane)),
            Self::SplitRight => Some(Intent::SplitPane {
                pane,
                orientation: PaneOrientation::Horizontal,
                target_on_head: true,
            }),
            Self::SplitDown => Some(Intent::SplitPane {
                pane,
                orientation: PaneOrientation::Vertical,
                target_on_head: true,
            }),
            Self::Close => Some(Intent::ClosePane(pane)),
        }
    }

    const fn icon(self) -> IconKind {
        match self {
            Self::Widget => IconKind::PuzzlePiece,
            Self::SplitRight => IconKind::ColumnsPlusRight,
            Self::SplitDown => IconKind::RowsPlusBottom,
            Self::Close => IconKind::X,
        }
    }

    const fn label_key(self) -> &'static str {
        match self {
            Self::Widget => "window.pane.select_widget",
            Self::SplitRight => "window.pane.split_keep_right",
            Self::SplitDown => "window.pane.split_keep_bottom",
            Self::Close => "window.pane.close",
        }
    }

    const fn selector(self) -> &'static str {
        match self {
            Self::Widget => PANE_WIDGET_BUTTON,
            Self::SplitRight => PANE_SPLIT_RIGHT,
            Self::SplitDown => PANE_SPLIT_DOWN,
            Self::Close => PANE_CLOSE_BUTTON,
        }
    }
}

pub(crate) fn header_action_key(pane: PaneId, action: HeaderAction) -> SmolStr {
    format_smolstr!("kytrix.pane.{}.{action:?}", pane.index())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_header_action_should_map_to_its_intent() {
        let pane = PaneId::test(3);
        assert_eq!(
            HeaderAction::Widget.intent(pane),
            Some(Intent::OpenGallery(pane))
        );
        assert_eq!(
            HeaderAction::Close.intent(pane),
            Some(Intent::ClosePane(pane))
        );
        assert!(matches!(
            HeaderAction::SplitRight.intent(pane),
            Some(Intent::SplitPane {
                orientation: PaneOrientation::Horizontal,
                ..
            })
        ));
        assert!(matches!(
            HeaderAction::SplitDown.intent(pane),
            Some(Intent::SplitPane {
                orientation: PaneOrientation::Vertical,
                ..
            })
        ));
    }

    #[test]
    fn every_header_action_should_have_its_own_identity() {
        let actions = [
            HeaderAction::Widget,
            HeaderAction::SplitRight,
            HeaderAction::SplitDown,
            HeaderAction::Close,
        ];
        let mut keys = Vec::new();
        let mut selectors = Vec::new();
        for action in actions {
            keys.push(action.label_key());
            selectors.push(action.selector());
        }
        keys.sort_unstable();
        keys.dedup();
        selectors.sort_unstable();
        selectors.dedup();
        assert_eq!(keys.len(), 4, "each control has its own label");
        assert_eq!(selectors.len(), 4, "each control has its own selector");
    }
}

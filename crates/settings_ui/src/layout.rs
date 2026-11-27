use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    error::EngineError,
    interaction::{ScrollConfig, ScrollHandle},
    math::{LogicalPx, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
    paint::{
        primitive::Border,
        style::{PaintSpec, Style, StylePatch},
        style_builder::{PaintBuilder, StyleBuilder},
    },
    text::{TextStyle, TextWrap},
    window::WindowId,
};
use ui_kit::{
    kit::{
        scroll_bar::{ScrollBar, ScrollbarAxis, ScrollbarOptions},
        select::{Select, SelectItem, SelectOptions},
    },
    theme::{appearance::Appearance, handle::ThemeHandle, role::Role},
};

use crate::host::SettingsHost;

pub(crate) const CONTROL_WIDTH: LogicalPx = px(240.0);

const SEED_APPEARANCE: Appearance = Appearance::Dark;

const LABEL_SIZE: f32 = 13.0;
const DESCRIPTION_SIZE: f32 = 11.0;

pub(crate) type RowFn = Box<dyn FnOnce(&mut App, WindowId, NodeId) -> Result<(), EngineError>>;

pub(crate) fn build_node(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    build: impl FnOnce(&mut NodeTree, NodeId) -> Result<NodeId, EngineError>,
) -> Result<NodeId, EngineError> {
    let tree = app
        .window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree_mut();
    build(tree, parent)
}

pub(crate) fn container(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    style: Style,
) -> Result<NodeId, EngineError> {
    build_node(app, window, parent, |tree, parent| {
        NodeBuilder::boxed().style(style).build(tree, parent)
    })
}

fn text_style(theme: &ThemeHandle, size: f32, weight: u16, role: Role) -> TextStyle {
    TextStyle::new(px(size))
        .with_weight(weight)
        .with_color(theme.theme_for(SEED_APPEARANCE).role(role))
        .with_wrap(TextWrap::Wrap)
}

pub(crate) fn themed_box(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    id: SmolStr,
    style: Style,
    paint: PaintSpec,
    theme: &ThemeHandle,
    background: Option<Role>,
    border: Option<Role>,
) -> Result<NodeId, EngineError> {
    let background_theme = theme.clone();
    let border_theme = theme.clone();
    build_node(app, window, parent, |tree, parent| {
        NodeBuilder::boxed()
            .id(id)
            .style(style)
            .paint(paint)
            .bind_style(move |cx| StylePatch {
                background: background.map(|role| background_theme.role(cx, role)),
                border: border.map(|role| Border {
                    width: px(1.0),
                    color: border_theme.role(cx, role),
                }),
                ..StylePatch::default()
            })
            .build(tree, parent)
    })
}

pub(crate) fn themed_text(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    id: Option<SmolStr>,
    text: SmolStr,
    size: f32,
    weight: u16,
    role: Role,
    theme: &ThemeHandle,
) -> Result<NodeId, EngineError> {
    let style = text_style(theme, size, weight, role);
    let theme = theme.clone();
    build_node(app, window, parent, move |tree, parent| {
        let mut builder = NodeBuilder::text(text, style).bind_style(move |cx| StylePatch {
            foreground: Some(theme.role(cx, role)),
            ..StylePatch::default()
        });
        if let Some(id) = id {
            builder = builder.id(id);
        }
        builder.build(tree, parent)
    })
}

pub(crate) fn themed_text_in_tree(
    tree: &mut NodeTree,
    parent: NodeId,
    id: Option<SmolStr>,
    text: SmolStr,
    size: f32,
    weight: u16,
    role: Role,
    theme: &ThemeHandle,
) -> Result<NodeId, EngineError> {
    let style = text_style(theme, size, weight, role);
    let theme = theme.clone();
    let mut builder = NodeBuilder::text(text, style).bind_style(move |cx| StylePatch {
        foreground: Some(theme.role(cx, role)),
        ..StylePatch::default()
    });
    if let Some(id) = id {
        builder = builder.id(id);
    }
    builder.build(tree, parent)
}

pub(crate) fn row(
    theme: &ThemeHandle,
    id: &'static str,
    label: SmolStr,
    description: SmolStr,
    control: impl FnOnce(&mut App, WindowId, NodeId) -> Result<(), EngineError> + 'static,
) -> RowFn {
    let theme = theme.clone();
    Box::new(move |app: &mut App, window: WindowId, parent: NodeId| {
        let row = build_node(app, window, parent, |tree, parent| {
            NodeBuilder::boxed()
                .id(format_smolstr!("SETTINGS_FIELD_{id}"))
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .width_full()
                        .min_width_0()
                        .gap(px(24.0))
                        .padding_sides(px(12.0), px(20.0), px(12.0), px(20.0))
                        .build(),
                )
                .build(tree, parent)
        })?;
        let text = container(
            app,
            window,
            row,
            StyleBuilder::new()
                .flex_1()
                .min_width_0()
                .flex_col()
                .gap(px(3.0))
                .build(),
        )?;
        themed_text(
            app,
            window,
            text,
            Some(format_smolstr!("SETTINGS_LABEL_{id}")),
            label,
            LABEL_SIZE,
            500,
            Role::FgDefault,
            &theme,
        )?;
        if !description.is_empty() {
            themed_text(
                app,
                window,
                text,
                Some(format_smolstr!("SETTINGS_DESCRIPTION_{id}")),
                description,
                DESCRIPTION_SIZE,
                400,
                Role::FgMuted,
                &theme,
            )?;
        }
        let control_column = build_node(app, window, row, |tree, parent| {
            NodeBuilder::boxed()
                .id(format_smolstr!("SETTINGS_CONTROL_{id}"))
                .style(
                    StyleBuilder::new()
                        .flex_none()
                        .width(CONTROL_WIDTH)
                        .min_width_0()
                        .build(),
                )
                .build(tree, parent)
        })?;
        control(app, window, control_column)
    })
}

pub(crate) fn section(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    theme: &ThemeHandle,
    id: &'static str,
    title: Option<SmolStr>,
    rows: Vec<RowFn>,
) -> Result<(), EngineError> {
    if rows.is_empty() {
        return Ok(());
    }

    let section = container(
        app,
        window,
        parent,
        StyleBuilder::new()
            .flex_col()
            .flex_none()
            .width_full()
            .min_width_0()
            .gap(px(6.0))
            .build(),
    )?;
    if let Some(title) = title {
        let title_theme = theme.clone();
        build_node(app, window, section, move |tree, parent| {
            let style = title_theme.theme_for(SEED_APPEARANCE).role(Role::FgDefault);
            NodeBuilder::text(
                title,
                TextStyle::new(px(LABEL_SIZE))
                    .with_weight(600)
                    .with_color(style),
            )
            .id(format_smolstr!("SETTINGS_SECTION_TITLE_{id}"))
            .a11y(A11yOverride {
                role: Some(A11yRole::Heading),
                ..A11yOverride::default()
            })
            .bind_style(move |cx| StylePatch {
                foreground: Some(title_theme.role(cx, Role::FgDefault)),
                ..StylePatch::default()
            })
            .build(tree, parent)
        })?;
    }

    let card = themed_box(
        app,
        window,
        section,
        format_smolstr!("SETTINGS_CARD_{id}"),
        StyleBuilder::new()
            .flex_col()
            .flex_none()
            .width_full()
            .min_width_0()
            .overflow_hidden()
            .build(),
        PaintBuilder::new().rounded(px(8.0)).build(),
        theme,
        Some(Role::SurfaceSunken),
        None,
    )?;

    for (index, build) in rows.into_iter().enumerate() {
        if index > 0 {
            let divider = container(
                app,
                window,
                card,
                StyleBuilder::new()
                    .flex_none()
                    .width_full()
                    .padding_x(px(20.0))
                    .build(),
            )?;
            themed_box(
                app,
                window,
                divider,
                format_smolstr!("SETTINGS_DIVIDER_{id}_{index}"),
                StyleBuilder::new()
                    .flex_none()
                    .width_full()
                    .height(px(1.0))
                    .build(),
                PaintSpec::default(),
                theme,
                Some(Role::LineDivider),
                None,
            )?;
        }
        build(app, window, card)?;
    }

    Ok(())
}

pub(crate) fn choice_select(
    app: &mut App,
    host: &SettingsHost,
    key: SmolStr,
    selected: &str,
    choices: impl IntoIterator<Item = (SmolStr, SmolStr)>,
    aria_label: SmolStr,
    on_change: impl Fn(&mut App, &str) + 'static,
) -> Select {
    let select = Select::new(
        app,
        host.theme_handle(),
        key,
        SelectOptions::default()
            .width(CONTROL_WIDTH)
            .label(aria_label),
    );
    let items = choices
        .into_iter()
        .map(|(id, label)| SelectItem::new(id, label))
        .collect::<Vec<_>>();
    let _ = select.set_items(app, items);
    select.set_chosen(app, [selected], false);

    select.on_change(move |app, chosen| {
        if let Some(id) = chosen.first() {
            on_change(app, id.as_str());
        }
    })
}

pub(crate) fn scroll_region(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    theme: &ThemeHandle,
    key: &str,
) -> Result<(ScrollHandle, NodeId, ScrollBar), EngineError> {
    let outer = build_node(app, window, parent, |tree, parent| {
        NodeBuilder::boxed()
            .id(format_smolstr!("{key}.viewport-outer"))
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .width_full()
                    .min_width_0()
                    .min_height_0()
                    .relative()
                    .overflow_hidden()
                    .build(),
            )
            .build(tree, parent)
    })?;
    let viewport = build_node(app, window, outer, |tree, parent| {
        NodeBuilder::boxed()
            .id(format_smolstr!("{key}.viewport"))
            .style(
                StyleBuilder::new()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .relative()
                    .overflow_hidden()
                    .build(),
            )
            .build(tree, parent)
    })?;
    let content = build_node(app, window, viewport, |tree, parent| {
        NodeBuilder::boxed()
            .id(page_content_id())
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .flex_none()
                    .width_full()
                    .min_width_0()
                    .padding_sides(px(4.0), px(0.0), px(12.0), px(0.0))
                    .gap(px(16.0))
                    .build(),
            )
            .build(tree, parent)
    })?;
    let handle = app.make_scroll(window, viewport, ScrollConfig::default())?;
    let bar = ScrollBar::new(
        app,
        theme.clone(),
        format_smolstr!("{key}.scrollbar"),
        ScrollbarAxis::Vertical,
        ScrollbarOptions::default(),
    );
    build_node(app, window, outer, |tree, parent| {
        bar.build(tree, parent).map(|_| parent)
    })?;
    bar.attach(app, window, viewport);

    Ok((handle, content, bar))
}

pub(crate) fn sync_scroll_content(
    app: &mut App,
    window: WindowId,
    handle: &ScrollHandle,
    content: NodeId,
) {
    let Some(window_ref) = app.window(window) else {
        return;
    };
    let Ok(entry) = window_ref.tree().get(content) else {
        return;
    };
    let size = entry.layout().bounds.size;
    if size.width <= px(0.0) || size.height <= px(0.0) {
        return;
    }
    let _ = app.set_scroll_content(handle, LogicalSize::new(size.width, size.height));
}

pub(crate) fn page_content_id() -> SmolStr {
    SmolStr::new_static("SETTINGS_PAGE_CONTENT")
}

//! The native building blocks every settings page draws with.
//!
//! A setting is one row: its name and supplementary explanation on the left, the control that
//! changes it on the right. Rows of one group sit in one card, separated by a line; a page is a
//! column of those groups. Nothing here describes what a page holds — a page writes its own rows
//! with these helpers — so a setting that needs a custom control builds it beside a row rather than
//! teaching this module a new shape.
//!
//! The helpers only decide geometry, chrome and the debug ids the tests reach a control by. What a
//! control reads and writes stays with the page that drew it.
//!
//! # Text follows the theme, not the frame
//!
//! Nothing here bakes a colour into a node: every text node and every surface reads its colour
//! through `bind_style` from the injected [`ThemeHandle`], so switching the appearance re-colours
//! the page on the frame that follows without rebuilding it (the same rule kit components follow).

use base_ui::{
    kit::{
        scroll_bar::{ScrollBar, ScrollbarAxis, ScrollbarOptions},
        select::{Select, SelectItem, SelectOptions},
    },
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{appearance::Appearance, handle::ThemeHandle, role::Role},
};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    error::EngineError,
    interaction::{ScrollConfig, ScrollHandle},
    math::Size,
    node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
    paint::{
        primitive::Border,
        style::{PaintSpec, Style, StylePatch},
    },
    text::{TextStyle, TextWrap},
    window::WindowId,
};

use crate::host::SettingsHost;

/// The width of the column a control is drawn in.
pub(crate) const CONTROL_WIDTH: f32 = 240.0;

/// The appearance a bound colour is seeded from.
///
/// It is only the value a node draws before its first style pass evaluates the patch, so the seed
/// needs to be one of the two appearances rather than the current one.
const SEED_APPEARANCE: Appearance = Appearance::Dark;

/// The size of a row's label.
const LABEL_SIZE: f32 = 13.0;
/// The size of a row's explanation.
const DESCRIPTION_SIZE: f32 = 11.0;

/// One row of a card, built when the card is: the closure is what draws the control, because a kit
/// component needs `&mut App` and the row is decided before the card's nodes exist.
pub(crate) type RowFn = Box<dyn FnOnce(&mut App, WindowId, NodeId) -> Result<(), EngineError>>;

/// Builds one node under `parent`.
///
/// The node tree lives inside the window, which lives inside `App`, so a `&mut NodeTree` borrow
/// cannot be held across a kit call: the borrow lives for the duration of `build` only, and the
/// caller gets the tree back before the next component it builds.
///
/// # Errors
/// The window is gone (`UnknownWindow`), or the builder failed (usually `StaleNode`).
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

/// Builds one box node under `parent`.
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

/// One text style as a node is seeded with: the size, the weight, and the colour of `role` in the
/// seed appearance (the `bind_style` patch is what the node actually draws with).
fn text_style(theme: &ThemeHandle, size: f32, weight: u16, role: Role) -> TextStyle {
    TextStyle::new(size)
        .with_weight(weight)
        .with_color(theme.theme_for(SEED_APPEARANCE).role(role))
        .with_wrap(TextWrap::Wrap)
}

/// Builds one box node with a debug id, a style, a paint spec and the theme colours it follows.
///
/// `background` and `border` are roles rather than colours: the patch is evaluated on every style
/// pass, so a surface written with this helper re-colours itself when the appearance changes.
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
                    width: 1.0,
                    color: border_theme.role(cx, role),
                }),
                ..StylePatch::default()
            })
            .build(tree, parent)
    })
}

/// Builds one text node that follows the theme's foreground role.
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

/// Builds one text node that follows the theme's foreground role, on a tree the caller already
/// holds (a virtual list's row factory has no `App`, so it builds through this form).
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

/// One setting: its name and, when it has one, its explanation on the left, its control on the
/// right.
///
/// The control is drawn in a column of its own so every row of a card lines up on the same edge.
/// The row, its name and its explanation carry the debug ids the tests read their geometry and
/// their text by. A row whose setting has no explanation draws none.
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
                        .gap(24.0)
                        .padding_sides(12.0, 20.0, 12.0, 20.0)
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
                .gap(3.0)
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

/// One group of settings: the heading that names it, when it has one, and the card under it.
///
/// The card is the group's whole surface; the selected rows are the ones the search left, so a card
/// never draws a line above a row that is not there, and a group left without rows draws nothing at
/// all (its heading would name nothing).
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
            .gap(6.0)
            .build(),
    )?;
    if let Some(title) = title {
        let title_theme = theme.clone();
        build_node(app, window, section, move |tree, parent| {
            let style = title_theme.theme_for(SEED_APPEARANCE).role(Role::FgDefault);
            NodeBuilder::text(
                title,
                TextStyle::new(LABEL_SIZE)
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
        PaintBuilder::new().rounded(8.0).build(),
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
                    .padding_x(20.0)
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
                    .height(1.0)
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

/// A select over a fixed set of choices, drawn in the width of the row that holds it.
///
/// The choices are the ids the settings store beside the words they are drawn with. A selected id
/// that no choice names is still selected: a value read from settings that no longer offers it is
/// what the user has, and the control that hid it would report a value the settings do not hold.
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
        host.layer(),
        host.animation(),
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

/// The scroll region one page's content lives in.
///
/// Returns the handle the caller keeps (it is what the content height is written through), the
/// content node the page builds under, and the scroll bar drawn beside it.
///
/// # Errors
/// The window is gone, or the scroll container could not be made.
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
                    .padding_sides(4.0, 0.0, 12.0, 0.0)
                    .gap(16.0)
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

/// Writes the measured height of `content` into the scroll range of `handle`.
///
/// The engine's scroll container only knows the content size the caller states, while a settings
/// page's height is whatever its rows laid out to: the height is read off the content node after
/// the frame that laid it out. Writing the same size twice is a no-op, so a steady page asks for no
/// frame (I3).
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
    if size.width <= 0.0 || size.height <= 0.0 {
        return;
    }
    let _ = app.set_scroll_content(handle, Size::new(size.width, size.height));
}

/// The debug id of the node a page builds its groups under.
pub(crate) fn page_content_id() -> SmolStr {
    SmolStr::new_static("SETTINGS_PAGE_CONTENT")
}

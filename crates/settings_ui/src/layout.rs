//! The native building blocks every settings page draws with.
//!
//! A setting is one row: its name and supplementary explanation on the left, the control that
//! changes it on the right. Rows of one group sit in one card, separated by a line; a page is a
//! column of those groups. Nothing here describes what a page holds — a page writes its own rows
//! with these helpers — so a setting that needs a custom control builds it beside a row rather
//! than teaching a rendering layer a new shape.
//!
//! The helpers only decide geometry, chrome and the debug selectors the tests reach a control by.
//! What a control reads and writes stays with the page that drew it.

use std::{rc::Rc, sync::Arc};

use app_config::settings::Config;
use base_ui::{
    interactive::InteractiveVariantExt,
    menu::{MenuItem, MenuItemAction},
    select::Select,
    style::theme::ThemeExt,
};
use gpui_kit::{
    AnyElement, App, Context, ElementId, Entity, InteractiveElement as _, IntoElement,
    ParentElement, Pixels, ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled,
    Window, base::Disableable as _, div, px,
};

use crate::{session::SettingsChange, settings::SettingsPage};

/// The width of the column a control is drawn in.
pub(crate) const CONTROL_WIDTH: Pixels = px(240.0);

/// The element id one control is keyed under.
///
/// The id is what keeps a control's state — an open menu, a caret — across the frames the page is
/// drawn in, so it is the setting's own id rather than a position on the page.
pub(crate) fn element_id(prefix: &'static str, id: &str) -> ElementId {
    ElementId::NamedChild(
        Arc::new(ElementId::Name(SharedString::new_static(prefix))),
        SharedString::from(id),
    )
}

/// One setting: its name and, when it has one, its explanation on the left, its control on the
/// right.
///
/// The control is drawn in a column of its own so every row of a card lines up on the same edge.
/// The row, its name and its explanation register the selectors the tests read their geometry
/// and their text by. A row whose setting has no explanation draws none.
pub(crate) fn row(
    id: &str,
    label: SharedString,
    description: SharedString,
    control: AnyElement,
    control_width: Pixels,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let row_selector = format!("SETTINGS_FIELD_{id}");
    let label_selector = format!("SETTINGS_LABEL_{id}");
    let description_selector = format!("SETTINGS_DESCRIPTION_{id}");
    let control_selector = format!("SETTINGS_CONTROL_{id}");

    let label = div()
        .debug_selector(move || label_selector)
        .text_sm()
        .text_color(theme.roles.fg.default.hsla)
        .child(label);
    let mut text = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(label);
    if !description.is_empty() {
        text = text.child(
            div()
                .debug_selector(move || description_selector)
                .text_xs()
                .text_color(theme.roles.fg.muted.hsla)
                .child(description),
        );
    }

    div()
        .debug_selector(move || row_selector)
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .justify_between()
        .gap_6()
        .px_5()
        .py_3()
        .child(text)
        .child(
            div()
                .debug_selector(move || control_selector)
                .flex_none()
                .w(control_width)
                .min_w_0()
                .child(div().w_full().min_w_0().child(control)),
        )
        .into_any_element()
}

/// One group of settings as a card: its rows separated by a line.
///
/// The card is the group's whole surface; the heading that names it is drawn outside it by
/// [`section`]. The selected rows are the ones the search left, so a card never draws a line above
/// a row that is not there.
pub(crate) fn card(id: &str, rows: Vec<AnyElement>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let card_selector = format!("SETTINGS_CARD_{id}");
    let mut card = div()
        .debug_selector(move || card_selector)
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .rounded_md()
        .bg(theme.roles.surface.sunken.hsla)
        .overflow_hidden();

    for (index, row) in rows.into_iter().enumerate() {
        if index > 0 {
            card = card.child(divider(id, index, cx));
        }
        card = card.child(row);
    }

    card.into_any_element()
}

/// The line between two rows of one card, inset by the row's own horizontal padding so it starts
/// and ends where the rows do.
fn divider(id: &str, index: usize, cx: &App) -> AnyElement {
    let selector = format!("SETTINGS_DIVIDER_{id}_{index}");
    let theme = cx.theme();

    div()
        .px_5()
        .child(
            div()
                .debug_selector(move || selector)
                .h_px()
                .w_full()
                .bg(theme.roles.line.divider.hsla),
        )
        .into_any_element()
}

/// One group of settings: the heading that names it, when it has one, and the card under it.
///
/// A page passes no heading when its row already says what the group is — the region a venue
/// account belongs to is one row named for itself, and repeating the name over it would be a
/// second title for one setting.
pub(crate) fn section(
    title: Option<SharedString>,
    id: &str,
    rows: Vec<AnyElement>,
    cx: &App,
) -> AnyElement {
    let heading_selector = format!("SETTINGS_SECTION_TITLE_{id}");
    let mut section = div().w_full().min_w_0().flex().flex_col().gap_2();

    if let Some(title) = title {
        section = section.child(
            div()
                .debug_selector(move || heading_selector)
                .child(section_title(title, cx)),
        );
    }

    section.child(card(id, rows, cx)).into_any_element()
}

/// The heading of one group.
pub(crate) fn section_title(title: SharedString, cx: &App) -> AnyElement {
    div()
        .text_sm()
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(cx.theme().roles.fg.default.hsla)
        .child(title)
        .into_any_element()
}

/// The column one page's groups are drawn in.
pub(crate) fn page(groups: Vec<AnyElement>) -> AnyElement {
    div()
        .debug_selector(|| "SETTINGS_PAGE_CONTENT".into())
        .w_full()
        .flex_none()
        .flex()
        .flex_col()
        .gap_4()
        .py_1()
        .children(groups)
        .into_any_element()
}

/// A page's content inside the viewport that scrolls it.
pub(crate) fn scroll(content: AnyElement, scroll: ScrollHandle) -> AnyElement {
    div()
        .relative()
        .size_full()
        .min_w_0()
        .min_h_0()
        .overflow_hidden()
        .child(
            div()
                .id("settings-page-scroll")
                .debug_selector(|| "SETTINGS_PAGE_VIEWPORT".into())
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&scroll)
                .child(content),
        )
        .child(base_ui::scroll_bar::Scrollbar::vertical(scroll))
        .into_any_element()
}

/// A select over a fixed set of choices, drawn in the width of the row that holds it.
///
/// The choices are the ids the settings store beside the words they are drawn with. A selected id
/// that no choice names is still selected: a value read from settings that no longer offers it is
/// what the user has, and the control that hid it would report a value the settings do not hold.
pub(crate) fn choice_select(
    id: ElementId,
    selected: Option<SharedString>,
    choices: impl IntoIterator<Item = (SharedString, SharedString)>,
    aria_label: SharedString,
    placeholder: Option<AnyElement>,
    disabled: bool,
    on_change: impl Fn(&Option<SharedString>, &mut Window, &mut App) + 'static,
) -> Select {
    let select = choices
        .into_iter()
        .fold(Select::new(id).tonal().w_full(), |select, (id, label)| {
            select.menu_item(MenuItem::Action(MenuItemAction::new(id, label)))
        });
    let select = match placeholder {
        Some(placeholder) => select.placeholder(placeholder),
        None => select,
    };

    select
        .selected_item(selected)
        .aria_label(aria_label)
        .disabled(disabled)
        .on_change(on_change)
}

/// The handler of a select's choice: writes the chosen id into the settings.
pub(crate) fn on_select(
    handle: &Entity<SettingsPage>,
    apply: impl Fn(&mut Config, &str) + 'static,
) -> impl Fn(&Option<SharedString>, &mut Window, &mut App) + 'static {
    let settings = handle.downgrade();
    let apply = Rc::new(apply);

    move |item_id, _, cx| {
        let Some(value) = item_id.clone() else {
            return;
        };
        let Some(settings) = settings.upgrade() else {
            return;
        };
        let apply = apply.clone();

        settings.update(cx, |settings, cx| {
            let value = value.clone();
            settings.apply(
                SettingsChange::edit(move |config| apply(config, value.as_str())),
                cx,
            );
        });
    }
}

/// The context a native page is drawn in.
///
/// It bundles what every page is handed rather than repeating five parameters in each page's
/// signature. The page owns nothing here: the settings are the session's, and the page entity is
/// the one drawing this frame.
pub(crate) struct PageArgs<'a, 'b> {
    pub config: &'a Config,
    pub filter: &'a crate::search::SearchFilter,
    pub settings: &'a mut SettingsPage,
    pub handle: &'a Entity<SettingsPage>,
    pub cx: &'a mut Context<'b, SettingsPage>,
}

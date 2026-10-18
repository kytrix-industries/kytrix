//! The page behind the selected tree entry: its sections, and the control each field draws.
//!
//! A control is built from what the catalog says about it, so this module is the only place
//! that turns a [`FieldAccess`] into an element. What a control is given is always the same
//! things — the settings this session holds, the dialog that owns the fields being typed
//! into, and the filter that decides which of the page's fields are drawn.
//!
//! What a control writes is that session rather than the saved settings: a choice is recorded
//! as a change to confirm, and the settings move only when the user confirms them.
//!
//! Text fields are the one kind that own state: a field is written when the caret leaves it
//! rather than on every keystroke, so a half-typed key is never recorded. What the dialog owns
//! is the input entities, keyed by field id — see
//! [`crate::dialog::SettingsDialog::input_for`].

use app_config::settings::Config;
use base_ui::{
    input::Input,
    interactive::InteractiveVariantExt,
    menu::{MenuItem, MenuItemAction},
    scroll_bar::Scrollbar,
    select::Select,
    separator::Separator,
    size::UnifiedSizable,
    style::theme::ThemeExt,
};
use gpui_kit::{
    AnyElement, App, Context, ElementId, Entity, InteractiveElement as _, IntoElement,
    ParentElement, ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled, Window,
    div,
};
use i18n::t;

use crate::{
    access::{FieldValue, PROXY_DIRECT, PROXY_INHERIT, PROXY_MIXED, PROXY_SYSTEM},
    catalog::{FieldAccess, FieldDef, PageDef, SelectOptionDef, Text},
    dialog::SettingsDialog,
    search::SearchFilter,
};

/// The page drawn behind one tree entry, or `None` for a page that has nothing to draw.
///
/// A page with no sections is drawn by something else — the proxy catalog is a table of its
/// own — so what it says here is that nothing is drawn from the declaration.
///
/// The scroll position is handed in rather than read back from the dialog: a page is drawn
/// while the dialog is being rendered, and the dialog cannot be read from inside its own
/// render. The settings are handed in for the same reason, and because what is drawn is the
/// session the dialog holds rather than what is saved.
pub(crate) fn page_body(
    page: &PageDef,
    config: &Config,
    filter: &SearchFilter,
    scroll: ScrollHandle,
    window: &mut Window,
    dialog: &mut SettingsDialog,
    handle: &Entity<SettingsDialog>,
    cx: &mut Context<SettingsDialog>,
) -> Option<AnyElement> {
    if page.sections.is_empty() {
        return None;
    }

    Some(page_scroll(
        page, config, filter, scroll, window, dialog, handle, cx,
    ))
}

/// The page's fields, inside the scroll viewport that holds them.
fn page_scroll(
    page: &PageDef,
    config: &Config,
    filter: &SearchFilter,
    scroll: ScrollHandle,
    window: &mut Window,
    dialog: &mut SettingsDialog,
    handle: &Entity<SettingsDialog>,
    cx: &mut Context<SettingsDialog>,
) -> AnyElement {
    let mut content = div()
        .debug_selector(|| "SETTINGS_PAGE_CONTENT".into())
        .w_full()
        .flex_none()
        .flex()
        .flex_col()
        .gap_4()
        .py_1();
    let mut visible = 0usize;

    for section in &page.sections {
        let fields = section
            .fields
            .iter()
            .filter(|field| filter.admits_field(&page.id, &field.id))
            .collect::<Vec<_>>();
        if fields.is_empty() {
            continue;
        }
        if visible > 0 && section.separated {
            content = content.child(Separator::horizontal());
        }
        if let Some(title) = section.title.as_ref() {
            content = content.child(section_title(title, cx));
        }
        for field in fields {
            visible += 1;
            content = content.child(field_row(field, config, window, dialog, handle, cx));
        }
    }

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
        .child(Scrollbar::vertical(scroll))
        .into_any_element()
}

/// One field: its label, its control, and what the control is for.
fn field_row(
    field: &FieldDef,
    config: &Config,
    window: &mut Window,
    dialog: &mut SettingsDialog,
    handle: &Entity<SettingsDialog>,
    cx: &mut Context<SettingsDialog>,
) -> AnyElement {
    let control = control(field, config, window, dialog, handle, cx);

    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .debug_selector({
            let id = field.id.clone();
            move || format!("SETTINGS_FIELD_{id}")
        })
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().roles.fg.default.hsla)
                .child(field.label.label(cx)),
        )
        .child(control)
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().roles.fg.muted.hsla)
                .child(field.description.label(cx)),
        )
        .into_any_element()
}

/// The control of one field, which is the only place a [`FieldAccess`] becomes an element.
fn control(
    field: &FieldDef,
    config: &Config,
    window: &mut Window,
    dialog: &mut SettingsDialog,
    handle: &Entity<SettingsDialog>,
    cx: &mut Context<SettingsDialog>,
) -> AnyElement {
    match &field.access {
        FieldAccess::Info { .. } => field
            .access
            .info(config)
            .unwrap_or_default()
            .into_any_element(),
        FieldAccess::Select { options, .. } => field_select(field, options, config, handle, cx),
        FieldAccess::ProxyLine { default_line, .. } => {
            field_proxy_line(field, *default_line, config, handle, cx)
        }
        FieldAccess::Text { .. } => field_text(field, config, window, dialog, cx),
    }
}

/// A fixed set of choices, recorded as a change to confirm as soon as one is chosen.
fn field_select(
    field: &FieldDef,
    options: &'static [SelectOptionDef],
    config: &Config,
    handle: &Entity<SettingsDialog>,
    cx: &mut Context<SettingsDialog>,
) -> AnyElement {
    let selected = field.access.current(config);
    let label = field.label.label(cx);
    let select = options
        .iter()
        .fold(
            Select::new(field_element_id("settings-select", &field.id)).tonal(),
            |select, option| {
                select.menu_item(MenuItem::Action(MenuItemAction::new(
                    option.id.as_str(),
                    option.label.label(cx),
                )))
            },
        )
        .selected_item(Some(SharedString::from(selected.as_str())))
        .aria_label(label)
        .on_change({
            let access = field.access.clone();
            let dialog = handle.downgrade();
            move |item_id, _, cx| {
                let Some(value) = item_id.clone() else {
                    return;
                };
                if let Some(dialog) = dialog.upgrade() {
                    dialog.update(cx, |dialog, cx| {
                        dialog.write_field(access.clone(), FieldValue::new(value.as_str()), cx);
                    });
                }
            }
        });

    div().w_full().min_w_0().child(select).into_any_element()
}

/// The line every target behind the field is bound to.
///
/// The choices are the entries of the session this dialog holds, which are not a compile-time
/// fact, so they are read from that session while the control is drawn. The line the row holds is
/// always one of them: a group whose targets disagree, and an entry the catalog no longer carries,
/// are the two lines a row can hold that no other choice names, and both are offered as
/// themselves — a control that showed anything else would report a binding the settings do not
/// have.
///
/// Every row can hold a line the settings can store, whatever the catalog holds: a row that may
/// follow the default line offers that choice even while no entry has been added yet, and the
/// entries of the catalog are offered beside it — to the row that sets the default line too.
fn field_proxy_line(
    field: &FieldDef,
    default_line: bool,
    config: &Config,
    handle: &Entity<SettingsDialog>,
    cx: &mut Context<SettingsDialog>,
) -> AnyElement {
    let selected = field.access.current(config);
    let label = field.label.label(cx);
    let mut select = Select::new(field_element_id("settings-proxy-line", &field.id)).tonal();

    if !default_line {
        select = select.menu_item(MenuItem::Action(MenuItemAction::new(
            PROXY_INHERIT,
            t!(cx, "settings_ui.proxies.inherit"),
        )));
    }
    select = select
        .menu_item(MenuItem::Action(MenuItemAction::new(
            PROXY_DIRECT,
            t!(cx, "settings_ui.proxies.no_proxy"),
        )))
        .menu_item(MenuItem::Action(MenuItemAction::new(
            PROXY_SYSTEM,
            t!(cx, "settings_ui.proxies.system_proxy"),
        )));
    if selected == PROXY_MIXED {
        select = select.menu_item(MenuItem::Action(MenuItemAction::new(
            PROXY_MIXED,
            t!(cx, "settings_ui.proxies.mixed"),
        )));
    } else {
        let held_by_the_catalog = config.proxies.contains_key(selected.as_str());
        let built_in = selected == PROXY_DIRECT
            || selected == PROXY_SYSTEM
            || (!default_line && selected == PROXY_INHERIT);
        if !held_by_the_catalog && !built_in {
            select = select.menu_item(MenuItem::Action(MenuItemAction::new(
                selected.as_str(),
                SharedString::from(selected.as_str()),
            )));
        }
    }
    for name in config.proxies.keys() {
        select = select.menu_item(MenuItem::Action(MenuItemAction::new(
            name.as_str(),
            SharedString::from(name.as_str()),
        )));
    }
    let access = field.access.clone();
    let dialog = handle.downgrade();

    div()
        .w_full()
        .min_w_0()
        // The box the control itself occupies, which is what a test presses to open the menu:
        // the row the field is drawn in holds its label above it and its description under it.
        .debug_selector({
            let id = field.id.clone();
            move || format!("SETTINGS_CONTROL_{id}")
        })
        .child(
            select
                .selected_item(Some(SharedString::from(selected.as_str())))
                .aria_label(label)
                .on_change(move |item_id, _, cx| {
                    let Some(value) = item_id.clone() else {
                        return;
                    };
                    if let Some(dialog) = dialog.upgrade() {
                        dialog.update(cx, |dialog, cx| {
                            dialog.write_field(access.clone(), FieldValue::new(value.as_str()), cx);
                        });
                    }
                }),
        )
        .into_any_element()
}

/// One line of text, recorded when the caret leaves it.
fn field_text(
    field: &FieldDef,
    config: &Config,
    window: &mut Window,
    dialog: &mut SettingsDialog,
    cx: &mut Context<SettingsDialog>,
) -> AnyElement {
    let stored = field.access.current(config);
    let input = dialog.input_for(field.clone(), &stored, window, cx);

    div()
        .w_full()
        .min_w_0()
        // The box the control itself occupies, which is what a test clicks to put the caret in
        // the field: the row the field is drawn in holds its label above it and its description
        // under it, so a point inside the row is not necessarily inside the field.
        .debug_selector({
            let id = field.id.clone();
            move || format!("SETTINGS_CONTROL_{id}")
        })
        .child(Input::new(&input).medium())
        .into_any_element()
}

/// The element id one field's control is keyed under.
///
/// The id is what keeps a control's state — an open menu, a caret — across the frames the
/// page is drawn in, so it is the field's own id rather than a position on the page.
fn field_element_id(prefix: &'static str, field_id: &SharedString) -> ElementId {
    ElementId::NamedChild(
        std::sync::Arc::new(ElementId::Name(SharedString::new_static(prefix))),
        field_id.clone(),
    )
}

/// The heading of one section.
fn section_title(title: &Text, cx: &App) -> AnyElement {
    div()
        .pt_1()
        .text_sm()
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(cx.theme().roles.fg.default.hsla)
        .child(title.label(cx))
        .into_any_element()
}

//! The widget gallery: the page in which a pane's widget is chosen.
//!
//! The gallery is a view rather than a window dialog: the workspace overlay embeds it
//! and owns what happens to a choice, while the gallery only reports one. It is built
//! from the registry rather than from a list of its own — the categories are
//! [`WidgetCategory::ALL`] and the widgets of a category are the registered widgets
//! that declare it and may be chosen — so a widget added to the catalogue appears here
//! without anything being written here. The one registration that is not listed is the
//! widget a pane falls back to: the framework builds it, the user never picks it, and
//! it is the pane's starting point rather than an answer to this page.
//!
//! What the page holds across frames is only what the user has done in it: the widget
//! the user has selected and the text the search field holds. Both open on the widget
//! the pane is already showing, so opening the gallery on a pane that has one is
//! opening it on the answer that pane already has.
//!
//! # What a selection means
//!
//! Selecting a row only changes what a confirmation would carry; the pane's widget is
//! not touched until the confirmation leaves this page. Cancelling therefore changes
//! nothing, and the confirm control refuses to confirm while nothing is selected.
//!
//! # Where the search narrows
//!
//! The field searches the whole catalogue rather than one category: every section is
//! drawn from the same match, a section a search leaves empty is not drawn, and a
//! search that matches nothing is drawn as a search that found nothing rather than as
//! a page with nothing on it. A widget matches when its localized name or its declared
//! description contains the text, case-insensitively; the description is the English
//! sentence the widget declares, because that is the text the catalogue carries.
//!
//! # What the page reports
//!
//! A confirmation and a cancellation are [`WidgetGalleryEvent`]s. The page does not
//! close itself and does not know what is behind it: what a confirmation changes, and
//! whether the page goes away, belongs to whoever embedded it.
//!
//! # Where the keyboard goes
//!
//! The page takes the keyboard when it opens — the search field is focused, so typing
//! narrows the catalogue at once — and holds it for as long as it is open: Tab and
//! Shift+Tab keep the keyboard in the page, and Escape and Enter are the page's own keys.
//! A pointer press in the page chooses a row or clears the field rather than moving the
//! keyboard, so what the reader types after a click still narrows the catalogue. The pane
//! behind the page is still there, with fields of its own, and a keystroke that reached one
//! of them would change a configuration the reader cannot see.

use base_ui::{
    button::Button,
    empty_state::EmptyState,
    icon::IconKind,
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    interactive::InteractiveVariantExt,
    scroll_bar::Scrollbar,
    separator::Separator,
    size::UnifiedSizable,
    style::theme::ThemeExt,
    toggle::Toggle,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement, Pixels, Render,
    ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled, Subscription, Window,
    base::{Disableable, Selectable as _},
    div, point, px,
};
use i18n::t;
use widget::{category::WidgetCategory, identity::WidgetId};
use widget_registry::registry::WidgetRegistry;

use crate::widget_icon::WidgetIcon;

/// The size of a widget's icon in the gallery's rows.
const ICON_SIZE: Pixels = px(32.0);

/// The height of one row, which holds the icon, the widget's name and description, and its
/// radio.
const ROW_HEIGHT: Pixels = px(56.0);

/// The width of the field the gallery is searched with.
const SEARCH_WIDTH: Pixels = px(240.0);

/// The widest the page's content column gets. The footer is drawn outside it, so it keeps the
/// page's full width while the header and catalogue are centred.
const CONTENT_MAX_WIDTH: Pixels = px(1000.0);

/// The element id of the scroll viewport, which is where the page remembers how far it
/// was scrolled.
const SCROLL_ID: &str = "widget-gallery-scroll";

/// The element id of the field the gallery is searched with.
const SEARCH_ID: &str = "widget-gallery-search";

/// Debug selectors of the page's own parts. The panel, confirm and cancel selectors are
/// what the workspace's integration tests look the page up by.
const PANEL_SELECTOR: &str = "WIDGET_GALLERY";
const COLUMN_SELECTOR: &str = "WIDGET_GALLERY_COLUMN";
const FOOTER_SELECTOR: &str = "WIDGET_GALLERY_FOOTER";
const SEARCH_SELECTOR: &str = "WIDGET_GALLERY_SEARCH";
const CONTENT_SELECTOR: &str = "WIDGET_GALLERY_CONTENT";
const VIEWPORT_SELECTOR: &str = "WIDGET_GALLERY_VIEWPORT";
const CONFIRM_SELECTOR: &str = "WIDGET_GALLERY_CONFIRM";
const CANCEL_SELECTOR: &str = "WIDGET_GALLERY_CANCEL";

/// The debug selector of one category's heading.
fn category_selector(category: WidgetCategory) -> &'static str {
    match category {
        WidgetCategory::Market => "WIDGET_GALLERY_CATEGORY_MARKET",
        WidgetCategory::Statistics => "WIDGET_GALLERY_CATEGORY_STATISTICS",
        WidgetCategory::Monitoring => "WIDGET_GALLERY_CATEGORY_MONITORING",
    }
}

/// What a choice made in the gallery is.
///
/// It is the page's whole output: [`WidgetGalleryEvent::Confirm`] carries the widget the
/// user selected and [`WidgetGalleryEvent::Cancel`] is the user leaving without choosing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WidgetGalleryEvent {
    /// The user confirmed the selection.
    Confirm(WidgetId),
    /// The user cancelled, by pressing the cancel control or Escape.
    Cancel,
}

/// One widget as the gallery lists it.
struct GalleryWidget {
    id: WidgetId,
    category: WidgetCategory,
    /// Message key of the widget's name.
    display_name: &'static str,
    /// The widget's own English sentence, drawn under its name.
    description: &'static str,
    /// The asset path the widget declared, if it declared one.
    icon: Option<&'static str>,
}

/// The page in which a pane's widget is chosen.
pub struct WidgetGallery {
    /// The field the catalogue is searched with.
    search: Entity<InputState>,
    /// The hint the field draws while it is empty, as it was last handed to the field. The
    /// field is built once and lives as long as the page, so a language change is found by
    /// translating the hint again and comparing it with this.
    search_placeholder: SharedString,
    /// The search the page is drawn against: what the field last reported, trimmed and
    /// lowercased once here rather than on every row.
    query: SharedString,
    /// The widget a confirmation would carry.
    selection: Option<WidgetId>,
    /// Every widget this page offers, in registry order, with the category it declares.
    widgets: Vec<GalleryWidget>,
    /// Where the page is scrolled to, which is put back to the top whenever the search
    /// changes what is drawn.
    scroll: ScrollHandle,
    /// Held because the subscription is what follows the field: dropping it would leave a
    /// field that is typed into and never searched.
    _search_change: Subscription,
}

impl WidgetGallery {
    /// The gallery, opened on the widget the pane is showing.
    ///
    /// `current` is the widget the pane has now, which is selected when the catalogue still
    /// offers it. The search field is built here because a field is built against a window,
    /// and the page is a view that is handed one rather than one that holds a window.
    pub fn new(current: Option<WidgetId>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let widgets = cx
            .global::<WidgetRegistry>()
            .iter()
            .filter(|registration| registration.descriptor().selectable())
            .map(|registration| {
                let descriptor = registration.descriptor();

                GalleryWidget {
                    id: descriptor.id().clone(),
                    category: descriptor.category(),
                    display_name: descriptor.display_name(),
                    description: descriptor.description(),
                    icon: descriptor.icon(),
                }
            })
            .collect::<Vec<_>>();
        // The page opens on the widget the pane is showing, when it is still an answer the
        // catalogue offers. A pane showing the framework's fallback opens with nothing
        // selected: there is no answer of the user's to preselect.
        let selection =
            current.filter(|current| widgets.iter().any(|widget| &widget.id == current));
        let search_placeholder = t!(cx, "widget_gallery.search_placeholder");
        let search = cx.new(|cx| {
            InputStateBuilder::new(SEARCH_ID)
                .placeholder(search_placeholder.clone())
                .build(window, cx)
        });
        // What is typed is followed through the events the field emits rather than read back
        // off it: the element is rebuilt every frame, and a keystroke has to redraw the
        // catalogue the page draws from it.
        let search_change = cx.subscribe(&search, |gallery, _, event: &InputStateEvent, cx| {
            let InputStateEvent::Change(value) = event else {
                return;
            };
            gallery.set_query(value.as_ref(), cx);
        });
        // The page opens with the caret in the field, so the reader narrows the catalogue by
        // typing rather than by finding the field with the pointer first. The page still
        // handles its own keys: Escape and Enter leave the field and reach the page.
        let search_focus = search.read(cx).engine_focus_handle(cx);
        window.focus(&search_focus, cx);

        Self {
            search,
            search_placeholder,
            query: SharedString::default(),
            selection,
            widgets,
            scroll: ScrollHandle::new(),
            _search_change: search_change,
        }
    }

    /// Draws the page against another search, and puts it back to the top: what the reader
    /// was looking at was the list the old search left.
    fn set_query(&mut self, query: &str, cx: &mut Context<Self>) {
        let query = SharedString::from(query.trim().to_lowercase());
        if self.query == query {
            return;
        }

        self.query = query;
        self.scroll.set_offset(point(px(0.0), px(0.0)));
        cx.notify();
    }

    /// Keeps the field's hint in the language the page is drawn in.
    ///
    /// The field is built once and lives as long as the page, so a language change while the
    /// page is open would otherwise leave the hint in the language the page opened in. The
    /// hint is translated again on every draw and compared with the one the field holds,
    /// because a draw is the one place the page has both the language and a window to set the
    /// hint with.
    fn localize_search_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = t!(cx, "widget_gallery.search_placeholder");
        if self.search_placeholder == placeholder {
            return;
        }

        self.search_placeholder = placeholder.clone();
        self.search.update(cx, |search, cx| {
            search.update_placeholder(placeholder, window, cx);
        });
    }

    /// Selects one widget of the catalogue.
    fn select(&mut self, widget_id: &WidgetId, cx: &mut Context<Self>) {
        if self.selection.as_ref() == Some(widget_id) {
            return;
        }

        self.selection = Some(widget_id.clone());
        cx.notify();
    }

    /// Whether one widget is named by the search the page is drawn against.
    ///
    /// The name is the localized one, because that is what the reader sees; the
    /// description is the widget's own English sentence, because that is what the
    /// catalogue declares. Both are compared case-insensitively, and a search of nothing
    /// names everything.
    fn matches(&self, widget: &GalleryWidget, cx: &App) -> bool {
        if self.query.is_empty() {
            return true;
        }

        t!(cx, widget.display_name)
            .to_lowercase()
            .contains(self.query.as_str())
            || widget
                .description
                .to_lowercase()
                .contains(self.query.as_str())
    }

    /// The widgets the search leaves, grouped by the category they are listed under.
    ///
    /// The groups are [`WidgetCategory::ALL`] in order and a group the search leaves empty
    /// is not there: the page draws sections only where there is something to choose.
    fn groups(&self, cx: &App) -> Vec<(WidgetCategory, Vec<&GalleryWidget>)> {
        WidgetCategory::ALL
            .into_iter()
            .filter_map(|category| {
                let widgets = self
                    .widgets
                    .iter()
                    .filter(|widget| widget.category == category && self.matches(widget, cx))
                    .collect::<Vec<_>>();

                (!widgets.is_empty()).then_some((category, widgets))
            })
            .collect()
    }

    /// Confirms the widget the page has selected, if it has one.
    ///
    /// The guard is the control's behavior as well as the keyboard's: `Button::disabled`
    /// paints a control as disabled without stopping the handler that was attached to it.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        let Some(widget_id) = self.selection.clone() else {
            return;
        };

        cx.emit(WidgetGalleryEvent::Confirm(widget_id));
    }

    /// Handles the page's keys: Escape leaves, Enter confirms, and Tab and Shift+Tab keep the
    /// keyboard in the page.
    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "escape" => {
                cx.emit(WidgetGalleryEvent::Cancel);
                cx.stop_propagation();
            }
            "enter" => {
                if self.selection.is_none() {
                    return;
                }
                self.confirm(cx);
                cx.stop_propagation();
            }
            "tab" => self.keep_focus(window, cx),
            _ => {}
        }
    }

    /// Puts the keyboard back on the field, which is the page's whole tab cycle.
    ///
    /// The field is the only control in the page that takes focus: a row and a footer button
    /// are pressed with the pointer, not reached with Tab. Walking the window's tab order
    /// would therefore take the keyboard out of the page and into the pane behind it, so the
    /// page names the stop it has instead of asking the window for the next one.
    fn keep_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let search = self.search.read(cx).engine_focus_handle(cx);
        search.focus(window, cx);
        cx.stop_propagation();
    }

    /// The top of the page: what it is for, and the field it is searched with.
    fn header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();

        div()
            .flex_none()
            .w_full()
            .min_w_0()
            .flex()
            .flex_wrap()
            .items_end()
            .justify_between()
            .gap_4()
            .px_6()
            .pt_5()
            .pb_3()
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::BOLD)
                            .child(t!(cx, "widget_gallery.title")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.roles.fg.muted.hsla)
                            .child(t!(cx, "widget_gallery.subtitle")),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| String::from(SEARCH_SELECTOR))
                    .flex_none()
                    .w(SEARCH_WIDTH)
                    .child(
                        Input::new(&self.search)
                            .medium()
                            .clearable(true)
                            .prefix_icon(IconKind::MagnifyingGlass),
                    ),
            )
            .into_any_element()
    }

    /// The catalogue, in its categories, inside the viewport that scrolls it.
    ///
    /// The body does not scroll by itself: the viewport inside it does, so the scrollbar
    /// the page draws is the one that reaches the rows that do not fit rather than one
    /// that belongs to a grid.
    fn body(&self, cx: &mut Context<Self>) -> AnyElement {
        let groups = self.groups(cx);
        let mut content = div()
            .debug_selector(|| String::from(CONTENT_SELECTOR))
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap_6()
            .px_6()
            .pb_4();

        for (category, widgets) in &groups {
            content = content.child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .debug_selector(|| String::from(category_selector(*category)))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!(cx, category.label())),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .grid()
                            .grid_cols(2)
                            .gap_2()
                            .children(
                                widgets
                                    .iter()
                                    .map(|widget| self.row(widget, cx))
                                    .collect::<Vec<_>>(),
                            ),
                    ),
            );
        }

        // A search that matched nothing is drawn as a search that found nothing rather than
        // as the empty catalogue: the two are different facts, and only one of them is
        // answered by typing something else.
        let no_matches = groups.is_empty().then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyState::new(
                    "widget-gallery-no-matches",
                    t!(cx, "common.no_matches"),
                ))
        });

        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .relative()
            .overflow_hidden()
            .child(
                div()
                    .id(SCROLL_ID)
                    .debug_selector(|| String::from(VIEWPORT_SELECTOR))
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(content),
            )
            .children(no_matches)
            .child(Scrollbar::vertical(self.scroll.clone()))
            .into_any_element()
    }

    /// One widget in the catalogue: what it is called, what it does, and whether it is the
    /// one a confirmation would carry.
    ///
    /// The whole row is the control. A row that could only be chosen by hitting its radio
    /// would be a list that is hard to use with a pointer, so the radio draws the state and
    /// the row takes the click — the radio is inside the row's hit area, so a click on it
    /// is a click on the row.
    fn row(&self, widget: &GalleryWidget, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selection.as_ref() == Some(&widget.id);
        let theme = cx.theme();
        let name = t!(cx, widget.display_name);
        let description = SharedString::new_static(widget.description);
        let widget_id = widget.id.clone();
        let row_element_id = (
            ElementId::Name(SharedString::new_static("WidgetGalleryRow")),
            SharedString::new(widget.id.as_str()),
        );
        let row_selector = format!("WIDGET_GALLERY_ROW_{}", widget.id);
        let radio_selector = format!("WIDGET_GALLERY_RADIO_{}", widget.id);

        let mut text = div()
            .min_w_0()
            .flex_1()
            .flex()
            .flex_col()
            .gap_0p5()
            .child(div().text_sm().truncate().child(name.clone()));
        if !description.is_empty() {
            text = text.child(
                div()
                    .truncate()
                    .text_xs()
                    .text_color(theme.roles.fg.muted.hsla)
                    .child(description),
            );
        }

        Button::new(row_element_id)
            .ghost()
            .w_full()
            .min_w_0()
            .h(ROW_HEIGHT)
            .px_3()
            .gap_3()
            .selected(selected)
            .debug_selector(move || row_selector)
            .aria_label(name)
            .child(WidgetIcon::new(widget.icon, ICON_SIZE))
            .child(text)
            .child(
                Toggle::new((
                    ElementId::Name(SharedString::new_static("WidgetGalleryRadio")),
                    SharedString::new(widget.id.as_str()),
                ))
                .radio()
                .small()
                .checked(selected)
                .debug_selector(move || radio_selector),
            )
            .on_click(cx.listener(move |gallery, _, _, cx| gallery.select(&widget_id, cx)))
            .into_any_element()
    }

    /// The two decisions: leave without choosing, or confirm the widget that is selected.
    ///
    /// The confirm control refuses input while nothing is selected: a page with a search and
    /// no choice holds no answer, and a control that confirmed nothing would leave as if it
    /// had chosen something.
    fn footer(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .debug_selector(|| String::from(FOOTER_SELECTOR))
            .flex_none()
            .w_full()
            .flex()
            .flex_col()
            .child(Separator::horizontal())
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .px_6()
                    .py_3()
                    .child(
                        Button::new("widget-gallery-cancel")
                            .tonal()
                            .medium()
                            .debug_selector(|| String::from(CANCEL_SELECTOR))
                            .aria_label(t!(cx, "common.cancel"))
                            .child(t!(cx, "common.cancel"))
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(WidgetGalleryEvent::Cancel);
                            })),
                    )
                    .child(
                        Button::new("widget-gallery-confirm")
                            .filled()
                            .medium()
                            .debug_selector(|| String::from(CONFIRM_SELECTOR))
                            .aria_label(t!(cx, "common.confirm"))
                            .disabled(self.selection.is_none())
                            .child(t!(cx, "common.confirm"))
                            .on_click(cx.listener(|gallery, _, _, cx| gallery.confirm(cx))),
                    ),
            )
            .into_any_element()
    }
}

/// A confirmation is a choice for the workspace to apply, and a cancellation is the user
/// leaving without one.
impl EventEmitter<WidgetGalleryEvent> for WidgetGallery {}

impl Render for WidgetGallery {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.localize_search_placeholder(window, cx);
        let header = self.header(cx);
        let body = self.body(cx);
        let footer = self.footer(cx);

        div()
            .debug_selector(|| String::from(PANEL_SELECTOR))
            .on_key_down(cx.listener(Self::handle_key_down))
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(cx.theme().roles.surface.canvas.hsla)
            .text_color(cx.theme().roles.fg.default.hsla)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .debug_selector(|| String::from(COLUMN_SELECTOR))
                            .w(CONTENT_MAX_WIDTH)
                            .max_w_full()
                            .h_full()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(header)
                            .child(body),
                    ),
            )
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, ffi::OsStr, path::Path, rc::Rc};

    use base_ui::input::{Input, InputState, InputStateBuilder};
    use gpui_kit::{
        App, AppContext as _, AssetSource as _, Bounds, Context, Entity, FocusHandle, Img,
        IntoElement, KeyDownEvent, Keystroke, Modifiers, ParentElement, Pixels, Render,
        Styled as _, Subscription, TestAppContext, VisualTestContext, Window, div, point, px, size,
    };
    use i18n::language::codes::LanguageCode;
    use widget::{category::WidgetCategory, identity::WidgetId};
    use widget_registry::registry::WidgetRegistry;

    use super::{
        CANCEL_SELECTOR, COLUMN_SELECTOR, CONFIRM_SELECTOR, CONTENT_SELECTOR, FOOTER_SELECTOR,
        PANEL_SELECTOR, SEARCH_SELECTOR, VIEWPORT_SELECTOR, WidgetGallery, WidgetGalleryEvent,
        category_selector,
    };

    /// Debug selectors of the rows and radios a test reaches for by name.
    const CANDLESTICK_ROW: &str = "WIDGET_GALLERY_ROW_kytrix.candlestick";
    const NEWS_ROW: &str = "WIDGET_GALLERY_ROW_kytrix.news";
    const EMPTY_ROW: &str = "WIDGET_GALLERY_ROW_kytrix.empty";
    const CANDLESTICK_RADIO: &str = "WIDGET_GALLERY_RADIO_kytrix.candlestick";

    /// The debug selector the page draws the row of one catalogue entry with.
    fn row_selector(widget_id: &str) -> &'static str {
        Box::leak(format!("WIDGET_GALLERY_ROW_{widget_id}").into_boxed_str())
    }

    /// What a gallery test listens for.
    type Emitted = Rc<RefCell<Vec<WidgetGalleryEvent>>>;

    /// The gallery a test drives, the field drawn behind it when there is one, the events it
    /// emitted, and the window it is drawn in.
    struct GalleryHarness<'a> {
        gallery: Entity<WidgetGallery>,
        /// The field of the window behind the page, in the overlay harnesses.
        background: Option<Entity<InputState>>,
        emitted: Emitted,
        _emitted_subscription: Subscription,
        cx: &'a mut VisualTestContext,
    }

    impl GalleryHarness<'_> {
        /// The widget a confirmation would carry, as the harness reads it.
        fn selection(&self) -> Option<String> {
            self.cx.read(|cx| {
                self.gallery
                    .read(cx)
                    .selection
                    .as_ref()
                    .map(|id| id.as_str().to_owned())
            })
        }

        /// The search the page is drawn against.
        fn query(&self) -> String {
            self.cx.read(|cx| self.gallery.read(cx).query.to_string())
        }

        /// Every widget the search leaves, in the order the page lists them.
        fn listed(&self) -> Vec<String> {
            self.cx.read(|cx| {
                self.gallery
                    .read(cx)
                    .groups(cx)
                    .into_iter()
                    .flat_map(|(_, widgets)| {
                        widgets
                            .into_iter()
                            .map(|widget| widget.id.as_str().to_owned())
                            .collect::<Vec<_>>()
                    })
                    .collect()
            })
        }

        /// The widgets the search leaves, grouped by the category they are listed under.
        fn groups(&self) -> Vec<(WidgetCategory, Vec<String>)> {
            self.cx.read(|cx| {
                self.gallery
                    .read(cx)
                    .groups(cx)
                    .into_iter()
                    .map(|(category, widgets)| {
                        (
                            category,
                            widgets
                                .into_iter()
                                .map(|widget| widget.id.as_str().to_owned())
                                .collect(),
                        )
                    })
                    .collect()
            })
        }

        fn emitted(&self) -> Vec<WidgetGalleryEvent> {
            self.emitted.borrow().clone()
        }

        /// The focus handle of the field the page is searched with.
        fn search_focus_handle(&self) -> FocusHandle {
            self.cx.read(|cx| {
                self.gallery
                    .read(cx)
                    .search
                    .read(cx)
                    .engine_focus_handle(cx)
            })
        }

        /// The focus handle of the field drawn behind the page.
        fn background_focus_handle(&self) -> FocusHandle {
            self.cx.read(|cx| {
                self.background
                    .as_ref()
                    .expect("the overlay harness should draw a field behind the page")
                    .read(cx)
                    .engine_focus_handle(cx)
            })
        }

        /// The element the window has the keyboard on.
        fn focused(&mut self) -> Option<FocusHandle> {
            self.cx.update(|window, cx| window.focused(cx))
        }

        /// Switches the interface language and redraws the page in it.
        fn set_language(&mut self, language: LanguageCode) {
            self.cx.update(|_, cx| cx.set_global(language));
            self.settle();
        }

        /// The rectangle the page drew one widget's row in.
        fn row(&mut self, widget_id: &str) -> Bounds<Pixels> {
            self.cx
                .debug_bounds(row_selector(widget_id))
                .unwrap_or_else(|| panic!("the page should draw the row for {widget_id}"))
        }

        /// The rectangle the page drew one category's heading in.
        fn heading(&mut self, category: WidgetCategory) -> Bounds<Pixels> {
            self.cx
                .debug_bounds(category_selector(category))
                .unwrap_or_else(|| panic!("the page should draw the {category:?} heading"))
        }

        /// Repaints, so what the next `debug_bounds` says is what the last change drew.
        fn settle(&mut self) {
            self.cx.update(|window, _| window.refresh());
            self.cx.run_until_parked();
        }

        /// Clicks the element drawn at one debug selector.
        fn click(&mut self, selector: &'static str) {
            let target = self
                .cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("the gallery should draw {selector}"))
                .center();
            self.cx.simulate_click(target, Modifiers::none());
            self.settle();
        }

        /// Clicks the field the page is searched with and types into it.
        fn type_into_search(&mut self, typed: &str) {
            let field = self
                .cx
                .debug_bounds(SEARCH_SELECTOR)
                .expect("the page should draw the field it is searched with")
                .center();
            self.cx.simulate_click(field, Modifiers::none());
            self.cx.run_until_parked();
            self.cx.simulate_input(typed);
            self.settle();
        }

        /// Types into whatever holds the keyboard, without pointing at the field first.
        fn type_without_clicking(&mut self, typed: &str) {
            self.cx.simulate_input(typed);
            self.settle();
        }

        /// Clicks the clear control the field draws while it holds text.
        fn clear_search(&mut self) {
            let field = self
                .cx
                .debug_bounds(SEARCH_SELECTOR)
                .expect("the page should draw the field it is searched with");
            let target = point(field.right() - px(10.0), field.center().y);
            self.cx.simulate_click(target, Modifiers::none());
            self.settle();
        }

        /// Sends a keystroke to the window the page is drawn in.
        fn press(&mut self, keystrokes: &str) {
            self.cx.simulate_keystrokes(keystrokes);
            self.settle();
        }
    }

    /// The harness for a window whose gallery was made in `build`.
    fn harness<'a>(
        cx: &'a mut VisualTestContext,
        gallery: Entity<WidgetGallery>,
        background: Option<Entity<InputState>>,
    ) -> GalleryHarness<'a> {
        let emitted: Emitted = Rc::new(RefCell::new(Vec::new()));
        let sink = emitted.clone();
        let subscription = cx.update(|_, cx| {
            cx.subscribe(&gallery, move |_, event: &WidgetGalleryEvent, _| {
                sink.borrow_mut().push(event.clone());
            })
        });
        cx.run_until_parked();
        let mut harness = GalleryHarness {
            gallery,
            background,
            emitted,
            _emitted_subscription: subscription,
            cx,
        };
        harness.settle();
        harness
    }

    /// A window whose only content is the gallery, drawn for `current`.
    fn gallery<'a>(cx: &'a mut TestAppContext, current: Option<&str>) -> GalleryHarness<'a> {
        cx.update(install);
        let current = current.map(widget);
        let (root, cx) = cx.add_window_view(move |window, cx| GalleryRoot {
            gallery: cx.new(|cx| WidgetGallery::new(current, window, cx)),
        });
        cx.simulate_resize(size(px(1000.0), px(1000.0)));
        cx.run_until_parked();
        let gallery = cx.update(|_, cx| root.read(cx).gallery.clone());
        harness(cx, gallery, None)
    }

    /// A window whose content is a field with the gallery drawn over it, which is how the
    /// workspace draws the page over the pane it was opened from.
    fn overlay_gallery<'a>(
        cx: &'a mut TestAppContext,
        current: Option<&str>,
    ) -> GalleryHarness<'a> {
        cx.update(install);
        let current = current.map(widget);
        let (root, cx) = cx.add_window_view(move |window, cx| {
            let background =
                cx.new(|cx| InputStateBuilder::new("background-input").build(window, cx));
            let gallery = cx.new(|cx| WidgetGallery::new(current, window, cx));
            OverlayRoot {
                background,
                gallery,
            }
        });
        cx.simulate_resize(size(px(1000.0), px(1000.0)));
        cx.run_until_parked();
        let (background, gallery) = cx.update(|_, cx| {
            let root = root.read(cx);

            (root.background.clone(), root.gallery.clone())
        });
        harness(cx, gallery, Some(background))
    }

    /// Installs what the gallery is built on: the base input layer, the theme, the
    /// translator and the catalogue.
    fn install(cx: &mut App) {
        gpui_kit::base::init(cx);
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("embedded locales should load");
        widget_registry::catalogue::install(cx).expect("the catalogue should install");
    }

    fn widget(id: &str) -> WidgetId {
        WidgetId::parse(id).expect("the test widget id should be valid")
    }

    /// Every registration in the catalogue, in registry order, with whether the page may
    /// offer it. The listing test checks the drawn rows against this rather than rebuilding
    /// the page's own filtered list.
    fn registry_entries(cx: &App) -> Vec<(WidgetId, bool)> {
        cx.global::<WidgetRegistry>()
            .iter()
            .map(|registration| {
                let descriptor = registration.descriptor();

                (descriptor.id().clone(), descriptor.selectable())
            })
            .collect()
    }

    struct GalleryRoot {
        gallery: Entity<WidgetGallery>,
    }

    impl Render for GalleryRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.gallery.clone())
        }
    }

    /// A window that keeps a field of its own with the gallery drawn over it, which is what
    /// an open page covers in the workspace.
    struct OverlayRoot {
        background: Entity<InputState>,
        gallery: Entity<WidgetGallery>,
    }

    impl Render for OverlayRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .relative()
                .size_full()
                .child(div().w(px(320.0)).child(Input::new(&self.background)))
                .child(div().absolute().inset_0().child(self.gallery.clone()))
        }
    }

    /// The page offers each widget the catalogue says may be chosen, and it offers nothing
    /// else: the test looks up the row the page drew for every registration rather than
    /// rebuilding the page's own filtered list.
    #[gpui_kit::test]
    fn the_gallery_should_list_every_widget_that_may_be_chosen_once(cx: &mut TestAppContext) {
        let harness = gallery(cx, None);
        let entries = harness.cx.read(registry_entries);
        let mut offered = 0;

        for (widget_id, selectable) in entries {
            let drawn = harness
                .cx
                .debug_bounds(row_selector(widget_id.as_str()))
                .is_some();
            assert_eq!(
                drawn, selectable,
                "{widget_id}: a row is drawn exactly when the catalogue offers the widget"
            );
            if selectable {
                offered += 1;
            }
        }

        assert!(offered > 0, "the catalogue should offer widgets");
        assert!(
            harness.cx.debug_bounds(EMPTY_ROW).is_none(),
            "the pane's fallback is not an answer this page offers"
        );
    }

    /// A pane showing a widget opens the gallery on that widget, which is the answer the
    /// pane already has.
    #[gpui_kit::test]
    fn a_pane_with_a_widget_should_open_on_it(cx: &mut TestAppContext) {
        let harness = gallery(cx, Some("kytrix.news"));

        assert_eq!(harness.selection().as_deref(), Some("kytrix.news"));
    }

    /// The widget a pane falls back to is not an answer this page offers, so a pane showing
    /// it opens with nothing selected.
    #[gpui_kit::test]
    fn a_pane_showing_the_fallback_should_open_with_nothing_selected(cx: &mut TestAppContext) {
        let harness = gallery(cx, Some("kytrix.empty"));

        assert_eq!(harness.selection(), None);
    }

    /// The catalogue is drawn in the categories of [`WidgetCategory::ALL`]: a category the
    /// page has something to list gets a heading, its rows are drawn under that heading and
    /// above the next one, and a category the page has nothing for is not drawn.
    ///
    /// The groups are the page's own, and the assertions are against the rectangles the last
    /// frame drew, so adding or reordering widgets cannot make the test disagree with the
    /// page.
    #[gpui_kit::test]
    fn the_gallery_should_group_widgets_under_their_category(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, None);
        let groups = harness.groups();
        assert!(
            !groups.is_empty(),
            "the catalogue should have something to list"
        );

        let mut previous_heading: Option<Pixels> = None;
        for (category, widget_ids) in &groups {
            let heading = harness.heading(*category);
            if let Some(previous_heading) = previous_heading {
                assert!(
                    previous_heading < heading.top(),
                    "the {category:?} heading should be drawn below the heading before it"
                );
            }
            previous_heading = Some(heading.top());

            for widget_id in widget_ids {
                assert!(
                    harness.row(widget_id).top() >= heading.bottom(),
                    "{widget_id} should be drawn below the {category:?} heading"
                );
            }
        }

        // A row belongs to the group whose heading it is drawn under: every row of a group
        // ends above the heading of the next group.
        for pair in groups.windows(2) {
            let (_, widget_ids) = &pair[0];
            let next_heading = harness.heading(pair[1].0).top();
            for widget_id in widget_ids {
                assert!(
                    harness.row(widget_id).bottom() <= next_heading,
                    "{widget_id} should be drawn above the next category's heading"
                );
            }
        }

        // A category the page has nothing for has no heading either.
        for category in WidgetCategory::ALL {
            let grouped = groups.iter().any(|(grouped, _)| *grouped == category);
            assert_eq!(
                harness
                    .cx
                    .debug_bounds(category_selector(category))
                    .is_some(),
                grouped,
                "the {category:?} heading should be drawn exactly when the page lists rows for it"
            );
        }
    }

    /// The cards are drawn in two columns: the widgets of a category fill a row of the grid
    /// before the next pair is drawn under them, and a card is never drawn beside one from
    /// another pair.
    ///
    /// The test walks the rows the page actually lists, so a category with one widget — or
    /// an odd last card — needs no special case.
    #[gpui_kit::test]
    fn the_catalogue_should_be_drawn_in_two_columns(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, None);
        let groups = harness.groups();
        let mut pairs = 0;

        for (category, widget_ids) in &groups {
            let rows = widget_ids
                .iter()
                .map(|widget_id| harness.row(widget_id))
                .collect::<Vec<_>>();

            for index in 2..rows.len() {
                assert!(
                    rows[index].top() >= rows[index - 2].bottom(),
                    "the {category:?} card at {index} should be drawn below the card in its own column"
                );
            }
            for pair in rows.chunks(2) {
                let [left, right] = pair else {
                    continue;
                };
                assert_eq!(
                    left.top(),
                    right.top(),
                    "the two cards of a {category:?} pair should share a row"
                );
                assert!(
                    left.left() < right.left(),
                    "the cards of a {category:?} pair should be drawn in two columns"
                );
                pairs += 1;
            }
        }

        assert!(
            pairs > 0,
            "at least one category should fill a two-column row"
        );
    }

    /// The header and the catalogue are one 800-pixel column in the middle of the page. The
    /// footer is outside that column, so it keeps the page's full width.
    #[gpui_kit::test]
    fn the_page_should_cap_its_content_width_and_centre_it_above_the_footer(
        cx: &mut TestAppContext,
    ) {
        let harness = gallery(cx, None);
        let panel = harness
            .cx
            .debug_bounds(PANEL_SELECTOR)
            .expect("the page should draw its panel");
        let column = harness
            .cx
            .debug_bounds(COLUMN_SELECTOR)
            .expect("the page should draw its content column");
        let footer = harness
            .cx
            .debug_bounds(FOOTER_SELECTOR)
            .expect("the page should draw its footer");
        let search = harness
            .cx
            .debug_bounds(SEARCH_SELECTOR)
            .expect("the page should draw its search field");

        assert_eq!(column.size.width, px(800.0));
        assert_eq!(column.center().x, panel.center().x);
        assert_eq!(footer.size.width, panel.size.width);
        assert_eq!(footer.left(), panel.left());
        assert!(footer.top() >= column.bottom());
        assert!(search.left() >= column.left());
        assert!(search.right() <= column.right());
    }

    /// A page narrower than the column fills the width it has instead of overflowing it.
    #[gpui_kit::test]
    fn a_narrow_page_should_fill_its_width_instead_of_overflowing(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, None);
        harness.cx.simulate_resize(size(px(600.0), px(800.0)));
        harness.settle();

        let panel = harness
            .cx
            .debug_bounds(PANEL_SELECTOR)
            .expect("the page should draw its panel");
        let column = harness
            .cx
            .debug_bounds(COLUMN_SELECTOR)
            .expect("the page should draw its content column");

        assert_eq!(column.size.width, panel.size.width);
        assert_eq!(column.left(), panel.left());
    }

    /// The search matches the name a reader sees, case-insensitively, and the sections it
    /// leaves empty are not drawn.
    #[gpui_kit::test]
    fn typing_a_name_should_narrow_the_gallery(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, None);

        harness.type_into_search("CANDLE");

        assert_eq!(harness.listed(), ["kytrix.candlestick"]);
        assert!(
            harness.cx.debug_bounds(CANDLESTICK_ROW).is_some(),
            "the widget the search names is drawn"
        );
        assert!(
            harness.cx.debug_bounds(NEWS_ROW).is_none(),
            "a widget the search does not name is not drawn"
        );
        assert!(
            harness
                .cx
                .debug_bounds(category_selector(WidgetCategory::Statistics))
                .is_none(),
            "a category the search leaves empty is not drawn"
        );
    }

    /// The search matches the English description a widget declares as well as its name,
    /// which is the sentence the catalogue carries rather than a translated one.
    #[gpui_kit::test]
    fn typing_a_description_should_narrow_the_gallery(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, None);

        harness.type_into_search("ohlcv");

        assert_eq!(
            harness.listed(),
            ["kytrix.candlestick"],
            "the query names the widget's description, not its displayed name"
        );
    }

    /// The name a search matches is translated while the description is not: with the
    /// interface in Chinese the widget's Chinese name narrows the list, and the English
    /// sentence the catalogue carries still does.
    #[gpui_kit::test]
    fn typing_a_localized_name_should_narrow_the_gallery(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, None);
        harness.set_language(LanguageCode::ZhCn);

        harness.type_into_search("线");
        assert_eq!(
            harness.listed(),
            ["kytrix.candlestick"],
            "the Chinese name of the candlestick widget should be matched"
        );
        assert!(
            harness.cx.debug_bounds(CANDLESTICK_ROW).is_some(),
            "the matched widget should be drawn"
        );

        harness.clear_search();
        harness.type_into_search("OHLCV");
        assert_eq!(
            harness.listed(),
            ["kytrix.candlestick"],
            "the English description should stay searchable in the Chinese interface"
        );
    }

    /// Clearing the field restores the whole catalogue, because the page is drawn from what
    /// the field holds.
    #[gpui_kit::test]
    fn clearing_the_search_should_restore_the_whole_gallery(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, None);
        let whole = harness.listed();

        harness.type_into_search("ohlcv");
        assert_eq!(harness.listed().len(), 1);

        harness.clear_search();

        assert!(
            harness.query().is_empty(),
            "the field should have been cleared"
        );
        assert_eq!(harness.listed(), whole);
    }

    /// The control that clears the field leaves the caret in it: clearing is an edit of the
    /// search, not a reason to stop searching, so what the reader types next still narrows
    /// the catalogue without pointing at the field again.
    #[gpui_kit::test]
    fn typing_after_clearing_the_search_should_still_narrow_the_gallery(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, None);
        let search = harness.search_focus_handle();

        harness.type_into_search("ohlcv");
        assert_eq!(harness.listed(), ["kytrix.candlestick"]);

        harness.clear_search();
        assert!(
            harness.query().is_empty(),
            "the field should have been cleared"
        );
        assert_eq!(
            harness.focused().as_ref(),
            Some(&search),
            "clearing the field should leave the keyboard in it"
        );

        harness.type_without_clicking("news");
        assert_eq!(harness.query(), "news");
        assert_eq!(harness.listed(), ["kytrix.news"]);
    }

    /// A search that matches nothing is drawn as a search that found nothing rather than as
    /// a catalogue with nothing in it.
    #[gpui_kit::test]
    fn a_search_that_matches_nothing_should_say_so(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, None);

        harness.type_into_search("nothing here");

        assert!(harness.listed().is_empty());
        assert!(
            harness.cx.debug_bounds("EMPTY_STATE").is_some(),
            "the page says the search matched nothing"
        );
        assert!(
            harness.cx.debug_bounds(CANDLESTICK_ROW).is_none(),
            "and the rows it left out are not drawn"
        );
    }

    /// Selecting a row replaces the selection rather than adding to it: the page carries the
    /// one widget a confirmation would name, and a click on the radio is a click on the row.
    #[gpui_kit::test]
    fn choosing_another_widget_should_replace_the_selection(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, None);

        harness.click(NEWS_ROW);
        assert_eq!(harness.selection().as_deref(), Some("kytrix.news"));

        harness.click(CANDLESTICK_RADIO);
        assert_eq!(
            harness.selection().as_deref(),
            Some("kytrix.candlestick"),
            "the newly chosen widget replaces the previous one"
        );

        harness.press("enter");
        assert_eq!(
            harness.emitted(),
            [WidgetGalleryEvent::Confirm(widget("kytrix.candlestick"))],
            "the keyboard confirms the widget chosen by pointer"
        );
    }

    /// Choosing a row is a pointer press in the catalogue, not a move of the keyboard: the
    /// search field keeps the caret, so what the reader types next still narrows the list
    /// rather than being typed into a page that cannot take it.
    #[gpui_kit::test]
    fn typing_after_choosing_a_widget_should_still_narrow_the_gallery(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, None);
        let search = harness.search_focus_handle();

        harness.click(NEWS_ROW);
        assert_eq!(harness.selection().as_deref(), Some("kytrix.news"));
        assert_eq!(
            harness.focused().as_ref(),
            Some(&search),
            "choosing a row should leave the keyboard in the search field"
        );

        harness.type_without_clicking("candle");
        assert_eq!(harness.query(), "candle");
        assert_eq!(harness.listed(), ["kytrix.candlestick"]);
        assert_eq!(
            harness.selection().as_deref(),
            Some("kytrix.news"),
            "narrowing the list should not change what a confirmation would carry"
        );
    }

    /// The confirm control reports the widget the page has selected.
    #[gpui_kit::test]
    fn the_confirm_control_should_confirm_the_selection(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, Some("kytrix.news"));

        harness.click(CONFIRM_SELECTOR);

        assert_eq!(
            harness.emitted(),
            [WidgetGalleryEvent::Confirm(widget("kytrix.news"))]
        );
    }

    /// The confirm control refuses input while nothing is selected: the page has no answer
    /// to confirm, and the disabled paint does not stop the handler on its own.
    #[gpui_kit::test]
    fn the_confirm_control_should_refuse_while_nothing_is_selected(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, None);

        harness.click(CONFIRM_SELECTOR);
        assert!(harness.emitted().is_empty());

        harness.press("enter");
        assert!(
            harness.emitted().is_empty(),
            "Enter with nothing selected confirms nothing either"
        );
    }

    /// The cancel control leaves without choosing anything.
    #[gpui_kit::test]
    fn the_cancel_control_should_emit_cancel(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, Some("kytrix.news"));

        harness.click(CANCEL_SELECTOR);

        assert_eq!(harness.emitted(), [WidgetGalleryEvent::Cancel]);
    }

    /// Escape leaves the page, whichever control has the focus — including the search field,
    /// whose own Escape handling propagates to the page.
    #[gpui_kit::test]
    fn escape_should_emit_cancel(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, None);

        harness.press("escape");
        assert_eq!(harness.emitted(), [WidgetGalleryEvent::Cancel]);

        harness.type_into_search("news");
        harness.press("escape");
        assert_eq!(
            harness.emitted(),
            [WidgetGalleryEvent::Cancel, WidgetGalleryEvent::Cancel],
            "Escape in the search field leaves exactly once more"
        );
    }

    /// Enter confirms the selection, once for one press — including while the search field
    /// has the caret, whose own Enter handling propagates to the page.
    #[gpui_kit::test]
    fn enter_should_confirm_the_selection(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, Some("kytrix.news"));

        harness.press("enter");
        assert_eq!(
            harness.emitted(),
            [WidgetGalleryEvent::Confirm(widget("kytrix.news"))]
        );

        harness.type_into_search("news");
        harness.press("enter");
        assert_eq!(
            harness.emitted(),
            [
                WidgetGalleryEvent::Confirm(widget("kytrix.news")),
                WidgetGalleryEvent::Confirm(widget("kytrix.news")),
            ],
            "Enter in the search field confirms exactly once more"
        );
    }

    /// The search narrows what is listed, not what a confirmation would carry: a widget
    /// selected before the search is still the answer when the search hides its row.
    ///
    /// This is the behavior the page was asked to keep rather than to change — the confirm
    /// control is not disabled and the selection is not cleared — so the test pins it: a
    /// search that filters the selected widget out must not silently make Enter confirm
    /// something else, or nothing at all.
    #[gpui_kit::test]
    fn a_selection_the_search_hides_should_still_be_confirmable(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, Some("kytrix.news"));

        harness.type_into_search("ohlcv");
        assert_eq!(harness.listed(), ["kytrix.candlestick"]);
        assert!(
            harness.cx.debug_bounds(NEWS_ROW).is_none(),
            "the search should hide the selected widget's row"
        );

        harness.press("enter");
        assert_eq!(
            harness.emitted(),
            [WidgetGalleryEvent::Confirm(widget("kytrix.news"))],
            "the search narrows the list, not the answer a confirmation carries"
        );
    }

    /// A search belongs to the list it was typed over, so changing it puts the page back to
    /// the top: the reader was looking at the list the old search left.
    #[gpui_kit::test]
    fn filtering_should_put_the_gallery_back_to_the_top(cx: &mut TestAppContext) {
        let mut harness = gallery(cx, None);
        let gallery = harness.gallery.clone();

        // Scrolling down is a negative offset in GPUI.
        harness.cx.update(|_, cx| {
            gallery.update(cx, |gallery, _| {
                gallery.scroll.set_offset(point(px(0.0), px(-240.0)))
            })
        });
        harness.settle();

        assert!(
            harness.cx.read(|cx| gallery.read(cx).scroll.offset().y) < px(0.0),
            "the page should be scrolled down before the search changes"
        );
        let viewport = harness
            .cx
            .debug_bounds(VIEWPORT_SELECTOR)
            .expect("the page should draw its viewport");
        let scrolled_content = harness
            .cx
            .debug_bounds(CONTENT_SELECTOR)
            .expect("the page should draw its content");
        assert!(
            scrolled_content.top() < viewport.top(),
            "the content should have moved up out of the viewport"
        );

        harness.cx.update(|_, cx| {
            gallery.update(cx, |gallery, cx| gallery.set_query("news", cx));
        });
        assert_eq!(
            harness.cx.read(|cx| gallery.read(cx).scroll.offset()),
            point(px(0.0), px(0.0)),
            "changing the search should put the page back before the next frame"
        );

        harness.settle();
        let viewport = harness
            .cx
            .debug_bounds(VIEWPORT_SELECTOR)
            .expect("the page should draw its viewport");
        let content = harness
            .cx
            .debug_bounds(CONTENT_SELECTOR)
            .expect("the page should draw its content");
        assert_eq!(
            content.top(),
            viewport.top(),
            "the narrowed content should be drawn at the top of the viewport again"
        );
    }

    /// Tab and Shift+Tab must not hand the keyboard to the pane behind the page.
    ///
    /// The page is drawn over a window that still holds a field of its own. That field is a
    /// tab stop, and in this window's tab order it is the stop just before the page's own —
    /// walking one stop backwards from the page's field reaches it — so a page that let Tab
    /// walk the window's order would hand the keyboard, and every following keystroke, to a
    /// field the reader cannot see.
    ///
    /// The application's window root is `base_ui::Layer`, which installs no Tab handling of
    /// its own, so what stops the walk here is the page's own `handle_key_down`.
    #[gpui_kit::test]
    fn tab_and_shift_tab_should_not_leave_the_page(cx: &mut TestAppContext) {
        let mut harness = overlay_gallery(cx, None);
        let search = harness.search_focus_handle();
        let background = harness.background_focus_handle();
        assert_eq!(
            harness.focused().as_ref(),
            Some(&search),
            "the page should open with its search field focused"
        );

        harness.cx.update(|window, cx| window.focus_prev(cx));
        assert_eq!(
            harness.focused().as_ref(),
            Some(&background),
            "the field behind the page should be the stop just before the page's own"
        );

        // Back to the field the page opens with, and then the keys the page handles: a page
        // that let Tab walk the window's order would lay the keyboard on the field behind it.
        harness.cx.update(|window, cx| search.focus(window, cx));
        for keystrokes in ["tab", "shift-tab", "tab", "shift-tab"] {
            harness.press(keystrokes);
            assert_eq!(
                harness.focused().as_ref(),
                Some(&search),
                "{keystrokes} should leave the keyboard on the page's field"
            );
            assert_ne!(
                harness.focused().as_ref(),
                Some(&background),
                "{keystrokes} should not reach the field behind the page"
            );
        }
        assert!(
            harness.query().is_empty(),
            "Tab should be handled as navigation rather than typed into the field"
        );

        // The window root installs no Tab handling of its own, so the page's own listener is
        // the one that acts on the key. Drive that handler — the method the page registers
        // with `on_key_down` — with the keyboard moved off the field: it takes it back.
        harness.cx.update(|window, cx| background.focus(window, cx));
        assert_eq!(harness.focused().as_ref(), Some(&background));
        let tab = KeyDownEvent {
            keystroke: Keystroke::parse("tab").expect("tab is a valid keystroke"),
            is_held: false,
            prefer_character_input: false,
        };
        let gallery = harness.gallery.clone();
        harness.cx.update(|window, cx| {
            gallery.update(cx, |gallery, cx| gallery.handle_key_down(&tab, window, cx));
        });
        assert_eq!(
            harness.focused().as_ref(),
            Some(&search),
            "the page's Tab handling should put the keyboard back on its field"
        );
    }

    /// Every widget the gallery lists brings a picture of its own, and that picture is one
    /// the application carries and the image pipeline decodes.
    ///
    /// The gallery draws the path a widget declares, so a path that names nothing — or a
    /// picture in a format the image pipeline cannot read, which draws nothing at all — is a
    /// widget that appears in the catalog without an icon and without anything saying so.
    #[gpui_kit::test]
    fn every_listed_widget_should_declare_an_icon_the_gallery_can_draw(cx: &mut TestAppContext) {
        cx.update(install);
        let declared = cx.update(|cx| {
            cx.global::<WidgetRegistry>()
                .iter()
                .filter(|registration| registration.descriptor().selectable())
                .map(|registration| {
                    (
                        registration.descriptor().id().clone(),
                        registration.descriptor().icon(),
                    )
                })
                .collect::<Vec<_>>()
        });

        for (id, icon) in declared {
            let path = icon.unwrap_or_else(|| panic!("{id} should declare an icon"));
            let format = Path::new(path)
                .extension()
                .and_then(OsStr::to_str)
                .unwrap_or_default();

            assert!(
                Img::extensions().contains(&format),
                "{id} declares {path}, which the image pipeline does not decode"
            );
            assert!(
                base_ui::assets::Assets
                    .load(path)
                    .expect("the asset source should answer")
                    .is_some(),
                "{id} declares {path}, which the application does not carry"
            );
        }
    }
}

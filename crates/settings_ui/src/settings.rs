//! The settings page: what the application is configured by, drawn in the page area of the main
//! window in place of the workspace.
//!
//! The page owns what outlives a frame: the page that is drawn and the scroll position of that
//! page. Each page is native UI — its rows, cards and controls are written where the page is
//! written — and this module is the frame around them: the sidebar, the search, the routing between
//! pages, and the one place a failed write is reported.
//!
//! The page has the gallery's shape: a title and the search field in its header, and the settings
//! menu on the left with the detail page behind the selected entry on the right. There is no
//! footer: a control writes the change it makes at once, so there is nothing to confirm and
//! nothing to cancel, and the page is left through the workspace rows alone. The sidebar does not
//! fold: a group is the heading over the pages under it, and a page is the one kind of row that
//! opens anything. What a page opens is the page drawn beside the list, so the two are one thing
//! and move together.
//!
//! A control writes the settings rather than a copy of them, through the session every view of
//! this page shares — see [`SettingsPage::apply`]. A write that cannot be stored leaves the
//! settings as they were and is reported over the page, so what the user had is still there to try
//! again with. A form that edits a credential set is a dialog on the window layer: it holds a draft
//! of one set and writes nothing until it is confirmed, so a cancelled dialog leaves the settings
//! untouched and a confirmed one writes the set through the same session.
//!
//! A search rebuilds the sidebar rather than dimming it: an entry that matches neither by its own
//! label nor by a setting on it is not drawn at all, and a group with none of its pages left is not
//! drawn either, so what is left is the way to what was searched for. What it narrows is the
//! sidebar; a query that matches nothing is answered in both columns rather than leaving the page
//! that was open behind.
//!
//! A page is native UI drawn on every frame, so it follows the language the window is showing by
//! itself. What the page holds across frames — the rows of the sidebar and the words the search is
//! answered over — does not, so it is built again when the language moves on — see
//! [`SettingsPage::adopt_language`].
//!
//! The proxy catalog and the translation page are views of their own. The form that changes the
//! proxy catalog is a dialog on the window layer, drawn over this page. A row cannot open one
//! itself — a table cell is drawn without a window — so it records the form it wants and this page
//! opens it on the layer. The form writes the same session, because it is one page either way.
//! Deleting a row is the one request that is put to the user before it is written, and the alert
//! it is put to is opened the same way — see [`SettingsPage::open_requested_form`]. The credential
//! dialog and its clear confirmation are opened by the venue row that holds them, which is drawn
//! with a window and with this page's session.

use base_ui::{
    button::Button,
    icon::{Icon, IconKind},
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    interactive::InteractiveVariantExt,
    layer::LayerExt,
    scroll_bar::Scrollbar,
    separator::Separator,
    size::UnifiedSizable,
    style::theme::ThemeExt,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement, Pixels, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Window, base::Selectable as _,
    div, px,
};
use i18n::{language::codes::LanguageCode, t};

use crate::{
    layout::{self, PageArgs},
    pages::{
        proxy::{
            editor::ProxyEditor,
            table::{ProxyRequest, ProxyTable},
        },
        registry::{self, PageId},
        translation::page::TranslationPage,
    },
    search::SearchFilter,
    session::{SettingsChange, SettingsSession},
};

/// The widest the page's content column gets. The column holds the header and the two panels
/// beside each other, and the page's background keeps the window's full width around it.
const CONTENT_MAX_WIDTH: Pixels = px(1000.0);

/// The width of the field the settings are searched with, drawn at the end of the header.
const SEARCH_WIDTH: Pixels = px(240.0);

/// The element id of the search field, which is also the key of its state.
const SEARCH_INPUT_ID: &str = "settings-search";

/// The whole of the settings, as the main window's settings page.
pub struct SettingsPage {
    /// The session every control of this page writes through.
    ///
    /// The settings themselves are not held here: a page reads them from the session on every
    /// frame, so what it draws is what is stored rather than what a session started from.
    session: Entity<SettingsSession>,
    /// Held because it is what draws the page again after a write, and what a failed write
    /// reaches the page through.
    _session: Subscription,
    current_page: SharedString,
    query: SharedString,
    filter: SearchFilter,
    /// The language the text this page holds was built in.
    ///
    /// A page is drawn natively afresh on every frame, so what it shows is the text of the
    /// frame's language. What is held across frames is not, so this is what the sidebar and the
    /// filter are compared against.
    language: LanguageCode,
    /// The query the sidebar was last built for, so it is rebuilt when the answer changed rather
    /// than on every frame.
    sidebar_query: Option<SharedString>,
    /// The rows of the sidebar, in the order they are drawn.
    ///
    /// A page the search took away is not a row, and neither is a heading left over nothing:
    /// what a heading names is the pages under it.
    sidebar: Vec<SidebarEntry>,
    sidebar_scroll: ScrollHandle,
    /// The search field, which is built the first time the page is drawn: a text field is filled
    /// in by a window, and the page is built before there is one to draw it in.
    search_input: Option<Entity<InputState>>,
    page_scroll: ScrollHandle,
    proxies: Entity<ProxyTable>,
    translation: Entity<TranslationPage>,
    /// Held because the subscription is what answers the search: dropping it would leave a
    /// field that is typed into and never searched by.
    search_events: Option<Subscription>,
}

impl SettingsPage {
    /// The settings page, opened over the settings that are stored now.
    ///
    /// Every page is drawn from the settings of the frame it is drawn in: nothing is copied when
    /// the page opens, and every value on a page is read from and written to the settings
    /// themselves.
    ///
    /// The search field needs a window and is built on the first frame rather than here, because
    /// the page is built before the window it is about to be shown in has drawn it.
    pub fn load(cx: &mut Context<Self>) -> Self {
        let session = cx.new(|_| SettingsSession::new());
        let filter = SearchFilter::new("", &registry::search_nodes(cx));
        let proxies = cx.new(|cx| ProxyTable::new(session.clone(), cx));
        let downloads = crate::translation_downloads::entity(cx);
        let translation = cx.new(|cx| TranslationPage::new(session.clone(), downloads, cx));
        // Every write of this page repaints it: the page draws what is stored, and a write that
        // failed is reported on the frame that follows it.
        let session_changes = cx.observe(&session, |_, _, cx| cx.notify());

        Self {
            session,
            _session: session_changes,
            current_page: SharedString::new_static(registry::DEFAULT_PAGE),
            query: SharedString::default(),
            filter,
            language: *cx.global::<LanguageCode>(),
            sidebar_query: None,
            sidebar: Vec::new(),
            sidebar_scroll: ScrollHandle::new(),
            search_input: None,
            page_scroll: ScrollHandle::new(),
            proxies,
            translation,
            search_events: None,
        }
    }

    /// The session every control of this page writes through, for a control that opens a dialog
    /// of its own.
    ///
    /// A credential row is drawn by the page's own frame, so what it is handed is the page; the
    /// dialog it opens writes the same session, and this is how the row gets it without reaching
    /// back into the page entity while that entity is being rendered.
    pub(crate) fn session_entity(&self) -> Entity<SettingsSession> {
        self.session.clone()
    }

    /// The search field, built the first time it is drawn.
    fn search_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(input) = self.search_input.as_ref() {
            return input.clone();
        }

        let input = cx.new(|cx| {
            InputStateBuilder::new(SEARCH_INPUT_ID)
                .placeholder(t!(cx, "settings_ui.search.placeholder"))
                .build(window, cx)
        });
        self.subscribe_search(&input, cx);
        self.search_input = Some(input.clone());

        input
    }

    /// Answers what is typed into the search field.
    ///
    /// The subscription is replaced with the field it belongs to, because a field that is
    /// built again is a different entity.
    fn subscribe_search(&mut self, input: &Entity<InputState>, cx: &mut Context<Self>) {
        self.search_events = Some(cx.subscribe(
            input,
            |settings, _, event: &InputStateEvent, cx| {
                let InputStateEvent::Change(query) = event else {
                    return;
                };
                settings.set_query(query.clone(), cx);
            },
        ));
    }

    /// Rebuilds what the page holds in the language the window is showing now.
    ///
    /// The language is a global the whole interface is rendered through, so switching it is
    /// what every frame that follows reads: a page follows by being drawn again, and what this
    /// page holds is built again here, because nothing else would draw it again.
    fn adopt_language(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let language = *cx.global::<LanguageCode>();
        if language == self.language {
            return;
        }

        self.language = language;
        // The filter answers over the rendered text of every label, so the words it holds are
        // of the language it was answered in.
        self.filter = SearchFilter::new(&self.query, &registry::search_nodes(cx));
        self.rebuild_sidebar(cx);
        // A query answered in one language does not have to answer the same way in another: the
        // page that is drawn may be one the new language no longer reaches, and what is drawn
        // then is the message in both columns. The first page that is still reachable is where
        // the page goes instead, the way a search moves it. A page the new language still reaches
        // is not moved, so what the user scrolled to on it stays where it was: the scroll is reset
        // by a page that moves, not by the language itself.
        let reachable = self
            .page()
            .is_some_and(|page| registry::page_is_reachable(page, &self.filter, cx));
        if !reachable && let Some(page) = registry::first_admitted_page(&self.filter, cx) {
            let id = page.id();
            self.move_to(id.as_ref(), cx);
        }
        if let Some(input) = self.search_input.as_ref() {
            let placeholder = t!(cx, "settings_ui.search.placeholder");
            input.update(cx, |input, cx| {
                input.update_placeholder(placeholder, window, cx);
            });
        }
    }

    /// Writes one change into the settings this page draws.
    pub(crate) fn apply(&mut self, change: SettingsChange, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| {
            let _ = session.write(change, cx);
        });
        // A control that offered a value the settings refused draws the old value again on the
        // frame that follows: the session is not written then, so the page is told to draw.
        cx.notify();
    }

    /// Says why the last write could not be stored, if one is waiting.
    ///
    /// The main window is what asks this before it takes the page away: a write that failed and
    /// was not drawn by this page — the settings were left before the frame that reports it — is
    /// carried to what is drawn next, rather than dropped with the page. The reason is taken out
    /// of the session, so it is reported once.
    pub fn take_write_failure(&mut self, cx: &mut Context<Self>) -> Option<String> {
        self.session
            .update(cx, |session, _| session.take_failure())
            .map(|error| error.to_string())
    }

    /// Reports the last write that could not be stored, if one is waiting.
    ///
    /// The reason is taken out of the session, so it is reported once. What a failed write leaves
    /// behind is the settings as they were: a choice is drawn from them again and shows the old
    /// value, and a dialog keeps what the user typed so the write can be made again.
    fn report_write_failure(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(error) = self.session.update(cx, |session, _| session.take_failure()) else {
            return;
        };

        show_write_failure(window, cx, error.to_string());
    }

    /// Selects the page one sidebar row names.
    ///
    /// The caret goes with the page it was on: a control of the page being left is not drawn any
    /// more, and a keystroke would reach something the user cannot see. Nothing is written here:
    /// a change is written by the control that makes it, and a dialog that holds a draft writes it
    /// only when the dialog is confirmed.
    pub(crate) fn select(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.current_page.as_ref() == id {
            return;
        }

        window.blur(cx);
        self.move_to(id, cx);
    }

    /// Moves the page to one entry of the sidebar.
    ///
    /// The page drawn and the row that names it are one thing: the row is drawn selected from
    /// the page the settings are on, so a page moved on its own would leave the row behind it.
    /// Nothing is written here: this is what the page moves by, and what a search or a language
    /// switch moves it with.
    ///
    /// The translation page repairs its settings when it is opened, and being moved to is what
    /// opens it: see [`crate::pages::translation::page::TranslationPage::repair_on_open`].
    fn move_to(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.current_page.as_ref() == id {
            return;
        }

        self.current_page = SharedString::from(id);
        self.page_scroll = ScrollHandle::new();
        self.reveal_current_page();
        if registry::page(id).is_some_and(PageId::is_translation) {
            self.translation
                .update(cx, |page, cx| page.repair_on_open(cx));
        }
        cx.notify();
    }

    /// Answers a new query.
    ///
    /// The sidebar is rebuilt from the answer while the page is drawn, so nothing is built
    /// here but the answer itself; the page follows the answer, because a page with neither a
    /// matching label nor a matching setting is a page that is no longer drawn.
    pub(crate) fn set_query(&mut self, query: SharedString, cx: &mut Context<Self>) {
        if self.query == query {
            return;
        }

        self.query = query;
        self.filter = SearchFilter::new(&self.query, &registry::search_nodes(cx));
        let reachable = self
            .page()
            .is_some_and(|page| registry::page_is_reachable(page, &self.filter, cx));
        let next = (!reachable)
            .then(|| registry::first_admitted_page(&self.filter, cx))
            .flatten();
        if let Some(page) = next {
            // The page moves the way a selection moves rather than on its own: the page drawn
            // and the row that names it are one thing, so a page moved without the sidebar is
            // a page the sidebar has no row for.
            let id = page.id();
            self.move_to(id.as_ref(), cx);
        }
        cx.notify();
    }

    /// The page currently drawn.
    fn page(&self) -> Option<PageId> {
        registry::page(self.current_page.as_str())
    }

    /// Gives the sidebar the rows the current answer admits.
    ///
    /// The rows are built again only when the answer changed: a frame that did not move the
    /// query would build the same rows, and the scroll position would be asked to follow them
    /// again.
    fn sync_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_query.as_ref() == Some(&self.query) {
            return;
        }

        self.rebuild_sidebar(cx);
    }

    /// Builds the rows the filter admits, labelled in the language the page is holding.
    fn rebuild_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar = sidebar_entries(&self.filter, cx);
        self.sidebar_query = Some(self.query.clone());
        self.reveal_current_page();
    }

    /// Scrolls the sidebar to the row of the page that is drawn.
    ///
    /// A page the settings moved to by itself is not necessarily one the user can see: what it
    /// moves to is moved into view, the way a row the user clicked already is.
    fn reveal_current_page(&self) {
        if let Some(index) = self
            .sidebar
            .iter()
            .position(|entry| entry.page_id() == Some(&self.current_page))
        {
            self.sidebar_scroll.scroll_to_item(index);
        }
    }

    /// Opens the form a proxy row asked for, or asks again about the one thing a row can do that
    /// takes something away.
    ///
    /// The request is taken on the frame that draws this page, and the form is opened on the
    /// window layer, which pushes it at the end of the effect cycle and draws it on the next frame.
    /// That is why this can open the form from inside a frame of its own — this page is drawn
    /// inside the layer's view — and why nothing here has to wait for a frame to come round: the
    /// layer's deferred push is what asks for one.
    ///
    /// A delete is not a form: it is one decision, and it is the one row action that cannot be
    /// taken back, because removing an entry resets every route that named it. So it is put to the
    /// user as an alert over the settings, and only what the alert confirms is written — walking
    /// away from it leaves the catalog the rows are drawn from as it was.
    fn open_requested_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.proxies.update(cx, |table, _| table.take_request()) {
            Some(ProxyRequest::Add) => ProxyEditor::show_add(self.session.clone(), window, cx),
            Some(ProxyRequest::Edit { name }) => {
                ProxyEditor::show_edit(&name, self.session.clone(), window, cx)
            }
            Some(ProxyRequest::Delete { name }) => {
                // Both the alert and its action are `'static`, so every sentence is resolved here,
                // where the application is still known to be in the language it is drawn in.
                let title = t!(cx, "settings_ui.proxies.delete_title");
                let message = t!(
                    cx,
                    "settings_ui.proxies.delete_message",
                    name = name.as_str()
                );
                let confirm = t!(cx, "common.confirm");
                let cancel = t!(cx, "common.cancel");
                let session = self.session.clone();

                window.alert(cx, move |alert, _, _| {
                    alert
                        .ok_cancel(title, message)
                        .ok_text(confirm)
                        .cancel_text(cancel)
                        .ok_action(move |_, _, cx| {
                            session.update(cx, |session, cx| {
                                let change = SettingsChange::RemoveProxy { name: name.clone() };
                                let _ = session.write(change, cx);
                            });
                        })
                });
            }
            None => {}
        }
    }

    /// The header: what the page is, and the field the settings are searched with.
    ///
    /// It is the gallery's header: the title on the left, and the search field at the end of the
    /// row. The field is the one the sidebar is filtered by; drawing it here rather than above
    /// the sidebar is what makes the two pages open the same way.
    fn header(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let input = self.search_input(window, cx);

        div()
            .debug_selector(|| "SETTINGS_HEADER".into())
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
                div().min_w_0().flex_1().child(
                    div()
                        .debug_selector(|| "SETTINGS_TITLE".into())
                        .text_2xl()
                        .font_weight(FontWeight::BOLD)
                        .child(t!(cx, "settings_ui.title")),
                ),
            )
            .child(
                div()
                    .debug_selector(|| "SETTINGS_SEARCH".into())
                    .flex_none()
                    .w(SEARCH_WIDTH)
                    .child(
                        Input::new(&input)
                            .medium()
                            .clearable(true)
                            .prefix_icon(IconKind::MagnifyingGlass),
                    ),
            )
            .into_any_element()
    }

    /// The sidebar: the rows the current answer admits, inside the viewport that scrolls them.
    ///
    /// A heading is drawn as text rather than as a control, because a group is not something a
    /// user opens: what it does is name the pages that follow it. A page is a button, and the
    /// one the settings are on is drawn selected.
    fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut rows = div()
            .id("settings-sidebar-scroll")
            .size_full()
            .flex()
            .flex_col()
            .gap_1()
            .overflow_y_scroll()
            .track_scroll(&self.sidebar_scroll);

        for entry in &self.sidebar {
            rows = rows.child(match entry {
                SidebarEntry::Heading(label) => div()
                    .flex_none()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .text_xs()
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(cx.theme().roles.fg.muted.hsla)
                    .child(label.clone())
                    .into_any_element(),
                SidebarEntry::Separator => div()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .debug_selector(|| "SETTINGS_SIDEBAR_SEPARATOR".into())
                    .child(Separator::horizontal())
                    .into_any_element(),
                SidebarEntry::Page { id, label } => {
                    let page = id.clone();
                    let element_id: ElementId = (
                        ElementId::Name(SharedString::new_static("settings-sidebar")),
                        id.clone(),
                    )
                        .into();

                    Button::new(element_id)
                        .ghost()
                        .selected(id == &self.current_page)
                        .flex_none()
                        .w_full()
                        .justify_start()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .debug_selector({
                            let id = id.clone();
                            move || format!("SETTINGS_ENTRY_{id}")
                        })
                        .child(Icon::new(IconKind::GearSix).size_4())
                        .child(label.clone())
                        .on_click(cx.listener(move |settings, _, window, cx| {
                            settings.select(page.as_str(), window, cx);
                        }))
                        .into_any_element()
                }
            });
        }

        div()
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(rows)
            .child(Scrollbar::vertical(self.sidebar_scroll.clone()))
            .into_any_element()
    }

    /// The page the settings are on, for the tests that assert where routing left it.
    #[cfg(test)]
    pub(crate) fn current_page_for_test(&self) -> &SharedString {
        &self.current_page
    }

    /// The query the page is answering, for the tests of the search.
    #[cfg(test)]
    pub(crate) fn query_for_test(&self) -> &SharedString {
        &self.query
    }

    /// What the query admits, for the tests of the search's answer.
    #[cfg(test)]
    pub(crate) fn filter_for_test(&self) -> &SearchFilter {
        &self.filter
    }

    /// The rows the sidebar is drawing, for the tests of its labels.
    #[cfg(test)]
    pub(crate) fn sidebar_for_test(&self) -> &[SidebarEntry] {
        &self.sidebar
    }

    /// The proxy catalog's table, for the tests that read its rows.
    #[cfg(test)]
    pub(crate) fn proxies_for_test(&self) -> &Entity<ProxyTable> {
        &self.proxies
    }

    /// The session the page writes through, for the tests that read what it stored.
    #[cfg(test)]
    pub(crate) fn session_for_test(&self) -> &Entity<SettingsSession> {
        &self.session
    }

    /// The translation page this page routes to, for the tests that read its directory.
    #[cfg(test)]
    pub(crate) fn translation_for_test(&self) -> &Entity<TranslationPage> {
        &self.translation
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.report_write_failure(window, cx);
        self.adopt_language(window, cx);
        self.sync_sidebar(cx);
        self.open_requested_form(window, cx);
        let foreground = cx.theme().roles.fg.default.hsla;
        let canvas = cx.theme().roles.surface.canvas.hsla;
        let handle = cx.entity();
        let header = self.header(window, cx);
        // A page with neither its own label nor one of its settings admitted by the query is a
        // page the search took away: what is left is the message, in this column as in the
        // sidebar.
        let selected = self
            .page()
            .filter(|page| registry::page_is_reachable(*page, &self.filter, cx));
        let page = match selected {
            Some(PageId::Translation) => div()
                .size_full()
                .min_w_0()
                .min_h_0()
                .child(self.translation.clone())
                .into_any_element(),
            Some(PageId::ServerManagement) => div()
                .size_full()
                .min_w_0()
                .min_h_0()
                .child(self.proxies.clone())
                .into_any_element(),
            Some(page) => {
                let content = {
                    let config = self.session.read(cx).config(cx);
                    let filter = self.filter.clone();
                    let mut args = PageArgs {
                        config: config.as_ref(),
                        filter: &filter,
                        settings: self,
                        handle: &handle,
                        cx,
                    };
                    registry::render_page(page, &mut args)
                };
                layout::scroll(content, self.page_scroll.clone())
            }
            None => div()
                .debug_selector(|| "SETTINGS_PAGE_NO_MATCHES".into())
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(no_matches(cx))
                .into_any_element(),
        };
        // A query that admitted nothing leaves the sidebar empty as well: the column says so
        // instead of drawing an empty list, so both columns answer the search the same way.
        let sidebar = if self.filter.is_nothing() {
            div()
                .debug_selector(|| "SETTINGS_SIDEBAR_NO_MATCHES".into())
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(no_matches(cx))
                .into_any_element()
        } else {
            self.sidebar(cx)
        };
        // The search narrows the sidebar and nothing else: the menu column is as wide as the
        // list it holds, and the page gets the rest of the column. The search itself is in the
        // header, where the gallery draws the field it filters its catalogue with.
        let entries = div().w_64().flex_none().min_h_0().flex().flex_col().child(
            div()
                .flex_1()
                .min_h_0()
                .debug_selector(|| "SETTINGS_SIDEBAR".into())
                .child(sidebar),
        );

        div()
            .debug_selector(|| "SETTINGS_PAGE".into())
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(canvas)
            .text_color(foreground)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .debug_selector(|| "SETTINGS_COLUMN".into())
                            .w(CONTENT_MAX_WIDTH)
                            .max_w_full()
                            .h_full()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(header)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .min_h_0()
                                    .px_6()
                                    .pb_4()
                                    .flex()
                                    .gap_2()
                                    .child(entries)
                                    .child(Separator::vertical())
                                    .child(div().min_w_0().min_h_0().flex_1().child(page)),
                            ),
                    ),
            )
    }
}

/// Shows why a change could not be stored.
///
/// The settings page reports its own failures with this, and so does the main window when a write
/// that failed was carried out of the page before a frame reported it: one sentence, wherever it
/// is reported.
pub fn show_write_failure(window: &mut Window, cx: &mut App, reason: String) {
    let title = t!(cx, "settings_ui.save_failed.title");
    let message = t!(cx, "settings_ui.save_failed.message", reason = reason);
    let confirm = t!(cx, "common.confirm");
    window.alert(cx, move |alert, _window, _cx| {
        alert.ok(title, message).ok_text(confirm)
    });
}

/// What a column says when the query admitted nothing it could draw.
fn no_matches(cx: &App) -> AnyElement {
    div()
        .text_sm()
        .text_color(cx.theme().roles.fg.muted.hsla)
        .child(t!(cx, "settings_ui.no_matches"))
        .into_any_element()
}

/// One row of the settings sidebar.
pub(crate) enum SidebarEntry {
    /// A group's name over the pages that follow it: text, because a group is not something a
    /// user opens.
    Heading(SharedString),
    /// A page, which is the one kind of row that opens something.
    Page {
        id: SharedString,
        label: SharedString,
    },
    /// A divider between one part of the sidebar and the next.
    ///
    /// A page at the top of the sidebar is a part of its own rather than another page of the
    /// group above it, and the divider is what says so.
    Separator,
}

impl SidebarEntry {
    /// The page this row opens, or `None` for anything else.
    pub(crate) fn page_id(&self) -> Option<&SharedString> {
        match self {
            Self::Heading(_) | Self::Separator => None,
            Self::Page { id, .. } => Some(id),
        }
    }
}

/// The sidebar as it is drawn, holding only what the filter leaves reachable.
///
/// A page is reached by its label or by a setting on it — see [`registry::page_is_reachable`] —
/// and a group with none of its pages left is not drawn at all, which is what keeps an unmatched
/// part of the settings out of the way instead of dimming it.
fn sidebar_entries(filter: &SearchFilter, cx: &App) -> Vec<SidebarEntry> {
    let mut entries = Vec::new();

    for group in registry::groups() {
        let pages = group
            .pages
            .into_iter()
            .filter(|page| registry::page_is_reachable(*page, filter, cx))
            .collect::<Vec<_>>();
        if pages.is_empty() {
            continue;
        }

        entries.push(SidebarEntry::Heading(t!(cx, group.label_key)));
        for page in pages {
            entries.push(SidebarEntry::Page {
                id: page.id(),
                label: page.title(cx),
            });
        }
    }

    let roots = registry::root_pages()
        .into_iter()
        .filter(|page| registry::page_is_reachable(*page, filter, cx))
        .collect::<Vec<_>>();
    if !roots.is_empty() {
        if !entries.is_empty() {
            entries.push(SidebarEntry::Separator);
        }
        for page in roots {
            entries.push(SidebarEntry::Page {
                id: page.id(),
                label: page.title(cx),
            });
        }
    }

    entries
}

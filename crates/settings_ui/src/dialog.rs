//! The settings dialog: the search field above the tree of settings in one column, the page
//! behind the selected entry beside it.
//!
//! The dialog owns what outlives a frame: the tree's selection, the state of every text field
//! on the selected page, and the scroll position of that page. Everything else — what a page
//! holds, what a control writes — belongs to the catalog and to the draft.
//!
//! A control writes the draft rather than the settings, so the settings are the ones the user
//! confirmed and nothing else: what leaves this dialog is one commit, made when the user asks
//! for it, and a commit that cannot be stored leaves the dialog open with what the user had.
//!
//! A search rebuilds the tree rather than dimming it: an entry that matches neither by its own
//! label nor by a field on it is not drawn at all, and a group with a match under it is drawn
//! open, so what is left is the way to what was searched for. What it narrows is the tree, so it
//! is drawn above the tree rather than across the whole dialog. A query that matches nothing is
//! answered in both columns rather than leaving the page that was open behind.
//!
//! A page is drawn from the catalog on every frame, so it follows the language the window is
//! showing by itself. What the dialog holds across frames — the labels of the tree, the words
//! the search is answered over, the placeholder of a field built before — does not, so it is
//! built again when the language moves on — see [`SettingsDialog::adopt_language`].
//!
//! The proxy catalog is a view of its own rather than a page of fields, and the form that
//! changes it is a dialog on the same window layer, drawn over this dialog. A row cannot open one
//! itself — a table cell is drawn without a window — so it records the form it wants and this
//! dialog opens it on the layer. The form writes the same draft, because it is one session
//! either way. Deleting a row is the one request that is put to the user before it is recorded,
//! and the alert it is put to is opened the same way — see [`SettingsDialog::open_requested_form`].

use app_config::settings::{Config, Language};
use app_config_provider::manager::ConfigManager;
use base_ui::{
    button::Button,
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    interactive::InteractiveVariantExt,
    layer::LayerExt,
    separator::Separator,
    size::UnifiedSizable,
    style::theme::ThemeExt,
    tree::Tree,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, BorrowAppContext as _, Context, DismissEvent, Entity,
    EventEmitter, InteractiveElement as _, IntoElement, ParentElement, Render, ScrollHandle,
    ScrollStrategy, SharedString, StatefulInteractiveElement, Styled, Subscription, Window,
    base::{TreeItem, TreeState},
    div, px,
};
use i18n::{language::codes::LanguageCode, t};
use market_participants_connector::venue::definition::supported_venues;

use crate::{
    access::FieldValue,
    appearance,
    catalog::{
        Catalog, FieldAccess, FieldDef, PageDef, TreeNodeDef, VenueView, catalog, default_page,
    },
    draft::{SettingsDraft, SettingsEdit},
    editor::ProxyEditor,
    page,
    search::{FieldSearch, PageSearch, SearchFilter, SearchNode},
    table::{ProxyRequest, ProxyTable},
};

/// The size of the panel the dialog draws.
const WIDTH: f32 = 1080.0;
const HEIGHT: f32 = 760.0;

/// How tall the search row is, which is what the tree starts below.
const SEARCH_HEIGHT: f32 = 32.0;

/// The element id of the search field, which is also the key of its state.
const SEARCH_INPUT_ID: &str = "settings-search";

/// One text field of one page, remembered for as long as the dialog is open.
struct FieldInput {
    /// The field this is the input of, which is what says how to read it and how to write it
    /// back: a commit made from the confirm button has no page to look the field up on.
    field: FieldDef,
    /// What the draft said when the field was built, which is what a rebuild compares
    /// against: a value that changed under the field is a value the field has to show.
    built_from: FieldValue,
    /// The placeholder the field carries, which is text of the language it was built in and is
    /// written again when that moved on.
    placeholder: Option<SharedString>,
    entity: Entity<InputState>,
    /// Held because the subscription is what commits the field: dropping it would leave a
    /// field that is typed into and never written.
    _events: Subscription,
}

/// The whole of the settings, as a dialog.
pub(crate) struct SettingsDialog {
    /// What this session has changed, which is what every page is drawn from.
    ///
    /// The settings themselves are not read here: a page that read them would show a value the
    /// user has already changed as the value they are changing it from.
    draft: Entity<SettingsDraft>,
    current_page: SharedString,
    /// The catalog this session draws: the pages of the venues this build speaks.
    ///
    /// The pages are a function of the venue list alone, so a session draws one catalog for as
    /// long as it is open: nothing a user changes moves which fields a venue's page holds.
    catalog: Catalog,
    query: SharedString,
    filter: SearchFilter,
    /// The language the text this dialog holds was built in.
    ///
    /// A page is drawn from the catalog afresh on every frame, so what it shows is the text of
    /// the frame's language. What is held across frames is not, so this is what the tree, the
    /// filter and the placeholder of a field are compared against.
    language: LanguageCode,
    /// The query the tree was last built for, so it is rebuilt when the answer changed rather
    /// than on every frame.
    tree_query: Option<SharedString>,
    /// The search field, which is built the first time the dialog is drawn: a text field is
    /// filled in by a window, and a dialog is built before there is one to draw it in.
    search_input: Option<Entity<InputState>>,
    tree: Entity<TreeState>,
    inputs: Vec<FieldInput>,
    page_scroll: ScrollHandle,
    proxies: Entity<ProxyTable>,
    /// Held because the subscription is what answers the search: dropping it would leave a
    /// field that is typed into and never searched by.
    search_events: Option<Subscription>,
    _selection: Subscription,
}

/// The settings take themselves off the layer by emitting the dismissal their dialog listens
/// for: what the close button and a stored commit do is end this session, so it is the session
/// that says so rather than the layer closing whatever happens to be on top of it.
impl EventEmitter<DismissEvent> for SettingsDialog {}

impl SettingsDialog {
    /// The whole of the settings, over the draft one session holds.
    ///
    /// The catalog is the venues this build speaks and the fields each of their APIs has: a venue
    /// this build speaks has its line and its page whether or not it is loaded, and every value
    /// on that page is read from and written to the venue's own settings row.
    ///
    /// What needs a window — the search field and the fields of the page that is drawn — is
    /// built on the first frame rather than here, because a dialog is built before the window
    /// it is about to be shown in has drawn it.
    pub fn new(draft: Entity<SettingsDraft>, cx: &mut Context<Self>) -> Self {
        let venues: VenueView = supported_venues().into();
        let catalog = catalog(&venues);
        let filter = SearchFilter::new("", &search_nodes(&catalog, cx));
        let tree = cx.new(|cx| {
            TreeState::new(cx).items(tree_items(&catalog, &filter, catalog.tree.as_slice(), cx))
        });
        let first_page = SharedString::new_static(default_page());
        tree.update(cx, |tree, cx| {
            // The page the dialog opens on is selected the way any other page is: the tree opens
            // what hides it, rather than opening with no entry selected because a group is closed
            // over the page it draws.
            tree.reveal_item(&first_page, ScrollStrategy::Top, cx);
            let index = tree.index_of(&first_page);
            tree.set_selected_index(index, cx);
        });
        let selection = cx.observe(&tree, |dialog, tree, cx| {
            if let Some(id) = tree
                .read(cx)
                .selected_item()
                .map(|item| item.id.to_string())
            {
                dialog.select(&id, cx);
            }
        });
        let proxies = cx.new(|cx| ProxyTable::new(draft.clone(), cx));

        Self {
            draft,
            current_page: SharedString::new_static(default_page()),
            catalog,
            query: SharedString::default(),
            filter,
            language: *cx.global::<LanguageCode>(),
            tree_query: None,
            search_input: None,
            tree,
            inputs: Vec::new(),
            page_scroll: ScrollHandle::new(),
            proxies,
            search_events: None,
            _selection: selection,
        }
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
        self.search_events = Some(
            cx.subscribe(input, |dialog, _, event: &InputStateEvent, cx| {
                let InputStateEvent::Change(query) = event else {
                    return;
                };
                dialog.set_query(query.clone(), cx);
            }),
        );
    }

    /// Rebuilds what the dialog holds in the language the window is showing now.
    ///
    /// The language is a global the whole interface is rendered through, so switching it is
    /// what every frame that follows reads: a page follows by being drawn again, and what this
    /// dialog holds is built again here, because nothing else would draw it again.
    fn adopt_language(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let language = *cx.global::<LanguageCode>();
        if language == self.language {
            return;
        }

        self.language = language;
        // The filter answers over the rendered text of every label, so the words it holds are
        // of the language it was answered in.
        self.filter = SearchFilter::new(&self.query, &search_nodes(&self.catalog, cx));
        self.build_tree(cx);
        if let Some(input) = self.search_input.as_ref() {
            let placeholder = t!(cx, "settings_ui.search.placeholder");
            input.update(cx, |input, cx| {
                input.update_placeholder(placeholder, window, cx);
            });
        }
    }

    /// The state of one text field, built the first time the field is drawn and reused
    /// afterwards.
    ///
    /// Reusing it is what keeps a caret where it was: an input rebuilt on every frame is an
    /// input the user is typing into that loses what they typed.
    pub fn input_for(
        &mut self,
        field: FieldDef,
        stored: &FieldValue,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let (password, declared) = match &field.access {
            FieldAccess::Text {
                password,
                placeholder,
                ..
            } => (*password, placeholder.clone()),
            _ => (false, None),
        };
        let placeholder = declared.map(|placeholder| placeholder.label(cx));

        if let Some(input) = self
            .inputs
            .iter_mut()
            .find(|input| input.field.id == field.id && &input.built_from == stored)
        {
            // What a field that is used again holds is left alone, because the user may be
            // typing into it. Its placeholder is text of the language it was built in, so that
            // is written again rather than left in the language the interface has left.
            if input.placeholder != placeholder {
                let text = placeholder.clone().unwrap_or_default();
                input.entity.update(cx, |input, cx| {
                    input.update_placeholder(text, window, cx);
                });
                input.placeholder = placeholder;
            }

            return input.entity.clone();
        }

        // A field whose drafted value changed elsewhere is rebuilt around the value the draft
        // now holds: what the field had is what the user was looking at, and what they are
        // configuring is the draft.
        self.inputs.retain(|input| input.field.id != field.id);
        let entity = cx.new(|cx| {
            let mut builder =
                InputStateBuilder::new(field.id.clone()).default_value(stored.as_str());
            if password {
                builder = builder.password();
            }
            if let Some(placeholder) = &placeholder {
                builder = builder.placeholder(placeholder.clone());
            }

            builder.build(window, cx)
        });
        let access = field.access.clone();
        let subscription = cx.subscribe(&entity, move |dialog, _, event: &InputStateEvent, cx| {
            let InputStateEvent::Blur(value) = event else {
                return;
            };
            let value = FieldValue::new(value.as_str());
            dialog.write_field(access.clone(), value, cx);
        });
        self.inputs.push(FieldInput {
            field,
            built_from: stored.clone(),
            placeholder,
            entity: entity.clone(),
            _events: subscription,
        });

        entity
    }

    /// Writes one value into the draft this session holds.
    ///
    /// A value the draft already holds is not recorded: what a commit replays is the changes
    /// the user made, not the value every field happens to be showing.
    pub(crate) fn write_field(
        &mut self,
        access: FieldAccess,
        value: FieldValue,
        cx: &mut Context<Self>,
    ) {
        let unchanged = {
            let config = self.draft.read(cx).config();
            access.current(config.as_ref()) == value
        };
        if unchanged {
            return;
        }

        let edit = SettingsEdit::Field { access, value };
        self.draft.update(cx, |draft, cx| draft.write(edit, cx));
        cx.notify();
    }

    /// Writes what every text field currently holds into the draft.
    ///
    /// A text field commits when the caret leaves it, and clicking a button does not take the
    /// caret away from it: without this, the last thing typed before the confirm button was
    /// pressed would be the one thing the commit did not carry.
    fn commit_inputs(&mut self, cx: &mut Context<Self>) {
        // The fields are read while the draft is written, and the draft is written through
        // `self`, so what to write is collected before anything is written.
        let pending = self
            .inputs
            .iter()
            .map(|input| {
                (
                    input.field.access.clone(),
                    FieldValue::new(input.entity.read(cx).value(cx).as_str()),
                )
            })
            .collect::<Vec<_>>();

        for (access, value) in pending {
            self.write_field(access, value, cx);
        }
    }

    /// Writes what this session changed into the settings, or says why it could not.
    ///
    /// What is written is the changes replayed onto the settings as they are now rather than
    /// the draft as it was built: work that wrote the settings while this dialog was open —
    /// a generated avatar, a persisted workspace layout — is not this dialog's to undo.
    ///
    /// A commit that cannot be stored leaves the dialog exactly as it was, draft included, so
    /// the user can try again rather than lose what they configured.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_inputs(cx);
        let previous = cx.global::<ConfigManager>().read();
        let applied = self.draft.read(cx).applied_to(previous.as_ref());
        let stored = cx.update_global::<ConfigManager, _>(|manager, _| manager.write(applied));

        match stored {
            Ok(()) => {
                self.apply_interface(window, &previous, cx);
                cx.emit(DismissEvent);
            }
            Err(error) => {
                let title = t!(cx, "settings_ui.save_failed.title");
                let message = t!(cx, "settings_ui.save_failed.message", reason = error);
                let confirm = t!(cx, "common.confirm");
                window.alert(cx, move |alert, _window, _cx| {
                    alert.ok(title, message).ok_text(confirm)
                });
            }
        }
    }

    /// Applies the settings the interface itself follows, if the commit moved them.
    ///
    /// The language and the appearance are read while the interface is rendered rather than from
    /// the settings, so what changed has to be handed to the translator and to the theme; a
    /// commit that did not touch one of them leaves that one alone.
    fn apply_interface(&mut self, window: &mut Window, previous: &Config, cx: &mut Context<Self>) {
        let stored = cx.global::<ConfigManager>().read();

        if stored.language != previous.language {
            // The language is the global every label is rendered through, so switching it
            // is the whole of applying a language: the windows are asked to draw again and
            // read the new text on their next frame.
            cx.set_global(language_code(stored.language));
            cx.refresh_windows();
        }
        if stored.appearance != previous.appearance {
            // The appearance is resolved against the settings rather than read while the
            // interface is rendered, so the schema the new preference resolves to is applied
            // here. `System` resolves against this window, which is the window the observer
            // watching the system appearance belongs to.
            appearance::apply(stored.appearance, window, cx);
        }
    }

    /// Selects the page behind one tree entry.
    ///
    /// The page drawn is the page behind the entry the tree selects, so the two are one thing
    /// and are moved together: a page moved on its own would be put back by the tree the next
    /// time it notified, and the field the user is typing into would stop being drawn under
    /// the caret that is still in it.
    ///
    /// A group is not a page, so selecting one leaves the page alone: what a group does is
    /// open and close.
    fn select(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(selected) = self.catalog.page_id(id) else {
            return;
        };
        if selected == self.current_page {
            return;
        }

        self.current_page = selected;
        self.page_scroll = ScrollHandle::new();
        let selected = self.current_page.clone();
        self.tree.update(cx, |tree, cx| {
            // The page may sit under a group that is closed: the tree opens what hides it
            // rather than losing the entry it is on.
            tree.reveal_item(&selected, ScrollStrategy::Top, cx);
            let index = tree.index_of(&selected);
            tree.set_selected_index(index, cx);
        });
        cx.notify();
    }

    /// Answers a new query.
    ///
    /// The tree and the page are rebuilt while they are drawn, so nothing is built here but
    /// the answer itself; what is selected follows the answer, because a page with neither a
    /// matching label nor a matching field is a page that is no longer drawn.
    fn set_query(&mut self, query: SharedString, cx: &mut Context<Self>) {
        if self.query == query {
            return;
        }

        self.query = query;
        self.filter = SearchFilter::new(&self.query, &search_nodes(&self.catalog, cx));
        let reachable = self
            .page()
            .is_some_and(|page| page_is_reachable(page, &self.filter));
        let next = (!reachable)
            .then(|| first_admitted_page(&self.catalog, &self.filter))
            .flatten();
        if let Some(page) = next {
            // The page moves the way a selection moves rather than on its own: the page drawn
            // and the entry the tree selects are one thing, so a page moved without the tree
            // is a page the tree puts back the next time it notifies.
            self.select(page.as_str(), cx);
        }
        cx.notify();
    }

    /// The page currently drawn.
    fn page(&self) -> Option<&PageDef> {
        self.catalog.page(self.current_page.as_str())
    }

    /// Gives the tree the entries the current answer admits.
    ///
    /// The tree is given its items again only when the answer changed: rebuilding them every
    /// frame would throw away which folders the user opened.
    fn sync_tree(&mut self, cx: &mut Context<Self>) {
        if self.tree_query.as_ref() == Some(&self.query) {
            return;
        }

        self.build_tree(cx);
    }

    /// Builds the tree the filter admits, labelled in the language the dialog is holding.
    fn build_tree(&mut self, cx: &mut Context<Self>) {
        let items = tree_items(
            &self.catalog,
            &self.filter,
            self.catalog.tree.as_slice(),
            cx,
        );
        let selected = self.current_page.clone();
        self.tree.update(cx, |tree, cx| {
            tree.set_items(items, cx);
            // The page may sit under a group that is closed: the tree opens what hides it
            // rather than losing the entry it is on.
            tree.reveal_item(&selected, ScrollStrategy::Top, cx);
            let index = tree.index_of(&selected);
            tree.set_selected_index(index, cx);
        });
        self.tree_query = Some(self.query.clone());
    }

    /// Opens the form a proxy row asked for, or asks again about the one thing a row can do that
    /// takes something away.
    ///
    /// The request is taken on the frame that draws this dialog, and the form is opened on the
    /// window layer, which pushes it at the end of the effect cycle and draws it on the next frame.
    /// That is why this can open the form from inside a frame of its own — this dialog is drawn *by*
    /// that layer — and why nothing here has to wait for a frame to come round: the layer's deferred
    /// push is what asks for one.
    ///
    /// A delete is not a form: it is one decision, and it is the one row action that cannot be
    /// taken back, because removing an entry resets every route that named it. So it is put to the
    /// user as an alert over the settings, and only what the alert confirms is recorded — walking
    /// away from it leaves the draft, and with it the catalog the rows are drawn from, as it was.
    fn open_requested_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.proxies.update(cx, |table, _| table.take_request()) {
            Some(ProxyRequest::Add) => ProxyEditor::show_add(self.draft.clone(), window, cx),
            Some(ProxyRequest::Edit { name }) => {
                ProxyEditor::show_edit(&name, self.draft.clone(), window, cx)
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
                let draft = self.draft.clone();

                window.alert(cx, move |alert, _, _| {
                    alert
                        .ok_cancel(title, message)
                        .ok_text(confirm)
                        .cancel_text(cancel)
                        .ok_action(move |_, _, cx| {
                            let edit = SettingsEdit::RemoveProxy { name: name.clone() };
                            draft.update(cx, |draft, cx| draft.write(edit, cx));
                        })
                });
            }
            None => {}
        }
    }

    /// The search row: what to look for, and the way to stop looking.
    ///
    /// The field is as wide as the column it filters, and the way to stop looking is the
    /// field's own: it is drawn inside the input rather than beside it, so the row and the
    /// tree below it are the same width and line up.
    fn search_row(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let input = self.search_input(window, cx);

        div()
            .debug_selector(|| "SETTINGS_SEARCH".into())
            .flex_none()
            .w_full()
            .h(px(SEARCH_HEIGHT))
            .flex()
            .items_center()
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .child(Input::new(&input).medium().clearable(true)),
            )
            .into_any_element()
    }
}

impl Render for SettingsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.adopt_language(window, cx);
        self.sync_tree(cx);
        self.open_requested_form(window, cx);
        let foreground = cx.theme().roles.fg.default.hsla;
        let dialog = cx.entity();
        let search = self.search_row(window, cx);
        // A page with neither its own label nor one of its fields admitted by the query is a
        // page the search took away: what is left is the message, in this column as in the tree.
        let selected = self
            .page()
            .filter(|page| page_is_reachable(page, &self.filter))
            .cloned();
        let page = match selected {
            // A page with no fields of its own is drawn by the view that owns it, which for
            // the proxy catalog is a table that reads the draft itself.
            Some(page_def) if page_def.sections.is_empty() => div()
                .size_full()
                .min_w_0()
                .min_h_0()
                .child(self.proxies.clone())
                .into_any_element(),
            Some(page_def) => {
                let config = self.draft.read(cx).config();
                let filter = self.filter.clone();
                page::page_body(
                    &page_def,
                    config.as_ref(),
                    &filter,
                    self.page_scroll.clone(),
                    window,
                    self,
                    &dialog,
                    cx,
                )
                .unwrap_or_else(|| div().into_any_element())
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
        // A query that admitted nothing leaves the tree empty as well: the column says so
        // instead of drawing an empty list, so both columns answer the search the same way.
        let tree = if self.filter.is_nothing() {
            div()
                .debug_selector(|| "SETTINGS_TREE_NO_MATCHES".into())
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(no_matches(cx))
                .into_any_element()
        } else {
            Tree::new("settings-tree".into(), self.tree.clone()).into_any_element()
        };
        // The search narrows the tree and nothing else, so the two are one column: the field
        // sits above what it filters, and the column is as wide as the tree it holds.
        let entries = div()
            .w_64()
            .flex_none()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(search)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .debug_selector(|| "SETTINGS_TREE".into())
                    .child(tree),
            );

        div()
            .debug_selector(|| "SETTINGS_DIALOG".into())
            .w(px(WIDTH))
            .h(px(HEIGHT))
            .flex()
            .flex_col()
            .gap_2()
            .text_color(foreground)
            .child(
                div()
                    .min_w_0()
                    .min_h_0()
                    .flex_1()
                    .flex()
                    .gap_2()
                    .child(entries)
                    .child(Separator::vertical())
                    .child(div().min_w_0().min_h_0().flex_1().child(page)),
            )
            .child(Separator::horizontal())
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("settings-close")
                            .ghost()
                            .debug_selector(|| "SETTINGS_CLOSE".into())
                            .aria_label(t!(cx, "common.close"))
                            .child(t!(cx, "common.close"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("settings-confirm")
                            .filled()
                            .debug_selector(|| "SETTINGS_CONFIRM".into())
                            .aria_label(t!(cx, "common.confirm"))
                            .child(t!(cx, "common.confirm"))
                            .on_click(cx.listener(|dialog, _, window, cx| {
                                dialog.confirm(window, cx);
                            })),
                    ),
            )
    }
}

/// The translator's own name for one persisted language.
fn language_code(language: Language) -> LanguageCode {
    match language {
        Language::ZhCn => LanguageCode::ZhCn,
        Language::En => LanguageCode::En,
    }
}

/// What a column says when the query admitted nothing it could draw.
fn no_matches(cx: &App) -> AnyElement {
    div()
        .text_sm()
        .text_color(cx.theme().roles.fg.muted.hsla)
        .child(t!(cx, "settings_ui.no_matches"))
        .into_any_element()
}

/// The tree the search is answered over.
///
/// One node per entry, with the text the entry is found by: what it renders as now, plus the
/// words the catalog declares beside it — which is what makes a setting reachable in either
/// language and by the name it is stored under.
fn search_nodes(catalog: &Catalog, cx: &App) -> Vec<SearchNode> {
    catalog
        .tree
        .iter()
        .map(|node| match node {
            TreeNodeDef::Page(id) => SearchNode {
                id: id.clone(),
                page: catalog.page(id).map(|page| page_search(page, cx)),
                keywords: Vec::new(),
                children: Vec::new(),
            },
            TreeNodeDef::Group {
                id,
                label,
                keywords,
                children,
                ..
            } => {
                let mut group_keywords = vec![label.label(cx).to_string()];
                group_keywords.extend(keywords.iter().map(|keyword| (*keyword).to_owned()));

                SearchNode {
                    id: SharedString::new_static(id),
                    page: None,
                    keywords: group_keywords,
                    children: children
                        .iter()
                        .map(|child| SearchNode {
                            id: SharedString::from(child.id()),
                            page: catalog.page(child.id()).map(|page| page_search(page, cx)),
                            keywords: Vec::new(),
                            children: Vec::new(),
                        })
                        .collect(),
                }
            }
        })
        .collect()
}

fn page_search(page: &PageDef, cx: &App) -> PageSearch {
    let mut label = page.title.search_words(cx);
    for keyword in page.keywords {
        label.push_str(&keyword.to_lowercase());
        label.push(' ');
    }

    PageSearch {
        label,
        fields: page
            .sections
            .iter()
            .flat_map(|section| section.fields.iter())
            .map(|field| {
                let mut text = field.label.search_words(&field.label.label(cx));
                text.push_str(&field.description.search_words(&field.description.label(cx)));
                for keyword in field.keywords {
                    text.push_str(&keyword.to_lowercase());
                    text.push(' ');
                }

                FieldSearch {
                    id: field.id.clone(),
                    text,
                }
            })
            .collect(),
    }
}

/// Whether the query leaves one page reachable.
///
/// A page is reachable by its own label, exactly the way the tree has always answered a query,
/// or through any field it holds: a query that names a setting on a page is a query that can
/// open that page, even when the page's own name says nothing about what was searched for. A
/// page reached the second way is listed in the tree, and [`SearchFilter::admits_field`] draws
/// only the fields that matched on it.
fn page_is_reachable(page: &PageDef, filter: &SearchFilter) -> bool {
    filter.admits_page(&page.id)
        || page
            .sections
            .iter()
            .flat_map(|section| &section.fields)
            .any(|field| filter.admits_field(&page.id, &field.id))
}

/// The first page a filter leaves reachable, in the order the tree draws them.
fn first_admitted_page(catalog: &Catalog, filter: &SearchFilter) -> Option<SharedString> {
    catalog
        .tree
        .iter()
        .find_map(|node| first_admitted_in(catalog, node, filter))
}

fn first_admitted_in(
    catalog: &Catalog,
    node: &TreeNodeDef,
    filter: &SearchFilter,
) -> Option<SharedString> {
    match node {
        TreeNodeDef::Page(id) => catalog
            .page(id)
            .filter(|page| page_is_reachable(page, filter))
            .map(|_| id.clone()),
        TreeNodeDef::Group { children, .. } => children
            .iter()
            .find_map(|child| first_admitted_in(catalog, child, filter)),
    }
}

/// The tree as the framework draws it, holding only what the filter leaves reachable.
///
/// A page is reached by its label or by a field on it — see [`page_is_reachable`] — and a group
/// with nothing under it is not drawn at all, which is what keeps an unmatched part of the tree
/// out of the way instead of dimming it.
fn tree_items(
    catalog: &Catalog,
    filter: &SearchFilter,
    nodes: &[TreeNodeDef],
    cx: &App,
) -> Vec<TreeItem> {
    nodes
        .iter()
        .filter_map(|node| match node {
            TreeNodeDef::Page(id) => catalog
                .page(id)
                .filter(|page| page_is_reachable(page, filter))
                .map(|_| TreeItem::new(id.clone(), catalog.page_label(id.as_str()).label(cx))),
            TreeNodeDef::Group {
                id, open, children, ..
            } => {
                let items = tree_items(catalog, filter, children, cx);
                if items.is_empty() {
                    return None;
                }

                Some(
                    TreeItem::new(SharedString::new_static(id), node.label(catalog).label(cx))
                        // A group is open when it says it is, and open whenever a query is
                        // narrowing the tree, because a match under a closed group is a match
                        // the user cannot see.
                        .expanded(*open || !filter.is_all())
                        .children(items),
                )
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use app_config::{
        market_participants::{
            credentials::{BinanceCredentials, LongbridgeCredentials, OkxCredentials},
            origins::LongbridgeRegion,
            settings::VenueSettings,
        },
        network::{ProxyAuthorizationConfig, ProxyConfig},
        settings::{AccountConfig, AccountWalletKind, Appearance, Config, Language},
        workspace::{WorkspaceConfig, WorkspaceLayout, WorkspacesConfig},
    };
    use app_config_provider::manager::{ConfigManager, ConfigPersistenceError};
    use base_ui::{
        animation::ANIMATION_TICK,
        layer::{Layer, LayerExt},
        size::UnifiedSize,
        style::{palette::Appearance as ThemeAppearance, theme::ThemeExt as _},
    };
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, BorrowAppContext as _, Context, Entity, IntoElement,
        Modifiers, Render, SharedString, Styled, TestAppContext, VisualTestContext, Window,
        WindowAppearance, div, px,
    };
    use i18n::{language::codes::LanguageCode, t};
    use market_participants_connector::connector::MarketParticipantsConnector;
    use market_participants_model::identifier::VenueId;
    use smol_str::SmolStr;
    use tempfile::TempDir;
    use widget_tokio_runtime::global;

    use super::{SettingsDialog, default_page, language_code};
    use crate::{
        access::FieldValue,
        appearance,
        catalog::{FieldAccess, PageDef, VenueView, catalog},
        draft::SettingsDraft,
        search::SearchFilter,
        table::ProxyRequest,
    };

    /// The venues every test of this module draws the settings from.
    ///
    /// A dialog draws its venue pages from whichever venue list it is handed, which in production
    /// is what this build speaks — see [`crate::entry::show_settings`] — and here is this fixture.
    fn venues() -> VenueView {
        crate::schema::tests::venues()
    }

    /// The catalog of one venue list, over the fixture settings.
    ///
    /// The catalog of the venues this build speaks.
    fn test_catalog() -> crate::catalog::Catalog {
        catalog(&venues())
    }

    /// The pages of the fixture catalog.
    fn pages() -> Vec<PageDef> {
        test_catalog().pages
    }

    /// The label one page of the fixture catalog is drawn with.
    ///
    /// A page is named by the message key it declares or by the identity it stands
    /// for, and the tree draws whichever the catalog carries.
    fn page_title(page: &str, cx: &TestAppContext) -> SharedString {
        cx.read(|cx| {
            test_catalog()
                .page(page)
                .unwrap_or_else(|| panic!("{page} should be in the catalog"))
                .title
                .label(cx)
        })
    }

    /// The window layer the settings are mounted on.
    ///
    /// The settings are drawn as the window's dialog rather than as a child of this view: the
    /// form over them is a dialog of its own, and a commit that failed is reported by an
    /// alert, neither of which exists without a window layer to open it on.
    struct Harness;

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    /// Opens the settings over a fresh window, the way the system button opens them.
    ///
    /// What is drawn in the window is read off the window rather than off the panel, so the
    /// window comes back with it. The interface and the settings are both in `language`, which
    /// is what a start with an interface that agrees with the settings looks like; a test that
    /// writes another language is then writing a change.
    fn settings_in(
        config: Config,
        language: Language,
        cx: &mut TestAppContext,
    ) -> (Entity<SettingsDialog>, &mut VisualTestContext) {
        let mut config = config;
        config.language = language;

        settings_over(ConfigManager::in_memory(config), language, cx)
    }

    /// Opens the settings over a manager the caller built, which is how a test gets settings
    /// that cannot be stored.
    ///
    /// The settings the manager holds are put in `language` as well as the interface: a start
    /// with an interface that agrees with the settings is what a test writing another language
    /// is then writing a change against.
    fn settings_over(
        manager: ConfigManager,
        language: Language,
        cx: &mut TestAppContext,
    ) -> (Entity<SettingsDialog>, &mut VisualTestContext) {
        let panel = cx.update(|cx| {
            // What a window installs before it draws anything, in the order the application
            // installs it: a field that is typed into reads the state base_ui's own fields are
            // built on.
            gpui_kit::base::init(cx);
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            cx.set_global(language_code(language));
            cx.set_global(manager);
            // A manager whose settings are stored under another language than the one the
            // interface is drawn in is a session that starts with a change nobody made, so the
            // two are made to agree here rather than left to what a default happens to be.
            cx.update_global::<ConfigManager, _>(|manager, _| {
                manager.update(|config| config.language = language);
            });
            let draft = cx.new(|cx| {
                let current = cx.global::<ConfigManager>().read();
                SettingsDraft::new(current.as_ref().clone())
            });

            cx.new(|cx| SettingsDialog::new(draft, cx))
        });
        let (_, cx) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Harness)));
        // The appearance the settings name is applied the way a window applies it when it is
        // built, so a test starts from the theme the stored preference resolves to rather than
        // from whichever one `install` left behind.
        cx.update(|window, cx| {
            let preference = cx.global::<ConfigManager>().read().appearance;
            appearance::apply(preference, window, cx);
        });
        // The settings are mounted once the window has its layer: a dialog is opened on the
        // window layer, and a window that is still being built has none.
        cx.update({
            let panel = panel.clone();
            move |window, cx| window.dialog(cx, panel)
        });
        settle(cx);

        (panel, cx)
    }

    /// Opens the settings with the interface in `language`.
    fn settings(
        config: Config,
        cx: &mut TestAppContext,
    ) -> (Entity<SettingsDialog>, &mut VisualTestContext) {
        settings_in(config, Language::En, cx)
    }

    /// Opens the settings again over a window that already holds them.
    ///
    /// Opening the settings installs the globals a session is built on, which a test can only do
    /// once, so a test that opens them twice — which is what a user who changes their mind does —
    /// builds the second session on the window the first one left behind.
    fn reopen_settings(
        window: AnyWindowHandle,
        app: &TestAppContext,
    ) -> (Entity<SettingsDialog>, VisualTestContext) {
        let mut cx = VisualTestContext::from_window(window, app);
        let panel = cx.update(|_, cx| {
            let draft = cx.new(|cx| {
                let stored = cx.global::<ConfigManager>().read();
                SettingsDraft::new(stored.as_ref().clone())
            });

            cx.new(|cx| SettingsDialog::new(draft, cx))
        });
        cx.update({
            let panel = panel.clone();
            move |window, cx| window.dialog(cx, panel)
        });
        settle(&mut cx);

        (panel, cx)
    }

    /// Opens the settings the way the system button opens them, over a build that has loaded
    /// nothing, and hands back the dialog the entry built.
    ///
    /// The market participants root is the application's own, and the scopes the dialog is handed are what
    /// the entry reads off it: installing the root loads no venue, so the settings are opened
    /// over an empty loaded-venues view and every row they offer is the static venue list's.
    fn settings_of_an_unconnected_build(
        cx: &mut TestAppContext,
    ) -> (Entity<SettingsDialog>, &mut VisualTestContext) {
        cx.update(|cx| {
            gpui_kit::base::init(cx);
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            cx.set_global(ConfigManager::in_memory(Config::default()));
            global::install(cx).expect("the test Tokio runtime should build");
            MarketParticipantsConnector::install(cx)
                .expect("the market participants root should install");
        });
        let panel = cx.update(|cx| {
            let draft = cx.new(|cx| {
                let stored = cx.global::<ConfigManager>().read();
                SettingsDraft::new(stored.as_ref().clone())
            });

            cx.new(|cx| SettingsDialog::new(draft, cx))
        });
        let (_, cx) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Harness)));
        // The appearance the settings name is applied the way a window applies it when it is
        // built, so a test starts from the theme the stored preference resolves to rather than
        // from whichever one `install` left behind.
        cx.update(|window, cx| {
            let preference = cx.global::<ConfigManager>().read().appearance;
            appearance::apply(preference, window, cx);
        });
        cx.update({
            let panel = panel.clone();
            move |window, cx| window.dialog(cx, panel)
        });
        settle(cx);

        (panel, cx)
    }

    /// Lets the dialog's own enter or exit transition finish and repaints.
    ///
    /// A dialog in its first frames is painted at zero opacity, and a dismissed one stays on
    /// the layer until its exit transition is over, so neither can be told from the other
    /// without waiting out the animation the interface declares.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(ANIMATION_TICK + std::time::Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// Clicks what a selector draws, and lets whatever it opened settle.
    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the window should draw {selector}"))
            .center();
        cx.simulate_click(target, Modifiers::none());
        settle(cx);
    }

    /// The access of a field of the catalog, so a test writes the way a control writes.
    fn field_access(page: &str, field: &str) -> FieldAccess {
        test_catalog()
            .page(page)
            .unwrap_or_else(|| panic!("{page} should be in the catalog"))
            .sections
            .iter()
            .flat_map(|section| &section.fields)
            .find(|definition| definition.id == field)
            .unwrap_or_else(|| panic!("{page} should offer {field}"))
            .access
            .clone()
    }

    /// Writes a field into the draft the way the control of that field does.
    fn write_draft_field(
        panel: &Entity<SettingsDialog>,
        page: &'static str,
        field: &'static str,
        value: &str,
        cx: &mut VisualTestContext,
    ) {
        let access = field_access(page, field);
        panel.update(cx, |dialog, cx| {
            dialog.write_field(access, FieldValue::new(value), cx);
        });
        settle(cx);
    }

    /// The settings this session would write, which is what the pages are drawn from.
    fn drafted(panel: &Entity<SettingsDialog>, cx: &VisualTestContext) -> Config {
        cx.read(|cx| {
            panel.read_with(cx, |dialog, cx| {
                dialog.draft.read(cx).config().as_ref().clone()
            })
        })
    }

    /// The settings as everyone outside this dialog sees them.
    fn stored(cx: &VisualTestContext) -> Config {
        cx.read(|cx| cx.global::<ConfigManager>().read().as_ref().clone())
    }

    /// The appearance the application is drawn in.
    fn drawn_appearance(cx: &VisualTestContext) -> ThemeAppearance {
        cx.read(|cx| cx.theme().appearance)
    }

    /// The page the dialog is drawing.
    fn current_page(panel: &Entity<SettingsDialog>, cx: &VisualTestContext) -> SharedString {
        cx.read(|cx| panel.read_with(cx, |dialog, _| dialog.current_page.clone()))
    }

    /// The labels the tree draws, in the order it draws them.
    fn labels(panel: &Entity<SettingsDialog>, cx: &VisualTestContext) -> Vec<SharedString> {
        cx.read(|cx| {
            panel.read_with(cx, |dialog, cx| {
                let tree = dialog.tree.read(cx);

                (0..)
                    .map_while(|index| tree.entry(index))
                    .map(|entry| entry.item().label.clone())
                    .collect()
            })
        })
    }

    /// Settings with something written everywhere the settings can hold something, so that a
    /// commit overwriting them wholesale rather than replaying changes would be visible.
    fn configured_settings() -> Config {
        let mut config = Config::default();
        config.account = Some(AccountConfig {
            user_id: "11111111-1111-4111-8111-111111111111".into(),
            wallet_address: "0x0123456789abcdef".into(),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        config.avatar = Some("<svg>configured-avatar</svg>".to_owned());
        config.proxies.insert(
            SmolStr::new_static("hk"),
            ProxyConfig {
                addr: SmolStr::new_static("127.0.0.1"),
                port: 1080,
                authorization: ProxyAuthorizationConfig::UsernameAndPassword {
                    username: SmolStr::new_static("proxy-user"),
                    password: SmolStr::new_static("proxy-password"),
                },
                resolve_dns_at_remote: true,
            },
        );

        {
            let VenueSettings::Binance(binance) = config
                .venues
                .get_mut(&VenueId::Binance)
                .expect("the default settings name every venue this build speaks")
            else {
                panic!("binance is configured as Binance");
            };
            binance.credentials = BinanceCredentials {
                spot_api_key: SmolStr::new_static("binance-spot-key"),
                spot_private_key: SmolStr::new_static("binance-spot-private"),
                spot_passphrase: SmolStr::new_static("binance-spot-passphrase"),
                usd_m_perp_api_key: SmolStr::new_static("binance-perp-key"),
                usd_m_perp_private_key: SmolStr::new_static("binance-perp-private"),
                usd_m_perp_passphrase: SmolStr::new_static("binance-perp-passphrase"),
            };
        }

        {
            let VenueSettings::Okx(okx) = config
                .venues
                .get_mut(&VenueId::Okx)
                .expect("the default settings name every venue this build speaks")
            else {
                panic!("okx is configured as OKX");
            };
            okx.credentials = OkxCredentials {
                api_key: SmolStr::new_static("okx-api-key"),
                secret_key: SmolStr::new_static("okx-secret-key"),
                passphrase: SmolStr::new_static("okx-passphrase"),
            };
        }

        config
    }

    /// A manager whose settings cannot be stored, the file that blocks it, and the directory the
    /// two of them are in.
    ///
    /// The settings sit behind a path that is a file rather than a directory, so creating the
    /// config directory there fails the way an unwritable config directory does. The settings it
    /// starts with are the defaults, which is what a first start looks like and what a language
    /// of `en` — the one the interface of a test starts in — is a change of.
    ///
    /// The path is one this run has to itself: it is made inside a directory that is created for
    /// the test and removed when the returned handle is dropped, so the file that blocks the
    /// settings is written at a path nothing else can already hold. A name built from the process
    /// id and a counter is not — a later run of a recycled id reaches the same name, and writing
    /// a file where that run left a directory fails with access denied instead of with the
    /// failure a test is about.
    ///
    /// A test that repairs the path the way a user repairs a permission is the one thing that can
    /// tell a retry apart from a commit that was never attempted again, so the handle lives for as
    /// long as the path is used: dropping it removes the directory and what the test put in it.
    fn manager_behind_a_file() -> (ConfigManager, PathBuf, TempDir) {
        let directory = test_temp("manager-behind-a-file");
        let blocking = directory.path().join("not-a-directory");
        fs::write(&blocking, b"block directory creation").expect("the blocking file is written");

        let manager = ConfigManager::from_path(blocking.join("config.json"))
            .expect("a config manager loads around a missing file");

        (manager, blocking, directory)
    }

    /// A temporary directory of the test's own, under the workspace's build output.
    ///
    /// The system temporary directory is not usable here: a test that runs behind a restricted
    /// integrity token cannot create directories in `%TEMP%`, which leaves every test about a file
    /// on disk failing for a reason that has nothing to do with the code under test. The build
    /// output is the workspace's own writable scratch space, so the directory goes there.
    fn test_temp(name: &str) -> TempDir {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("this crate lives two directories below the workspace root");
        let root = workspace.join("target").join("test-temp").join("settings");
        fs::create_dir_all(&root).expect("the test temporary directory should be created");
        tempfile::Builder::new()
            .prefix(name)
            .tempdir_in(&root)
            .expect("a temporary directory should be created")
    }

    /// What the rest of the application writes while the settings are open: an avatar the
    /// background generated, an account it signed in, a layout it persisted.
    ///
    /// Every one of them goes through `update`, which is the path those subsystems own; the
    /// dialog must neither know about them nor undo them.
    fn write_while_open(cx: &mut VisualTestContext) {
        cx.update(|_, cx| {
            cx.update_global::<ConfigManager, _>(|manager, _| {
                manager.update(|config| {
                    config.avatar = Some("<svg>generated-while-open</svg>".to_owned());
                    config.account = Some(AccountConfig {
                        user_id: "22222222-2222-4222-8222-222222222222".to_owned(),
                        wallet_address: "0xfeed".to_owned(),
                        wallet_kind: AccountWalletKind::Ethereum,
                    });
                    config.workspaces = WorkspacesConfig {
                        entries: vec![WorkspaceConfig {
                            name: SmolStr::new_static("written-while-open"),
                            layout: WorkspaceLayout::single_pane(),
                        }],
                        active: Some(SmolStr::new_static("written-while-open")),
                    };
                });
            });
        });
    }

    /// The settings behind a file that cannot be written are reported as not stored, rather
    /// than being published as if they had been.
    #[test]
    fn a_manager_behind_a_file_should_report_a_failed_write() {
        let (mut manager, _blocking, _directory) = manager_behind_a_file();
        let previous = manager.read();
        let mut candidate = previous.as_ref().clone();
        candidate.appearance = Appearance::Dark;

        let outcome = manager.write(candidate);

        assert!(
            outcome.is_err(),
            "storing behind a file must fail, and it failed with {outcome:?}"
        );
        assert_eq!(
            manager.read().appearance,
            previous.appearance,
            "a write that could not be stored must leave the snapshot alone"
        );
    }

    /// Opens the form that edits an entry, the way the row of the catalog that asks for one
    /// does.
    fn open_edit_form(
        panel: &Entity<SettingsDialog>,
        name: &'static str,
        cx: &mut VisualTestContext,
    ) {
        request(
            panel,
            ProxyRequest::Edit {
                name: SmolStr::new_static(name),
            },
            cx,
        );
    }

    /// Asks the catalog for something a row asks for, and lets the dialog take it.
    ///
    /// The window is not refreshed here, which is what makes the wait worth anything: the request
    /// is what marks the window dirty, and the frame that follows — drawn inside the effect flush —
    /// is what the assertions after this rely on, so refreshing here would draw what the request
    /// opened even if the request alone had left it undrawn. What the clock is for is the opened
    /// form's own transition, which the next `settle` or `click` moves on.
    fn request(panel: &Entity<SettingsDialog>, request: ProxyRequest, cx: &mut VisualTestContext) {
        panel.update(cx, |dialog, cx| {
            dialog.proxies.update(cx, |table, cx| {
                table.request(request, cx);
            });
        });
        cx.executor()
            .advance_clock(ANIMATION_TICK + std::time::Duration::from_millis(50));
        cx.run_until_parked();
    }

    /// Opens the form that adds an entry, the way the button above the catalog asks for one.
    fn open_add_form(panel: &Entity<SettingsDialog>, cx: &mut VisualTestContext) {
        request(panel, ProxyRequest::Add, cx);
    }

    /// Clicks one text field of a form and types into it without leaving it.
    ///
    /// The caret is left where it is, which is what a user who types and then presses a button
    /// does: it is the case that tells a field committed on blur from one the form reads.
    fn type_into(cx: &mut VisualTestContext, selector: &'static str, text: &str) {
        let field = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the form should draw {selector}"));
        cx.simulate_click(field.center(), Modifiers::none());
        cx.simulate_input(text);
        settle(cx);
    }

    /// Asks the catalog for a delete, the way a row of it asks for one, and lets the dialog take
    /// it.
    ///
    /// What a row asks for is the alert rather than the removal: the entry leaves the session
    /// through the alert's own action, so a test that wants it gone confirms it there.
    fn request_delete(
        panel: &Entity<SettingsDialog>,
        name: &'static str,
        cx: &mut VisualTestContext,
    ) {
        request(
            panel,
            ProxyRequest::Delete {
                name: SmolStr::new_static(name),
            },
            cx,
        );
    }

    /// A form the user walks away from is not a change of the session.
    ///
    /// The form was opened over the settings, which were never taken off the layer: what
    /// cancelling leaves behind is the panel that was always there, uncovered rather than put
    /// back, and what the form held is dropped rather than recorded, which is what makes it a
    /// cancel.
    ///
    /// The session is already holding a change when the form opens, so what this asks is that
    /// leaving the form neither records the entry nor loses the change, and the assertions on
    /// the panel are what tell the panel that was always there from one built in its place: a
    /// panel built again would open on the default page with an empty search.
    #[gpui_kit::test]
    fn a_cancelled_form_should_leave_the_session_and_the_settings_alone(cx: &mut TestAppContext) {
        let (panel, cx) = settings(configured_settings(), cx);
        write_draft_field(&panel, "appearance", "language", "zh-CN", cx);
        panel.update(cx, |dialog, cx| {
            dialog.select("market_participants.okx", cx)
        });
        panel.update(cx, |dialog, cx| {
            dialog.set_query("market_participants.okx".into(), cx)
        });
        settle(cx);
        let before = drafted(&panel, cx);
        let selected = current_page(&panel, cx);

        open_edit_form(&panel, "hk", cx);
        type_into(cx, "SETTINGS_PROXY_FIELD_name", "osaka");
        click(cx, crate::editor::CANCEL_SELECTOR);

        assert!(
            cx.debug_bounds("SETTINGS_PROXY_EDIT").is_none(),
            "cancelling should take the form away"
        );
        assert!(
            cx.debug_bounds("SETTINGS_DIALOG").is_some(),
            "the settings the form was opened over should still be drawn"
        );
        assert_eq!(
            drafted(&panel, cx),
            before,
            "a cancelled form must not change what the session holds"
        );
        assert_eq!(
            stored(cx).proxies,
            before.proxies,
            "a cancelled form must not write the settings"
        );
        assert_eq!(
            current_page(&panel, cx),
            selected,
            "the settings should still be on the page the form was opened over"
        );
        panel.read_with(cx, |dialog, _| {
            assert_eq!(
                dialog.query.as_ref(),
                "market_participants.okx",
                "the settings should still hold the search they were found by"
            );
        });
    }

    /// The form ends the way the application's other dialogs end: a line across it, and the two
    /// buttons under that line rather than in among the fields. What the line is for is telling
    /// what the form holds from what the form does, so the fields are above it and the buttons are
    /// below it, and both buttons are one size because neither is the field's own control.
    #[gpui_kit::test]
    fn the_form_buttons_should_sit_under_a_separator_at_the_foot_of_the_form(
        cx: &mut TestAppContext,
    ) {
        let (panel, cx) = settings(configured_settings(), cx);
        open_edit_form(&panel, "hk", cx);

        let form = cx
            .debug_bounds("SETTINGS_PROXY_EDIT")
            .expect("the form should be drawn");
        let footer = cx
            .debug_bounds(crate::editor::FOOTER_SELECTOR)
            .expect("the form should draw the row its buttons are in");
        let confirm = cx
            .debug_bounds(crate::editor::CONFIRM_SELECTOR)
            .expect("the form should draw the button that records the entry");
        let cancel = cx
            .debug_bounds(crate::editor::CANCEL_SELECTOR)
            .expect("the form should draw the button that leaves the entry alone");
        let field = cx
            .debug_bounds("SETTINGS_PROXY_FIELD_port")
            .expect("the form should draw the field the entry's port is typed into");

        assert!(
            field.bottom() <= footer.top(),
            "the line the row starts with belongs between the fields and the buttons: {field:?} \
             above {footer:?}"
        );
        assert!(
            footer.top() <= cancel.top() && footer.top() <= confirm.top(),
            "the buttons belong under the line rather than over it: {footer:?} over {cancel:?} and \
             {confirm:?}"
        );
        assert_eq!(
            footer.bottom(),
            form.bottom(),
            "the row should be the foot of the form: {footer:?} in {form:?}"
        );
        assert!(
            (confirm.right() - form.right()).abs() < px(1.0),
            "the row should be laid out from the right edge of the form: {confirm:?} in {form:?}"
        );
        assert_eq!(
            cancel.size.height, confirm.size.height,
            "the two buttons should be one size, which is neither of them being a control of a \
             field: {cancel:?} beside {confirm:?}"
        );
    }

    /// The alert a rejected commit raises says what went wrong, in either language.
    ///
    /// The message is only worth raising if the user can read the reason, so both locales have
    /// to carry the placeholder the reason is written into, and the reason has to arrive in the
    /// text the alert is given.
    ///
    /// The copy is centered when it is drawn, which it says with a `<p align="center">` tag pair:
    /// that pair is how the message opens and how it ends, so both readings here skip it and ask
    /// about the sentence inside it.
    ///
    /// A key that is missing from a locale translates to an empty string rather than failing,
    /// which is what makes "the message is not empty" the thing this has to check: a typo in a
    /// key would otherwise reach the user as an alert with nothing in it.
    #[gpui_kit::test]
    fn the_failure_alert_should_name_the_reason_in_both_languages(cx: &mut TestAppContext) {
        let (mut manager, _blocking, _directory) = manager_behind_a_file();
        let mut candidate = manager.read().as_ref().clone();
        candidate.appearance = Appearance::Dark;
        let reason: ConfigPersistenceError = manager
            .write(candidate)
            .expect_err("storing behind a file should fail");

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("embedded locales should load");
        });

        for (language, title, opening) in [
            (
                LanguageCode::En,
                "The settings could not be saved",
                "The settings were not written to disk: ",
            ),
            (LanguageCode::ZhCn, "设置保存失败", "设置未能写入磁盘："),
        ] {
            cx.update(|cx| {
                cx.set_global(language);
            });

            let (declared, drawn_title, message, filling) = cx.read(|cx| {
                let i18n = cx.global::<i18n::translator::types::I18n>();

                (
                    i18n.try_translate(language, "settings_ui.save_failed.message")
                        .is_some(),
                    t!(cx, "settings_ui.save_failed.title"),
                    t!(cx, "settings_ui.save_failed.message", reason = reason),
                    // The same message with a reason that is a word of its own, which is what
                    // says the placeholder of this locale was filled rather than dropped.
                    t!(cx, "settings_ui.save_failed.message", reason = "the-reason"),
                )
            });

            assert!(
                declared,
                "both locales should define the message a failed commit raises"
            );
            assert!(
                !drawn_title.is_empty(),
                "the title of {language:?} should not be an empty string"
            );
            assert!(
                !message.is_empty(),
                "the message of {language:?} should not be an empty string"
            );
            // The message is centered markdown, so the reason is followed by the align tag pair's
            // closing tag rather than by the end of the sentence.
            assert!(
                filling.contains("the-reason</p>"),
                "the message of {language:?} should write the reason where its placeholder is: {filling}"
            );
            assert_eq!(
                drawn_title.as_ref(),
                title,
                "the title should be written in {language:?}"
            );
            assert!(
                message
                    .trim_start_matches("<p align=\"center\">")
                    .starts_with(opening),
                "the message should be written in {language:?}: {message}"
            );
            assert!(
                message.contains(&reason.to_string()),
                "the message should carry the reason {reason}: {message}"
            );
            assert!(
                !message.contains("%{"),
                "every placeholder should be filled in: {message}"
            );
        }
    }

    /// A commit that could not be stored is not one the dialog treats as stored: once the
    /// reason is gone, the same change reaches the file.
    ///
    /// What this is about is the retry: the file was never written, so the settings in memory
    /// are the ones the user started with and the dialog still holds their change — asking
    /// again has to store it rather than report it as already stored.
    #[gpui_kit::test]
    fn a_commit_that_could_not_be_stored_should_be_stored_when_it_is_tried_again(
        cx: &mut TestAppContext,
    ) {
        let (manager, blocking, _directory) = manager_behind_a_file();
        let path = blocking.join("config.json");
        let (panel, cx) = settings_over(manager, Language::En, cx);

        write_draft_field(&panel, "appearance", "appearance", "Dark", cx);
        click(cx, "SETTINGS_CONFIRM");

        assert!(
            cx.debug_bounds("BASE_UI_ALERT").is_some(),
            "the failure should be reported before anything is tried again"
        );
        assert!(
            !path.exists(),
            "a commit that failed must not have left a file behind: {path:?}"
        );

        // Reading the alert is what leaves the dialog to be asked again.
        click(cx, "BASE_UI_ALERT_OK");
        assert!(
            cx.debug_bounds("BASE_UI_ALERT").is_none(),
            "the alert should be gone once it has been read"
        );

        fs::remove_file(&blocking).expect("the blocking file should be removable");

        click(cx, "SETTINGS_CONFIRM");

        assert_eq!(
            stored(cx).appearance,
            Appearance::Dark,
            "the retry should store what the user confirmed"
        );
        assert!(
            cx.debug_bounds("SETTINGS_DIALOG").is_none(),
            "a commit that was stored should close the settings"
        );

        let written = fs::read_to_string(&path).expect("the retry should have written a file");
        assert!(
            written.contains("\"appearance\": \"Dark\""),
            "the file should hold what the user confirmed: {written}"
        );
        let reloaded = ConfigManager::from_path(&path).expect("the stored settings should load");
        assert_eq!(
            reloaded.read().as_ref(),
            &stored(cx),
            "what was stored should load as the settings the application is running with"
        );
    }

    /// Work that writes the settings while the dialog is open is not the dialog's to undo.
    ///
    /// An avatar is generated in the background and a workspace layout is persisted on a
    /// delay, so the settings of the moment a commit is made are not the settings the dialog
    /// was opened with: what the user changed is replayed onto the settings of that moment
    /// rather than published as the snapshot the session started from.
    #[gpui_kit::test]
    fn a_commit_should_keep_the_settings_written_while_the_dialog_was_open(
        cx: &mut TestAppContext,
    ) {
        let (panel, cx) = settings(configured_settings(), cx);
        write_draft_field(&panel, "appearance", "appearance", "Dark", cx);

        write_while_open(cx);
        settle(cx);

        assert_eq!(
            drafted(&panel, cx).avatar.as_deref(),
            Some("<svg>configured-avatar</svg>"),
            "the session should still draw the settings it was opened with"
        );

        click(cx, "SETTINGS_CONFIRM");

        let published = stored(cx);
        assert_eq!(
            published.appearance,
            Appearance::Dark,
            "the change the user confirmed should be the one that is stored"
        );
        assert_eq!(
            published.avatar.as_deref(),
            Some("<svg>generated-while-open</svg>"),
            "a commit must not roll back the avatar the background wrote"
        );
        assert_eq!(
            published
                .account
                .as_ref()
                .map(|account| account.user_id.as_str()),
            Some("22222222-2222-4222-8222-222222222222"),
            "a commit must not roll back the account another subsystem signed in"
        );
        assert_eq!(
            published.workspaces.active.as_deref(),
            Some("written-while-open"),
            "a commit must not roll back a workspace layout persisted while it was open"
        );
    }

    /// A value the user changed reaches the draft and nothing else: the settings only move
    /// when the user says so.
    #[gpui_kit::test]
    fn a_field_change_should_reach_the_settings_only_when_it_is_confirmed(cx: &mut TestAppContext) {
        let (panel, cx) = settings(Config::default(), cx);

        write_draft_field(&panel, "appearance", "appearance", "Dark", cx);

        assert_eq!(
            drafted(&panel, cx).appearance,
            Appearance::Dark,
            "the draft should hold what the user chose"
        );
        assert_eq!(
            stored(cx).appearance,
            Appearance::System,
            "a control must not write the settings before the user confirms it"
        );

        click(cx, "SETTINGS_CONFIRM");

        assert_eq!(
            stored(cx).appearance,
            Appearance::Dark,
            "confirming should write what the user chose"
        );
        assert!(
            cx.debug_bounds("SETTINGS_DIALOG").is_none(),
            "a stored commit should close the settings"
        );
    }

    /// The theme is what the user confirmed, and only then.
    ///
    /// A control writes the session rather than the settings, so the appearance the interface is
    /// drawn in has to follow the same rule the stored preference does: choosing another mode
    /// while the dialog is open is not yet the appearance the application is in, and confirming
    /// it is what draws it.
    #[gpui_kit::test]
    fn the_theme_should_follow_the_appearance_the_user_confirmed(cx: &mut TestAppContext) {
        let (panel, cx) = settings(Config::default(), cx);

        write_draft_field(&panel, "appearance", "appearance", "Dark", cx);

        assert_eq!(
            drawn_appearance(cx),
            ThemeAppearance::Light,
            "the appearance reaches the theme when the user confirms it, not when they choose it"
        );

        click(cx, "SETTINGS_CONFIRM");

        assert_eq!(
            drawn_appearance(cx),
            ThemeAppearance::Dark,
            "the appearance the user confirmed should be the one the application is drawn in"
        );
    }

    /// The theme is drawn from the appearance the user confirmed, and a preference that names an
    /// appearance is not the system's to answer.
    ///
    /// The precedence itself — every preference against every appearance a system reports, the
    /// pinned cases included — is a table in [`crate::appearance`], where the system appearance is
    /// an argument and so can be put to it. What only a whole session can show is that the resolved
    /// appearance reaches the theme: a confirmed `Dark` draws the dark appearance, and handing the
    /// preference back to the system draws the appearance the system's own appearance resolves
    /// to — which the window reports itself, so the test asks the window rather than assuming one.
    #[gpui_kit::test]
    fn confirming_an_appearance_should_draw_the_appearance_it_resolves_to(cx: &mut TestAppContext) {
        let (panel, cx) = settings(Config::default(), cx);

        write_draft_field(&panel, "appearance", "appearance", "Dark", cx);
        click(cx, "SETTINGS_CONFIRM");

        assert_eq!(
            drawn_appearance(cx),
            ThemeAppearance::Dark,
            "the appearance the user confirmed is the one the application is drawn in"
        );

        let reported = cx.update(|window, _| window.appearance());
        let window = cx
            .windows()
            .into_iter()
            .next()
            .expect("the settings are drawn in a window");
        let (panel, mut cx) = reopen_settings(window, cx);
        write_draft_field(&panel, "appearance", "appearance", "System", &mut cx);
        click(&mut cx, "SETTINGS_CONFIRM");

        assert_eq!(
            stored(&cx).appearance,
            Appearance::System,
            "the draft should have moved the preference back to the system"
        );
        assert_eq!(
            drawn_appearance(&cx),
            match reported {
                WindowAppearance::Light | WindowAppearance::VibrantLight => ThemeAppearance::Light,
                WindowAppearance::Dark | WindowAppearance::VibrantDark => ThemeAppearance::Dark,
            },
            "following the system again means being drawn in the appearance the system reports"
        );
    }

    /// A text field commits when the caret leaves it, and pressing a button does not take the
    /// caret away from it: what was typed last is exactly what a commit has to carry.
    #[gpui_kit::test]
    fn what_was_typed_into_a_field_should_be_written_by_the_confirm_button(
        cx: &mut TestAppContext,
    ) {
        let (panel, cx) = settings(configured_settings(), cx);

        // The dialog opens the page the user would be on; what this is about is the field.
        panel.update(cx, |dialog, cx| {
            dialog.select("market_participants.okx", cx)
        });
        settle(cx);

        let control = cx
            .debug_bounds("SETTINGS_CONTROL_market_participants.okx.api_key")
            .expect("the OKX page should draw the field's own box");
        // The row holds the label above the field and the description under it, so a point inside
        // the row is not necessarily inside the field: what the click is aimed at is the field's own
        // box, which is the control the caret has to land in.
        cx.simulate_click(control.center(), Modifiers::none());
        assert_eq!(
            current_page(&panel, cx),
            "market_participants.okx",
            "clicking in the page must not move the dialog off the page the field is on"
        );
        cx.simulate_input("typed-without-leaving-the-field");
        settle(cx);

        // The field is not left before this runs, so what it holds is still only in the field:
        // clicking a button of the window's dialog does not take the caret away from it.
        click(cx, "SETTINGS_CONFIRM");

        let okx = VenueId::Okx;
        let written = match stored(cx).venue(&okx) {
            Some(VenueSettings::Okx(okx)) => okx.credentials.api_key.clone(),
            _ => panic!("the settings should still hold OKX"),
        };
        assert_eq!(
            written, "okx-api-keytyped-without-leaving-the-field",
            "confirming should read what the fields hold without them losing the caret first"
        );
    }

    /// The close button is a cancel: what the session changed is dropped rather than written.
    #[gpui_kit::test]
    fn closing_should_discard_what_the_session_changed(cx: &mut TestAppContext) {
        let (panel, cx) = settings(Config::default(), cx);

        write_draft_field(&panel, "appearance", "language", "zh-CN", cx);
        assert_eq!(
            drafted(&panel, cx).language,
            Language::ZhCn,
            "the draft should hold what the user changed"
        );

        click(cx, "SETTINGS_CLOSE");

        assert_eq!(
            stored(cx).language,
            Language::En,
            "closing the settings must not write what the session changed"
        );
        assert!(
            cx.debug_bounds("SETTINGS_DIALOG").is_none(),
            "closing should take the settings away"
        );
    }

    /// The close button is the settings' own way off the layer: what takes the panel away is the
    /// dismissal the panel emits, so the panel leaves as the dialog it was opened as rather than
    /// as whatever the layer happens to have on top, and the state it held goes with it.
    #[gpui_kit::test]
    fn the_close_button_should_close_the_settings_by_their_own_signal(cx: &mut TestAppContext) {
        let (panel, cx) = settings(Config::default(), cx);
        // What is kept here is a weak handle: a test holding the panel would be a second strong
        // reference, and the panel would outlive its dialog for as long as the test does, which
        // is exactly what tells a panel that was released from one that leaked.
        let dismissed = panel.downgrade();
        drop(panel);
        assert!(
            cx.debug_bounds("SETTINGS_DIALOG").is_some(),
            "the settings should be drawn while they are open"
        );

        click(cx, "SETTINGS_CLOSE");

        assert!(
            cx.debug_bounds("SETTINGS_DIALOG").is_none(),
            "the signal the close button emits should take the settings off the layer"
        );
        assert!(
            dismissed.upgrade().is_none(),
            "the panel that closed itself should have been released with its dialog"
        );
    }

    /// A commit that cannot be stored is reported rather than swallowed, and the dialog stays
    /// where it is so the user can try again instead of losing what they configured.
    #[gpui_kit::test]
    fn a_commit_that_cannot_be_stored_should_be_reported_and_keep_the_dialog(
        cx: &mut TestAppContext,
    ) {
        let (manager, _blocking, _directory) = manager_behind_a_file();
        let (panel, cx) = settings_over(manager, Language::ZhCn, cx);

        write_draft_field(&panel, "appearance", "language", "en", cx);
        assert_eq!(
            drafted(&panel, cx).language,
            Language::En,
            "the session should hold the language the user chose"
        );
        click(cx, "SETTINGS_CONFIRM");

        assert!(
            cx.debug_bounds("BASE_UI_ALERT").is_some(),
            "a commit that could not be stored should tell the user"
        );
        assert_eq!(
            stored(cx).language,
            Language::ZhCn,
            "settings that could not be stored must not be published as if they had been"
        );
        assert!(
            cx.debug_bounds("SETTINGS_DIALOG").is_some(),
            "the settings should stay open so the user can try again"
        );
        assert_eq!(
            drafted(&panel, cx).language,
            Language::En,
            "what the user changed should survive a commit that failed"
        );
        assert_eq!(
            cx.read(|cx| *cx.global::<LanguageCode>()),
            LanguageCode::ZhCn,
            "an interface the settings did not move should stay where it was"
        );
    }

    /// The proxy catalog is part of the same session: a row asks for a delete, the user says so in
    /// the alert that asks, and what the rows are drawn from loses the entry.
    ///
    /// What the alert confirms is one change of the session and nothing more: the settings the
    /// session was opened over keep the entry until the user confirms them in turn.
    #[gpui_kit::test]
    fn a_deleted_proxy_should_leave_the_catalog_only_when_it_is_confirmed(cx: &mut TestAppContext) {
        let (panel, cx) = settings(configured_settings(), cx);
        request_delete(&panel, "hk", cx);
        click(cx, "BASE_UI_ALERT_OK");

        assert!(
            !drafted(&panel, cx).proxies.contains_key("hk"),
            "the draft should hold the catalog without the entry the user deleted"
        );
        assert!(
            stored(cx).proxies.contains_key("hk"),
            "a deleted row must not reach the settings before the user confirms it"
        );

        click(cx, "SETTINGS_CONFIRM");

        assert!(
            !stored(cx).proxies.contains_key("hk"),
            "confirming should remove the entry from the settings"
        );
    }

    /// A delete is the one row action that takes something away, so it is asked about before it is
    /// recorded: the alert is where the user hears that the routes naming the entry go with it, and
    /// a user who walks away from it leaves the catalog exactly as it was.
    #[gpui_kit::test]
    fn a_delete_should_ask_before_it_takes_the_entry_out_of_the_catalog(cx: &mut TestAppContext) {
        let (panel, cx) = settings(configured_settings(), cx);
        request_delete(&panel, "hk", cx);

        assert!(
            cx.debug_bounds("BASE_UI_ALERT").is_some(),
            "a row that asks for a delete should be answered with an alert rather than the entry \
             going at once"
        );
        assert!(
            drafted(&panel, cx).proxies.contains_key("hk"),
            "the entry should still be in the session while the alert asking about it is up"
        );

        click(cx, "BASE_UI_ALERT_CANCEL");

        assert!(
            drafted(&panel, cx).proxies.contains_key("hk"),
            "walking away from the alert should leave the entry in the session"
        );
        assert!(
            cx.debug_bounds("BASE_UI_ALERT").is_none(),
            "the alert should leave the window once it is dismissed"
        );
    }

    /// The alert a delete raises names the entry it is about, in either language.
    ///
    /// The name is the one thing that tells two rows of the catalog apart, so a message that
    /// dropped it would ask the user to confirm a deletion without saying which entry goes. Both
    /// locales therefore have to carry the placeholder the name is written into, and the name has
    /// to arrive in the text the alert is given.
    #[gpui_kit::test]
    fn the_delete_alert_should_name_the_entry_in_both_languages(cx: &mut TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("embedded locales should load");
        });

        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            cx.update(|cx| {
                cx.set_global(language);
            });

            let (declared, title, message) = cx.read(|cx| {
                let i18n = cx.global::<i18n::translator::types::I18n>();

                (
                    i18n.try_translate(language, "settings_ui.proxies.delete_message")
                        .is_some(),
                    t!(cx, "settings_ui.proxies.delete_title"),
                    t!(cx, "settings_ui.proxies.delete_message", name = "osaka"),
                )
            });

            assert!(
                declared,
                "both locales should define the message a delete is confirmed by"
            );
            assert!(
                !title.is_empty() && !message.is_empty(),
                "the title and the message of {language:?} should not be empty strings"
            );
            assert!(
                message.contains("osaka"),
                "the message of {language:?} should name the entry it is about: {message}"
            );
            assert!(
                !message.contains("%{"),
                "every placeholder of {language:?} should be filled in: {message}"
            );
        }
    }

    /// The form over the settings is a dialog of its own on the same session, drawn over them:
    /// the settings stay on the layer while the form is open, and what takes the form away is the
    /// form being dismissed rather than the settings being mounted again under it.
    ///
    /// What the form was started from is an entry the settings hold, so confirming it records
    /// an entry the session already has: what this asks is that the session survives the form,
    /// that the settings are still the ones it was opened over, and that they are not written by
    /// it.
    #[gpui_kit::test]
    fn a_form_over_the_settings_should_write_the_draft_and_return_to_them(cx: &mut TestAppContext) {
        let (panel, cx) = settings(configured_settings(), cx);
        open_edit_form(&panel, "hk", cx);

        assert!(
            cx.debug_bounds("SETTINGS_PROXY_EDIT").is_some(),
            "the form should be drawn over the settings"
        );
        assert!(
            cx.debug_bounds("SETTINGS_DIALOG").is_some(),
            "the settings should still be drawn under the form"
        );

        click(cx, crate::editor::CONFIRM_SELECTOR);

        assert!(
            drafted(&panel, cx).proxies.contains_key("hk"),
            "the session should be the one the form wrote in"
        );
        assert!(
            stored(cx).proxies.contains_key("hk"),
            "a form over the settings must not write them"
        );
        assert!(
            cx.debug_bounds("SETTINGS_DIALOG").is_some(),
            "the settings the form was opened over should be the ones left showing"
        );

        click(cx, "SETTINGS_CONFIRM");

        assert!(
            stored(cx).proxies.contains_key("hk"),
            "the settings should hold what the form wrote once they are confirmed"
        );
    }

    /// An entry the user types into the form and confirms is one change of the session, and
    /// the settings only move when the user confirms them.
    ///
    /// What the user typed is in the field and nowhere else: a field reports itself when the
    /// caret leaves it, and the confirm button does not take the caret away, so the form has to
    /// read the fields back. This is the whole of adding a proxy, from the button to the file.
    #[gpui_kit::test]
    fn a_typed_entry_should_reach_the_settings_only_when_the_session_is_confirmed(
        cx: &mut TestAppContext,
    ) {
        let (panel, cx) = settings(Config::default(), cx);
        open_add_form(&panel, cx);

        type_into(cx, "SETTINGS_PROXY_FIELD_name", "osaka");
        type_into(cx, "SETTINGS_PROXY_FIELD_addr", "127.0.0.1");
        type_into(cx, "SETTINGS_PROXY_FIELD_port", "1080");
        click(cx, crate::editor::CONFIRM_SELECTOR);

        assert!(
            cx.debug_bounds("SETTINGS_PROXY_ADD").is_none(),
            "confirming a form should take it away"
        );
        assert!(
            cx.debug_bounds("SETTINGS_DIALOG").is_some(),
            "the settings the form was opened over should still be drawn when the form is confirmed"
        );
        let session = drafted(&panel, cx);
        assert_eq!(
            session.proxies.get("osaka").map(|proxy| proxy.port),
            Some(1080),
            "the entry the user typed should be what the session holds: {:#?}",
            session.proxies
        );
        assert!(
            !stored(cx).proxies.contains_key("osaka"),
            "a form over the settings must not write them"
        );

        click(cx, "SETTINGS_CONFIRM");

        assert_eq!(
            stored(cx).proxies.get("osaka").map(|proxy| proxy.port),
            Some(1080),
            "confirming the settings should store the entry the form added"
        );
    }

    /// A new entry resolves names on this side of the proxy until the form is asked for the other
    /// mode: the control opens on the mode the entry is recorded with, so what a user who touches
    /// nothing gets is the default and not whatever the form happened to hold.
    #[gpui_kit::test]
    fn a_new_entry_should_resolve_names_here_until_the_form_is_asked_otherwise(
        cx: &mut TestAppContext,
    ) {
        let (panel, cx) = settings(Config::default(), cx);
        open_add_form(&panel, cx);

        type_into(cx, "SETTINGS_PROXY_FIELD_name", "osaka");
        type_into(cx, "SETTINGS_PROXY_FIELD_addr", "127.0.0.1");
        type_into(cx, "SETTINGS_PROXY_FIELD_port", "1080");
        click(cx, crate::editor::CONFIRM_SELECTOR);

        assert_eq!(
            drafted(&panel, cx)
                .proxies
                .get("osaka")
                .map(|proxy| proxy.resolve_dns_at_remote),
            Some(false),
            "the entry the form recorded should resolve names here"
        );
    }

    /// The mode the control shows is the mode the entry is recorded with, and the other choice of
    /// it is what a user picks to have the proxy resolve the names.
    #[gpui_kit::test]
    fn choosing_the_proxy_mode_should_be_what_the_entry_is_recorded_with(cx: &mut TestAppContext) {
        let (panel, cx) = settings(Config::default(), cx);
        open_add_form(&panel, cx);

        type_into(cx, "SETTINGS_PROXY_FIELD_name", "osaka");
        type_into(cx, "SETTINGS_PROXY_FIELD_addr", "127.0.0.1");
        type_into(cx, "SETTINGS_PROXY_FIELD_port", "1080");
        click(cx, crate::editor::DNS_MODE_SELECTOR);
        assert!(
            cx.debug_bounds("TEST_MENU_ITEM_proxy").is_some(),
            "the control should offer the mode that resolves names there"
        );
        click(cx, "TEST_MENU_ITEM_proxy");
        click(cx, crate::editor::CONFIRM_SELECTOR);

        assert_eq!(
            drafted(&panel, cx)
                .proxies
                .get("osaka")
                .map(|proxy| proxy.resolve_dns_at_remote),
            Some(true),
            "the entry should be recorded with the mode the user chose"
        );
    }

    /// The form that changes an entry is opened on the mode the entry is stored under: a form that
    /// started from the default would record the entry as a mode the user never chose, which is the
    /// one way an edit the user made nowhere could still change the settings.
    #[gpui_kit::test]
    fn an_edit_form_should_open_on_the_mode_the_entry_is_stored_under(cx: &mut TestAppContext) {
        let (panel, cx) = settings(configured_settings(), cx);
        assert!(
            stored(cx)
                .proxies
                .get("hk")
                .is_some_and(|proxy| proxy.resolve_dns_at_remote),
            "the entry this test edits should be one the proxy resolves names for"
        );

        open_edit_form(&panel, "hk", cx);
        click(cx, crate::editor::CONFIRM_SELECTOR);

        assert!(
            drafted(&panel, cx)
                .proxies
                .get("hk")
                .is_some_and(|proxy| proxy.resolve_dns_at_remote),
            "confirming the form should record the entry with the mode it opened on"
        );
    }

    /// A page follows the language by being drawn again, but the tree holds the labels it was
    /// built with and nothing draws it again: a tree that kept them would be the one part of
    /// the settings still written in the language the user just left.
    #[gpui_kit::test]
    fn the_tree_should_be_labelled_in_the_language_the_user_confirms(cx: &mut TestAppContext) {
        let (panel, cx) = settings_in(Config::default(), Language::ZhCn, cx);

        assert!(
            labels(&panel, cx).contains(&SharedString::new_static("外观")),
            "the tree should open in the language the window opened in"
        );

        write_draft_field(&panel, "appearance", "language", "en", cx);
        assert_eq!(
            cx.read(|cx| *cx.global::<LanguageCode>()),
            LanguageCode::ZhCn,
            "the interface should not switch to a language the user has not confirmed"
        );

        click(cx, "SETTINGS_CONFIRM");

        assert_eq!(
            cx.read(|cx| *cx.global::<LanguageCode>()),
            LanguageCode::En,
            "confirming the language should switch the interface to it"
        );
        let translated = labels(&panel, cx);
        assert!(
            translated.contains(&SharedString::new_static("Appearance")),
            "the tree should be labelled in the language the user confirmed: {translated:?}"
        );
    }

    #[gpui_kit::test]
    fn the_dialog_should_open_on_the_default_page(cx: &mut TestAppContext) {
        let (dialog, cx) = settings(Config::default(), cx);

        dialog.read_with(cx, |dialog, cx| {
            assert_eq!(dialog.current_page, default_page());
            assert_eq!(dialog.query, gpui_kit::SharedString::default());
            assert!(dialog.filter.is_all());
            // The tree is selected on the page it shows, so the two columns say the same
            // thing from the first frame.
            let selected = dialog
                .tree
                .read(cx)
                .selected_item()
                .map(|item| item.id.to_string());
            assert_eq!(selected.as_deref(), Some(default_page()));
        });
    }

    /// The search narrows the tree and nothing else, so the two are one column: the field is as
    /// wide as the tree it filters, sits directly above it, and leaves the rest of the dialog
    /// to the page.
    #[gpui_kit::test]
    fn the_search_should_sit_in_the_column_of_the_tree_it_filters(cx: &mut TestAppContext) {
        let (_, cx) = settings(Config::default(), cx);

        let search = cx
            .debug_bounds("SETTINGS_SEARCH")
            .expect("the search field should be drawn");
        let tree = cx
            .debug_bounds("SETTINGS_TREE")
            .expect("the tree should be drawn");
        let dialog = cx
            .debug_bounds("SETTINGS_DIALOG")
            .expect("the dialog should be drawn");

        assert_eq!(search.size.width, px(256.0), "the search field is w_64");
        assert_eq!(
            tree.size.width, search.size.width,
            "the tree should be as wide as the field above it"
        );
        assert_eq!(tree.left(), search.left(), "the two should line up");
        assert!(
            tree.top() >= search.bottom(),
            "the search should sit above the tree it filters"
        );
        assert!(
            tree.right() < dialog.right(),
            "the column should leave the rest of the dialog to the page"
        );
    }

    /// The clear control is the field's own, and clearing the field is what brings the whole
    /// tree back: the field and the answer are one thing, so emptying one empties the other.
    #[gpui_kit::test]
    fn the_field_should_clear_the_search_it_was_filtered_by(cx: &mut TestAppContext) {
        let (dialog, cx) = settings(Config::default(), cx);

        let search = cx
            .debug_bounds("SETTINGS_SEARCH")
            .expect("the search field should be drawn");
        cx.simulate_click(search.center(), Modifiers::none());
        cx.simulate_input("proxy");
        cx.run_until_parked();

        assert!(
            !dialog.read_with(cx, |dialog, _| dialog.filter.is_all()),
            "typing into the field should narrow the tree"
        );

        // The control is drawn inside the field, at the end of the row the field fills.
        cx.simulate_click(
            gpui_kit::point(search.right() - px(18.0), search.center().y),
            Modifiers::none(),
        );
        cx.run_until_parked();

        assert!(
            dialog.read_with(cx, |dialog, _| dialog.filter.is_all()),
            "clearing the field should admit everything again"
        );
    }

    /// A group is not a page: selecting one leaves the page behind it where it was, which is
    /// what makes opening and closing a folder harmless.
    #[gpui_kit::test]
    fn selecting_a_group_should_leave_the_page_alone(cx: &mut TestAppContext) {
        let (dialog, cx) = settings(Config::default(), cx);
        cx.run_until_parked();

        dialog.update(cx, |dialog, cx| dialog.select("proxy", cx));

        dialog.read_with(cx, |dialog, _| {
            assert_eq!(dialog.current_page, default_page());
        });
    }

    #[gpui_kit::test]
    fn selecting_a_page_should_show_it_and_reset_its_scroll(cx: &mut TestAppContext) {
        let (dialog, cx) = settings(configured_settings(), cx);
        cx.run_until_parked();

        dialog.update(cx, |dialog, cx| {
            dialog.select("market_participants.okx", cx)
        });

        dialog.read_with(cx, |dialog, _| {
            assert_eq!(dialog.current_page, "market_participants.okx");
        });
    }

    /// The page drawn is the page behind the entry the tree selects, so a selection moves the
    /// two together: a page moved on its own is one the tree puts back the next time it
    /// notifies, which is what leaves a field the user is typing into undrawn.
    ///
    /// The page selected here sits under a group the catalog closes, so this is also what says
    /// the tree reaches it: an entry the tree cannot select is one it draws no page for.
    #[gpui_kit::test]
    fn selecting_a_page_should_move_the_tree_with_it(cx: &mut TestAppContext) {
        let (panel, cx) = settings(configured_settings(), cx);

        panel.update(cx, |dialog, cx| {
            dialog.select("market_participants.okx", cx)
        });
        settle(cx);

        let selected = panel.read_with(cx, |dialog, cx| {
            dialog
                .tree
                .read(cx)
                .selected_item()
                .map(|item| item.id.to_string())
        });
        assert_eq!(
            selected.as_deref(),
            Some("market_participants.okx"),
            "the page and the entry the tree selects are one thing"
        );

        // Working in the page is what the tree used to answer with its own selection: the caret
        // goes into the field, and a tree that disagreed put its own page back under it.
        click(cx, "SETTINGS_CONTROL_market_participants.okx.api_key");
        assert_eq!(
            current_page(&panel, cx),
            "market_participants.okx",
            "the tree must not be able to move the page back off the page it is on"
        );
    }

    /// A venue's page is named by the venue it configures, and the tree entry is what opens it:
    /// every venue this build speaks is one page, so a shared label would make two venues tell
    /// apart by nothing the user can read.
    #[gpui_kit::test]
    fn a_venue_page_should_be_named_by_the_venue_it_configures(cx: &mut TestAppContext) {
        let (_panel, cx) = settings(Config::default(), cx);

        assert_eq!(
            page_title("market_participants.okx", cx),
            SharedString::new_static("OKX"),
            "the venue's page is named by the venue it configures"
        );
        assert_eq!(
            page_title("appearance", cx),
            SharedString::new_static("Appearance"),
            "every other page is named by the message key it declares"
        );
    }

    /// A group the catalog closes is built closed, and the page drawn under it is what opens
    /// it: what a group declares is the tree it is drawn as, and the page drawn and the entry
    /// the tree selects are one thing.
    #[gpui_kit::test]
    fn a_group_the_catalog_closes_should_be_built_closed_and_opened_for_the_page_that_is_drawn(
        cx: &mut TestAppContext,
    ) {
        let (panel, cx) = settings(configured_settings(), cx);

        let opening = labels(&panel, cx);
        assert!(
            opening.contains(&SharedString::new_static("Appearance"))
                && opening.contains(&SharedString::new_static("Market participants")),
            "the root page and the group should be drawn: {opening:?}"
        );
        assert!(
            !opening.contains(&SharedString::new_static("OKX")),
            "a group that is not open should hide what is under it: {opening:?}"
        );

        panel.update(cx, |dialog, cx| {
            dialog.select("market_participants.okx", cx)
        });
        settle(cx);

        let opened = labels(&panel, cx);
        assert!(
            opened.contains(&SharedString::new_static("OKX")),
            "the tree should open what hides the page it is on: {opened:?}"
        );
        let selected = panel.read_with(cx, |dialog, cx| {
            dialog
                .tree
                .read(cx)
                .selected_item()
                .map(|item| item.id.to_string())
        });
        assert_eq!(
            selected.as_deref(),
            Some("market_participants.okx"),
            "the page that is drawn should be the page the tree shows: {opened:?}"
        );
    }

    /// A query opens the groups it matches in: a match under a closed group is a match the
    /// user cannot see, which is what makes the search a way to a page rather than a way to
    /// its name.
    ///
    /// The page drawn is left where it is by the query used here, so what the tree draws the
    /// pages under a closed group from is the query and not the page the tree was moved to.
    #[gpui_kit::test]
    fn a_query_should_open_the_group_it_matches_in(cx: &mut TestAppContext) {
        let (panel, cx) = settings(configured_settings(), cx);

        panel.update(cx, |dialog, cx| dialog.set_query("a".into(), cx));
        settle(cx);

        assert_eq!(
            current_page(&panel, cx),
            "appearance",
            "the query admits the page that is drawn, so the page should not have moved"
        );
        let narrowed = labels(&panel, cx);
        assert!(
            narrowed.contains(&SharedString::new_static("Market participants")),
            "the query should keep the group it matches in: {narrowed:?}"
        );
        assert!(
            narrowed.contains(&SharedString::new_static("OKX")),
            "a group drawn for a query should be drawn open, or the match is one the user \
             cannot see: {narrowed:?}"
        );
    }

    /// A page the query does not admit is a page that is no longer drawn, so what is drawn
    /// moves to the first page the query does admit.
    #[gpui_kit::test]
    fn a_query_should_move_the_page_to_one_it_admits(cx: &mut TestAppContext) {
        let (dialog, cx) = settings(Config::default(), cx);

        dialog.update(cx, |dialog, cx| {
            dialog.select("market_participants.okx", cx);
            dialog.set_query("proxy".into(), cx);
        });

        dialog.read_with(cx, |dialog, _| {
            assert!(
                !dialog.filter.admits_page("market_participants.okx"),
                "the query should not admit the page that was selected"
            );
            assert!(
                dialog.filter.admits_page(&dialog.current_page),
                "the page that is drawn must be one the query admits"
            );
        });
    }

    /// A page whose own label the query does not match but one of whose fields it does is a
    /// page the query reaches: it is listed in the tree, opened as the page that is drawn, and
    /// only the fields that matched are drawn on it.
    #[gpui_kit::test]
    fn a_query_that_matches_a_field_should_open_the_page_that_holds_it(cx: &mut TestAppContext) {
        let (panel, cx) = settings(configured_settings(), cx);

        panel.update(cx, |dialog, cx| dialog.set_query("api_key".into(), cx));
        settle(cx);

        assert_eq!(
            current_page(&panel, cx),
            "market_participants.binance",
            "the page that holds the field should be the one the query opens"
        );
        let narrowed = labels(&panel, cx);
        assert!(
            narrowed.contains(&SharedString::new_static("Market participants")),
            "the group that holds the page should be kept: {narrowed:?}"
        );
        assert!(
            narrowed.contains(&SharedString::new_static("Binance"))
                && narrowed.contains(&SharedString::new_static("OKX")),
            "every page a matching field sits on should be listed: {narrowed:?}"
        );
        assert!(
            !narrowed.contains(&SharedString::new_static("Appearance")),
            "a page with no matching label or field should not be listed: {narrowed:?}"
        );
        assert!(
            cx.debug_bounds("SETTINGS_FIELD_market_participants.binance.spot.spot_api_key")
                .is_some(),
            "the field the query matched should be drawn on the page it opened"
        );
        assert!(
            cx.debug_bounds("SETTINGS_FIELD_market_participants.binance.spot.spot_private_key")
                .is_none(),
            "a field the query did not match should not be drawn on a page reached by a field"
        );
    }

    /// A query that matches nothing is answered in both columns: the tree has no entry left and
    /// the page that was open is not drawn behind it, so the two do not disagree the way they
    /// would if the page stayed selected while the tree went empty.
    #[gpui_kit::test]
    fn a_query_that_matches_nothing_should_say_so_in_both_columns(cx: &mut TestAppContext) {
        let (panel, cx) = settings(configured_settings(), cx);
        panel.update(cx, |dialog, cx| {
            dialog.select("proxy-management", cx);
            dialog.set_query("555".into(), cx);
        });
        settle(cx);

        panel.read_with(cx, |dialog, _| {
            assert!(
                dialog.filter.is_nothing(),
                "the query should admit nothing at all"
            );
        });
        assert!(
            labels(&panel, cx).is_empty(),
            "the tree should draw no entry for a query that admits nothing"
        );
        assert!(
            cx.debug_bounds("SETTINGS_TREE_NO_MATCHES").is_some(),
            "the tree column should say that nothing matched"
        );
        assert!(
            cx.debug_bounds("SETTINGS_PAGE_NO_MATCHES").is_some(),
            "the page column should say that nothing matched"
        );
        assert!(
            cx.debug_bounds("SETTINGS_PROXY_PAGE").is_none(),
            "the page the query took away must not be drawn behind the empty tree"
        );

        panel.update(cx, |dialog, cx| dialog.set_query("".into(), cx));
        settle(cx);

        assert!(
            cx.debug_bounds("SETTINGS_TREE_NO_MATCHES").is_none()
                && cx.debug_bounds("SETTINGS_PAGE_NO_MATCHES").is_none(),
            "clearing the query should take the message away"
        );
        assert!(
            cx.debug_bounds("SETTINGS_PROXY_PAGE").is_some(),
            "clearing the query should bring back the page that was open before it"
        );
    }

    /// Clearing a query brings the whole tree back, and with it the page that was drawn.
    #[gpui_kit::test]
    fn clearing_a_query_should_admit_everything_again(cx: &mut TestAppContext) {
        let (dialog, cx) = settings(Config::default(), cx);

        dialog.update(cx, |dialog, cx| {
            dialog.set_query("appearance".into(), cx);
            dialog.set_query("".into(), cx);
        });

        dialog.read_with(cx, |dialog, _| {
            assert!(dialog.filter.is_all());
        });
    }

    /// A build that has loaded nothing still offers the rows of its own venues: the venue list
    /// is this build's static answer, so a row per venue is there before anything is
    /// loaded, while a scope line is what the loaded venues answer and appears only once its
    /// venue is loaded.
    ///
    /// What is asserted here is the dialog the entry's own read of the loaded venues built —
    /// not a catalog assembled beside it — so a page source that went back to a declaration
    /// view, or one that read a scope list before its venue was loaded, would be caught here.
    #[gpui_kit::test]
    fn an_unconnected_build_should_still_offer_the_declared_settings(cx: &mut TestAppContext) {
        let (dialog, cx) = settings_of_an_unconnected_build(cx);
        let connector = cx.read(|cx| cx.global::<MarketParticipantsConnector>().clone());
        let venues = connector.supported_venues();

        assert!(
            connector.venues().scopes().is_empty(),
            "the settings are opened over an empty loaded-venues view"
        );

        let applications = dialog.read_with(cx, |dialog, _| {
            dialog
                .catalog
                .page("proxy-applications")
                .expect("the applications page is built")
                .clone()
        });
        let rows = applications
            .sections
            .iter()
            .filter(|section| {
                section.title.as_ref().and_then(crate::catalog::Text::key)
                    == Some("settings_ui.proxies.venue_section")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows.len(),
            venues.len(),
            "one row per venue this build speaks, whether or not anything is loaded"
        );
        for (row, venue) in rows.iter().zip(venues.iter()) {
            assert_eq!(
                row.fields.len(),
                1,
                "a venue's own line is the whole of its row"
            );
            assert_eq!(
                row.fields[0].id,
                format!("proxy-applications.{}", venue.as_str()),
                "the row stands for the venue it is drawn under"
            );
        }

        // The page is drawn rather than skipped: the proxy lines that are not a market's are
        // there for a build that has loaded nothing.
        type_into(cx, "SETTINGS_SEARCH", "applications");
        assert!(
            cx.debug_bounds("SETTINGS_FIELD_proxy-applications.kytrix.cloud")
                .is_some(),
            "the rows that belong to no market are drawn by a build that has loaded nothing"
        );

        // A venue's origins are its own configuration's answer rather than a load's, so they are
        // there for a build that has loaded nothing — and a load does not add or move one.
        let binance = VenueId::Binance;
        let page = catalog(&crate::schema::tests::venues())
            .page("market_participants.binance")
            .expect("the venue's page is built")
            .clone();
        let scope = page
            .sections
            .iter()
            .flat_map(|section| &section.fields)
            .find(|field| field.label.key().is_none())
            .expect("a venue that has loaded nothing still offers the origin of a scope")
            .clone();
        let before = page
            .sections
            .iter()
            .flat_map(|section| &section.fields)
            .map(|field| field.id.clone())
            .collect::<Vec<_>>();

        cx.update(|_, cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&venues),
                    &[],
                    &[],
                )
                .expect("the declared scopes load");
        });
        let loaded = catalog(&crate::schema::tests::venues());
        let after = loaded
            .page("market_participants.binance")
            .expect("the venue's page is built")
            .sections
            .iter()
            .flat_map(|section| &section.fields)
            .map(|field| field.id.clone())
            .collect::<Vec<_>>();

        assert_eq!(
            before, after,
            "loading a venue does not add or move an origin row: the rows are the stored scopes"
        );
        assert!(
            scope
                .id
                .as_ref()
                .starts_with(&format!("market_participants.{}.", binance.as_str())),
            "the origin row stands for a scope of its venue: {}",
            scope.id
        );
    }

    /// Every page is drawn, one after another.
    ///
    /// What this is for is the frames rather than the pixels: a page is built while the
    /// dialog is being rendered, so a field that reached back into the dialog — or a view
    /// that read it while it drew itself — would be a panic rather than a wrong color, and
    /// this is what would find it.
    #[gpui_kit::test]
    fn every_page_should_be_drawable(cx: &mut TestAppContext) {
        let (dialog, cx) = settings(configured_settings(), cx);
        let pages = pages()
            .iter()
            .map(|page| page.id.clone())
            .collect::<Vec<_>>();

        for page in pages {
            dialog.update(cx, |dialog, cx| dialog.select(page.as_str(), cx));
            cx.run_until_parked();

            dialog.read_with(cx, |dialog, _| {
                assert_eq!(
                    dialog.current_page, page,
                    "selecting {page} should draw it rather than another"
                );
            });
        }
    }

    /// A venue's line is a row a user can set before any proxy exists: the control is operable
    /// with an empty catalog, and the lines it offers are the row's own choices — the default
    /// line among them.
    ///
    /// Every venue starts on the default line, so a row that was only operable once the catalog
    /// held an entry would be one a fresh installation cannot put back on it.
    #[gpui_kit::test]
    fn a_proxy_line_should_be_settable_with_an_empty_catalog(cx: &mut TestAppContext) {
        let (dialog, cx) = settings(Config::default(), cx);
        assert!(
            stored(cx).proxies.is_empty(),
            "the settings this test opens hold no proxy at all"
        );

        dialog.update(cx, |dialog, cx| dialog.select("proxy-applications", cx));
        settle(cx);

        click(cx, "SETTINGS_CONTROL_proxy-applications.binance");

        assert!(
            cx.debug_bounds("TEST_DROPDOWN_PANEL").is_some(),
            "a venue's line opens with nothing in the catalog: a disabled control opens nothing"
        );
        // An entry is drawn by a button the menu names after the entry's own id, so the lines the
        // open menu offers are read back by the tokens the settings store those lines under.
        assert!(
            cx.debug_bounds("TEST_MENU_ITEM_@inherit").is_some(),
            "the row offers the default line, which is the line every venue starts on"
        );
        assert!(
            cx.debug_bounds("TEST_MENU_ITEM_@direct").is_some(),
            "beside the one that names no proxy at all"
        );
    }

    /// The proxy catalog is drawn by the table rather than by the page's own fields, and a
    /// catalog with nothing in it says so once, across the body, rather than in every column
    /// of a row that has no entry behind it.
    #[gpui_kit::test]
    fn the_proxy_page_should_be_drawable_with_an_empty_catalog(cx: &mut TestAppContext) {
        let (dialog, cx) = settings(Config::default(), cx);

        dialog.update(cx, |dialog, cx| dialog.select("proxy-management", cx));
        settle(cx);

        dialog.read_with(cx, |dialog, cx| {
            assert_eq!(dialog.current_page, "proxy-management");
            assert_eq!(
                base_ui::table::TableDataSet::<6>::rows_count(dialog.proxies.read(cx)),
                0,
                "a catalog with nothing in it has no rows for the table to draw"
            );
        });

        let message = cx
            .debug_bounds("SETTINGS_PROXY_EMPTY")
            .expect("the empty catalog should draw the message it has to say");
        let table = cx
            .debug_bounds("SETTINGS_PROXY_TABLE")
            .expect("the page should draw the frame the table is drawn in");
        assert!(
            (message.center().x - table.center().x).abs() < px(1.0),
            "the message should be drawn across the whole table rather than inside one of its \
             columns: it is {message:?} in {table:?}"
        );
    }

    /// The way to add an entry is the right end of the row above the catalog, and the catalog
    /// below that row is the rest of the page: the page is as tall as the dialog gives it, so the
    /// table is not a height of its own but whatever the row above it left.
    #[gpui_kit::test]
    fn the_add_button_should_sit_above_a_table_that_fills_the_page(cx: &mut TestAppContext) {
        let (dialog, cx) = settings(Config::default(), cx);
        dialog.update(cx, |dialog, cx| dialog.select("proxy-management", cx));
        settle(cx);

        let page = cx
            .debug_bounds("SETTINGS_PROXY_PAGE")
            .expect("the proxy page should be drawn");
        let add = cx
            .debug_bounds("SETTINGS_PROXY_ADD_BUTTON")
            .expect("the page should draw the button that adds an entry");
        let table = cx
            .debug_bounds("SETTINGS_PROXY_TABLE")
            .expect("the page should draw the frame the table is drawn in");
        // The tree is the foot of the column beside the page, so where it ends is where the body
        // the page is drawn in ends: a page that stopped at its own two rows would end higher.
        let column = cx
            .debug_bounds("SETTINGS_TREE")
            .expect("the dialog should draw the tree beside the page");

        assert_eq!(
            add.size.height,
            UnifiedSize::Medium.h(),
            "the button that adds an entry is a medium one: it is {add:?}"
        );
        assert!(
            (add.right() - page.right()).abs() < px(1.0),
            "the button belongs at the right end of the row above the table: it is {add:?} in \
             {page:?}"
        );
        assert!(
            add.bottom() <= table.top(),
            "the table belongs below the button rather than beside it: {table:?} under {add:?}"
        );
        assert_eq!(
            page.bottom(),
            column.bottom(),
            "the page should be the whole of the body it is drawn in: {page:?} beside {column:?}"
        );
        assert_eq!(
            table.bottom(),
            page.bottom(),
            "the table should be the rest of the page: {table:?} in {page:?}"
        );
        assert_eq!(
            table.size.width, page.size.width,
            "the table should be as wide as the page it fills: {table:?} in {page:?}"
        );
    }

    /// An entry is a row of the catalog: the table counts it, and the body draws it instead of
    /// the message that says there is nothing in the catalog.
    #[gpui_kit::test]
    fn a_catalog_with_an_entry_should_draw_the_entry_rather_than_the_empty_message(
        cx: &mut TestAppContext,
    ) {
        let (panel, cx) = settings(configured_settings(), cx);
        panel.update(cx, |dialog, cx| dialog.select("proxy-management", cx));
        settle(cx);

        panel.read_with(cx, |dialog, cx| {
            assert_eq!(
                base_ui::table::TableDataSet::<6>::rows_count(dialog.proxies.read(cx)),
                1,
                "the entry the catalog holds should be the one row of the table"
            );
        });
        assert!(
            cx.debug_bounds("SETTINGS_PROXY_PAGE").is_some(),
            "the catalog should still be the page that is drawn, so that the message being gone \
             is the table's doing rather than the page's"
        );
        assert!(
            cx.debug_bounds("SETTINGS_PROXY_EMPTY").is_none(),
            "a catalog that holds an entry must not draw the message that it holds none"
        );
    }

    /// Every page the catalog declares is reachable from the tree, and every entry of the
    /// tree leads to one: a page nobody can select is a page that does not exist, and an
    /// entry that selects nothing is a dead end.
    #[test]
    fn the_tree_and_the_pages_should_name_each_other() {
        let catalog = test_catalog();
        let mut reachable = catalog.tree.iter().flat_map(leaves).collect::<Vec<_>>();
        reachable.sort_unstable();

        let mut declared = catalog
            .pages
            .iter()
            .map(|page| page.id.clone())
            .collect::<Vec<_>>();
        declared.sort_unstable();

        assert_eq!(reachable, declared);
        assert!(SearchFilter::new("", &[]).is_all());
    }

    /// The form is drawn over the settings and takes the mouse from them: a click at the
    /// settings' own confirm button reaches the form over it instead, so the settings under an
    /// open form are out of reach while it is up.
    ///
    /// What draws the form is the frame the request for it asks for: nothing here refreshes the
    /// window, so a form that reached the layer without a frame ever drawing it would leave the
    /// click to land on the settings underneath.
    #[gpui_kit::test]
    fn a_form_should_keep_the_settings_under_it_out_of_reach(cx: &mut TestAppContext) {
        let (panel, cx) = settings(configured_settings(), cx);
        // The session holds a change before the form opens, so the click below has something to
        // write if it reaches the settings: without it, "the settings were not written" would hold
        // even for a click that went straight through the form.
        write_draft_field(&panel, "appearance", "appearance", "Dark", cx);
        open_edit_form(&panel, "hk", cx);
        let before = stored(cx);

        let form = cx.debug_bounds("SETTINGS_PROXY_EDIT").expect(
            "the request alone should be enough for the form over the settings to be drawn",
        );
        let confirm = cx
            .debug_bounds("SETTINGS_CONFIRM")
            .expect("the settings should draw their own confirm button");
        assert!(
            !form.contains(&confirm.center()),
            "the settings' confirm button should be outside the form's own panel, or a swallowed \
             click would prove nothing: the form is {form:?} and the button is {confirm:?}"
        );

        click(cx, "SETTINGS_CONFIRM");

        assert!(
            cx.debug_bounds("SETTINGS_DIALOG").is_some(),
            "the settings should not be confirmed by a click that was aimed through the form"
        );
        assert!(
            cx.debug_bounds("SETTINGS_PROXY_EDIT").is_some(),
            "the form should still be open over the settings"
        );
        assert_eq!(
            stored(cx),
            before,
            "the click should have been swallowed by the form rather than confirm the settings"
        );
        assert_eq!(
            stored(cx).appearance,
            Appearance::System,
            "the change the session was holding must not have been written by a click that was \
             aimed at the settings through the form"
        );
    }

    /// The pages a tree entry leads to, which for a group are the pages of its children.
    fn leaves(node: &crate::catalog::TreeNodeDef) -> Vec<SharedString> {
        match node {
            crate::catalog::TreeNodeDef::Page(id) => vec![id.clone()],
            crate::catalog::TreeNodeDef::Group { children, .. } => {
                children.iter().flat_map(leaves).collect()
            }
        }
    }

    /// A write that moves a venue's credential shape moves the page with it: the fields the user
    /// is looking at are the ones the shape the venue holds now has, so what they type into the
    /// page as it is drawn lands in that shape rather than nowhere.
    ///
    /// A write through a control of a venue's page lands in that venue's own row, and a venue's
    /// market holds its own values: what a control offers is the venue's own API rather than
    /// anything a value moves.
    ///
    /// The write goes through the field's own access, which is what the control of that field
    /// holds — the same conversion the dialog's `write_field` is handed — so this is about where a
    /// page's write lands, not about where a click lands.
    #[gpui_kit::test]
    fn a_write_through_a_venue_row_should_land_in_that_venues_row(cx: &mut TestAppContext) {
        let (panel, cx) = settings(configured_settings(), cx);
        panel.update(cx, |dialog, cx| {
            dialog.select("market_participants.binance", cx)
        });
        settle(cx);

        let spot = "SETTINGS_CONTROL_market_participants.binance.spot.spot_api_key";
        assert!(
            cx.debug_bounds(spot).is_some(),
            "the page opens on the market the venue publishes"
        );

        let access = field_access(
            "market_participants.binance",
            "market_participants.binance.spot.spot_api_key",
        );
        panel.update(cx, |dialog, cx| {
            dialog.write_field(access, FieldValue::new_static("typed-key"), cx);
        });
        settle(cx);

        panel.read_with(cx, |dialog, cx| {
            let stored = dialog.draft.read(cx).config();
            let VenueSettings::Binance(binance) = stored
                .venue(&VenueId::Binance)
                .expect("the fixture settings name every venue this build speaks")
            else {
                panic!("binance is configured as Binance");
            };
            assert_eq!(
                binance.credentials.spot_api_key, "typed-key",
                "the value landed in the venue the row belongs to"
            );
            assert_eq!(
                binance.credentials.usd_m_perp_api_key, "binance-perp-key",
                "and the venue's other market kept its own"
            );
        });
    }

    /// Longbridge is a page of the settings a user can open, not one that exists only in the
    /// catalog's own builder.
    ///
    /// The dialog builds its venue pages from the venue list this build *serves*, so a venue a
    /// previous build declared settings for without serving it is a page nothing can reach: this
    /// opens that page through the served list and reads the boxes its rows are drawn in. The
    /// query narrows the page to its own rows — a page is drawn inside a scroll viewport, and a
    /// row below the fold is one the window never lays out — so every row the design names is
    /// asserted rather than the first one that happens to fit.
    ///
    /// A row's *control* is asserted for the rows a text field draws. The region row's control is
    /// a `Select`, which registers no debug bounds of its own — the same is true of every select
    /// in the settings, the appearance page's and OKX's region among them — so what is asserted
    /// for that row is that the row itself is drawn; which choice it offers is the catalogue's,
    /// and the page's own round trip below is what writes through it.
    #[gpui_kit::test]
    fn longbridges_page_should_be_a_page_of_the_served_settings(cx: &mut TestAppContext) {
        let (panel, cx) = settings(configured_settings(), cx);

        assert!(
            !labels(&panel, cx).is_empty(),
            "the tree should draw at least one entry"
        );

        panel.update(cx, |dialog, cx| {
            dialog.select("market_participants.longbridge", cx);
            dialog.set_query("longbridge".into(), cx);
        });
        settle(cx);

        assert_eq!(
            current_page(&panel, cx),
            "market_participants.longbridge",
            "the page the served settings declare is the page that is opened"
        );
        assert_eq!(
            page_title("market_participants.longbridge", cx),
            SharedString::new_static("Longbridge"),
            "the page is named by the venue it configures"
        );

        for field in [
            "SETTINGS_FIELD_market_participants.longbridge.region",
            "SETTINGS_FIELD_market_participants.longbridge.app_key",
            "SETTINGS_FIELD_market_participants.longbridge.app_secret",
            "SETTINGS_FIELD_market_participants.longbridge.access_token",
        ] {
            assert!(
                cx.debug_bounds(field).is_some(),
                "the page should draw the row {field} names"
            );
        }
        for control in [
            "SETTINGS_CONTROL_market_participants.longbridge.app_key",
            "SETTINGS_CONTROL_market_participants.longbridge.app_secret",
            "SETTINGS_CONTROL_market_participants.longbridge.access_token",
        ] {
            assert!(
                cx.debug_bounds(control).is_some(),
                "the page should draw the field behind {control}"
            );
        }
    }

    /// What a user configures on Longbridge's page is what the settings hold: the region moves
    /// the site the venue is reached on, and the three values land in the venue's own credential
    /// set, once the session is confirmed.
    ///
    /// The write goes through the page's own rows and the commit through the dialog's own confirm
    /// button, over the manager the settings session was opened from — which is what makes this a
    /// round trip through the settings rather than through the configuration type: the values are
    /// read back from the manager every other part of the application reads.
    #[gpui_kit::test]
    fn longbridges_page_should_write_the_region_and_the_credentials(cx: &mut TestAppContext) {
        let longbridge = VenueId::Longbridge;
        let (panel, cx) = settings(Config::default(), cx);

        panel.update(cx, |dialog, cx| {
            dialog.select("market_participants.longbridge", cx)
        });
        settle(cx);

        write_draft_field(
            &panel,
            "market_participants.longbridge",
            "market_participants.longbridge.region",
            "china",
            cx,
        );

        // The three values are typed into the controls the page draws, and the commit is what
        // reads them: a text row commits when the caret leaves it, and pressing a button of the
        // dialog does not take the caret away from it.
        for (selector, value) in [
            (
                "SETTINGS_CONTROL_market_participants.longbridge.app_key",
                "typed-app-key",
            ),
            (
                "SETTINGS_CONTROL_market_participants.longbridge.app_secret",
                "typed-app-secret",
            ),
            (
                "SETTINGS_CONTROL_market_participants.longbridge.access_token",
                "typed-access-token",
            ),
        ] {
            type_into(cx, selector, value);
        }

        click(cx, "SETTINGS_CONFIRM");

        let written = stored(cx);
        let Some(VenueSettings::Longbridge(settings)) = written.venue(&longbridge) else {
            panic!("longbridge is configured as Longbridge");
        };
        assert_eq!(
            settings.credentials,
            LongbridgeCredentials {
                app_key: SmolStr::new_static("typed-app-key"),
                app_secret: SmolStr::new_static("typed-app-secret"),
                access_token: SmolStr::new_static("typed-access-token"),
            },
            "the three values landed in the venue's own credential set"
        );
        assert_eq!(
            settings.region,
            LongbridgeRegion::China,
            "the region row wrote the region the user chose"
        );
        assert_eq!(
            settings.addresses().http.as_str(),
            "https://openapi.longbridge.cn/",
            "and the venue is reached on the site that region publishes"
        );
    }
}

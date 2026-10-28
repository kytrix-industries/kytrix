//! The index selector: the dialog a pane opens to choose which indices it follows.
//!
//! The catalogue is the service's, so the dialog asks for it when it opens and shows
//! what came back. The column on the left is that catalogue's categories, drawn as a
//! tree: which category the list is narrowed to is the row the tree has selected, and a
//! row is named after the category it stands for rather than by the position it is drawn
//! at.
//!
//! It is a view rather than a widget: the framework's status handle belongs to the pane,
//! and a dialog that reported into it would put a pane's failure banner over a list the
//! user is still reading. So the dialog keeps its own three states — loading, listed,
//! failed — and offers its own way to read the catalogue again, which is what the
//! selector in `instrument_selector_ui` does as well.
//!
//! # What the user's changes mean
//!
//! The dialog holds a *set* of chosen topics while it is open; the pane's selection is
//! only replaced when the user confirms. Cancelling therefore changes nothing, and a
//! confirmation is the whole selection rather than a difference, which is what makes
//! the pane's own scope unambiguous.
//!
//! # Where a search narrows
//!
//! The field over the list searches the category on show rather than the catalogue: the
//! category is what the list is drawn from, and how far down it a reader is. Two
//! consequences follow. Changing the category clears the field, because a search belongs
//! to the list it was typed over — and a search that outlived its category would be a
//! query the reader never asked of the list now on screen. And a search that leaves
//! nothing is drawn as a search that found nothing rather than as a category with nothing
//! in it: those are two different facts, and only one of them is answered by clearing the
//! field.

use std::{rc::Rc, sync::Arc};

use ahash::{HashSet, HashSetExt};
use base_ui::{
    button::Button,
    icon::IconKind,
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    interactive::InteractiveVariantExt,
    layer::LayerExt,
    separator::Separator,
    size::UnifiedSizable,
    spinner::Spinner,
    static_sized_list::StaticSizedList,
    style::theme::ThemeExt,
    toggle::Toggle,
    tree::Tree,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, DismissEvent, ElementId, Entity, EventEmitter,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window,
    base::{Disableable, Selectable, TreeItem, TreeState},
    div, px,
};
use i18n::{language::codes::LanguageCode, t};
use kytrix_service_provider::service::{
    connector::KytrixServiceConnector, engine::FinancialIndexTopic,
};
use smol_str::SmolStr;
use widget::{
    binding::{WidgetBinding, WidgetRpcResult},
    error::LocalizedError,
    runtime::WidgetService,
};
use widget_tokio_runtime::global;

use super::{
    model::{
        FinancialIndexCatalog, FinancialIndexDescriptor, FinancialIndicesDataError,
        MAX_SELECTED_INDICES,
    },
    service::{
        FinancialIndicesProtocol, FinancialIndicesRequest, FinancialIndicesResponse,
        FinancialIndicesScope, FinancialIndicesService, FinancialIndicesStreamMessage,
    },
};

/// The size of the dialog's content area, which the dialog panel is given.
const DIALOG_WIDTH: f32 = 680.0;
const DIALOG_HEIGHT: f32 = 460.0;
/// The width of the column of categories.
const CATEGORY_COLUMN_WIDTH: f32 = 160.0;
/// Element id of the index list.
const LIST_ID: &str = "indices-selector-list";
/// Element id of the column of categories.
const CATEGORY_TREE_ID: &str = "indices-selector-categories";
/// Element id of the field the list is searched with.
const FILTER_INPUT_ID: &str = "indices-selector-filter";
/// The place the field over the list is drawn, which is what a test measures it by.
const FILTER_FIELD_SELECTOR: &str = "TEST_INDICES_SELECTOR_FILTER";
/// The place what the selection is worth is drawn.
const COUNT_LABEL_SELECTOR: &str = "TEST_INDICES_SELECTOR_COUNT";

/// Runs after the user confirms a selection.
type ConfirmHandler =
    Rc<dyn Fn(Vec<super::model::SelectedFinancialIndex>, &mut Window, &mut App) + 'static>;

/// What the dialog has of the catalogue.
enum ManifestState {
    /// The catalogue is being read.
    Loading,
    /// The catalogue, as the service published it.
    Ready(FinancialIndexCatalog),
    /// Why it could not be listed, already localized.
    Failed(SharedString),
}

/// One open selector.
pub(super) struct FinancialIndicesSelector {
    binding: WidgetBinding<FinancialIndicesProtocol>,
    /// The topics the user has chosen so far, which is what the rows are checked
    /// against.
    selected: HashSet<FinancialIndexTopic>,
    manifest: ManifestState,
    /// The category the list is showing.
    ///
    /// The column owns which category is highlighted — it is the row the tree has
    /// selected — and what this holds is the category behind that row, which is what the
    /// list is narrowed by.
    category: Option<SmolStr>,
    /// The field the list is searched with.
    filter: Entity<InputState>,
    /// The search the list is drawn against: what the field last reported, trimmed.
    ///
    /// The input element is rebuilt every frame and its state is read for what it draws, so
    /// what was typed is followed through the events the field emits rather than read back
    /// off it. Choosing another category empties this without a field to empty with, and the
    /// field itself is put right by the frame that follows — which is `Render`'s business.
    typed_filter: SmolStr,
    /// The indices of that category a search leaves, resolved when the catalogue, the
    /// category or the search changes rather than on every frame.
    visible: Arc<[FinancialIndexDescriptor]>,
    /// The number of indices the account may follow, as the read of the catalogue carried
    /// it.
    ///
    /// It is what the selection is drawn against, and `None` is a session that has not
    /// published a limit: a number nobody gave is not one to draw beside the count.
    limit: Option<u32>,
    /// What the last attempt to change the selection refused, if it did.
    selection_error: Option<FinancialIndicesDataError>,
    /// The column of categories, which the tree draws and keeps the selection of.
    categories: Entity<TreeState>,
    /// The language the rows of the column were labelled in.
    ///
    /// A row holds its label, so rows of a language the window has left are rows that
    /// have to be built again.
    language: LanguageCode,
    /// The indices of the category on show, as rows a reader can choose.
    list: Entity<StaticSizedList<FinancialIndexDescriptor>>,
    /// The indices the list was last told to draw, which is how a frame knows whether the
    /// category on show is one the list has not been given.
    shown: Option<Arc<[FinancialIndexDescriptor]>>,
    on_confirm: ConfirmHandler,
    /// Held because the subscription is what follows the column: dropping it would leave
    /// a row that is clicked and never read.
    _category_selection: Subscription,
}

/// The selector closes itself: both of its ways out — cancel, and a confirmation that has
/// run — are a [`DismissEvent`] the dialog it was opened in subscribes to, so the dialog
/// leaves without the selector having to be the one on top of the layer or to hold a
/// window to say so.
impl EventEmitter<DismissEvent> for FinancialIndicesSelector {}

impl FinancialIndicesSelector {
    /// Presents the selector over the window, and calls `on_confirm` with the
    /// selection the user confirmed.
    ///
    /// The catalogue belongs to the account the session is for, and reading it is the
    /// service's business rather than the pane's, so the dialog reads through the same
    /// connector the pane does.
    pub(super) fn show(
        current: &[super::model::SelectedFinancialIndex],
        on_confirm: impl Fn(Vec<super::model::SelectedFinancialIndex>, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) {
        let source = cx.global::<KytrixServiceConnector>().clone();
        Self::show_with_service(source, current, Rc::new(on_confirm), window, cx);
    }

    /// Presents the selector over a connector of the caller's choosing, which is the
    /// seam a test drives the catalogue through.
    fn show_with_service(
        source: KytrixServiceConnector,
        current: &[super::model::SelectedFinancialIndex],
        on_confirm: ConfirmHandler,
        window: &mut Window,
        cx: &mut App,
    ) {
        // The field is built here rather than inside the selector because a field is built
        // against a window, and a selector is a view that is handed one rather than one that
        // holds one.
        let filter = cx.new(|cx| {
            InputStateBuilder::new(FILTER_INPUT_ID)
                .placeholder(t!(
                    cx,
                    "widget.financial_indices.selector.filter_placeholder"
                ))
                .build(window, cx)
        });
        let selector = cx.new(|cx| {
            Self::new(
                FinancialIndicesService::new(source),
                current,
                filter,
                on_confirm,
                cx,
            )
        });
        // The read the dialog is opened to show starts before the dialog itself: the push is
        // deferred to the end of the effect cycle, so either way the first frame the panel draws
        // is a frame of a selector that has already asked.
        selector.update(cx, |selector, cx| selector.reload(cx));
        window.dialog(cx, selector);
    }

    fn new<S>(
        service: S,
        current: &[super::model::SelectedFinancialIndex],
        filter: Entity<InputState>,
        on_confirm: ConfirmHandler,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: WidgetService<FinancialIndicesProtocol>,
    {
        let mut selected = HashSet::with_capacity(current.len());
        selected.extend(current.iter().map(|index| index.topic.clone()));
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            service,
            cx,
            Self::ignore_stream,
            Self::ignore_stream,
        );

        // What is typed is followed through the events the field emits, because the element
        // is rebuilt every frame and what it holds is not what this view reads back. What a
        // search leaves is the list's rows, so a keystroke is a frame of its own.
        cx.subscribe(&filter, |selector, _, event: &InputStateEvent, cx| {
            let InputStateEvent::Change(value) = event else {
                return;
            };
            let typed = SmolStr::new(value.trim());
            if selector.typed_filter == typed {
                return;
            }
            selector.typed_filter = typed;
            selector.resolve_visible();
            cx.notify();
        })
        .detach();

        // The column holds the catalogue's categories, which arrive with the read this
        // dialog is about to make: this is the tree they are given to.
        let categories = cx.new(|cx| TreeState::new(cx));
        // The tree moves its own selection — a row is selected by clicking it, and the
        // arrow keys move it while the tree has focus — so this follows the row the tree
        // reports rather than the row a click happened to land on.
        let category_selection = cx.observe(&categories, |selector, tree, cx| {
            let selected = tree.read(cx).selected_item().map(|item| item.id.clone());
            selector.follow_category_row(selected.as_ref(), cx);
        });

        // The list is built once and told what to draw, so the row it draws is given the
        // selector rather than the state of the frame it is drawn in: what a row is checked
        // against is the selection the selector holds now.
        let selector = cx.entity().downgrade();
        let list = cx.new(|_| {
            StaticSizedList::new(
                ElementId::Name(SharedString::new_static(LIST_ID)),
                move |_index, descriptor: &FinancialIndexDescriptor, _window, cx| {
                    index_row(descriptor, &selector, cx)
                },
            )
        });

        Self {
            binding,
            selected,
            manifest: ManifestState::Loading,
            category: None,
            filter,
            typed_filter: SmolStr::default(),
            visible: Arc::from([]),
            limit: None,
            selection_error: None,
            categories,
            language: *cx.global::<LanguageCode>(),
            list,
            shown: None,
            on_confirm,
            _category_selection: category_selection,
        }
    }

    /// Reads the catalogue again.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.selection_error = None;
        self.category = None;
        self.typed_filter = SmolStr::default();
        self.visible = Arc::from([]);
        self.manifest = ManifestState::Loading;
        // The column goes with the catalogue it was built from: a category of the read
        // that failed is not a category of the read that is starting.
        self.show_category_rows(Vec::new(), cx);
        self.binding.call(
            FinancialIndicesRequest::FetchManifest,
            cx,
            Self::apply_manifest,
        );
        cx.notify();
    }

    fn apply_manifest(
        &mut self,
        result: WidgetRpcResult<FinancialIndicesProtocol>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(FinancialIndicesResponse::Manifest { catalog, limit }) => {
                // The column is the catalogue's own categories, and the list opens on the
                // first of them: a selector that opened on none would be a dialog with a
                // blank half.
                let rows = category_rows(&catalog, cx);
                self.category = catalog
                    .categories
                    .first()
                    .map(|category| category.code.clone());
                self.limit = limit;
                self.manifest = ManifestState::Ready(catalog);
                self.resolve_visible();
                self.show_category_rows(rows, cx);
            }
            // The protocol types the answer, so this is a service that answered the
            // wrong question rather than a shape a reader could describe.
            Ok(_) => self.fail(t!(cx, "widget.financial_indices.selector.load_failed"), cx),
            Err(error) => self.fail(error.localized_message(cx), cx),
        }
        cx.notify();
    }

    /// Shows why the catalogue could not be listed, and takes the column with it: a
    /// category the service did not list is not a row to choose from.
    fn fail(&mut self, detail: SharedString, cx: &mut Context<Self>) {
        self.visible = Arc::from([]);
        self.manifest = ManifestState::Failed(detail);
        self.show_category_rows(Vec::new(), cx);
    }

    /// The selector has no stream of its own: it reads a catalogue and nothing else.
    fn ignore_stream(
        &mut self,
        _: FinancialIndicesScope,
        _: Arc<FinancialIndicesStreamMessage>,
        _: &mut Context<Self>,
    ) -> bool {
        false
    }

    /// Follows the category row the tree has selected.
    ///
    /// Every row of the column is a category, so what the tree reports is the category to
    /// narrow the list by: no row of it stands for a fold rather than a choice.
    fn follow_category_row(&mut self, selected: Option<&SharedString>, cx: &mut Context<Self>) {
        if let Some(category) = selected {
            self.select_category(SmolStr::new(category.as_str()), cx);
        }
    }

    /// Narrows the list to one category, and takes the search with it.
    ///
    /// The category is what the list is drawn from rather than a state of its own: the
    /// column reports it through the row the tree has highlighted, and this is what
    /// resolves the indices behind it. The field is emptied here rather than left standing
    /// over the list that replaces it, because a search belongs to the list it was typed
    /// over.
    fn select_category(&mut self, category: SmolStr, cx: &mut Context<Self>) {
        if self.category.as_ref() == Some(&category) {
            return;
        }
        self.category = Some(category);
        self.typed_filter = SmolStr::default();
        self.resolve_visible();
        cx.notify();
    }

    /// Resolves the indices of the category on show that the search leaves.
    ///
    /// The field is part of the resolution rather than a state the list is drawn against:
    /// the list is told what it draws once, here, and the reader is put back at the top of
    /// it by that telling — which is what another category or another search leaves them
    /// owed.
    fn resolve_visible(&mut self) {
        self.visible = match &self.manifest {
            ManifestState::Ready(catalog) => catalog.descriptors_for(
                self.category.as_ref(),
                (!self.typed_filter.is_empty()).then_some(self.typed_filter.as_str()),
            ),
            ManifestState::Loading | ManifestState::Failed(_) => Arc::from([]),
        };
    }

    fn toggle(&mut self, topic: &FinancialIndexTopic, cx: &mut Context<Self>) {
        let checked = !self.selected.contains(topic);
        match update_selection(&mut self.selected, topic, checked) {
            Ok(true) => {
                self.selection_error = None;
                cx.notify();
            }
            Ok(false) => {}
            Err(error) => {
                self.selection_error = Some(error);
                cx.notify();
            }
        }
    }

    /// The selection a confirmation would carry, in the order the catalogue lists it.
    ///
    /// The order is the catalogue's rather than the order the user clicked in: a pane's
    /// grid is read as a list of markets, and the catalogue is the only order that
    /// means anything to a reader of it.
    fn selection(&self) -> Option<Vec<super::model::SelectedFinancialIndex>> {
        let ManifestState::Ready(catalog) = &self.manifest else {
            return None;
        };
        let mut indices = Vec::with_capacity(self.selected.len().min(catalog.indices.len()));
        indices.extend(
            catalog
                .indices
                .iter()
                .filter(|descriptor| self.selected.contains(&descriptor.index.topic))
                .map(|descriptor| descriptor.index.clone()),
        );
        Some(indices)
    }

    /// The column of categories, which is the tree's: which category the list is narrowed
    /// to is the row the tree has selected, and what this module holds is the category
    /// behind that row.
    fn category_column(&self) -> AnyElement {
        div()
            .w(px(CATEGORY_COLUMN_WIDTH))
            .h_full()
            .flex_none()
            .min_h_0()
            .child(Tree::new(CATEGORY_TREE_ID.into(), self.categories.clone()))
            .into_any_element()
    }

    /// Gives the column the categories to draw, pointing its highlight at the category on
    /// show.
    ///
    /// The tree is told its rows rather than asked for them, so a highlight left over from
    /// a catalogue that is no longer listed is replaced here along with the rows.
    fn show_category_rows(&mut self, rows: Vec<TreeItem>, cx: &mut Context<Self>) {
        let selected = self.category.as_deref().map(SharedString::new);
        self.categories.update(cx, |tree, cx| {
            tree.set_items(rows, cx);
            let index = selected.and_then(|selected| tree.index_of(&selected));
            tree.set_selected_index(index, cx);
        });
    }

    /// Rebuilds the column in the language the window is showing now.
    ///
    /// A row holds the label it was built with, so the column is the one part of the
    /// dialog that does not follow a language by being drawn again: it is built again
    /// here, with the category on show highlighted, because the tree is given its rows
    /// afresh.
    fn adopt_language(&mut self, cx: &mut Context<Self>) {
        let language = *cx.global::<LanguageCode>();
        if language == self.language {
            return;
        }
        self.language = language;
        let ManifestState::Ready(catalog) = &self.manifest else {
            return;
        };

        let rows = category_rows(catalog, cx);
        self.show_category_rows(rows, cx);
    }

    fn manifest_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match &self.manifest {
            ManifestState::Loading => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .size_6()
                        .child(Spinner::new("indices-selector-loading")),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().roles.fg.default.hsla)
                        .child(t!(cx, "widget.financial_indices.selector.loading")),
                )
                .into_any_element(),
            ManifestState::Failed(detail) => self.load_failure(detail.clone(), cx),
            // A search that matched nothing is not a category with nothing in it: the
            // category may well list indices, and what a reader is owed is the search they
            // typed rather than the catalogue they did not open.
            ManifestState::Ready(_) if self.visible.is_empty() && !self.typed_filter.is_empty() => {
                centered_message(
                    t!(cx, "common.no_matches"),
                    cx.theme().roles.fg.muted.hsla,
                    "TEST_INDICES_SELECTOR_NO_MATCHES",
                )
            }
            ManifestState::Ready(_) if self.visible.is_empty() => centered_message(
                t!(cx, "widget.financial_indices.selector.empty"),
                cx.theme().roles.fg.muted.hsla,
                "TEST_INDICES_SELECTOR_EMPTY",
            ),
            ManifestState::Ready(_) => self.index_list(cx),
        }
    }

    /// The catalogue could not be listed: what was being read, why, and the way to
    /// read it again.
    fn load_failure(&self, detail: SharedString, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();

        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .px_4()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.roles.status.danger.base.hsla)
                    .child(t!(cx, "widget.financial_indices.selector.load_failed")),
            )
            .child(
                div()
                    .max_w(px(420.0))
                    .truncate()
                    .text_xs()
                    .text_color(theme.roles.fg.default.hsla)
                    .child(detail),
            )
            .child(
                Button::new("indices-selector-retry")
                    .outline()
                    .small()
                    .on_click(cx.listener(|selector, _, _, cx| selector.reload(cx)))
                    .child(t!(cx, "common.retry")),
            )
            .into_any_element()
    }

    /// The indices of the category on show, each a checkbox and its two names.
    ///
    /// The whole row is the control: a row that could only be chosen by hitting its
    /// checkbox would be a list that is hard to use with a pointer and impossible with
    /// a keyboard, and the checkbox is exactly the state that row has.
    fn index_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.show_indices(cx);

        div()
            .size_full()
            .min_h_0()
            .child(self.list.clone())
            .into_any_element()
    }

    /// Shows the indices of the category on show, if they are not the ones the list holds.
    ///
    /// A list draws what it was given and remembers where the reader is, so it is told about
    /// another category or another search rather than rebuilt every frame: being told is what
    /// puts the reader back at the top, which is where either of those leaves them.
    fn show_indices(&mut self, cx: &mut Context<Self>) {
        if self
            .shown
            .as_ref()
            .is_some_and(|shown| Arc::ptr_eq(shown, &self.visible))
        {
            return;
        }

        self.shown = Some(self.visible.clone());
        self.list.update(cx, |list, cx| {
            list.replace(self.visible.iter().cloned());
            cx.notify();
        });
    }

    /// The field the list is searched with.
    ///
    /// It is a sibling of the list's panel rather than part of it, and it stands over that
    /// panel the way the panel stands beside the column of categories: the field is what
    /// decides which rows the list is given, not a control the list draws.
    fn filter_field(&self) -> AnyElement {
        div()
            .debug_selector(|| FILTER_FIELD_SELECTOR.into())
            .flex_none()
            .min_w_0()
            .child(
                Input::new(&self.filter)
                    .medium()
                    .clearable(true)
                    .prefix_icon(IconKind::MagnifyingGlass),
            )
            .into_any_element()
    }

    /// What the selection is worth against the account's limit, drawn at the end of the row
    /// the field is in.
    ///
    /// The count is the selection this dialog holds rather than the pane's configuration:
    /// a reader who is choosing is owed what they have chosen so far, and a confirmation is
    /// what makes that the pane's. Nothing is drawn for a session that published no limit —
    /// a count beside a question mark is worse than no count — so the whole label goes with
    /// the limit it is measured against.
    fn count_label(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let limit = self.limit?;
        let current = selection_count(&self.selected);

        Some(
            div()
                .debug_selector(|| COUNT_LABEL_SELECTOR.into())
                .flex_none()
                .truncate()
                .text_xs()
                .text_color(cx.theme().roles.fg.muted.hsla)
                .child(t!(
                    cx,
                    "widget.financial_indices.selector.selection_count",
                    current = current,
                    limit = limit
                ))
                .into_any_element(),
        )
    }

    /// The field the list is searched with and what the selection is worth against the
    /// account's limit, which are one row: the field takes the room the label does not.
    fn search_row(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .flex_none()
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            .child(div().min_w_0().flex_1().child(self.filter_field()))
            .children(self.count_label(cx))
            .into_any_element()
    }

    fn footer(&self, cx: &Context<Self>) -> AnyElement {
        // A selection can only be confirmed out of a catalogue the user has seen, which
        // is also what makes an empty selection mean "follow nothing" rather than "the
        // list never arrived".
        let can_confirm = matches!(self.manifest, ManifestState::Ready(_));

        div()
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .child(Separator::horizontal())
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_xs()
                            .text_color(cx.theme().roles.status.danger.base.hsla)
                            .children(
                                self.selection_error
                                    .as_ref()
                                    .map(|error| error.localized_message(cx)),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("indices-selector-cancel")
                                    .ghost()
                                    .medium()
                                    .debug_selector(|| "TEST_INDICES_SELECTOR_CANCEL".into())
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
                                    .child(t!(cx, "common.cancel")),
                            )
                            .child(
                                Button::new("indices-selector-confirm")
                                    .filled()
                                    .medium()
                                    .debug_selector(|| "TEST_INDICES_SELECTOR_CONFIRM".into())
                                    .disabled(!can_confirm)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let Some(selection) = this.selection() else {
                                            return;
                                        };
                                        let on_confirm = this.on_confirm.clone();
                                        // The pane is handed the selection before the
                                        // dialog is told to leave: the emit only reaches
                                        // the dialog at the end of the effect cycle, and
                                        // the confirmation is what the user clicked for.
                                        on_confirm(selection, window, cx);
                                        cx.emit(DismissEvent);
                                    }))
                                    .child(t!(cx, "common.confirm")),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Render for FinancialIndicesSelector {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.adopt_language(cx);
        // A category is chosen from the tree's own selection, which is reported to this view
        // without a window to empty a field with. The emptying is a frame's work rather than
        // a message's: a field still showing a search no list is drawn from is a field that
        // lies about the rows under it, and this is where that is put right.
        if self.typed_filter.is_empty() {
            let shown = self.filter.read(cx).value(cx);
            if !shown.is_empty() {
                self.filter
                    .update(cx, |filter, cx| filter.update_value("", window, cx));
            }
        }
        let theme = cx.theme();
        // Only a listed catalogue has anything to search, so the field is drawn over a
        // panel that is about to list something and not over a spinner or a failure.
        let search = matches!(self.manifest, ManifestState::Ready(_)).then(|| self.search_row(cx));

        div()
            .w(px(DIALOG_WIDTH))
            .h(px(DIALOG_HEIGHT))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .min_w_0()
                    .min_h_0()
                    .flex_1()
                    .flex()
                    .gap_2()
                    .child(self.category_column())
                    .child(Separator::vertical())
                    .child(
                        div()
                            .min_w_0()
                            .min_h_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .children(search)
                            .child(
                                // The panel belongs to the list alone: the categories beside
                                // it and the field above it are how the list is narrowed, not
                                // part of what it lists.
                                div()
                                    .min_w_0()
                                    .min_h_0()
                                    .flex_1()
                                    .overflow_hidden()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(theme.roles.line.divider.hsla)
                                    .bg(theme.roles.surface.raised.hsla)
                                    .child(self.manifest_body(cx)),
                            ),
                    ),
            )
            .child(self.footer(cx))
    }
}

/// The number of indices a selection holds.
///
/// The hard limit is what a set can hold, so what fits in one is a count: a selection the
/// selector will accept is a selection whose length is the number drawn beside it.
fn selection_count(selected: &HashSet<FinancialIndexTopic>) -> u32 {
    u32::try_from(selected.len()).unwrap_or(u32::MAX)
}

/// Applies one checked change to a selection set.
///
/// Removing is always allowed and adding is refused past the hard limit rather than
/// silently dropped: a user who cannot check another box is owed the reason.
fn update_selection(
    selected: &mut HashSet<FinancialIndexTopic>,
    topic: &FinancialIndexTopic,
    checked: bool,
) -> Result<bool, FinancialIndicesDataError> {
    if !checked {
        return Ok(selected.remove(topic));
    }
    if selected.contains(topic) {
        return Ok(false);
    }
    if selected.len() >= MAX_SELECTED_INDICES {
        return Err(FinancialIndicesDataError::SelectionCapacity {
            limit: MAX_SELECTED_INDICES,
        });
    }
    Ok(selected.insert(topic.clone()))
}

/// The categories of the catalogue, as the rows the column draws.
///
/// A row is named after the category's code, which is what the catalogue calls it and
/// what the selector reads back when the tree reports a row: a row is not a position, so
/// it keeps meaning the same category when the catalogue is listed again.
fn category_rows(catalog: &FinancialIndexCatalog, cx: &App) -> Vec<TreeItem> {
    let language = *cx.global::<LanguageCode>();

    catalog
        .categories
        .iter()
        .map(|category| {
            TreeItem::new(
                SharedString::new(category.code.as_str()),
                SharedString::new(category.name.get(language)),
            )
        })
        .collect()
}

/// The element id of one row's checkbox.
fn row_id(code: &str) -> ElementId {
    (
        ElementId::Name(SharedString::new_static("FinancialIndicesSelectorRow")),
        code,
    )
        .into()
}

/// One index of the category on show: what it is called, and whether it is followed.
///
/// The selector is read for what the row is checked against rather than handed the answer, so
/// that a row drawn by the list reads the selection the selector holds now: a list draws its
/// rows again on every frame of the dialog, and choosing a row is one of those frames.
///
/// The row is drawn ghost rather than filled: a list of indices is a list of choices, and a
/// filled row per index would draw every one of them as a control that has already been
/// pressed. What says a row is chosen is its checkbox and the row's own selected state, which
/// the ghost variant draws as a tint over the row rather than as a button that stands out of
/// the list.
fn index_row(
    descriptor: &FinancialIndexDescriptor,
    selector: &WeakEntity<FinancialIndicesSelector>,
    cx: &App,
) -> AnyElement {
    let topic = descriptor.index.topic.clone();
    let code = SmolStr::new(topic.code());
    let checked = selector
        .upgrade()
        .is_some_and(|selector| selector.read(cx).selected.contains(&topic));

    Button::new((row_id(&code), "button"))
        .ghost()
        .w_full()
        .selected(checked)
        .on_click({
            let selector = selector.clone();
            move |_, _, cx| {
                let topic = topic.clone();
                let Some(selector) = selector.upgrade() else {
                    return;
                };
                selector.update(cx, |this, cx| this.toggle(&topic, cx));
            }
        })
        .child(Toggle::new(row_id(&code)).small().checked(checked))
        .child(
            div()
                .flex_none()
                .text_sm()
                .child(SharedString::new(code.as_str())),
        )
        .child(
            div()
                .debug_selector(|| "TEST_INDICES_SELECTOR_ROW".into())
                .min_w_0()
                .flex_1()
                .truncate()
                .text_right()
                .text_xs()
                .child(SharedString::new(
                    descriptor.index.name.get(*cx.global::<LanguageCode>()),
                )),
        )
        .into_any_element()
}

/// A message in the middle of the panel.
///
/// It names itself, because the panel has two of them and what a reader is owed is the one
/// that fits: a category with nothing in it and a search that matched nothing are two
/// different answers.
fn centered_message(
    message: SharedString,
    color: gpui_kit::Hsla,
    selector: &'static str,
) -> AnyElement {
    div()
        .debug_selector(move || selector.into())
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(color)
        .child(message)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, time::Duration};

    use base_ui::{animation::ANIMATION_TICK, layer::Layer};
    use gpui_kit::{AppContext, EmptyView, Entity, Modifiers, TestAppContext, VisualTestContext};

    use super::*;
    use crate::{
        model::SelectedFinancialIndex,
        test_support::{FakeSource, selected, settle_until},
    };

    /// What a confirmation carried, as the pane would see it.
    type Confirmed = Rc<RefCell<Option<Vec<SelectedFinancialIndex>>>>;

    /// A selector opened over a fake session, with nothing signed in but the read
    /// itself.
    fn selector(
        cx: &mut TestAppContext,
        source: FakeSource,
        current: &[SelectedFinancialIndex],
    ) -> (Entity<FinancialIndicesSelector>, Confirmed) {
        cx.executor().allow_parking();
        let confirmed: Confirmed = Rc::new(RefCell::new(None));
        let sink = confirmed.clone();
        let current = current.to_vec();
        cx.update(|cx| {
            // The base layer comes first: a field that is typed into reads the state its own
            // `init` installs.
            gpui_kit::base::init(cx);
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("the embedded locales should load");
            global::install(cx).expect("the test Tokio runtime should build");
        });
        // A field is built against a window, and the dialog is a view rather than one that
        // holds a window of its own, so one of the test's own stands in for the window the
        // selector would be opened from.
        let window = cx.add_empty_window();
        let selector = window.update(|window, cx| {
            let filter = cx.new(|cx| {
                InputStateBuilder::new(FILTER_INPUT_ID)
                    .placeholder(t!(
                        cx,
                        "widget.financial_indices.selector.filter_placeholder"
                    ))
                    .build(window, cx)
            });
            cx.new(|cx| {
                FinancialIndicesSelector::new(
                    FinancialIndicesService::new(source),
                    &current,
                    filter,
                    Rc::new(move |selection, _, _| {
                        *sink.borrow_mut() = Some(selection);
                    }),
                    cx,
                )
            })
        });
        selector.update(cx, |selector, cx| selector.reload(cx));
        (selector, confirmed)
    }

    /// Waits for the catalogue the selector asked for to arrive.
    fn wait_for_manifest(cx: &mut TestAppContext, selector: &Entity<FinancialIndicesSelector>) {
        settle_until(cx, "the catalogue to arrive", |cx| {
            !matches!(selector.read(cx).manifest, ManifestState::Loading)
        });
    }

    /// Confirms through the same path the button takes, short of dismissing the dialog.
    fn confirm(cx: &mut TestAppContext, selector: &Entity<FinancialIndicesSelector>) {
        let window = cx.add_empty_window();
        window.update(|window, cx| {
            let confirmation = selector.update(cx, |this, _| {
                this.selection()
                    .map(|selection| (selection, this.on_confirm.clone()))
            });
            let Some((selection, on_confirm)) = confirmation else {
                return;
            };
            on_confirm(selection, window, cx);
        });
    }

    fn visible_codes(selector: &Entity<FinancialIndicesSelector>, cx: &App) -> Vec<String> {
        selector
            .read(cx)
            .visible
            .iter()
            .map(|descriptor| descriptor.index.topic.code().to_owned())
            .collect()
    }

    /// The rows of the column as the tree draws them: what each row is named, and what it
    /// reads.
    fn column(
        selector: &Entity<FinancialIndicesSelector>,
        cx: &TestAppContext,
    ) -> Vec<(String, String)> {
        cx.read(|cx| {
            let categories = selector.read(cx).categories.clone();
            let tree = categories.read(cx);

            (0..)
                .map_while(|index| tree.entry(index))
                .map(|entry| (entry.item().id.to_string(), entry.item().label.to_string()))
                .collect()
        })
    }

    /// The row of the column the tree has highlighted.
    fn highlighted(
        selector: &Entity<FinancialIndicesSelector>,
        cx: &TestAppContext,
    ) -> Option<String> {
        cx.read(|cx| {
            selector
                .read(cx)
                .categories
                .read(cx)
                .selected_item()
                .map(|item| item.id.to_string())
        })
    }

    /// A frame is what draws the rows of the category on show, and a row is drawn checked
    /// against the selection the selector is holding while the list draws it.
    #[gpui_kit::test]
    fn a_frame_should_draw_the_rows_of_the_category_on_show(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        source.set_manifest(crate::test_support::manifest());
        let (selector, _) = selector(cx, source, &[selected("HSI")]);
        wait_for_manifest(cx, &selector);

        let (_, window) = cx.add_window_view({
            let selector = selector.clone();
            move |_, _| SelectorHarness { selector }
        });
        window.run_until_parked();
        assert!(
            window.debug_bounds("TEST_INDICES_SELECTOR_ROW").is_some(),
            "the rows of the category on show are drawn"
        );

        // An index is one of the ones being followed only once the catalogue has named it, and
        // choosing a row is a frame of its own.
        window.update(|_, cx| {
            selector.update(cx, |selector, cx| {
                selector.toggle(&crate::test_support::topic("HSI"), cx);
            });
        });
        window.update(|window, _| window.refresh());
        assert!(
            window.debug_bounds("TEST_INDICES_SELECTOR_ROW").is_some(),
            "the rows are drawn again for the selection they were changed to"
        );
    }

    /// A frame of its own for a selector, which the window layer would otherwise hold.
    struct SelectorHarness {
        selector: Entity<FinancialIndicesSelector>,
    }

    impl Render for SelectorHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.selector.clone())
        }
    }

    /// The field the list is searched with, as the selector holds it.
    fn filter_field(
        cx: &mut TestAppContext,
        selector: &Entity<FinancialIndicesSelector>,
    ) -> Entity<InputState> {
        cx.update(|cx| selector.read(cx).filter.clone())
    }

    /// The codes the list is holding, read where the frame that drew them is.
    fn listed(
        window: &mut VisualTestContext,
        selector: &Entity<FinancialIndicesSelector>,
    ) -> Vec<String> {
        window.update(|_, cx| visible_codes(selector, cx))
    }

    /// Types into the field the way a reader does, with the caret in it.
    ///
    /// The field answers with the events a reader's typing makes it emit, so what is
    /// exercised here is the whole way in rather than the state behind it.
    fn type_into_the_filter(window: &mut VisualTestContext, typed: &str) {
        let field = window
            .debug_bounds(FILTER_FIELD_SELECTOR)
            .expect("the panel should draw the field the list is searched with")
            .center();
        window.simulate_click(field, Modifiers::none());
        window.simulate_input(typed);
        window.run_until_parked();
        window.update(|window, _| window.refresh());
        window.run_until_parked();
    }

    /// The search narrows the category on show, and it is the list's rows that change: the
    /// catalogue is not read again for a search.
    #[gpui_kit::test]
    fn typing_in_the_field_should_narrow_the_list_to_what_it_matches(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        source.set_manifest(crate::test_support::manifest());
        let (selector, _) = selector(cx, source.clone(), &[]);
        wait_for_manifest(cx, &selector);

        let (_, window) = cx.add_window_view({
            let selector = selector.clone();
            move |_, _| SelectorHarness { selector }
        });
        window.run_until_parked();
        assert_eq!(
            listed(window, &selector),
            ["HSI"],
            "the list opens on the category the column highlights"
        );

        type_into_the_filter(window, "hsi");
        assert_eq!(
            listed(window, &selector),
            ["HSI"],
            "the search leaves the index the field names"
        );

        type_into_the_filter(window, "nothing here");
        assert!(
            listed(window, &selector).is_empty(),
            "a search that names no index leaves the list empty"
        );
        assert_eq!(
            source.manifest_calls(),
            1,
            "a search is answered from the catalogue the dialog already read"
        );
    }

    /// A search that matches nothing is drawn as a search that found nothing rather than as
    /// a category with nothing in it: those are two different facts.
    #[gpui_kit::test]
    fn a_search_that_matches_nothing_should_say_so(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        source.set_manifest(crate::test_support::manifest());
        let (selector, _) = selector(cx, source, &[]);
        wait_for_manifest(cx, &selector);

        let (_, window) = cx.add_window_view({
            let selector = selector.clone();
            move |_, _| SelectorHarness { selector }
        });
        window.run_until_parked();

        // Nothing is searched for, so the category on show lists what it holds.
        assert!(
            window.debug_bounds("TEST_INDICES_SELECTOR_ROW").is_some(),
            "the category lists its indices while nothing is searched for"
        );
        assert!(
            window
                .debug_bounds("TEST_INDICES_SELECTOR_NO_MATCHES")
                .is_none()
        );

        type_into_the_filter(window, "nothing here");

        assert!(
            window
                .debug_bounds("TEST_INDICES_SELECTOR_NO_MATCHES")
                .is_some(),
            "a search that matched nothing is drawn as one"
        );
        assert!(
            window.debug_bounds("TEST_INDICES_SELECTOR_ROW").is_none(),
            "and the rows it left out are not drawn"
        );
    }

    /// A search belongs to the list it was typed over, so another category takes it away:
    /// the field is emptied and the new category is drawn whole.
    ///    /// The emptying is a frame's work rather than a message's — the column reports the row it
    /// moved to without a window to empty a field with — so it is the frame below that puts
    /// the field right, and this is what says it does.
    #[gpui_kit::test]
    fn choosing_another_category_should_clear_the_search(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        source.set_manifest(crate::test_support::manifest());
        let (selector, _) = selector(cx, source, &[]);
        wait_for_manifest(cx, &selector);

        let (_, window) = cx.add_window_view({
            let selector = selector.clone();
            move |_, _| SelectorHarness { selector }
        });
        window.run_until_parked();
        type_into_the_filter(window, "hsi");
        assert_eq!(
            listed(window, &selector),
            ["HSI"],
            "the search is the one the list was drawn against"
        );

        // Moving the highlight is what a click on a row of the column does.
        let categories = window.update(|_, cx| selector.read(cx).categories.clone());
        window.update(|window, cx| {
            categories.update(cx, |tree, cx| tree.set_selected_index(Some(1), cx));
            window.refresh();
        });
        window.run_until_parked();
        window.update(|window, _| window.refresh());

        assert_eq!(
            listed(window, &selector),
            ["SPX"],
            "the list is the category the column moved to, unsearched"
        );
        let field = filter_field(&mut window.cx, &selector);
        assert!(
            window.update(|_, cx| field.read(cx).value(cx).is_empty()),
            "and the field the search was typed in does not still show it"
        );
    }

    /// What the selection is worth is drawn at the end of the row the field is in, and it is
    /// the selection this dialog holds: a check made but not yet confirmed counts, because a
    /// reader who is choosing is owed what they have chosen so far.
    #[gpui_kit::test]
    fn the_count_label_should_draw_the_selection_against_the_account_limit(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::with_session(3);
        source.set_manifest(crate::test_support::manifest());
        let (selector, _) = selector(cx, source, &[selected("HSI")]);
        wait_for_manifest(cx, &selector);

        let (_, window) = cx.add_window_view({
            let selector = selector.clone();
            move |_, _| SelectorHarness { selector }
        });
        window.run_until_parked();

        let count = window
            .debug_bounds(COUNT_LABEL_SELECTOR)
            .expect("a limit the session published is one the selection is drawn against");
        let field = window
            .debug_bounds(FILTER_FIELD_SELECTOR)
            .expect("the panel draws the field the list is searched with");
        assert!(
            count.left() >= field.right(),
            "the count is drawn after the field rather than over it"
        );

        // Choosing a row of the list is what moves the count, and it moves it before the
        // selection is confirmed into anything.
        window.update(|_, cx| {
            selector.update(cx, |selector, cx| {
                selector.toggle(&crate::test_support::topic("SPX"), cx);
            });
        });
        window.update(|window, _| window.refresh());
        assert!(
            window.debug_bounds(COUNT_LABEL_SELECTOR).is_some(),
            "a selection that grew is still drawn against the limit"
        );
    }

    /// A session that published no limit is not one to draw a count beside: the number the
    /// selection would be measured against does not exist, so the label does not either —
    /// while the field it stands beside is drawn as it always was.
    #[gpui_kit::test]
    fn the_count_label_should_be_left_out_when_no_limit_was_published(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        source.set_manifest(crate::test_support::manifest());
        let (selector, _) = selector(cx, source, &[selected("HSI")]);
        wait_for_manifest(cx, &selector);

        let (_, window) = cx.add_window_view({
            let selector = selector.clone();
            move |_, _| SelectorHarness { selector }
        });
        window.run_until_parked();

        assert!(
            window.debug_bounds(FILTER_FIELD_SELECTOR).is_some(),
            "the field is drawn whether or not there is a limit to draw"
        );
        assert!(
            window.debug_bounds(COUNT_LABEL_SELECTOR).is_none(),
            "a count measured against nothing is not drawn"
        );
    }

    /// What the label says: the selection this dialog holds, and the limit the service
    /// named.
    ///
    /// The count is asserted through the message rather than through the set, because what a
    /// reader is owed is the number on screen: a label drawn against the pane's committed
    /// configuration would read the same until the first box was ticked, and this is the
    /// assertion that tells the two apart.
    #[gpui_kit::test]
    fn the_count_label_should_read_the_selection_it_holds(cx: &mut TestAppContext) {
        let source = FakeSource::with_session(3);
        source.set_manifest(crate::test_support::manifest());
        let (selector, _) = selector(cx, source, &[selected("HSI")]);
        wait_for_manifest(cx, &selector);

        let rendered = |selector: &Entity<FinancialIndicesSelector>, cx: &mut TestAppContext| {
            cx.update(|cx| {
                t!(
                    cx,
                    "widget.financial_indices.selector.selection_count",
                    current = selection_count(&selector.read(cx).selected),
                    limit = selector.read(cx).limit.unwrap_or_default()
                )
                .to_string()
            })
        };

        assert_eq!(rendered(&selector, cx), "Added/Limit: 1/3");

        cx.update(|cx| {
            selector.update(cx, |selector, cx| {
                selector.toggle(&crate::test_support::topic("SPX"), cx);
            });
        });
        assert_eq!(
            rendered(&selector, cx),
            "Added/Limit: 2/3",
            "a box ticked but not confirmed is a box the count is drawn for"
        );

        cx.update(|cx| {
            selector.update(cx, |selector, cx| {
                selector.toggle(&crate::test_support::topic("HSI"), cx);
            });
        });
        assert_eq!(
            rendered(&selector, cx),
            "Added/Limit: 1/3",
            "and one unticked is one the count gives back"
        );
    }

    #[test]
    fn adding_past_the_hard_limit_should_report_it_rather_than_drop_the_click() {
        let mut selected = HashSet::with_capacity(MAX_SELECTED_INDICES);
        for index in 0..MAX_SELECTED_INDICES {
            let topic = FinancialIndexTopic::from_code(&format!("INDEX_{index}"))
                .expect("a test index code should be valid");
            assert!(
                update_selection(&mut selected, &topic, true)
                    .expect("the limit should be accepted")
            );
        }
        let one_too_many = FinancialIndexTopic::from_code("ONE_TOO_MANY")
            .expect("a test index code should be valid");

        assert_eq!(
            update_selection(&mut selected, &one_too_many, true),
            Err(FinancialIndicesDataError::SelectionCapacity {
                limit: MAX_SELECTED_INDICES,
            })
        );
        assert_eq!(selected.len(), MAX_SELECTED_INDICES);
    }

    #[test]
    fn removing_should_stay_possible_at_the_hard_limit() {
        let mut selected = (0..MAX_SELECTED_INDICES)
            .map(|index| {
                FinancialIndexTopic::from_code(&format!("INDEX_{index}"))
                    .expect("a test index code should be valid")
            })
            .collect::<HashSet<_>>();
        let first =
            FinancialIndexTopic::from_code("INDEX_0").expect("a test index code should be valid");

        assert!(
            update_selection(&mut selected, &first, false)
                .expect("a chosen index should stay removable")
        );
        assert_eq!(selected.len(), MAX_SELECTED_INDICES - 1);
    }

    #[gpui_kit::test]
    fn a_selector_should_open_on_the_first_category(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        source.set_manifest(crate::test_support::manifest());
        let (selector, _) = selector(cx, source.clone(), &[]);
        wait_for_manifest(cx, &selector);

        cx.update(|cx| {
            assert_eq!(
                visible_codes(&selector, cx),
                ["HSI"],
                "the list opens on the catalogue's first category"
            );
            assert!(matches!(
                selector.read(cx).manifest,
                ManifestState::Ready(_)
            ));
            assert_eq!(source.manifest_calls(), 1);
        });
        assert_eq!(
            highlighted(&selector, cx).as_deref(),
            Some("asia"),
            "the column highlights the category the list opened on"
        );
    }

    /// The column is the catalogue's categories, one row each, in the order it lists them.
    #[gpui_kit::test]
    fn the_column_should_hold_a_row_per_category(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        source.set_manifest(crate::test_support::manifest());
        let (selector, _) = selector(cx, source, &[]);
        wait_for_manifest(cx, &selector);

        assert_eq!(
            column(&selector, cx),
            vec![
                ("asia".to_owned(), "Asia".to_owned()),
                ("america".to_owned(), "America".to_owned()),
            ]
        );
    }

    /// The tree moves its own highlight, and the row it reports is the category the list is
    /// narrowed by: choosing another row is what replaces the list.
    #[gpui_kit::test]
    fn choosing_another_category_row_should_replace_the_list(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        source.set_manifest(crate::test_support::manifest());
        let (selector, _) = selector(cx, source, &[]);
        wait_for_manifest(cx, &selector);

        // Moving the highlight is what a click on a row does.
        let categories = cx.update(|cx| selector.read(cx).categories.clone());
        cx.update_entity(&categories, |tree, cx| tree.set_selected_index(Some(1), cx));

        cx.update(|cx| {
            assert_eq!(visible_codes(&selector, cx), ["SPX"]);
            assert_eq!(
                selector.read(cx).category.as_ref().map(SmolStr::as_str),
                Some("america")
            );
        });
    }

    /// A row holds the label it was built with, so a column built in one language is built
    /// again when the window moves to another.
    #[gpui_kit::test]
    fn the_column_should_be_labelled_in_the_language_the_window_shows(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        source.set_manifest(crate::test_support::manifest());
        let (selector, _) = selector(cx, source, &[]);
        wait_for_manifest(cx, &selector);

        cx.update(|cx| {
            cx.set_global(LanguageCode::ZhCn);
            selector.update(cx, |selector, cx| selector.adopt_language(cx));
        });

        assert_eq!(
            column(&selector, cx),
            vec![
                ("asia".to_owned(), "亚洲".to_owned()),
                ("america".to_owned(), "美洲".to_owned()),
            ],
            "the column reads the names the catalogue carries for the language it is built in"
        );
        assert_eq!(
            highlighted(&selector, cx).as_deref(),
            Some("asia"),
            "a column built again stays on the category the list is showing"
        );
    }

    #[gpui_kit::test]
    fn confirming_should_carry_the_selection_in_catalogue_order(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        source.set_manifest(crate::test_support::manifest());
        let (selector, confirmed) = selector(cx, source, &[]);
        wait_for_manifest(cx, &selector);

        cx.update(|cx| {
            // Chosen the other way round from the order the catalogue lists them in.
            selector.update(cx, |selector, cx| {
                selector.toggle(&crate::test_support::topic("SPX"), cx);
                selector.toggle(&crate::test_support::topic("HSI"), cx);
            });
        });
        confirm(cx, &selector);

        assert_eq!(
            confirmed.borrow().as_ref().map(|selection| selection
                .iter()
                .map(|index| index.topic.code().to_owned())
                .collect::<Vec<_>>()),
            Some(vec!["HSI".to_owned(), "SPX".to_owned()])
        );
    }

    #[gpui_kit::test]
    fn a_selector_should_start_from_the_selection_it_was_opened_on(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        source.set_manifest(crate::test_support::manifest());
        let (selector, confirmed) = selector(cx, source, &[selected("HSI")]);
        wait_for_manifest(cx, &selector);

        cx.update(|cx| {
            assert!(
                selector
                    .read(cx)
                    .selected
                    .contains(&crate::test_support::topic("HSI"))
            );
        });
        // Confirming without touching anything keeps what the pane already follows.
        confirm(cx, &selector);

        assert_eq!(
            confirmed.borrow().as_ref().map(|selection| selection.len()),
            Some(1)
        );
    }

    #[gpui_kit::test]
    fn a_catalogue_that_could_not_be_read_should_offer_another_read(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let mut broken = crate::test_support::manifest();
        broken.indices[0].name = None;
        source.set_manifest(broken);
        let (selector, _) = selector(cx, source.clone(), &[]);
        wait_for_manifest(cx, &selector);

        cx.update(|cx| {
            assert!(matches!(
                selector.read(cx).manifest,
                ManifestState::Failed(_)
            ));
            assert!(selector.read(cx).visible.is_empty());
        });
        assert_eq!(
            column(&selector, cx),
            Vec::new(),
            "a category the service did not list is not a row to choose from"
        );

        source.set_manifest(crate::test_support::manifest());
        cx.update(|cx| selector.update(cx, |selector, cx| selector.reload(cx)));
        wait_for_manifest(cx, &selector);

        cx.update(|cx| {
            assert!(matches!(
                selector.read(cx).manifest,
                ManifestState::Ready(_)
            ));
            assert_eq!(source.manifest_calls(), 2);
        });
        assert_eq!(
            column(&selector, cx).len(),
            2,
            "the column is the catalogue the read that followed listed"
        );
    }

    #[gpui_kit::test]
    fn a_catalogue_with_no_categories_should_list_nothing(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        source.set_manifest(kytrix_proto::types::v1::FinancialIndices {
            categories: Vec::new(),
            indices: Vec::new(),
        });
        let (selector, _) = selector(cx, source, &[]);
        wait_for_manifest(cx, &selector);

        cx.update(|cx| {
            assert!(matches!(
                selector.read(cx).manifest,
                ManifestState::Ready(_)
            ));
            assert!(selector.read(cx).visible.is_empty());
            assert!(selector.read(cx).category.is_none());
        });
        assert_eq!(column(&selector, cx), Vec::new());
        assert_eq!(highlighted(&selector, cx), None);
    }

    /// A window whose root is a layer, the way the pane's window mounts one, so the
    /// selector has a window layer to be opened on and to leave.
    fn dialog_window(cx: &mut TestAppContext) -> &mut VisualTestContext {
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| EmptyView)));
        window.run_until_parked();

        window
    }

    /// Opens the selector in the window's dialog layer, the way the pane's choose control
    /// does.
    fn open_selector(window: &mut VisualTestContext, selector: &Entity<FinancialIndicesSelector>) {
        window.update(|window, cx| window.dialog(cx, selector.clone()));
    }

    /// Lets the dialog's own transition finish and repaints, so a dialog just opened is
    /// past its first frames rather than still appearing, and one just closed is gone
    /// rather than still fading out.
    fn settle(window: &mut VisualTestContext) {
        window
            .executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        window.update(|window, _| window.refresh());
        window.run_until_parked();
    }

    fn click(window: &mut VisualTestContext, selector: &'static str) {
        let target = window
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the window should draw {selector}"))
            .center();
        window.simulate_click(target, Modifiers::none());
        window.run_until_parked();
    }

    /// The selector closes itself rather than the window closing the dialog on top: its
    /// cancel button emits the event the dialog subscribes to, so the panel — and the rows
    /// drawn in it — leave once the exit transition has run.
    #[gpui_kit::test]
    fn the_selector_should_close_itself_when_its_cancel_button_is_clicked(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        source.set_manifest(crate::test_support::manifest());
        let (selector, _) = selector(cx, source, &[]);
        wait_for_manifest(cx, &selector);

        let window = dialog_window(cx);
        open_selector(window, &selector);
        settle(window);

        assert!(
            window.debug_bounds("TEST_INDICES_SELECTOR_ROW").is_some(),
            "the layer draws the selector it was opened with"
        );

        click(window, "TEST_INDICES_SELECTOR_CANCEL");
        settle(window);

        assert!(
            window.debug_bounds("TEST_INDICES_SELECTOR_ROW").is_none(),
            "the selector that emitted has left the layer once its exit transition was over"
        );
    }

    /// A confirmation carries the selection to whoever asked for it and only then closes
    /// the selector: the emit is delivered at the end of the effect cycle, so the pane is
    /// handed the indices the user confirmed before the dialog goes.
    #[gpui_kit::test]
    fn confirming_should_carry_the_selection_and_then_close_the_selector(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        source.set_manifest(crate::test_support::manifest());
        let (selector, confirmed) = selector(cx, source, &[]);
        wait_for_manifest(cx, &selector);
        cx.update(|cx| {
            selector.update(cx, |selector, cx| {
                selector.toggle(&crate::test_support::topic("SPX"), cx);
            });
        });

        let window = dialog_window(cx);
        open_selector(window, &selector);
        settle(window);

        click(window, "TEST_INDICES_SELECTOR_CONFIRM");
        settle(window);

        assert_eq!(
            confirmed.borrow().as_ref().map(|selection| selection
                .iter()
                .map(|index| index.topic.code().to_owned())
                .collect::<Vec<_>>()),
            Some(vec!["SPX".to_owned()]),
            "the pane is handed the selection the user confirmed"
        );
        assert!(
            window.debug_bounds("TEST_INDICES_SELECTOR_ROW").is_none(),
            "and the selector that confirmed has left the layer"
        );
    }
}

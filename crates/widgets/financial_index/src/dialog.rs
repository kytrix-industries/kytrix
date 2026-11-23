use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::Arc,
};

use ahash::{HashSet, HashSetExt};
use base_ui::{
    kit::{
        button::Button,
        dialog::{Dialog, DialogBuilder, DialogError},
        empty_state::EmptyState,
        input::{self, InputNode, InputSpec, InputState},
        list::{List, ListOptions},
        overlay::OverlayError,
        separator::Separator,
        size::{UnifiedSizable, UnifiedSize},
        spinner::Spinner,
        tree::{Tree, TreeItem, TreeOptions},
    },
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::engine::{FinancialIndexTopic, KytrixServiceError};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    event::{Event, InputEvent},
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::{primitive::Border, style::StylePatch},
    text::{TextAlign, TextOverflow, TextStyle, TextWrap},
    window::WindowId,
};
use widget::{
    binding::{Inbound, WidgetBinding},
    cx::WidgetCx,
    error::TranslatedError,
    protocol::RequestId,
};

use super::{
    model::{
        FinancialIndexCatalog, FinancialIndexDataError, FinancialIndexDescriptor,
        MAX_SELECTED_INDICES, SelectedFinancialIndex,
    },
    service::{
        FinancialIndexProtocol, FinancialIndexRequest, FinancialIndexResponse,
        FinancialIndexService, FinancialIndexServiceError, FinancialIndexSource,
    },
};

const DIALOG_WIDTH: f32 = 680.0;
const DIALOG_HEIGHT: f32 = 460.0;
const CATEGORY_COLUMN_WIDTH: f32 = 160.0;
const ROW_HEIGHT: f32 = 28.0;
const CORNER_RADIUS: f32 = 4.0;
const MESSAGE_WIDTH: f32 = 420.0;
const CHECK_MARK: &str = "✓";

type OnConfirm = Rc<dyn Fn(&mut App, Vec<SelectedFinancialIndex>)>;

enum ManifestState {
    Loading,
    Ready(FinancialIndexCatalog),
    Failed(Arc<dyn TranslatedError>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectorMessage {
    Loading,
    Failed,
    NoMatches,
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SelectionCount {
    current: u32,
    limit: u32,
}

#[derive(Clone)]
struct SelectorKeys {
    dialog: SmolStr,
    tree: SmolStr,
    list: SmolStr,
    filter: SmolStr,
    retry: SmolStr,
    spinner: SmolStr,
    empty: SmolStr,
}

struct SelectorComponents {
    dialog: Option<Dialog>,
    list: Option<List>,
    tree: Option<Tree>,
    input: Option<InputNode>,
    spinner: Option<Spinner>,
    retry: Option<Button>,
}

pub(super) struct IndexSelector {
    inner: Rc<RefCell<SelectorState>>,
}

struct SelectorState {
    window: WindowId,
    scope: SignalScope,
    theme: ThemeHandle,
    translate: Translate,
    language: Signal<LanguageCode>,
    keys: SelectorKeys,
    binding: Option<WidgetBinding<FinancialIndexProtocol>>,
    dialog: Option<Dialog>,
    closed: Rc<Cell<bool>>,
    released: bool,
    dialog_language: Option<LanguageCode>,
    last_reply: Option<RequestId>,

    selected: HashSet<FinancialIndexTopic>,
    catalog: ManifestState,
    category: Option<SmolStr>,
    typed_filter: SmolStr,
    visible: Arc<[FinancialIndexDescriptor]>,
    limit: Option<u32>,
    selection_error: Option<FinancialIndexDataError>,

    tree: Option<Tree>,
    list: Option<List>,
    input: Option<InputNode>,
    spinner: Option<Spinner>,
    retry_button: Option<Button>,
    message_block: Option<NodeId>,
    message_built: Option<(LanguageCode, Option<SelectorMessage>)>,
    message_slot: Option<NodeId>,
    rows: Rc<RefCell<Arc<[FinancialIndexDescriptor]>>>,
    selected_codes: Signal<Arc<HashSet<SmolStr>>>,
    message: Signal<Option<SelectorMessage>>,
    search_visible: Signal<bool>,
    count: Signal<Option<SelectionCount>>,
    selection_error_signal: Signal<Option<FinancialIndexDataError>>,
    input_state: Signal<InputState>,
    input_placeholder: Signal<SmolStr>,
    retry_requested: Rc<Cell<bool>>,
    on_confirm: OnConfirm,
}

struct UnexpectedResponse;

impl TranslatedError for UnexpectedResponse {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        t_smol!(translate, "widget.financial_index.selector.load_failed")
    }
}

impl IndexSelector {
    pub(super) fn open(
        cx: &mut WidgetCx<'_>,
        current: &[SelectedFinancialIndex],
        on_confirm: OnConfirm,
    ) -> Result<Self, EngineError> {
        let source = cx.services().kytrix_service().cloned();
        Self::start(cx, current, on_confirm, source)
    }

    #[cfg(test)]
    pub(super) fn open_with_source<S: FinancialIndexSource>(
        cx: &mut WidgetCx<'_>,
        current: &[SelectedFinancialIndex],
        on_confirm: OnConfirm,
        source: S,
    ) -> Result<Self, EngineError> {
        Self::start(cx, current, on_confirm, Some(source))
    }

    fn start<S: FinancialIndexSource>(
        cx: &mut WidgetCx<'_>,
        current: &[SelectedFinancialIndex],
        on_confirm: OnConfirm,
        source: Option<S>,
    ) -> Result<Self, EngineError> {
        let state = SelectorState::new(cx, current, on_confirm, source);
        let mut selector = Self {
            inner: Rc::new(RefCell::new(state)),
        };
        if let Err(error) = selector.build_and_open(cx) {
            selector.release(cx);
            return Err(error);
        }
        selector.inner.borrow_mut().begin_read(cx);
        let _ = cx.request_frame();
        Ok(selector)
    }

    pub(super) fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.finished() {
            self.release(cx);
            return Ok(());
        }
        if self.needs_rebuild(cx)
            && let Err(error) = self.build_and_open(cx)
        {
            log::warn!("the index selector could not be rebuilt: {error}");
            self.release(cx);
            return Ok(());
        }
        let result = self.inner.borrow_mut().sync_frame(cx);
        if let Err(error) = result {
            log::warn!("the index selector could not be synced: {error}");
            self.release(cx);
            return Ok(());
        }
        if self.finished() {
            self.release(cx);
        }
        Ok(())
    }

    pub(super) fn is_open(&self, app: &App) -> bool {
        let state = self.inner.borrow();
        if state.released || state.closed.get() {
            return false;
        }
        state
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.is_open(app))
    }

    pub(super) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        let (components, scope, binding) = {
            let mut state = self.inner.borrow_mut();
            if state.released {
                return;
            }
            state.released = true;
            (state.take_components(), state.scope, state.binding.take())
        };
        release_components(cx.app_mut(), components);
        drop(binding);
        let _ = cx.app_mut().release_scope(scope);
    }

    fn finished(&self) -> bool {
        let state = self.inner.borrow();
        state.released || state.closed.get()
    }

    fn needs_rebuild(&self, cx: &WidgetCx<'_>) -> bool {
        let language = {
            let state = self.inner.borrow();
            *cx.read(&state.language)
        };
        let state = self.inner.borrow();
        !state.released && !state.closed.get() && state.dialog_language != Some(language)
    }

    fn build_and_open(&self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let components = self.inner.borrow_mut().take_components();
        release_components(cx.app_mut(), components);
        self.inner.borrow_mut().create_kit(cx)?;
        let dialog = SelectorState::build_dialog(&self.inner, cx);
        self.inner.borrow_mut().dialog = Some(dialog.clone());
        let window = cx.window();
        if let Err(error) = dialog.open(cx.app_mut(), window) {
            let components = self.inner.borrow_mut().take_components();
            release_components(cx.app_mut(), components);
            return Err(engine_error_of(error, window));
        }
        self.inner.borrow_mut().after_open(cx)?;
        let _ = cx.request_frame();
        Ok(())
    }
}

impl SelectorState {
    fn new<S: FinancialIndexSource>(
        cx: &mut WidgetCx<'_>,
        current: &[SelectedFinancialIndex],
        on_confirm: OnConfirm,
        source: Option<S>,
    ) -> Self {
        let scope = cx.app_mut().signal_scope();
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let keys = SelectorKeys {
            dialog: cx.unique_key("financial_index.selector.dialog"),
            tree: cx.unique_key("financial_index.selector.categories"),
            list: cx.unique_key("financial_index.selector.indices"),
            filter: cx.unique_key("financial_index.selector.filter"),
            retry: cx.unique_key("financial_index.selector.retry"),
            spinner: cx.unique_key("financial_index.selector.loading"),
            empty: cx.unique_key("financial_index.selector.empty"),
        };
        let mut selected = HashSet::with_capacity(current.len());
        selected.extend(current.iter().map(|index| index.topic.clone()));
        let binding =
            source.map(|source| WidgetBinding::spawn(cx, FinancialIndexService::new(source)));
        let catalog = if binding.is_some() {
            ManifestState::Loading
        } else {
            ManifestState::Failed(Arc::new(FinancialIndexServiceError::Service(
                KytrixServiceError::Closed,
            )))
        };
        let placeholder = t_smol!(
            translate,
            "widget.financial_index.selector.filter_placeholder"
        );
        let input_state = cx.app_mut().signal_scoped(scope, InputState::new(""));
        let input_placeholder = cx.app_mut().signal_scoped(scope, placeholder);
        let message = cx
            .app_mut()
            .signal_scoped(scope, Some(SelectorMessage::Loading));
        let search_visible = cx.app_mut().signal_scoped(scope, false);
        let count = cx.app_mut().signal_scoped(scope, None);
        let selected_codes = cx.app_mut().signal_scoped(scope, Arc::new(HashSet::new()));
        let selection_error_signal = cx.app_mut().signal_scoped(scope, None);
        let rows: Rc<RefCell<Arc<[FinancialIndexDescriptor]>>> =
            Rc::new(RefCell::new(Arc::from([])));
        Self {
            window: cx.window(),
            scope,
            theme,
            translate,
            language,
            keys,
            binding,
            dialog: None,
            closed: Rc::new(Cell::new(false)),
            released: false,
            dialog_language: None,
            last_reply: None,
            selected,
            catalog,
            category: None,
            typed_filter: SmolStr::default(),
            visible: Arc::from([]),
            limit: None,
            selection_error: None,
            tree: None,
            list: None,
            input: None,
            spinner: None,
            retry_button: None,
            message_block: None,
            message_built: None,
            message_slot: None,
            rows,
            selected_codes,
            message,
            search_visible,
            count,
            selection_error_signal,
            input_state,
            input_placeholder,
            retry_requested: Rc::new(Cell::new(false)),
            on_confirm,
        }
    }

    fn create_kit(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        let roots = category_rows(&self.catalog, language);
        let tree = Tree::new(
            cx.app_mut(),
            self.theme.clone(),
            self.keys.tree.clone(),
            roots,
            TreeOptions::default(),
        );
        let rows = Rc::clone(&self.rows);
        let selected_codes = self.selected_codes;
        let theme = self.theme.clone();
        let list_key = self.keys.list.clone();
        let dialog_key = self.keys.dialog.clone();
        let list_language = self.language;
        let list = List::new(
            cx.app_mut(),
            self.theme.clone(),
            list_key,
            ListOptions::fixed(ROW_HEIGHT),
            move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                let Some(descriptor) = rows.borrow().get(index).cloned() else {
                    return Ok(());
                };
                build_index_row(
                    tree,
                    parent,
                    &descriptor,
                    &dialog_key,
                    selected_codes,
                    &theme,
                    list_language,
                )
            },
        );
        list.set_initial_count(cx.app_mut(), self.visible.len());
        let placeholder = t_smol!(
            self.translate,
            "widget.financial_index.selector.filter_placeholder"
        );
        let _ = cx.write(&self.input_placeholder, placeholder);
        self.tree = Some(tree);
        self.list = Some(list);
        self.input = None;
        self.spinner = None;
        self.retry_button = None;
        self.message_block = None;
        self.message_slot = None;
        self.message_built = None;
        Ok(())
    }

    fn build_dialog(inner: &Rc<RefCell<Self>>, cx: &mut WidgetCx<'_>) -> Dialog {
        let (theme, layer, animation, key, closed, title, confirm_label, cancel_label) = {
            let state = inner.borrow();
            let translate = &state.translate;
            (
                state.theme.clone(),
                cx.layer().clone(),
                cx.animation().clone(),
                state.keys.dialog.clone(),
                Rc::clone(&state.closed),
                t_smol!(translate, "widget.financial_index.choose"),
                t_smol!(translate, "common.confirm"),
                t_smol!(translate, "common.cancel"),
            )
        };
        let weak = Rc::downgrade(inner);
        let content_weak = Weak::clone(&weak);
        let confirm_weak = Weak::clone(&weak);
        DialogBuilder::new(title)
            .header(false)
            .close_button(false)
            .width(DIALOG_WIDTH)
            .content(move |app, window, body| {
                let Some(inner) = content_weak.upgrade() else {
                    return Ok(());
                };
                inner.borrow_mut().build_content(app, window, body)
            })
            .confirm(confirm_label, move |app| {
                let Some(inner) = confirm_weak.upgrade() else {
                    return;
                };
                let (selection, on_confirm) = {
                    let Ok(state) = inner.try_borrow() else {
                        return;
                    };
                    (state.selection(), Rc::clone(&state.on_confirm))
                };
                if let Some(selection) = selection {
                    on_confirm(app, selection);
                }
            })
            .cancel(cancel_label, |_app| {})
            .on_close(move |_close, _app| closed.set(true))
            .build(cx.app_mut(), theme, layer, animation, key)
    }

    fn after_open(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        self.dialog_language = Some(language);
        self.publish(cx);
        self.select_category_row(cx)?;
        self.sync_message_block(cx)?;
        Ok(())
    }

    fn begin_read(&mut self, cx: &mut WidgetCx<'_>) {
        if let Some(binding) = self.binding.as_mut() {
            binding.call(cx, FinancialIndexRequest::FetchManifest);
        }
    }

    fn build_content(
        &mut self,
        app: &mut App,
        window: WindowId,
        body: NodeId,
    ) -> Result<(), EngineError> {
        let (Some(tree), Some(list)) = (self.tree.clone(), self.list.clone()) else {
            return Ok(());
        };
        let key = self.keys.dialog.clone();
        let theme = self.theme.clone();
        let translate = self.translate.clone();
        let text_size = UnifiedSize::Small.text_size();

        let panel = build_node(app, window, body, |tree, body| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:panel"))
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .height(DIALOG_HEIGHT)
                        .flex_col()
                        .gap(8.0)
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .build(tree, body)
        })?;

        let top = build_node(app, window, panel, |tree, panel| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:top"))
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .flex_1()
                        .gap(8.0)
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .build(tree, panel)
        })?;

        let column = build_node(app, window, top, |tree, top| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:categories"))
                .style(
                    StyleBuilder::new()
                        .width(CATEGORY_COLUMN_WIDTH)
                        .height_full()
                        .flex_none()
                        .min_height_0()
                        .overflow_hidden()
                        .build(),
                )
                .paint(PaintBuilder::new().rounded(CORNER_RADIUS).build())
                .bind_style(panel_style(theme.clone(), Role::SurfaceSunken))
                .build(tree, top)
        })?;
        tree.mount(app, window, column)?;

        build_node(app, window, top, |tree, top| {
            Separator::vertical(theme.clone()).build(tree, top)
        })?;

        let right = build_node(app, window, top, |tree, top| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:right"))
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .flex_1()
                        .gap(8.0)
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .build(tree, top)
        })?;

        let search_row = build_node(app, window, right, |tree, right| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:search"))
                .style(
                    StyleBuilder::new()
                        .flex_none()
                        .width_full()
                        .flex_row()
                        .items_center()
                        .gap(8.0)
                        .min_width_0()
                        .build(),
                )
                .bind_style(visible_when(self.search_visible))
                .build(tree, right)
        })?;

        let filter_holder = build_node(app, window, search_row, |tree, search| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:filter"))
                .style(StyleBuilder::new().flex_1().min_width_0().build())
                .build(tree, search)
        })?;
        let spec = Arc::new(InputSpec {
            name: self.keys.filter.clone(),
            placeholder: SmolStr::default(),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            size: ControlSize::Small,
            submit: None,
        });
        let filter = {
            let tree = app
                .window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut();
            input::build(
                tree,
                filter_holder,
                spec,
                &theme,
                &self.input_state,
                &self.input_placeholder,
            )
        };
        filter.install(app);
        self.input = Some(filter);

        let count = self.count;
        let count_language = self.language;
        let count_translate = translate.clone();
        let count_theme = theme.clone();
        build_node(app, window, search_row, |tree, search| {
            NodeBuilder::text(SmolStr::default(), TextStyle::new(text_size))
                .id(format_smolstr!("{key}:count"))
                .style(
                    StyleBuilder::new()
                        .flex_none()
                        .max_width(CATEGORY_COLUMN_WIDTH * 2.0)
                        .min_width_0()
                        .build(),
                )
                .bind_style(move |cx| {
                    let limit = cx.read(&count).is_some();
                    StylePatch {
                        visible: Some(limit),
                        foreground: Some(count_theme.role(cx, Role::FgMuted)),
                        ..StylePatch::default()
                    }
                })
                .bind_text(move |cx| {
                    let _language = cx.read(&count_language);
                    match *cx.read(&count) {
                        Some(count) => t_smol!(
                            count_translate,
                            "widget.financial_index.selector.selection_count",
                            current = count.current,
                            limit = count.limit
                        ),
                        None => SmolStr::default(),
                    }
                })
                .build(tree, search)
        })?;

        let list_panel = build_node(app, window, right, |tree, right| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:list-panel"))
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .flex_1()
                        .min_width_0()
                        .min_height_0()
                        .overflow_hidden()
                        .build(),
                )
                .paint(PaintBuilder::new().rounded(CORNER_RADIUS).build())
                .bind_style(panel_style(theme.clone(), Role::SurfaceRaised))
                .build(tree, right)
        })?;

        let list_holder = build_node(app, window, list_panel, |tree, panel| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:indices"))
                .style(
                    StyleBuilder::new()
                        .flex_1()
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .bind_style(visible_when_none(self.message))
                .build(tree, panel)
        })?;
        list.mount(app, window, list_holder)?;

        let slot = build_node(app, window, list_panel, |tree, panel| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:notice"))
                .style(
                    StyleBuilder::new()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .padding(8.0)
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .bind_style(visible_when_some(self.message))
                .build(tree, panel)
        })?;
        self.message_slot = Some(slot);

        let footer = build_node(app, window, panel, |tree, panel| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:footer"))
                .style(
                    StyleBuilder::new()
                        .flex_none()
                        .width_full()
                        .flex_col()
                        .gap(2.0)
                        .min_width_0()
                        .build(),
                )
                .build(tree, panel)
        })?;
        build_node(app, window, footer, |tree, footer| {
            Separator::horizontal(theme.clone()).build(tree, footer)
        })?;
        let error = self.selection_error_signal;
        let error_language = self.language;
        let error_translate = translate;
        let error_theme = theme;
        build_node(app, window, footer, |tree, footer| {
            NodeBuilder::text(
                SmolStr::default(),
                TextStyle::new(text_size).with_overflow(TextOverflow::Ellipsis, 1),
            )
            .id(format_smolstr!("{key}:selection-error"))
            .style(StyleBuilder::new().width_full().min_width_0().build())
            .bind_style(move |cx| StylePatch {
                visible: Some(cx.read(&error).is_some()),
                foreground: Some(error_theme.role(cx, Role::StatusDangerBase)),
                ..StylePatch::default()
            })
            .bind_text(move |cx| {
                let _language = cx.read(&error_language);
                match cx.read(&error).as_ref() {
                    Some(failure) => failure.translated_message(&error_translate),
                    None => SmolStr::default(),
                }
            })
            .build(tree, footer)
        })?;
        Ok(())
    }

    fn sync_frame(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.apply_delivered(cx)?;
        if self.retry_requested.take() {
            self.reload(cx)?;
        }
        self.dispatch_text_events(cx);
        self.follow_tree(cx)?;
        self.follow_filter(cx)?;
        self.follow_list_click(cx)?;
        self.publish(cx);
        self.sync_message_block(cx)?;
        if let Some(tree) = self.tree.as_ref() {
            tree.sync(cx.app_mut())?;
        }
        if let Some(list) = self.list.as_ref() {
            list.sync(cx.app_mut())?;
        }
        if let Some(input) = self.input.as_ref() {
            input.sync(cx.app_mut());
        }
        if let Some(spinner) = self.spinner.as_ref() {
            spinner.sync(cx.app_mut())?;
        }
        self.sync_confirm(cx);
        if let Some(dialog) = self.dialog.as_ref() {
            dialog
                .sync(cx.app_mut())
                .map_err(|error| engine_error_of(error, self.window))?;
        }
        Ok(())
    }

    fn apply_delivered(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(binding) = self.binding.as_ref() else {
            return Ok(());
        };
        for inbound in binding.take() {
            let Inbound::Rpc { request_id, result } = inbound else {
                continue;
            };
            if self.last_reply.is_some_and(|last| request_id <= last) {
                continue;
            }
            self.last_reply = Some(request_id);
            match result {
                Ok(FinancialIndexResponse::Manifest { catalog, limit }) => {
                    self.accept_catalog(catalog, limit, cx)?;
                }
                Ok(_) => self.fail(Arc::new(UnexpectedResponse), cx)?,
                Err(error) => self.fail(Arc::new(error), cx)?,
            }
        }
        Ok(())
    }

    fn accept_catalog(
        &mut self,
        catalog: FinancialIndexCatalog,
        limit: Option<u32>,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        self.category = catalog
            .categories
            .first()
            .map(|category| category.code.clone());
        self.typed_filter = SmolStr::default();
        if let Some(input) = self.input.as_ref() {
            input.update_value(cx.app_mut(), "");
        }
        self.limit = limit;
        self.selection_error = None;
        self.catalog = ManifestState::Ready(catalog);
        self.resolve_visible();
        let roots = category_rows(&self.catalog, language);
        if let Some(tree) = self.tree.as_ref() {
            tree.set_roots(cx.app_mut(), roots)?;
        }
        self.select_category_row(cx)?;
        self.show_visible(cx)?;
        let _ = cx.request_frame();
        Ok(())
    }

    fn fail(
        &mut self,
        error: Arc<dyn TranslatedError>,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        self.category = None;
        self.typed_filter = SmolStr::default();
        self.visible = Arc::from([]);
        *self.rows.borrow_mut() = Arc::clone(&self.visible);
        self.catalog = ManifestState::Failed(error);
        if let Some(input) = self.input.as_ref() {
            input.update_value(cx.app_mut(), "");
        }
        if let Some(tree) = self.tree.as_ref() {
            tree.set_roots(cx.app_mut(), std::iter::empty())?;
        }
        if let Some(list) = self.list.as_ref() {
            list.set_items(cx.app_mut(), 0)?;
        }
        let _ = cx.request_frame();
        Ok(())
    }

    fn reload(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.selection_error = None;
        self.category = None;
        self.typed_filter = SmolStr::default();
        self.visible = Arc::from([]);
        *self.rows.borrow_mut() = Arc::clone(&self.visible);
        self.limit = None;
        self.catalog = ManifestState::Loading;
        if let Some(input) = self.input.as_ref() {
            input.update_value(cx.app_mut(), "");
        }
        if let Some(tree) = self.tree.as_ref() {
            tree.set_roots(cx.app_mut(), std::iter::empty())?;
        }
        if let Some(list) = self.list.as_ref() {
            list.set_items(cx.app_mut(), 0)?;
        }
        if let Some(binding) = self.binding.as_mut() {
            binding.call(cx, FinancialIndexRequest::FetchManifest);
        }
        let _ = cx.request_frame();
        Ok(())
    }

    fn dispatch_text_events(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(filter) = self.input.as_ref() else {
            return;
        };
        if !self.owns_modal(cx.app()) {
            return;
        }
        let window = self.window;
        let events = cx
            .app_mut()
            .drain_events_where(|target, event| target == window && is_text_input(event));
        for (_, event) in events {
            let _ = filter.dispatch(cx.app_mut(), &event);
        }
    }

    fn owns_modal(&self, app: &App) -> bool {
        let Some(dialog) = self.dialog.as_ref() else {
            return false;
        };
        matches!(
            (app.modal_scope(self.window), dialog.overlay_node()),
            (Some(top), Some(own)) if top == own
        )
    }

    fn follow_tree(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(category) = self.tree.as_ref().and_then(|tree| tree.selected(cx.app())) else {
            return Ok(());
        };
        if self.category.as_ref() == Some(&category) {
            return Ok(());
        }
        self.category = Some(category);
        self.typed_filter = SmolStr::default();
        if let Some(input) = self.input.as_ref() {
            input.update_value(cx.app_mut(), "");
        }
        self.resolve_visible();
        self.show_visible(cx)?;
        let _ = cx.request_frame();
        Ok(())
    }

    fn follow_filter(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let query = SmolStr::new(cx.read(&self.input_state).text().trim());
        if query == self.typed_filter {
            return Ok(());
        }
        self.typed_filter = query;
        self.resolve_visible();
        self.show_visible(cx)?;
        let _ = cx.request_frame();
        Ok(())
    }

    fn follow_list_click(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(index) = self
            .list
            .as_ref()
            .and_then(|list| list.selection(cx.app()).first())
        else {
            return Ok(());
        };
        if let Some(list) = self.list.as_ref() {
            list.select(cx.app_mut(), None);
        }
        let Some(descriptor) = self.visible.get(index).cloned() else {
            return Ok(());
        };
        self.toggle(&descriptor.index.topic, cx);
        Ok(())
    }

    fn toggle(&mut self, topic: &FinancialIndexTopic, cx: &mut WidgetCx<'_>) {
        let checked = !self.selected.contains(topic);
        match update_selection(&mut self.selected, topic, checked) {
            Ok(true) => self.selection_error = None,
            Ok(false) => return,
            Err(error) => self.selection_error = Some(error),
        }
        self.publish(cx);
        let _ = cx.request_frame();
    }

    fn resolve_visible(&mut self) {
        let filter = (!self.typed_filter.is_empty()).then_some(self.typed_filter.as_str());
        self.visible = match &self.catalog {
            ManifestState::Ready(catalog) => {
                catalog.descriptors_for(self.category.as_ref(), filter)
            }
            ManifestState::Loading | ManifestState::Failed(_) => Arc::from([]),
        };
        *self.rows.borrow_mut() = Arc::clone(&self.visible);
    }

    fn show_visible(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if let Some(list) = self.list.as_ref() {
            list.set_items(cx.app_mut(), self.visible.len())?;
        }
        let _ = cx.request_frame();
        Ok(())
    }

    fn select_category_row(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let index = match (&self.catalog, &self.category) {
            (ManifestState::Ready(catalog), Some(category)) => catalog
                .categories
                .iter()
                .position(|listed| &listed.code == category),
            _ => None,
        };
        if let Some(tree) = self.tree.as_ref() {
            match index {
                Some(index) => tree.select_row(cx.app_mut(), index)?,
                None => {
                    tree.list().select(cx.app_mut(), None);
                }
            }
        }
        Ok(())
    }

    fn is_ready(&self) -> bool {
        matches!(self.catalog, ManifestState::Ready(_))
    }

    fn selection(&self) -> Option<Vec<SelectedFinancialIndex>> {
        let ManifestState::Ready(catalog) = &self.catalog else {
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

    fn message(&self) -> Option<SelectorMessage> {
        match &self.catalog {
            ManifestState::Loading => Some(SelectorMessage::Loading),
            ManifestState::Failed(_) => Some(SelectorMessage::Failed),
            ManifestState::Ready(_) if self.visible.is_empty() && !self.typed_filter.is_empty() => {
                Some(SelectorMessage::NoMatches)
            }
            ManifestState::Ready(_) if self.visible.is_empty() => Some(SelectorMessage::Empty),
            ManifestState::Ready(_) => None,
        }
    }

    fn publish(&self, cx: &mut WidgetCx<'_>) -> bool {
        let message = self.message();
        let count = self.limit.map(|limit| SelectionCount {
            current: selection_count(&self.selected),
            limit,
        });
        let codes = self.selected_codes();
        let error = self.selection_error.clone();
        let mut changed = false;
        changed |= cx.write(&self.message, message);
        changed |= cx.write(&self.search_visible, self.is_ready());
        changed |= cx.write(&self.count, count);
        changed |= cx.write(&self.selection_error_signal, error);
        let unchanged = {
            let current = cx.read(&self.selected_codes);
            **current == codes
        };
        if !unchanged {
            changed |= cx.write(&self.selected_codes, Arc::new(codes));
        }
        changed
    }

    fn selected_codes(&self) -> HashSet<SmolStr> {
        let mut codes = HashSet::with_capacity(self.selected.len());
        codes.extend(self.selected.iter().map(|topic| SmolStr::new(topic.code())));
        codes
    }

    fn sync_confirm(&self, cx: &mut WidgetCx<'_>) {
        let Some(dialog) = self.dialog.as_ref() else {
            return;
        };
        if let Some(button) = dialog.confirm_button() {
            let _ = button.set_disabled(cx.app_mut(), !self.is_ready());
        }
    }

    fn sync_message_block(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        let message = self.message();
        if self.message_built == Some((language, message)) {
            return Ok(());
        }
        self.message_built = Some((language, message));
        self.clear_message_block(cx)?;
        let Some(slot) = self.message_slot else {
            return Ok(());
        };
        let Some(message) = message else {
            return Ok(());
        };
        self.build_message(cx, slot, message)?;
        Ok(())
    }

    fn clear_message_block(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if let Some(node) = self.message_block.take() {
            let window = cx.window();
            if cx.tree().get(node).is_ok() {
                let _ = cx.app_mut().remove_node(window, node)?;
            }
        }
        if let Some(spinner) = self.spinner.take() {
            spinner.release(cx.app_mut());
        }
        if let Some(retry) = self.retry_button.take() {
            retry.release(cx.app_mut());
        }
        Ok(())
    }

    fn build_message(
        &mut self,
        cx: &mut WidgetCx<'_>,
        slot: NodeId,
        message: SelectorMessage,
    ) -> Result<(), EngineError> {
        let window = cx.window();
        let key = self.keys.dialog.clone();
        let theme = self.theme.clone();
        let translate = self.translate.clone();
        let wrapper = build_node(cx.app_mut(), window, slot, |tree, slot| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:message"))
                .style(
                    StyleBuilder::new()
                        .flex_1()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(8.0)
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .build(tree, slot)
        })?;
        match message {
            SelectorMessage::Loading => {
                let spinner = Spinner::new(cx.app_mut(), theme.clone(), self.keys.spinner.clone())
                    .with_size(UnifiedSize::Medium.icon_size())
                    .with_role(Role::FgMuted);
                spinner.build(cx.app_mut(), window, wrapper)?;
                self.spinner = Some(spinner);
                static_text(
                    cx.app_mut(),
                    window,
                    wrapper,
                    format_smolstr!("{key}:loading"),
                    t_smol!(translate, "widget.financial_index.selector.loading"),
                    UnifiedSize::Small.text_size(),
                    Role::FgMuted,
                    &theme,
                )?;
            }
            SelectorMessage::Failed => {
                static_text(
                    cx.app_mut(),
                    window,
                    wrapper,
                    format_smolstr!("{key}:failure-title"),
                    t_smol!(translate, "widget.financial_index.selector.load_failed"),
                    UnifiedSize::Medium.text_size(),
                    Role::StatusDangerBase,
                    &theme,
                )?;
                let detail = match &self.catalog {
                    ManifestState::Failed(error) => error.translated_message(&translate),
                    ManifestState::Loading | ManifestState::Ready(_) => SmolStr::default(),
                };
                static_text(
                    cx.app_mut(),
                    window,
                    wrapper,
                    format_smolstr!("{key}:failure-detail"),
                    detail,
                    UnifiedSize::Small.text_size(),
                    Role::FgDefault,
                    &theme,
                )?;
                if self.binding.is_some() {
                    self.build_retry(cx, window, wrapper)?;
                }
            }
            SelectorMessage::NoMatches => {
                let empty = EmptyState::new(theme, self.keys.empty.clone())
                    .text(t_smol!(translate, "common.no_matches"));
                build_node(cx.app_mut(), window, wrapper, |tree, wrapper| {
                    empty.build(tree, wrapper)
                })?;
            }
            SelectorMessage::Empty => {
                let empty = EmptyState::new(theme, self.keys.empty.clone())
                    .text(t_smol!(translate, "widget.financial_index.selector.empty"));
                build_node(cx.app_mut(), window, wrapper, |tree, wrapper| {
                    empty.build(tree, wrapper)
                })?;
            }
        }
        self.message_block = Some(wrapper);
        Ok(())
    }

    fn build_retry(
        &mut self,
        cx: &mut WidgetCx<'_>,
        window: WindowId,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let requested = Rc::clone(&self.retry_requested);
        let button = Button::new(cx.app_mut(), self.theme.clone(), self.keys.retry.clone())
            .outline()
            .small()
            .label(t_smol!(self.translate, "common.retry"))
            .on_click(move |ctx| {
                requested.set(true);
                let _ = ctx.app_mut().request_repaint(window);
            });
        button.build(cx.app_mut(), window, parent)?;
        self.retry_button = Some(button);
        Ok(())
    }

    fn take_components(&mut self) -> SelectorComponents {
        self.message_block = None;
        self.message_slot = None;
        self.message_built = None;
        SelectorComponents {
            dialog: self.dialog.take(),
            list: self.list.take(),
            tree: self.tree.take(),
            input: self.input.take(),
            spinner: self.spinner.take(),
            retry: self.retry_button.take(),
        }
    }
}

fn release_components(app: &mut App, components: SelectorComponents) {
    if let Some(dialog) = components.dialog {
        dialog.release(app);
    }
    if let Some(input) = components.input {
        input.release(app);
    }
    if let Some(list) = components.list {
        list.release(app);
    }
    if let Some(tree) = components.tree {
        tree.release(app);
    }
    if let Some(spinner) = components.spinner {
        spinner.release(app);
    }
    if let Some(retry) = components.retry {
        retry.release(app);
    }
}

fn category_rows(catalog: &ManifestState, language: LanguageCode) -> Vec<TreeItem> {
    let ManifestState::Ready(catalog) = catalog else {
        return Vec::new();
    };
    catalog
        .categories
        .iter()
        .map(|category| {
            TreeItem::new(
                category.code.clone(),
                SmolStr::new(category.name.get(language)),
            )
        })
        .collect()
}

fn build_index_row(
    tree: &mut NodeTree,
    parent: NodeId,
    descriptor: &FinancialIndexDescriptor,
    dialog_key: &str,
    selected: Signal<Arc<HashSet<SmolStr>>>,
    theme: &ThemeHandle,
    language: Signal<LanguageCode>,
) -> Result<(), EngineError> {
    let code = SmolStr::new(descriptor.index.topic.code());
    let row = NodeBuilder::boxed()
        .id(format_smolstr!("{dialog_key}:row:{code}"))
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(6.0)
                .width_full()
                .min_width_0()
                .build(),
        )
        .bind_style({
            let row_theme = theme.clone();
            let row_code = code.clone();
            move |cx| {
                let chosen = cx.read(&selected).contains(&row_code);
                StylePatch {
                    background: chosen.then(|| row_theme.role(cx, Role::Selection)),
                    ..StylePatch::default()
                }
            }
        })
        .build(tree, parent)?;

    let mark_theme = theme.clone();
    let mark_code = code.clone();
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(UnifiedSize::Small.text_size()),
    )
    .id(format_smolstr!("{dialog_key}:row:{code}:mark"))
    .style(
        StyleBuilder::new()
            .width(UnifiedSize::Small.text_size())
            .build(),
    )
    .bind_text(move |cx| {
        if cx.read(&selected).contains(&mark_code) {
            SmolStr::new_static(CHECK_MARK)
        } else {
            SmolStr::default()
        }
    })
    .bind_style(move |cx| StylePatch {
        foreground: Some(mark_theme.role(cx, Role::BrandBase)),
        ..StylePatch::default()
    })
    .build(tree, row)?;

    let code_theme = theme.clone();
    NodeBuilder::text(code.clone(), TextStyle::new(UnifiedSize::Small.text_size()))
        .id(format_smolstr!("{dialog_key}:row:{code}:code"))
        .style(StyleBuilder::new().flex_none().build())
        .bind_style(move |cx| StylePatch {
            foreground: Some(code_theme.role(cx, Role::FgDefault)),
            ..StylePatch::default()
        })
        .build(tree, row)?;

    let name = descriptor.index.name.clone();
    let name_theme = theme.clone();
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(UnifiedSize::Small.text_size())
            .with_wrap(TextWrap::NoWrap)
            .with_overflow(TextOverflow::Ellipsis, 1)
            .with_alignment(TextAlign::Right),
    )
    .id(format_smolstr!("{dialog_key}:row:{code}:name"))
    .style(StyleBuilder::new().flex_1().min_width_0().build())
    .bind_text(move |cx| {
        let language = *cx.read(&language);
        SmolStr::new(name.get(language))
    })
    .bind_style(move |cx| StylePatch {
        foreground: Some(name_theme.role(cx, Role::FgMuted)),
        ..StylePatch::default()
    })
    .build(tree, row)?;
    Ok(())
}

fn panel_style(theme: ThemeHandle, fill: Role) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch {
    move |cx| StylePatch {
        background: Some(theme.role(cx, fill)),
        border: Some(Border {
            width: theme.tokens().border_width,
            color: theme.role(cx, Role::LineBorder),
        }),
        ..StylePatch::default()
    }
}

fn visible_when(signal: Signal<bool>) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch {
    move |cx| StylePatch {
        visible: Some(*cx.read(&signal)),
        ..StylePatch::default()
    }
}

fn visible_when_none<T: Clone + 'static>(
    signal: Signal<Option<T>>,
) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch {
    move |cx| StylePatch {
        visible: Some(cx.read(&signal).is_none()),
        ..StylePatch::default()
    }
}

fn visible_when_some<T: Clone + 'static>(
    signal: Signal<Option<T>>,
) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch {
    move |cx| StylePatch {
        visible: Some(cx.read(&signal).is_some()),
        ..StylePatch::default()
    }
}

fn static_text(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    id: SmolStr,
    text: SmolStr,
    size: f32,
    role: Role,
    theme: &ThemeHandle,
) -> Result<NodeId, EngineError> {
    let theme = theme.clone();
    build_node(app, window, parent, move |tree, parent| {
        NodeBuilder::text(text, TextStyle::new(size).with_wrap(TextWrap::Wrap))
            .id(id)
            .style(
                StyleBuilder::new()
                    .max_width(MESSAGE_WIDTH)
                    .min_width_0()
                    .build(),
            )
            .bind_style(move |cx| StylePatch {
                foreground: Some(theme.role(cx, role)),
                ..StylePatch::default()
            })
            .build(tree, parent)
    })
}

fn build_node(
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

fn is_text_input(event: &Event) -> bool {
    matches!(
        event,
        Event::Input(InputEvent::Text { .. } | InputEvent::Key { .. }) | Event::Ime(_)
    )
}

fn selection_count(selected: &HashSet<FinancialIndexTopic>) -> u32 {
    u32::try_from(selected.len()).unwrap_or(u32::MAX)
}

fn update_selection(
    selected: &mut HashSet<FinancialIndexTopic>,
    topic: &FinancialIndexTopic,
    checked: bool,
) -> Result<bool, FinancialIndexDataError> {
    if !checked {
        return Ok(selected.remove(topic));
    }
    if selected.contains(topic) {
        return Ok(false);
    }
    if selected.len() >= MAX_SELECTED_INDICES {
        return Err(FinancialIndexDataError::SelectionCapacity {
            limit: MAX_SELECTED_INDICES,
        });
    }
    Ok(selected.insert(topic.clone()))
}

fn engine_error_of(error: DialogError, window: WindowId) -> EngineError {
    match error {
        DialogError::Engine(error) | DialogError::Overlay(OverlayError::Engine(error)) => error,
        DialogError::Overlay(OverlayError::PlacementConfig { .. }) | DialogError::NotOpen => {
            EngineError::UnknownWindow { window }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        rc::Rc,
        time::{Duration, Instant},
    };

    use ahash::{HashSet, HashSetExt};
    use base_ui::kit::{button::Button, dialog::Dialog, input::InputNode, list::List, tree::Tree};
    use i18n::language::codes::LanguageCode;
    use kytrix_proto::types::v1 as proto_types;
    use kytrix_service_provider::service::engine::FinancialIndexTopic;
    use smol_str::{SmolStr, format_smolstr};
    use ui_core::{
        app::App,
        event::{Action, ActionSource, Event, InputEvent},
        node::id::NodeId,
        test_support::diagnostics::signal_slot_stats,
    };
    use widget::{cx::WidgetCx, test_support::WidgetTestApp};

    use super::*;
    use crate::{
        model::{MAX_SELECTED_INDICES, SelectedFinancialIndex},
        test_support::{self, FakeSource, selected},
    };

    type Confirmed = Rc<RefCell<Option<Vec<SelectedFinancialIndex>>>>;

    const SETTLE_TURNS: usize = 64;

    fn harness() -> WidgetTestApp {
        test_support::harness()
    }

    fn selector(
        harness: &mut WidgetTestApp,
        source: FakeSource,
        current: &[SelectedFinancialIndex],
    ) -> (IndexSelector, Confirmed) {
        let confirmed: Confirmed = Rc::new(RefCell::new(None));
        let sink = Rc::clone(&confirmed);
        let current = current.to_vec();
        let selector = harness
            .with_cx(|cx| {
                IndexSelector::open_with_source(
                    cx,
                    &current,
                    Rc::new(
                        move |_app: &mut App, selection: Vec<SelectedFinancialIndex>| {
                            *sink.borrow_mut() = Some(selection);
                        },
                    ),
                    source,
                )
            })
            .expect("the selector opens");
        (selector, confirmed)
    }

    fn peer_selector(
        harness: &mut WidgetTestApp,
        source: FakeSource,
        current: &[SelectedFinancialIndex],
    ) -> IndexSelector {
        let services = Rc::clone(harness.services_rc());
        let window = harness.window();
        let scope = harness.app_mut().signal_scope();
        let current = current.to_vec();
        let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
        IndexSelector::open_with_source(&mut cx, &current, Rc::new(|_, _| {}), source)
            .expect("the peer selector opens")
    }

    fn sync(harness: &mut WidgetTestApp, selector: &mut IndexSelector) {
        harness
            .with_cx(|cx| selector.sync(cx))
            .expect("the selector syncs");
    }

    fn settle(
        harness: &mut WidgetTestApp,
        selector: &mut IndexSelector,
        what: &str,
        ready: impl Fn(&IndexSelector) -> bool,
    ) {
        for _ in 0..SETTLE_TURNS {
            harness.drain_runtime();
            sync(harness, selector);
            harness.drain();
            if ready(selector) {
                return;
            }
        }
        panic!("{what} should happen");
    }

    fn wait_for_manifest(harness: &mut WidgetTestApp, selector: &mut IndexSelector) {
        settle(harness, selector, "the catalogue to arrive", |selector| {
            !matches!(selector.inner.borrow().catalog, ManifestState::Loading)
        });
    }

    fn settle_frames(harness: &mut WidgetTestApp, selector: &mut IndexSelector) {
        for _ in 0..4 {
            sync(harness, selector);
            harness.drain();
        }
    }

    fn overlay(selector: &IndexSelector) -> NodeId {
        selector
            .inner
            .borrow()
            .dialog
            .as_ref()
            .expect("the dialog is built")
            .overlay_node()
            .expect("the dialog is open")
    }

    fn key(selector: &IndexSelector) -> SmolStr {
        selector.inner.borrow().keys.dialog.clone()
    }

    fn find(harness: &WidgetTestApp, selector: &IndexSelector, suffix: &str) -> Option<NodeId> {
        harness.find_by_debug_prefix(
            overlay(selector),
            &format_smolstr!("{}:{suffix}", key(selector)),
        )
    }

    fn node_at(harness: &WidgetTestApp, selector: &IndexSelector, suffix: &str) -> NodeId {
        find(harness, selector, suffix)
            .unwrap_or_else(|| panic!("the selector draws its {suffix} node"))
    }

    fn text_at(harness: &WidgetTestApp, selector: &IndexSelector, suffix: &str) -> String {
        harness.text_of(node_at(harness, selector, suffix))
    }

    fn visible_at(harness: &WidgetTestApp, selector: &IndexSelector, suffix: &str) -> bool {
        harness.visible(node_at(harness, selector, suffix))
    }

    fn tree(selector: &IndexSelector) -> Tree {
        selector
            .inner
            .borrow()
            .tree
            .as_ref()
            .expect("the column is built")
            .clone()
    }

    fn list(selector: &IndexSelector) -> List {
        selector
            .inner
            .borrow()
            .list
            .as_ref()
            .expect("the list is built")
            .clone()
    }

    fn dialog(selector: &IndexSelector) -> Dialog {
        selector
            .inner
            .borrow()
            .dialog
            .as_ref()
            .expect("the dialog is built")
            .clone()
    }

    fn input(selector: &IndexSelector) -> InputNode {
        selector
            .inner
            .borrow()
            .input
            .as_ref()
            .expect("the filter is built")
            .clone()
    }

    fn input_text(harness: &WidgetTestApp, selector: &IndexSelector) -> String {
        harness.app().read(&input(selector).state).text().to_owned()
    }

    fn row_content(harness: &WidgetTestApp, selector: &IndexSelector, index: usize) -> NodeId {
        list(selector)
            .row_content_node(harness.app(), index)
            .unwrap_or_else(|| panic!("row {index} of the list is mounted"))
    }

    fn visible_codes(selector: &IndexSelector) -> Vec<String> {
        selector
            .inner
            .borrow()
            .visible
            .iter()
            .map(|descriptor| descriptor.index.topic.code().to_owned())
            .collect()
    }

    fn confirmed_codes(confirmed: &Confirmed) -> Option<Vec<String>> {
        confirmed.borrow().as_ref().map(|selection| {
            selection
                .iter()
                .map(|index| index.topic.code().to_owned())
                .collect()
        })
    }

    fn type_into_filter(harness: &mut WidgetTestApp, selector: &mut IndexSelector, typed: &str) {
        let filter = input(selector);
        let window = harness.window();
        let _ = harness.app_mut().set_focus(window, Some(filter.root));
        assert_eq!(
            harness.app().focused(window),
            Some(filter.root),
            "the field takes the focus"
        );
        let event = Event::Input(InputEvent::Text {
            text: SmolStr::new(typed),
            timestamp: Instant::now(),
        });
        assert!(
            filter.dispatch(harness.app_mut(), &event),
            "the focused field takes the text"
        );
        sync(harness, selector);
        harness.drain();
    }

    fn set_filter(harness: &mut WidgetTestApp, selector: &mut IndexSelector, query: &str) {
        let filter = input(selector);
        filter.update_value(harness.app_mut(), query);
        sync(harness, selector);
        harness.drain();
    }

    fn choose_category(harness: &mut WidgetTestApp, selector: &mut IndexSelector, index: usize) {
        let tree = tree(selector);
        tree.select_row(harness.app_mut(), index)
            .expect("the row is the tree's");
        sync(harness, selector);
        harness.drain();
    }

    fn select_list_row(harness: &mut WidgetTestApp, selector: &IndexSelector, index: usize) {
        let list = list(selector);
        let mut selection = list.selection(harness.app());
        selection.select(index);
        assert!(
            harness.app_mut().write(&list.selection_signal(), selection),
            "clicking a row changes the list's selection"
        );
    }

    fn click_row(harness: &mut WidgetTestApp, selector: &mut IndexSelector, index: usize) {
        select_list_row(harness, selector, index);
        sync(harness, selector);
        harness.drain();
    }

    fn toggle(harness: &mut WidgetTestApp, selector: &mut IndexSelector, code: &str) {
        let topic = test_support::topic(code);
        harness.with_cx(|cx| selector.inner.borrow_mut().toggle(&topic, cx));
        sync(harness, selector);
        harness.drain();
    }

    fn click_kit(harness: &mut WidgetTestApp, node: NodeId) {
        let debug = harness
            .tree()
            .debug_id(node)
            .expect("a kit control carries its debug id")
            .to_owned();
        let key = debug
            .strip_suffix(":interactive")
            .expect("a kit control's node id ends with its interactive suffix");
        let action = Action::Custom(format_smolstr!("{key}.click"));
        let window = harness.window();
        assert!(
            harness
                .app_mut()
                .dispatch_action(window, node, action, ActionSource::Programmatic),
            "the control is registered"
        );
    }

    fn click_button(harness: &mut WidgetTestApp, button: &Button) {
        let node = button.node().expect("the button is built");
        click_kit(harness, node);
    }

    fn confirm(harness: &mut WidgetTestApp, selector: &mut IndexSelector) {
        let button = dialog(selector)
            .confirm_button()
            .expect("the dialog has a confirm button");
        click_button(harness, &button);
        sync(harness, selector);
        harness.drain();
    }

    fn cancel(harness: &mut WidgetTestApp, selector: &mut IndexSelector) {
        let button = dialog(selector)
            .cancel_button()
            .expect("the dialog has a cancel button");
        click_button(harness, &button);
        sync(harness, selector);
        harness.drain();
    }

    fn settle_close(harness: &mut WidgetTestApp, selector: &mut IndexSelector) -> bool {
        for _ in 0..SETTLE_TURNS {
            harness.advance(Duration::from_millis(40));
            sync(harness, selector);
            if !selector.is_open(harness.app()) {
                return true;
            }
        }
        !selector.is_open(harness.app())
    }

    fn confirm_disabled(harness: &WidgetTestApp, selector: &IndexSelector) -> bool {
        let button = dialog(selector)
            .confirm_button()
            .expect("the dialog has a confirm button");
        *harness.app().read(&button.interactive().disabled_signal())
    }

    fn empty_manifest() -> proto_types::FinancialIndices {
        proto_types::FinancialIndices {
            categories: Vec::new(),
            indices: Vec::new(),
        }
    }

    #[test]
    fn a_frame_should_draw_the_rows_of_the_category_on_show() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, _) = selector(&mut harness, source, &[selected("HSI")]);
        wait_for_manifest(&mut harness, &mut selector);

        assert_eq!(
            list(&selector).count(harness.app()),
            1,
            "the category on show is one row"
        );
        let row = row_content(&harness, &selector, 0);
        assert!(harness.has_text(row, "HSI"), "the row draws its code");
        assert!(
            harness.has_text(row, "Hang Seng Index"),
            "and the name the manifest published"
        );
        assert!(
            harness.has_text(row, CHECK_MARK),
            "the index the pane already follows is drawn chosen"
        );

        toggle(&mut harness, &mut selector, "HSI");
        assert_eq!(
            list(&selector).count(harness.app()),
            1,
            "the rows are drawn again for the selection they were changed to"
        );
        assert!(
            !harness.has_text(row_content(&harness, &selector, 0), CHECK_MARK),
            "a row that is no longer chosen does not draw the mark"
        );
    }

    #[test]
    fn typing_in_the_field_should_narrow_the_list_to_what_it_matches() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, _) = selector(&mut harness, source.clone(), &[]);
        wait_for_manifest(&mut harness, &mut selector);
        settle_frames(&mut harness, &mut selector);
        assert_eq!(
            visible_codes(&selector),
            ["HSI"],
            "the list opens on the category the column highlights"
        );

        type_into_filter(&mut harness, &mut selector, "hsi");
        assert_eq!(
            visible_codes(&selector),
            ["HSI"],
            "the search leaves the index the field names"
        );

        set_filter(&mut harness, &mut selector, "nothing here");
        assert!(
            visible_codes(&selector).is_empty(),
            "a search that names no index leaves the list empty"
        );
        assert_eq!(
            source.manifest_calls(),
            1,
            "a search is answered from the catalogue the dialog already read"
        );
    }

    #[test]
    fn a_search_that_matches_nothing_should_say_so() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, _) = selector(&mut harness, source, &[]);
        wait_for_manifest(&mut harness, &mut selector);
        settle_frames(&mut harness, &mut selector);

        assert_eq!(
            list(&selector).count(harness.app()),
            1,
            "the category lists its indices while nothing is searched for"
        );
        assert!(
            find(&harness, &selector, "message").is_none(),
            "no message is drawn over the rows"
        );

        set_filter(&mut harness, &mut selector, "nothing here");

        assert_eq!(
            list(&selector).count(harness.app()),
            0,
            "the rows the search left out are not drawn"
        );
        let message = node_at(&harness, &selector, "message");
        assert!(
            harness.has_text(message, "No matches"),
            "a search that matched nothing is drawn as one"
        );
        assert!(
            !visible_at(&harness, &selector, "indices"),
            "and the list holder is hidden under it"
        );
    }

    #[test]
    fn choosing_another_category_should_clear_the_search() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, _) = selector(&mut harness, source, &[]);
        wait_for_manifest(&mut harness, &mut selector);

        set_filter(&mut harness, &mut selector, "hsi");
        assert_eq!(
            visible_codes(&selector),
            ["HSI"],
            "the search is the one the list was drawn against"
        );

        choose_category(&mut harness, &mut selector, 1);

        assert_eq!(
            visible_codes(&selector),
            ["SPX"],
            "the list is the category the column moved to, unsearched"
        );
        assert!(
            input_text(&harness, &selector).is_empty(),
            "and the field the search was typed in does not still show it"
        );
    }

    #[test]
    fn the_count_label_should_draw_the_selection_against_the_account_limit() {
        let mut harness = harness();
        let source = FakeSource::with_session(3);
        source.set_manifest(test_support::manifest());
        let (mut selector, _) = selector(&mut harness, source, &[selected("HSI")]);
        wait_for_manifest(&mut harness, &mut selector);
        settle_frames(&mut harness, &mut selector);

        assert!(
            visible_at(&harness, &selector, "count"),
            "a limit the session published is one the selection is drawn against"
        );
        assert_eq!(
            text_at(&harness, &selector, "count"),
            "Added/Limit: 1/3",
            "the label reads the selection the dialog holds"
        );

        toggle(&mut harness, &mut selector, "SPX");
        assert!(
            visible_at(&harness, &selector, "count"),
            "a selection that grew is still drawn against the limit"
        );
        assert_eq!(
            text_at(&harness, &selector, "count"),
            "Added/Limit: 2/3",
            "a box ticked but not confirmed is a box the count is drawn for"
        );
    }

    #[test]
    fn the_count_label_should_be_left_out_when_no_limit_was_published() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, _) = selector(&mut harness, source, &[selected("HSI")]);
        wait_for_manifest(&mut harness, &mut selector);
        settle_frames(&mut harness, &mut selector);

        assert!(
            visible_at(&harness, &selector, "search"),
            "the field is drawn whether or not there is a limit to draw"
        );
        assert!(
            !visible_at(&harness, &selector, "count"),
            "a count measured against nothing is not drawn"
        );
    }

    #[test]
    fn the_count_label_should_read_the_selection_it_holds() {
        let mut harness = harness();
        let source = FakeSource::with_session(3);
        source.set_manifest(test_support::manifest());
        let (mut selector, _) = selector(&mut harness, source, &[selected("HSI")]);
        wait_for_manifest(&mut harness, &mut selector);
        settle_frames(&mut harness, &mut selector);
        assert_eq!(text_at(&harness, &selector, "count"), "Added/Limit: 1/3");

        toggle(&mut harness, &mut selector, "SPX");
        assert_eq!(
            text_at(&harness, &selector, "count"),
            "Added/Limit: 2/3",
            "a box ticked but not confirmed is a box the count is drawn for"
        );

        toggle(&mut harness, &mut selector, "HSI");
        assert_eq!(
            text_at(&harness, &selector, "count"),
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
            Err(FinancialIndexDataError::SelectionCapacity {
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

    #[test]
    fn a_selector_should_open_on_the_first_category() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, _) = selector(&mut harness, source.clone(), &[]);
        wait_for_manifest(&mut harness, &mut selector);

        assert_eq!(
            visible_codes(&selector),
            ["HSI"],
            "the list opens on the catalogue's first category"
        );
        assert!(matches!(
            selector.inner.borrow().catalog,
            ManifestState::Ready(_)
        ));
        assert_eq!(source.manifest_calls(), 1);
        assert_eq!(
            tree(&selector).selected(harness.app()).as_deref(),
            Some("asia"),
            "the column highlights the category the list opened on"
        );
    }

    #[test]
    fn the_column_should_hold_a_row_per_category() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, _) = selector(&mut harness, source, &[]);
        wait_for_manifest(&mut harness, &mut selector);

        let rows = tree(&selector)
            .rows()
            .into_iter()
            .map(|row| (row.id.to_string(), row.label.to_string()))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                ("asia".to_owned(), "Asia".to_owned()),
                ("america".to_owned(), "America".to_owned()),
            ]
        );
    }

    #[test]
    fn choosing_another_category_row_should_replace_the_list() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, _) = selector(&mut harness, source, &[]);
        wait_for_manifest(&mut harness, &mut selector);

        choose_category(&mut harness, &mut selector, 1);

        assert_eq!(visible_codes(&selector), ["SPX"]);
        assert_eq!(
            selector.inner.borrow().category.as_deref(),
            Some("america"),
            "the category behind the row the tree reports is the one the list shows"
        );
    }

    #[test]
    fn the_column_should_be_labelled_in_the_language_the_window_shows() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, _) = selector(&mut harness, source, &[]);
        wait_for_manifest(&mut harness, &mut selector);

        harness.switch_language(LanguageCode::ZhCn);
        sync(&mut harness, &mut selector);
        harness.drain();

        let rows = tree(&selector)
            .rows()
            .into_iter()
            .map(|row| (row.id.to_string(), row.label.to_string()))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                ("asia".to_owned(), "亚洲".to_owned()),
                ("america".to_owned(), "美洲".to_owned()),
            ],
            "the column reads the names the catalogue carries for the language it is built in"
        );
        assert_eq!(
            tree(&selector).selected(harness.app()).as_deref(),
            Some("asia"),
            "a column built again stays on the category the list is showing"
        );
    }

    #[test]
    fn a_language_switch_should_rebuild_the_dialogs_static_copy() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(empty_manifest());
        let (mut selector, _) = selector(&mut harness, source, &[]);
        wait_for_manifest(&mut harness, &mut selector);
        settle_frames(&mut harness, &mut selector);

        assert!(
            harness.has_text(
                node_at(&harness, &selector, "message"),
                "No indices available"
            ),
            "the empty catalogue says so"
        );
        assert!(
            harness.has_text(
                dialog(&selector)
                    .confirm_button()
                    .expect("confirm")
                    .node()
                    .expect("built"),
                "OK"
            ),
            "the footer is built in the language on show"
        );

        harness.switch_language(LanguageCode::ZhCn);
        sync(&mut harness, &mut selector);
        harness.drain();

        assert!(
            harness.has_text(node_at(&harness, &selector, "message"), "暂无可用指数"),
            "the message the list area draws is rebuilt in the new language"
        );
        let confirm = dialog(&selector).confirm_button().expect("confirm");
        let cancel = dialog(&selector).cancel_button().expect("cancel");
        assert!(
            harness.has_text(confirm.node().expect("built"), "确认"),
            "and so is the confirm button"
        );
        assert!(
            harness.has_text(cancel.node().expect("built"), "取消"),
            "and the cancel button"
        );
    }

    #[test]
    fn confirming_should_carry_the_selection_in_catalogue_order() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, confirmed) = selector(&mut harness, source, &[]);
        wait_for_manifest(&mut harness, &mut selector);

        toggle(&mut harness, &mut selector, "SPX");
        toggle(&mut harness, &mut selector, "HSI");
        confirm(&mut harness, &mut selector);

        assert_eq!(
            confirmed_codes(&confirmed),
            Some(vec!["HSI".to_owned(), "SPX".to_owned()])
        );
    }

    #[test]
    fn a_selector_should_start_from_the_selection_it_was_opened_on() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, confirmed) = selector(&mut harness, source, &[selected("HSI")]);
        wait_for_manifest(&mut harness, &mut selector);

        assert!(
            selector
                .inner
                .borrow()
                .selected
                .contains(&test_support::topic("HSI"))
        );
        confirm(&mut harness, &mut selector);

        assert_eq!(
            confirmed_codes(&confirmed).map(|selection| selection.len()),
            Some(1)
        );
    }

    #[test]
    fn a_catalogue_that_could_not_be_read_should_offer_another_read() {
        let mut harness = harness();
        let source = FakeSource::default();
        let mut broken = test_support::manifest();
        broken.indices[0].name = None;
        source.set_manifest(broken);
        let (mut selector, _) = selector(&mut harness, source.clone(), &[]);
        wait_for_manifest(&mut harness, &mut selector);
        settle_frames(&mut harness, &mut selector);

        assert!(matches!(
            selector.inner.borrow().catalog,
            ManifestState::Failed(_)
        ));
        assert!(visible_codes(&selector).is_empty());
        assert!(
            harness.has_text(
                node_at(&harness, &selector, "failure-title"),
                "Could not load index manifest"
            ),
            "the failure says what could not be read"
        );
        assert!(
            harness.has_text(
                node_at(&harness, &selector, "failure-detail"),
                "An index manifest entry has no name"
            ),
            "and why the service could not list it"
        );
        assert!(
            tree(&selector).rows().is_empty(),
            "a category the service did not list is not a row to choose from"
        );
        assert!(
            confirm_disabled(&harness, &selector),
            "a confirmation out of nothing is not one the dialog offers"
        );

        source.set_manifest(test_support::manifest());
        let retry = selector
            .inner
            .borrow()
            .retry_button
            .as_ref()
            .expect("the failure offers another read")
            .node()
            .expect("the retry button is built");
        click_kit(&mut harness, retry);
        sync(&mut harness, &mut selector);
        settle(
            &mut harness,
            &mut selector,
            "the catalogue read again",
            |selector| matches!(selector.inner.borrow().catalog, ManifestState::Ready(_)),
        );

        assert_eq!(source.manifest_calls(), 2);
        assert_eq!(
            tree(&selector).rows().len(),
            2,
            "the column is the catalogue the read that followed listed"
        );
        assert!(
            !confirm_disabled(&harness, &selector),
            "and a listed catalogue can be confirmed"
        );
    }

    #[test]
    fn a_catalogue_with_no_categories_should_list_nothing() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(empty_manifest());
        let (mut selector, _) = selector(&mut harness, source, &[]);
        wait_for_manifest(&mut harness, &mut selector);
        settle_frames(&mut harness, &mut selector);

        assert!(matches!(
            selector.inner.borrow().catalog,
            ManifestState::Ready(_)
        ));
        assert!(visible_codes(&selector).is_empty());
        assert!(selector.inner.borrow().category.is_none());
        assert!(tree(&selector).rows().is_empty());
        assert_eq!(tree(&selector).selected(harness.app()), None);
        assert!(
            harness.has_text(
                node_at(&harness, &selector, "message"),
                "No indices available"
            ),
            "a catalogue that lists nothing says so"
        );
    }

    #[test]
    fn a_selector_without_a_service_connection_should_draw_the_failure() {
        let mut harness = harness();
        let mut selector = harness
            .with_cx(|cx| IndexSelector::open(cx, &[], Rc::new(|_, _| {})))
            .expect("the selector opens without a connection");
        sync(&mut harness, &mut selector);
        harness.drain();

        assert!(matches!(
            selector.inner.borrow().catalog,
            ManifestState::Failed(_)
        ));
        assert!(
            harness.has_text(
                node_at(&harness, &selector, "failure-title"),
                "Could not load index manifest"
            ),
            "the failure is drawn"
        );
        assert!(
            harness
                .text_of(node_at(&harness, &selector, "failure-detail"))
                .contains("connector is closed"),
            "the connector's own sentence names the reason"
        );
        assert!(
            selector.inner.borrow().retry_button.is_none(),
            "a host with nothing to read through offers no read-again control"
        );
        assert!(confirm_disabled(&harness, &selector));
    }

    #[test]
    fn the_loading_state_should_say_it_is_loading() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, _) = selector(&mut harness, source, &[]);
        sync(&mut harness, &mut selector);
        harness.drain();

        assert_eq!(
            selector.inner.borrow().message_built,
            Some((LanguageCode::En, Some(SelectorMessage::Loading)))
        );
        assert!(
            harness.has_text(
                node_at(&harness, &selector, "message"),
                "Loading index manifest..."
            ),
            "the dialog says what it is waiting for"
        );
        assert!(
            !visible_at(&harness, &selector, "search"),
            "a search is not drawn over a spinner"
        );
        assert!(confirm_disabled(&harness, &selector));
    }

    #[test]
    fn the_selector_should_close_itself_when_its_cancel_button_is_clicked() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, confirmed) = selector(&mut harness, source, &[]);
        wait_for_manifest(&mut harness, &mut selector);
        settle_frames(&mut harness, &mut selector);
        assert!(selector.is_open(harness.app()));

        cancel(&mut harness, &mut selector);
        assert!(
            settle_close(&mut harness, &mut selector),
            "the selector that cancelled has left the layer"
        );

        assert!(!selector.is_open(harness.app()));
        assert!(
            confirmed_codes(&confirmed).is_none(),
            "cancel carries nothing"
        );
    }

    #[test]
    fn confirming_should_carry_the_selection_and_then_close_the_selector() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, confirmed) = selector(&mut harness, source, &[]);
        wait_for_manifest(&mut harness, &mut selector);
        settle_frames(&mut harness, &mut selector);

        toggle(&mut harness, &mut selector, "SPX");
        confirm(&mut harness, &mut selector);

        assert_eq!(
            confirmed_codes(&confirmed),
            Some(vec!["SPX".to_owned()]),
            "the pane is handed the selection the user confirmed"
        );
        assert!(
            settle_close(&mut harness, &mut selector),
            "and the selector that confirmed has left the layer"
        );
        assert!(!selector.is_open(harness.app()));
    }

    #[test]
    fn two_selectors_in_one_host_should_not_share_keys_or_handlers() {
        let mut harness = harness();
        let first_source = FakeSource::default();
        first_source.set_manifest(test_support::manifest());
        let (mut first, _) = selector(&mut harness, first_source, &[]);
        let second_source = FakeSource::default();
        second_source.set_manifest(test_support::manifest());
        let mut second = peer_selector(&mut harness, second_source, &[]);
        wait_for_manifest(&mut harness, &mut first);
        wait_for_manifest(&mut harness, &mut second);
        settle_frames(&mut harness, &mut first);
        settle_frames(&mut harness, &mut second);

        assert_ne!(
            key(&first),
            key(&second),
            "the two panes mints two key sets"
        );
        let first_action = dialog(&first)
            .confirm_button()
            .expect("confirm")
            .action_identity()
            .expect("a click handler");
        let second_action = dialog(&second)
            .confirm_button()
            .expect("confirm")
            .action_identity()
            .expect("a click handler");
        assert_ne!(first_action, second_action);
        assert!(harness.app().has_action_handler(&first_action));
        assert!(harness.app().has_action_handler(&second_action));
        assert_eq!(
            harness.app().modal_scope(harness.window()),
            dialog(&second).overlay_node(),
            "the dialog opened last is the window's modal scope"
        );

        harness.with_cx(|cx| first.release(cx));

        assert!(
            !harness.app().has_action_handler(&first_action),
            "the released pane unregistered its own handler"
        );
        assert!(
            harness.app().has_action_handler(&second_action),
            "releasing one pane removed the other pane's handler"
        );
        assert!(second.is_open(harness.app()));
        assert!(!first.is_open(harness.app()));
        assert_eq!(
            harness.layer().len(harness.window()),
            1,
            "only the open selector's dialog is still in the layer"
        );

        harness.with_cx(|cx| second.release(cx));
        assert!(!harness.app().has_action_handler(&second_action));
        assert!(harness.layer().is_empty(harness.window()));
    }

    #[test]
    fn opening_and_releasing_the_selector_a_hundred_times_reclaims_everything() {
        let mut harness = harness();
        let nodes = harness.node_count();
        let first_action = {
            let source = FakeSource::default();
            source.set_manifest(test_support::manifest());
            let (mut selector, _) = selector(&mut harness, source, &[]);
            sync(&mut harness, &mut selector);
            let action = dialog(&selector)
                .confirm_button()
                .expect("confirm")
                .action_identity()
                .expect("a click handler");
            harness.with_cx(|cx| selector.release(cx));
            assert!(
                !harness.app().has_action_handler(&action),
                "the released dialog unregistered its own confirm action"
            );
            action
        };
        let settled = signal_slot_stats(harness.app());

        for cycle in 0..100 {
            let source = FakeSource::default();
            source.set_manifest(test_support::manifest());
            let (mut selector, _) = selector(&mut harness, source, &[]);
            sync(&mut harness, &mut selector);
            harness.with_cx(|cx| selector.release(cx));
            assert_eq!(
                harness.node_count(),
                nodes,
                "cycle {cycle} left a node behind"
            );
        }

        let after = signal_slot_stats(harness.app());
        assert_eq!(
            after.live, settled.live,
            "a hundred open/release cycles left live signal slots behind"
        );
        assert_eq!(
            after.slots, settled.slots,
            "a hundred open/release cycles allocated value slots for good"
        );
        assert!(!harness.app().has_action_handler(&first_action));
        assert!(harness.layer().is_empty(harness.window()));
    }

    #[test]
    fn releasing_the_selector_should_detach_its_filter_handlers() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, _) = selector(&mut harness, source, &[]);
        sync(&mut harness, &mut selector);
        let filter = input(&selector);
        let click = Action::Custom(format_smolstr!("base_ui.input.{}.click", filter.spec.name));
        let drag = Action::Custom(format_smolstr!("base_ui.input.{}.drag", filter.spec.name));
        assert!(
            harness.app().has_action_handler(&click),
            "the filter registers its click"
        );
        assert!(
            harness.app().has_action_handler(&drag),
            "the filter registers its drag"
        );

        harness.with_cx(|cx| selector.release(cx));

        assert!(
            !harness.app().has_action_handler(&click),
            "the released selector left its filter's click handler behind"
        );
        assert!(
            !harness.app().has_action_handler(&drag),
            "the released selector left its filter's drag handler behind"
        );
    }

    #[test]
    fn clicking_an_index_row_should_toggle_it() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let (mut selector, _) = selector(&mut harness, source, &[]);
        wait_for_manifest(&mut harness, &mut selector);

        click_row(&mut harness, &mut selector, 0);
        assert!(
            selector
                .inner
                .borrow()
                .selected
                .contains(&test_support::topic("HSI"))
        );
        assert!(
            harness.has_text(row_content(&harness, &selector, 0), CHECK_MARK),
            "and the row draws its mark"
        );
        assert!(
            list(&selector).selection(harness.app()).is_empty(),
            "the click channel is cleared so the same row is a change again"
        );

        click_row(&mut harness, &mut selector, 0);
        assert!(
            !selector
                .inner
                .borrow()
                .selected
                .contains(&test_support::topic("HSI"))
        );
    }

    #[test]
    fn checking_past_the_hard_limit_should_draw_the_refusal() {
        let mut harness = harness();
        let source = FakeSource::with_session(3);
        source.set_manifest(test_support::manifest());
        let full = (0..MAX_SELECTED_INDICES)
            .map(|index| selected(&format!("MANY_{index}")))
            .collect::<Vec<_>>();
        let (mut selector, _) = selector(&mut harness, source, &full);
        wait_for_manifest(&mut harness, &mut selector);
        settle_frames(&mut harness, &mut selector);

        click_row(&mut harness, &mut selector, 0);

        assert_eq!(
            selector.inner.borrow().selected.len(),
            MAX_SELECTED_INDICES,
            "the refused check did not change the selection"
        );
        let error = node_at(&harness, &selector, "selection-error");
        assert!(
            harness.visible(error),
            "the refusal is drawn rather than dropped"
        );
        assert!(
            harness
                .text_of(error)
                .contains(&format!("at most {MAX_SELECTED_INDICES}")),
            "and says what the limit is"
        );
        assert_eq!(
            text_at(&harness, &selector, "count"),
            format!("Added/Limit: {MAX_SELECTED_INDICES}/3"),
            "a selection over the account limit is still drawn"
        );
    }

    #[test]
    fn unchecking_at_the_hard_limit_should_be_accepted() {
        let mut harness = harness();
        let source = FakeSource::default();
        source.set_manifest(test_support::manifest());
        let mut full = (0..MAX_SELECTED_INDICES - 1)
            .map(|index| selected(&format!("MANY_{index}")))
            .collect::<Vec<_>>();
        full.push(selected("HSI"));
        let (mut selector, _) = selector(&mut harness, source, &full);
        wait_for_manifest(&mut harness, &mut selector);
        settle_frames(&mut harness, &mut selector);

        click_row(&mut harness, &mut selector, 0);

        assert_eq!(
            selector.inner.borrow().selected.len(),
            MAX_SELECTED_INDICES - 1,
            "a chosen index stays removable at the cap"
        );
        assert!(selector.inner.borrow().selection_error.is_none());
    }

    #[test]
    fn every_message_this_dialog_shows_should_resolve_to_text() {
        let translate = test_support::product_translate(LanguageCode::En);
        for key in [
            "common.cancel",
            "common.confirm",
            "common.no_matches",
            "common.retry",
            "widget.financial_index.choose",
            "widget.financial_index.selector.empty",
            "widget.financial_index.selector.filter_placeholder",
            "widget.financial_index.selector.load_failed",
            "widget.financial_index.selector.loading",
            "widget.financial_index.selector.selection_count",
        ] {
            assert!(!t_smol!(translate, key).is_empty(), "{key} has no copy");
        }
    }
}

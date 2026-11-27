use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::{Rc, Weak},
    sync::Arc,
};

use ahash::AHashMap as HashMap;
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::x::{XApiError, XProfile, XSelection, XSelectionStatus};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::A11yOverride,
    app::App,
    error::EngineError,
    event::Action,
    interaction::ActionCtx,
    math::{LogicalPx, px},
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        id::NodeId,
        image::{ImageFit, ImageSource},
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::{color::Color, style::StylePatch, style_builder::StyleBuilder},
    text::{TextAlign, TextOverflow, TextStyle},
    window::WindowId,
};
use ui_kit::{
    kit::{
        alert::AlertBuilder,
        badge::Badge,
        button::Button,
        dialog::{Dialog, DialogBuilder, DialogClose},
        icon::{ICON_INTRINSIC_SIZE, IconKind, svg},
        input::{self, InputNode, InputSpec, InputState},
        list::{List, ListOptions, SelectionMode},
        separator::Separator,
        size::{UnifiedSizable, UnifiedSize},
        spinner::Spinner,
    },
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
};
use widget::{
    cx::WidgetCx,
    error::{TranslatedError, dialog_engine_error},
    rpc::RpcCallError,
    status::WidgetAction,
};

use super::{
    model::{MediaState, MediaStates, SelectionData, selection_name},
    view::{SelectionNotice, SelectionOperation, status_label, valid_search_query},
};

const DIALOG_WIDTH: f32 = 580.0;

const CONTENT_HEIGHT: f32 = 520.0;

const MESSAGE_WIDTH: f32 = 420.0;

const ROW_AVATAR_SIZE: f32 = 42.0;

const ROW_HEIGHT: LogicalPx = px(72.0);

const VERIFIED_ICON_SIZE: f32 = 12.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DialogPage {
    Monitored,
    Add,
}

#[derive(Clone)]
pub(super) struct DialogView {
    pub signed_in: bool,
    pub ready: bool,
    pub selections: Option<SelectionData>,
    pub searching: Option<SmolStr>,
    pub operation: Option<SelectionOperation>,
    pub notice: Option<SelectionNotice>,
    pub hints: Arc<HashMap<SmolStr, SmolStr>>,
    pub media: MediaStates,
    pub images: Rc<RefCell<HashMap<SmolStr, ImageSource>>>,
    pub recovery: WidgetAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DialogRequest {
    Reload,
    Search(SmolStr),
    Add(SmolStr),
    Delete(SmolStr),
}

pub(super) enum SearchAnswer {
    Found(Arc<[XProfile]>),
    Failed(RpcCallError<XApiError>),
}

pub(super) struct TweetDialog {
    inner: Rc<RefCell<DialogState>>,
}

impl TweetDialog {
    pub(super) fn show(cx: &mut WidgetCx<'_>) -> Result<Self, EngineError> {
        let inner = Rc::new(RefCell::new(DialogState::new(cx)));
        inner.borrow_mut().create_kit(cx)?;

        let dialog = {
            let weak = Rc::downgrade(&inner);
            DialogState::build_dialog(&weak, cx)
        };
        inner.borrow_mut().dialog = Some(dialog.clone());

        let window = cx.window();
        if let Err(error) = dialog.open(cx.app_mut(), window) {
            let (components, scope) = {
                let mut state = inner.borrow_mut();
                state.released = true;
                (state.take_components(), state.scope)
            };
            release_components(cx.app_mut(), components);
            let _ = cx.app_mut().release_scope(scope);
            return Err(dialog_engine_error(error));
        }

        let _ = cx.request_frame();
        Ok(Self { inner })
    }

    pub(super) fn sync(
        &mut self,
        cx: &mut WidgetCx<'_>,
        view: &DialogView,
    ) -> Result<(), EngineError> {
        if self.finished() {
            return Ok(());
        }
        self.inner.borrow_mut().frame(cx, view)
    }

    pub(super) fn take_requests(&mut self, cx: &mut WidgetCx<'_>) -> Vec<DialogRequest> {
        if self.finished() {
            return Vec::new();
        }
        let (search_request, add_request, delete_request, reload_request, seen_reload) = {
            let state = self.inner.borrow();
            (
                state.search_request,
                state.add_request,
                state.delete_request,
                state.reload_request,
                state.seen_reload,
            )
        };

        let mut requests = Vec::new();
        let reload = *cx.read(&reload_request);
        if reload != seen_reload {
            self.inner.borrow_mut().seen_reload = reload;
            requests.push(DialogRequest::Reload);
        }
        let search = cx.read(&search_request).clone();
        if let Some(query) = search {
            cx.write(&search_request, None);
            requests.push(DialogRequest::Search(query));
        }
        let add = cx.read(&add_request).clone();
        if let Some(username) = add {
            cx.write(&add_request, None);
            requests.push(DialogRequest::Add(username));
        }
        let delete = cx.read(&delete_request).clone();
        if let Some(x_user_id) = delete {
            cx.write(&delete_request, None);
            requests.push(DialogRequest::Delete(x_user_id));
        }

        requests
    }

    pub(super) fn answered_search(
        &mut self,
        cx: &mut WidgetCx<'_>,
        query: &SmolStr,
        answer: SearchAnswer,
    ) {
        if self.finished() {
            return;
        }
        let mut state = self.inner.borrow_mut();
        if state.typed_query(cx).as_deref() != Some(query.as_str()) {
            return;
        }
        state.adopt_answer(cx, query, answer);
    }

    pub(super) fn finished(&self) -> bool {
        let state = self.inner.borrow();
        state.released || state.closed.get()
    }

    pub(super) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        let (components, scope) = {
            let mut state = self.inner.borrow_mut();
            if state.released {
                return;
            }
            state.released = true;
            (state.take_components(), state.scope)
        };
        release_components(cx.app_mut(), components);
        let _ = cx.app_mut().release_scope(scope);
    }
}

#[derive(Clone)]
struct DialogKeys {
    dialog: SmolStr,
    alert: SmolStr,
    accounts: SmolStr,
    results: SmolStr,
    filter: SmolStr,
    query: SmolStr,
    add_account: SmolStr,
    refresh: SmolStr,
    back: SmolStr,
    search: SmolStr,
    close: SmolStr,
    retry: SmolStr,
    recover: SmolStr,
}

struct RowControl {
    slot: NodeId,
    language: LanguageCode,
    button: Button,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StaticButton {
    AddAccount,
    Refresh,
    Back,
    Search,
    Close,
    Retry,
    Recover,
}

struct DialogComponents {
    dialog: Option<Dialog>,
    alert: Option<Dialog>,
    filter: Option<InputNode>,
    query: Option<InputNode>,
    query_submit: Option<Action>,
    accounts: Option<List>,
    results: Option<List>,
    spinner: Option<Spinner>,
    add_account: Option<Button>,
    refresh: Option<Button>,
    back: Option<Button>,
    search: Option<Button>,
    close: Option<Button>,
    retry: Option<Button>,
    recover: Option<Button>,
    row_controls: Vec<Button>,
}

struct DialogState {
    window: WindowId,
    scope: SignalScope,
    theme: ThemeHandle,
    translate: Translate,
    language: Signal<LanguageCode>,
    keys: DialogKeys,
    built_language: Option<LanguageCode>,
    recover_built: Option<WidgetAction>,
    dialog: Option<Dialog>,
    alert: Option<Dialog>,
    filter: Option<InputNode>,
    query: Option<InputNode>,
    query_submit: Option<Action>,
    accounts: Option<List>,
    results: Option<List>,
    spinner: Option<Spinner>,
    spinner_parent: Option<NodeId>,
    add_account: Option<Button>,
    refresh: Option<Button>,
    back: Option<Button>,
    search: Option<Button>,
    close: Option<Button>,
    retry: Option<Button>,
    recover: Option<Button>,
    delete_buttons: BTreeMap<usize, RowControl>,
    monitor_buttons: BTreeMap<usize, RowControl>,
    account_slots: Rc<RefCell<BTreeMap<usize, NodeId>>>,
    result_slots: Rc<RefCell<BTreeMap<usize, NodeId>>>,
    add_account_parent: Option<NodeId>,
    refresh_parent: Option<NodeId>,
    back_parent: Option<NodeId>,
    search_parent: Option<NodeId>,
    close_parent: Option<NodeId>,
    retry_parent: Option<NodeId>,
    recover_parent: Option<NodeId>,
    account_rows_held: Rc<RefCell<Arc<[XSelection]>>>,
    result_rows_held: Rc<RefCell<Arc<[XProfile]>>>,
    hints_held: Rc<RefCell<Arc<HashMap<SmolStr, SmolStr>>>>,
    images: Rc<RefCell<HashMap<SmolStr, ImageSource>>>,
    images_source: Option<Rc<RefCell<HashMap<SmolStr, ImageSource>>>>,
    media: Rc<RefCell<MediaStates>>,
    verified: Option<ImageSource>,
    shown_indices: Vec<usize>,
    rows_moved: bool,
    account_shape: Vec<AccountRowShape>,
    account_language: Option<LanguageCode>,
    account_images: usize,
    shown_accounts: Option<usize>,
    result_rows: Option<Arc<[XProfile]>>,
    result_language: Option<LanguageCode>,
    result_images: usize,
    last_view: Option<DialogView>,
    answered_query: Option<SmolStr>,
    reported: Option<Option<SelectionNotice>>,
    signed_in: Signal<bool>,
    ready: Signal<bool>,
    known: Signal<bool>,
    count: Signal<usize>,
    limit: Signal<Option<usize>>,
    searching: Signal<bool>,
    operation: Signal<Option<SelectionOperation>>,
    page: Signal<DialogPage>,
    account_rows: Signal<Arc<[XSelection]>>,
    hints: Signal<Arc<HashMap<SmolStr, SmolStr>>>,
    shown_rows: Signal<usize>,
    filter_state: Signal<InputState>,
    filter_placeholder: Signal<SmolStr>,
    query_state: Signal<InputState>,
    query_placeholder: Signal<SmolStr>,
    result_rows_signal: Signal<Arc<[XProfile]>>,
    search_phase: Signal<SearchPhase>,
    search_error: Signal<Option<RpcCallError<XApiError>>>,
    search_request: Signal<Option<SmolStr>>,
    add_request: Signal<Option<SmolStr>>,
    delete_request: Signal<Option<SmolStr>>,
    reload_request: Signal<u64>,
    seen_reload: u64,
    released: bool,
    closed: Rc<Cell<bool>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SearchPhase {
    #[default]
    Idle,
    Found,
    Empty,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AccountRowShape {
    avatar_url: SmolStr,
    profiled: bool,
    status: XSelectionStatus,
    verified: bool,
    protected: bool,
}

impl DialogState {
    fn new(cx: &mut WidgetCx<'_>) -> Self {
        let scope = cx.app_mut().signal_scope();
        let language = cx.language();
        let theme = cx.theme().clone();
        let translate = cx.translate().clone();
        let keys = DialogKeys {
            dialog: cx.unique_key("x.dialog"),
            alert: cx.unique_key("x.dialog.alert"),
            accounts: cx.unique_key("x.dialog.accounts"),
            results: cx.unique_key("x.dialog.results"),
            filter: cx.unique_key("x.dialog.filter"),
            query: cx.unique_key("x.dialog.query"),
            add_account: cx.unique_key("x.dialog.add_account"),
            refresh: cx.unique_key("x.dialog.refresh"),
            back: cx.unique_key("x.dialog.back"),
            search: cx.unique_key("x.dialog.search"),
            close: cx.unique_key("x.dialog.close"),
            retry: cx.unique_key("x.dialog.retry"),
            recover: cx.unique_key("x.dialog.recover"),
        };
        let filter_placeholder = t_smol!(translate, "widget.x.filter_placeholder");
        let query_placeholder = t_smol!(translate, "widget.x.search_placeholder");
        let empty_rows: Arc<[XSelection]> = Arc::from([]);
        let empty_results: Arc<[XProfile]> = Arc::from([]);
        let empty_hints: Arc<HashMap<SmolStr, SmolStr>> = Arc::new(HashMap::default());
        Self {
            window: cx.window(),
            scope,
            theme,
            translate,
            language,
            keys,
            built_language: Some(*cx.read(&language)),
            recover_built: None,
            dialog: None,
            alert: None,
            filter: None,
            query: None,
            query_submit: None,
            accounts: None,
            results: None,
            spinner: None,
            spinner_parent: None,
            add_account: None,
            refresh: None,
            back: None,
            search: None,
            close: None,
            retry: None,
            recover: None,
            delete_buttons: BTreeMap::new(),
            monitor_buttons: BTreeMap::new(),
            account_slots: Rc::new(RefCell::new(BTreeMap::new())),
            result_slots: Rc::new(RefCell::new(BTreeMap::new())),
            add_account_parent: None,
            refresh_parent: None,
            back_parent: None,
            search_parent: None,
            close_parent: None,
            retry_parent: None,
            recover_parent: None,
            account_rows_held: Rc::new(RefCell::new(empty_rows)),
            result_rows_held: Rc::new(RefCell::new(empty_results)),
            hints_held: Rc::new(RefCell::new(empty_hints)),
            images: Rc::new(RefCell::new(HashMap::default())),
            images_source: None,
            media: Rc::new(RefCell::new(Arc::new(HashMap::default()))),
            verified: None,
            shown_indices: Vec::new(),
            rows_moved: false,
            account_shape: Vec::new(),
            account_language: None,
            account_images: 0,
            shown_accounts: None,
            result_rows: None,
            result_language: None,
            result_images: 0,
            last_view: None,
            answered_query: None,
            reported: None,
            signed_in: cx.app_mut().signal_scoped(scope, false),
            ready: cx.app_mut().signal_scoped(scope, false),
            known: cx.app_mut().signal_scoped(scope, false),
            count: cx.app_mut().signal_scoped(scope, 0_usize),
            limit: cx.app_mut().signal_scoped(scope, None),
            searching: cx.app_mut().signal_scoped(scope, false),
            operation: cx.app_mut().signal_scoped(scope, None),
            page: cx.app_mut().signal_scoped(scope, DialogPage::Monitored),
            account_rows: cx.app_mut().signal_scoped(scope, Arc::from([])),
            hints: cx
                .app_mut()
                .signal_scoped(scope, Arc::new(HashMap::default())),
            shown_rows: cx.app_mut().signal_scoped(scope, 0_usize),
            filter_state: cx.app_mut().signal_scoped(scope, InputState::new("")),
            filter_placeholder: cx.app_mut().signal_scoped(scope, filter_placeholder),
            query_state: cx.app_mut().signal_scoped(scope, InputState::new("")),
            query_placeholder: cx.app_mut().signal_scoped(scope, query_placeholder),
            result_rows_signal: cx.app_mut().signal_scoped(scope, Arc::from([])),
            search_phase: cx.app_mut().signal_scoped(scope, SearchPhase::Idle),
            search_error: cx.app_mut().signal_scoped(scope, None),
            search_request: cx.app_mut().signal_scoped(scope, None),
            add_request: cx.app_mut().signal_scoped(scope, None),
            delete_request: cx.app_mut().signal_scoped(scope, None),
            reload_request: cx.app_mut().signal_scoped(scope, 0_u64),
            seen_reload: 0,
            released: false,
            closed: Rc::new(Cell::new(false)),
        }
    }

    fn create_kit(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let verified = {
            let window = cx.window();
            let image = cx.app_mut().insert_svg_for_window(
                window,
                svg(IconKind::CheckCircle).as_bytes(),
                ICON_INTRINSIC_SIZE,
            )?;
            ImageSource::Ready(image.id())
        };
        self.verified = Some(verified);

        let accounts = AccountRows {
            key: self.keys.accounts.clone(),
            held: Rc::clone(&self.account_rows_held),
            rows: self.account_rows,
            hints: self.hints,
            language: self.language,
            theme: self.theme.clone(),
            translate: self.translate.clone(),
            media: Rc::clone(&self.media),
            images: Rc::clone(&self.images),
            verified,
            slots: Rc::clone(&self.account_slots),
        };
        let list = List::new(
            cx.app_mut(),
            self.theme.clone(),
            self.keys.accounts.clone(),
            ListOptions::fixed(ROW_HEIGHT)
                .selection(SelectionMode::None)
                .padding(px(0.0), px(0.0)),
            move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                build_account_row(tree, parent, index, &accounts)
            },
        );
        list.set_initial_count(cx.app_mut(), 0);
        self.accounts = Some(list);

        let results = ResultRows {
            key: self.keys.results.clone(),
            held: Rc::clone(&self.result_rows_held),
            rows: self.result_rows_signal,
            language: self.language,
            theme: self.theme.clone(),
            translate: self.translate.clone(),
            media: Rc::clone(&self.media),
            images: Rc::clone(&self.images),
            verified,
            slots: Rc::clone(&self.result_slots),
        };
        let list = List::new(
            cx.app_mut(),
            self.theme.clone(),
            self.keys.results.clone(),
            ListOptions::fixed(ROW_HEIGHT)
                .selection(SelectionMode::None)
                .padding(px(0.0), px(0.0)),
            move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                build_result_row(tree, parent, index, &results)
            },
        );
        list.set_initial_count(cx.app_mut(), 0);
        self.results = Some(list);
        Ok(())
    }

    fn build_dialog(inner: &Weak<RefCell<DialogState>>, cx: &mut WidgetCx<'_>) -> Dialog {
        let (theme, key, title, close_label, closed) = {
            let state = inner
                .upgrade()
                .expect("the dialog is built while its state is alive");
            let state = state.borrow();
            (
                state.theme.clone(),
                state.keys.dialog.clone(),
                t_smol!(state.translate, "widget.x.dialog_title"),
                t_smol!(state.translate, "common.close"),
                Rc::clone(&state.closed),
            )
        };
        let content = Weak::clone(inner);
        DialogBuilder::new(title)
            .width(DIALOG_WIDTH)
            .close_button(true)
            .close_label(close_label)
            .content(move |app, window, body| {
                let Some(state) = content.upgrade() else {
                    return Ok(());
                };
                state.borrow_mut().build_content(app, window, body)
            })
            .on_close(move |reason, _app| {
                if reason == DialogClose::Dismissed || reason == DialogClose::Cancelled {
                    closed.set(true);
                }
            })
            .build(cx.app_mut(), theme, key)
    }

    fn frame(&mut self, cx: &mut WidgetCx<'_>, view: &DialogView) -> Result<(), EngineError> {
        if self.released || self.closed.get() {
            return Ok(());
        }
        if let Some(input) = &self.filter {
            let _ = input.sync(cx.app_mut());
        }
        if let Some(input) = &self.query {
            let _ = input.sync(cx.app_mut());
        }
        self.rebuild_static_copy(cx)?;
        self.sync_recover(cx)?;
        self.publish(cx, view);
        self.follow_typed_query(cx);
        self.sync_lists(cx)?;
        self.sync_components(cx)?;
        self.report_notice(cx, view)?;
        self.sync_alert(cx)?;
        if !self
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.is_open(cx.app()))
        {
            self.closed.set(true);
        }
        Ok(())
    }

    fn publish(&mut self, cx: &mut WidgetCx<'_>, view: &DialogView) {
        let selections = view
            .selections
            .as_ref()
            .map(SelectionData::shared_selections)
            .unwrap_or_default();
        let limit = view.selections.as_ref().map(SelectionData::limit);
        let hints = Arc::clone(&view.hints);
        let _ = cx.write(&self.signed_in, view.signed_in);
        let _ = cx.write(&self.ready, view.ready);
        let _ = cx.write(&self.known, view.selections.is_some());
        let _ = cx.write(&self.count, selections.len());
        let _ = cx.write(&self.limit, limit);
        let _ = cx.write(&self.hints, Arc::clone(&hints));
        let _ = cx.write(&self.searching, view.searching.is_some());
        let _ = cx.write(&self.operation, view.operation);
        *self.hints_held.borrow_mut() = Arc::clone(&hints);

        let source = Rc::clone(&view.images);
        let changed = self
            .images_source
            .as_ref()
            .is_none_or(|held| !Rc::ptr_eq(held, &source));
        if changed || source.borrow().len() != self.images.borrow().len() {
            *self.images.borrow_mut() = source.borrow().clone();
        }
        self.images_source = Some(source);
        *self.media.borrow_mut() = Arc::clone(&view.media);

        let filter = self.filter_text(cx);
        let indices = filtered_selection_indices(&selections, &hints, filter.as_deref());
        let moved = indices != self.shown_indices;
        let rows: Arc<[XSelection]> = if moved {
            indices
                .iter()
                .filter_map(|index| selections.get(*index).cloned())
                .collect::<Vec<_>>()
                .into()
        } else {
            let unchanged = {
                let held = self.account_rows_held.borrow();
                held.len() == indices.len()
                    && indices
                        .iter()
                        .zip(held.iter())
                        .all(|(index, row)| selections.get(*index) == Some(row))
            };
            if unchanged {
                Arc::clone(&self.account_rows_held.borrow())
            } else {
                indices
                    .iter()
                    .filter_map(|index| selections.get(*index).cloned())
                    .collect::<Vec<_>>()
                    .into()
            }
        };
        if moved {
            self.rows_moved = true;
            self.shown_indices = indices;
        }
        let _ = cx.write(&self.shown_rows, rows.len());
        let _ = cx.write(&self.account_rows, Arc::clone(&rows));
        *self.account_rows_held.borrow_mut() = rows;
        self.last_view = Some(view.clone());
    }

    fn follow_typed_query(&mut self, cx: &mut WidgetCx<'_>) {
        if self.answered_query == self.typed_query(cx) {
            return;
        }
        self.answered_query = None;
        *self.result_rows_held.borrow_mut() = Arc::from([]);
        let _ = cx.write(&self.result_rows_signal, Arc::from([]));
        let _ = cx.write(&self.search_phase, SearchPhase::Idle);
        let _ = cx.write(&self.search_error, None);
    }

    fn adopt_answer(&mut self, cx: &mut WidgetCx<'_>, query: &SmolStr, answer: SearchAnswer) {
        let (phase, profiles, error) = match answer {
            SearchAnswer::Found(profiles) if profiles.is_empty() => {
                (SearchPhase::Empty, Arc::from([]), None)
            }
            SearchAnswer::Found(profiles) => (SearchPhase::Found, profiles, None),
            SearchAnswer::Failed(error) => (SearchPhase::Failed, Arc::from([]), Some(error)),
        };
        *self.result_rows_held.borrow_mut() = Arc::clone(&profiles);
        let _ = cx.write(&self.result_rows_signal, profiles);
        let _ = cx.write(&self.search_error, error);
        let _ = cx.write(&self.search_phase, phase);
        self.answered_query = Some(query.clone());
        let _ = cx.request_frame();
    }

    fn typed_query(&self, cx: &WidgetCx<'_>) -> Option<SmolStr> {
        typed_text(&self.query_state, cx)
    }

    fn filter_text(&self, cx: &WidgetCx<'_>) -> Option<SmolStr> {
        typed_text(&self.filter_state, cx)
    }

    fn may_change(&self) -> bool {
        self.last_view.as_ref().is_some_and(|view| {
            view.ready
                && view.selections.is_some()
                && view.operation.is_none()
                && view.searching.is_none()
        })
    }

    fn search_rows(&self) -> Option<SearchRows<'_>> {
        let view = self.last_view.as_ref()?;
        Some(SearchRows {
            monitored: view.selections.as_ref(),
            hints: view.hints.as_ref(),
            quota_full: view
                .selections
                .as_ref()
                .is_some_and(SelectionData::quota_full),
            enabled: self.may_change(),
        })
    }

    fn rebuild_static_copy(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        if self.built_language == Some(language) {
            return Ok(());
        }
        let filter_placeholder = t_smol!(self.translate, "widget.x.filter_placeholder");
        let query_placeholder = t_smol!(self.translate, "widget.x.search_placeholder");
        if let Some(input) = self.filter.as_ref() {
            input.update_label(cx.app_mut(), filter_placeholder.clone());
            input.update_placeholder(cx.app_mut(), filter_placeholder);
        }
        if let Some(input) = self.query.as_ref() {
            input.update_label(cx.app_mut(), query_placeholder.clone());
            input.update_placeholder(cx.app_mut(), query_placeholder);
        }
        if let Some(spinner) = self.spinner.take() {
            spinner.release(cx.app_mut());
        }
        for (parent, kind) in [
            (self.add_account_parent, StaticButton::AddAccount),
            (self.refresh_parent, StaticButton::Refresh),
            (self.back_parent, StaticButton::Back),
            (self.search_parent, StaticButton::Search),
            (self.retry_parent, StaticButton::Retry),
            (self.recover_parent, StaticButton::Recover),
            (self.close_parent, StaticButton::Close),
        ] {
            let Some(parent) = parent else {
                continue;
            };
            self.rebuild_button(cx, parent, kind)?;
        }
        self.built_language = Some(language);
        Ok(())
    }

    fn sync_recover(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let current = self.last_view.as_ref().map(|view| view.recovery.clone());
        if self.recover_built == current {
            return Ok(());
        }
        self.recover_built = current;
        let Some(parent) = self.recover_parent else {
            return Ok(());
        };
        self.rebuild_button(cx, parent, StaticButton::Recover)
    }

    fn rebuild_button(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        kind: StaticButton,
    ) -> Result<(), EngineError> {
        if cx.tree().get(parent).is_err() {
            return Ok(());
        }
        if let Some(button) = self.take_button(kind) {
            let window = self.window;
            remove_button_node(cx.app_mut(), window, &button)?;
            let _ = button.release(cx.app_mut());
        }
        let window = self.window;
        self.build_button(cx.app_mut(), window, parent, kind)
    }

    fn build_button(
        &mut self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        kind: StaticButton,
    ) -> Result<(), EngineError> {
        let theme = self.theme.clone();
        let translate = self.translate.clone();
        match kind {
            StaticButton::AddAccount => {
                let label = t_smol!(translate, "widget.x.add_account");
                let page = self.page;
                let known = self.known;
                let button = Button::new(app, theme, self.keys.add_account.clone())
                    .primary()
                    .small()
                    .icon(IconKind::Plus)
                    .a11y_label(label.clone())
                    .tooltip_text(label)
                    .on_click(move |ctx| {
                        if !*ctx.read(&known) {
                            return;
                        }
                        ctx.write(&page, DialogPage::Add);
                        let window = ctx.window();
                        let _ = ctx.app_mut().request_repaint(window);
                    });
                button.build(app, window, parent)?;
                self.add_account = Some(button);
            }
            StaticButton::Refresh => {
                let label = t_smol!(translate, "widget.x.refresh");
                let reload = self.reload_request;
                let ready = self.ready;
                let operation = self.operation;
                let searching = self.searching;
                let button = Button::new(app, theme, self.keys.refresh.clone())
                    .secondary()
                    .small()
                    .icon(IconKind::ArrowsClockwise)
                    .a11y_label(label.clone())
                    .tooltip_text(label)
                    .on_click(move |ctx| {
                        if !(*ctx.read(&ready)
                            && ctx.read(&operation).is_none()
                            && !*ctx.read(&searching))
                        {
                            return;
                        }
                        ctx.update(&reload, |requests| *requests += 1);
                        let window = ctx.window();
                        let _ = ctx.app_mut().request_repaint(window);
                    });
                button.build(app, window, parent)?;
                self.refresh = Some(button);
            }
            StaticButton::Back => {
                let label = t_smol!(translate, "widget.x.back");
                let page = self.page;
                let button = Button::new(app, theme, self.keys.back.clone())
                    .ghost()
                    .small()
                    .icon(IconKind::CaretLeft)
                    .a11y_label(label.clone())
                    .tooltip_text(label)
                    .on_click(move |ctx| {
                        ctx.write(&page, DialogPage::Monitored);
                        let window = ctx.window();
                        let _ = ctx.app_mut().request_repaint(window);
                    });
                button.build(app, window, parent)?;
                self.back = Some(button);
            }
            StaticButton::Search => {
                let label = t_smol!(translate, "widget.x.search_users");
                let search_input = self.query.clone();
                let ready = self.ready;
                let operation = self.operation;
                let searching = self.searching;
                let search_request = self.search_request;
                let button = Button::new(app, theme, self.keys.search.clone())
                    .ghost()
                    .small()
                    .icon(IconKind::MagnifyingGlass)
                    .a11y_label(label.clone())
                    .tooltip_text(label)
                    .on_click(move |ctx| {
                        let Some(input) = search_input.as_ref() else {
                            return;
                        };
                        let text = input.value(ctx.app_mut());
                        offer_search(
                            ctx,
                            text.as_str(),
                            &ready,
                            &operation,
                            &searching,
                            &search_request,
                        );
                    });
                button.build(app, window, parent)?;
                self.search = Some(button);
            }
            StaticButton::Close => {
                let label = t_smol!(translate, "common.close");
                let dialog = self
                    .dialog
                    .as_ref()
                    .cloned()
                    .expect("the dialog is built before its content");
                let button = Button::new(app, theme, self.keys.close.clone())
                    .secondary()
                    .medium()
                    .label(label)
                    .on_click(move |ctx| {
                        dialog.close(ctx.app_mut());
                        let window = ctx.window();
                        let _ = ctx.app_mut().request_repaint(window);
                    });
                button.build(app, window, parent)?;
                self.close = Some(button);
            }
            StaticButton::Retry => {
                let label = t_smol!(translate, "common.retry");
                let reload = self.reload_request;
                let button = Button::new(app, theme, self.keys.retry.clone())
                    .outline()
                    .small()
                    .label(label)
                    .on_click(move |ctx| {
                        ctx.update(&reload, |requests| *requests += 1);
                        let window = ctx.window();
                        let _ = ctx.app_mut().request_repaint(window);
                    });
                button.build(app, window, parent)?;
                self.retry = Some(button);
            }
            StaticButton::Recover => {
                let action = self
                    .last_view
                    .as_ref()
                    .map(|view| view.recovery.clone())
                    .unwrap_or_else(|| WidgetAction::new("common.retry", |_| {}));
                let label = action.label(&translate);
                let enabled = action.is_enabled();
                let button = Button::new(app, theme, self.keys.recover.clone())
                    .outline()
                    .small()
                    .label(label)
                    .disabled(!enabled)
                    .on_click(move |ctx| action.run(ctx.app_mut()));
                button.build(app, window, parent)?;
                self.recover = Some(button);
                self.recover_built = self.last_view.as_ref().map(|view| view.recovery.clone());
            }
        }
        Ok(())
    }

    fn take_button(&mut self, kind: StaticButton) -> Option<Button> {
        match kind {
            StaticButton::AddAccount => self.add_account.take(),
            StaticButton::Refresh => self.refresh.take(),
            StaticButton::Back => self.back.take(),
            StaticButton::Search => self.search.take(),
            StaticButton::Close => self.close.take(),
            StaticButton::Retry => self.retry.take(),
            StaticButton::Recover => self.recover.take(),
        }
    }

    fn sync_lists(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.sync_account_list(cx)?;
        self.sync_result_list(cx)?;
        Ok(())
    }

    fn sync_account_list(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(list) = self.accounts.clone() else {
            return Ok(());
        };
        let rows = Arc::clone(&self.account_rows_held.borrow());
        let shape = account_row_shape(&rows);
        let language = *cx.read(&self.language);
        let images = self.images.borrow().len();
        let moved = std::mem::replace(&mut self.rows_moved, false);
        let count = rows.len();
        let shown = self.shown_accounts;
        let rebuild = moved
            || shown != Some(count)
            || self.account_shape != shape
            || self.account_language != Some(language)
            || self.account_images != images;
        if !rebuild {
            return Ok(());
        }
        if moved || shown.is_none() {
            list.set_items(cx.app_mut(), count)?;
        } else {
            list.set_count(cx.app_mut(), count)?;
        }
        self.account_shape = shape;
        self.account_language = Some(language);
        self.account_images = images;
        self.shown_accounts = Some(count);
        let _ = cx.request_frame();
        Ok(())
    }

    fn sync_result_list(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(list) = self.results.clone() else {
            return Ok(());
        };
        let rows = Arc::clone(&self.result_rows_held.borrow());
        let language = *cx.read(&self.language);
        let images = self.images.borrow().len();
        let rebuilt = self
            .result_rows
            .as_ref()
            .is_none_or(|shown| !Arc::ptr_eq(shown, &rows));
        if !rebuilt && self.result_language == Some(language) && self.result_images == images {
            return Ok(());
        }
        list.set_count(cx.app_mut(), rows.len())?;
        self.result_rows = Some(rows);
        self.result_language = Some(language);
        self.result_images = images;
        let _ = cx.request_frame();
        Ok(())
    }

    fn sync_components(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if let Some(dialog) = self.dialog.as_ref() {
            dialog.sync(cx.app_mut()).map_err(dialog_engine_error)?;
        }
        if let Some(list) = self.accounts.as_ref() {
            list.sync(cx.app_mut())?;
        }
        if let Some(list) = self.results.as_ref() {
            list.sync(cx.app_mut())?;
        }
        if let Some(input) = self.filter.as_ref() {
            input.sync(cx.app_mut());
        }
        if let Some(input) = self.query.as_ref() {
            input.sync(cx.app_mut());
        }
        self.sync_spinner(cx)?;
        self.sync_row_controls(cx)?;
        self.sync_button_states(cx);
        Ok(())
    }

    fn sync_spinner(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if *cx.read(&self.searching) {
            if let Some(spinner) = self.spinner.as_ref() {
                spinner.sync(cx.app_mut())?;
            } else if let Some(parent) = self.spinner_parent {
                let key = format_smolstr!("{}:spinner", self.keys.dialog);
                let label = t_smol!(self.translate, "widget.x.searching_users");
                let spinner = Spinner::new(cx.app_mut(), self.theme.clone(), key).with_label(label);
                let window = self.window;
                spinner.build(cx.app_mut(), window, parent)?;
                self.spinner = Some(spinner);
            }
        } else if let Some(spinner) = self.spinner.take() {
            spinner.release(cx.app_mut());
        }
        Ok(())
    }

    fn sync_button_states(&mut self, cx: &mut WidgetCx<'_>) {
        let ready = *cx.read(&self.ready);
        let known = *cx.read(&self.known);
        let operation = *cx.read(&self.operation);
        let searching = *cx.read(&self.searching);
        let typed = self.typed_query(cx);
        let busy = operation.is_some() || searching;
        let searchable = ready
            && operation.is_none()
            && !searching
            && typed.as_deref().is_some_and(valid_search_query);
        let recovery_enabled = self
            .last_view
            .as_ref()
            .is_none_or(|view| view.recovery.is_enabled());
        let app = cx.app_mut();
        if let Some(button) = self.add_account.as_ref() {
            let _ = button.set_disabled(app, !known);
        }
        if let Some(button) = self.refresh.as_ref() {
            let _ = button.set_disabled(app, !ready || busy);
        }
        if let Some(button) = self.search.as_ref() {
            let _ = button.set_disabled(app, !searchable);
        }
        if let Some(button) = self.recover.as_ref() {
            let _ = button.set_disabled(app, !recovery_enabled);
        }
    }

    fn sync_row_controls(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        let may_change = self.may_change();
        let theme = self.theme.clone();
        let translate = self.translate.clone();
        let window = self.window;

        if let Some(list) = self.accounts.clone() {
            let mounted: BTreeMap<usize, NodeId> =
                list.mounted_rows(cx.app()).into_iter().collect();
            self.account_slots
                .borrow_mut()
                .retain(|index, _| mounted.contains_key(index));
            let stale: Vec<usize> = self
                .delete_buttons
                .keys()
                .copied()
                .filter(|index| !mounted.contains_key(index))
                .collect();
            for index in stale {
                if let Some(entry) = self.delete_buttons.remove(&index) {
                    remove_button_node(cx.app_mut(), window, &entry.button)?;
                    let _ = entry.button.release(cx.app_mut());
                }
            }

            let accounts = self.account_rows;
            let delete_request = self.delete_request;
            for (index, slot) in mounted {
                let replace = self
                    .delete_buttons
                    .get(&index)
                    .is_none_or(|entry| entry.slot != slot || entry.language != language);
                if !replace {
                    if let Some(entry) = self.delete_buttons.get(&index) {
                        let _ = entry.button.set_disabled(cx.app_mut(), !may_change);
                    }
                    continue;
                }
                if let Some(entry) = self.delete_buttons.remove(&index) {
                    remove_button_node(cx.app_mut(), window, &entry.button)?;
                    let _ = entry.button.release(cx.app_mut());
                }
                let name = {
                    let rows = self.account_rows_held.borrow();
                    let hints = self.hints_held.borrow();
                    rows.get(index)
                        .map_or_else(SmolStr::default, |row| account_name(row, &hints))
                };
                let label = t_smol!(translate, "widget.x.delete_account", account = name);
                let key = cx.unique_key(&format_smolstr!("x.dialog.delete.{index}"));
                let button = Button::new(cx.app_mut(), theme.clone(), key)
                    .ghost()
                    .small()
                    .icon(IconKind::Trash)
                    .a11y_label(label.clone())
                    .tooltip_text(label)
                    .disabled(!may_change)
                    .on_click(move |ctx| {
                        let x_user_id = ctx
                            .read(&accounts)
                            .get(index)
                            .map(|selection| selection.x_user_id.clone());
                        if let Some(x_user_id) = x_user_id {
                            ctx.write(&delete_request, Some(x_user_id));
                            let window = ctx.window();
                            let _ = ctx.app_mut().request_repaint(window);
                        }
                    });
                button.build(cx.app_mut(), window, slot)?;
                self.delete_buttons.insert(
                    index,
                    RowControl {
                        slot,
                        language,
                        button,
                    },
                );
            }
        }

        if let Some(list) = self.results.clone() {
            let mounted: BTreeMap<usize, NodeId> =
                list.mounted_rows(cx.app()).into_iter().collect();
            self.result_slots
                .borrow_mut()
                .retain(|index, _| mounted.contains_key(index));
            let stale: Vec<usize> = self
                .monitor_buttons
                .keys()
                .copied()
                .filter(|index| !mounted.contains_key(index))
                .collect();
            for index in stale {
                if let Some(entry) = self.monitor_buttons.remove(&index) {
                    remove_button_node(cx.app_mut(), window, &entry.button)?;
                    let _ = entry.button.release(cx.app_mut());
                }
            }

            let results = self.result_rows_signal;
            let add_request = self.add_request;
            for (index, slot) in mounted {
                let (username, action) = {
                    let profiles = self.result_rows_held.borrow();
                    let Some(profile) = profiles.get(index).cloned() else {
                        continue;
                    };
                    let action = self
                        .search_rows()
                        .map_or(SearchProfileAction::Add { disabled: true }, |rows| {
                            rows.action(profile.username.as_str())
                        });
                    (profile.username, action)
                };
                let replace = self
                    .monitor_buttons
                    .get(&index)
                    .is_none_or(|entry| entry.slot != slot || entry.language != language);
                if !replace {
                    if let Some(entry) = self.monitor_buttons.get(&index) {
                        let _ = entry.button.set_disabled(cx.app_mut(), action.disabled());
                    }
                    continue;
                }
                if let Some(entry) = self.monitor_buttons.remove(&index) {
                    remove_button_node(cx.app_mut(), window, &entry.button)?;
                    let _ = entry.button.release(cx.app_mut());
                }
                let label = match action {
                    SearchProfileAction::Selected => {
                        t_smol!(translate, "widget.x.already_monitored")
                    }
                    SearchProfileAction::Add { .. } => t_smol!(
                        translate,
                        "widget.x.add_search_result",
                        username = username.clone()
                    ),
                };
                let key = cx.unique_key(&format_smolstr!("x.dialog.add.{index}"));
                let button = Button::new(cx.app_mut(), theme.clone(), key)
                    .ghost()
                    .small()
                    .icon(action.icon())
                    .a11y_label(label.clone())
                    .tooltip_text(label)
                    .disabled(action.disabled())
                    .on_click(move |ctx| {
                        let username = ctx
                            .read(&results)
                            .get(index)
                            .map(|profile| profile.username.clone());
                        if let Some(username) = username {
                            ctx.write(&add_request, Some(username));
                            let window = ctx.window();
                            let _ = ctx.app_mut().request_repaint(window);
                        }
                    });
                button.build(cx.app_mut(), window, slot)?;
                self.monitor_buttons.insert(
                    index,
                    RowControl {
                        slot,
                        language,
                        button,
                    },
                );
            }
        }
        Ok(())
    }

    fn report_notice(
        &mut self,
        cx: &mut WidgetCx<'_>,
        view: &DialogView,
    ) -> Result<(), EngineError> {
        match &self.reported {
            None => {
                self.reported = Some(view.notice.clone());
                Ok(())
            }
            Some(reported) if *reported == view.notice => Ok(()),
            Some(_) => {
                self.reported = Some(view.notice.clone());
                match view.notice.as_ref() {
                    Some(notice) => self.raise_alert(cx, notice),
                    None => Ok(()),
                }
            }
        }
    }

    fn raise_alert(
        &mut self,
        cx: &mut WidgetCx<'_>,
        notice: &SelectionNotice,
    ) -> Result<(), EngineError> {
        let (title, message) = report_text(notice, &self.translate);
        let confirm = t_smol!(self.translate, "common.confirm");
        let alert_title = title.clone();
        let alert_message = message;
        let alert_confirm = confirm;
        let theme = self.theme.clone();
        let key = self.keys.alert.clone();
        let alert = DialogBuilder::alert(title, move |dialog, _theme| {
            let dialog = dialog.clone();
            AlertBuilder::new()
                .ok(alert_title.clone(), alert_message.clone())
                .ok_text(alert_confirm.clone())
                .ok_action(move |_, app| {
                    dialog.close(app);
                })
        })
        .build(cx.app_mut(), theme, key);
        let window = self.window;
        alert
            .open(cx.app_mut(), window)
            .map_err(dialog_engine_error)?;
        if let Some(previous) = self.alert.replace(alert) {
            previous.release(cx.app_mut());
        }
        Ok(())
    }

    fn sync_alert(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(alert) = self.alert.clone() else {
            return Ok(());
        };
        alert.sync(cx.app_mut()).map_err(dialog_engine_error)?;
        if !alert.is_open(cx.app()) {
            self.alert = None;
            alert.release(cx.app_mut());
        }
        Ok(())
    }

    fn build_content(
        &mut self,
        app: &mut App,
        window: WindowId,
        body: NodeId,
    ) -> Result<(), EngineError> {
        let key = self.keys.dialog.clone();
        let panel = build_node(app, window, body, |tree, body| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:panel"))
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .height(px(CONTENT_HEIGHT))
                        .flex_col()
                        .gap(px(8.0))
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .build(tree, body)
        })?;
        let pages = build_node(app, window, panel, |tree, panel| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:pages"))
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .flex_1()
                        .min_height_0()
                        .relative()
                        .build(),
                )
                .build(tree, panel)
        })?;
        self.build_monitored_page(app, window, pages)?;
        self.build_add_page(app, window, pages)?;
        self.build_footer(app, window, panel)?;
        Ok(())
    }

    fn build_monitored_page(
        &mut self,
        app: &mut App,
        window: WindowId,
        pages: NodeId,
    ) -> Result<(), EngineError> {
        let key = self.keys.dialog.clone();
        let theme = self.theme.clone();
        let translate = self.translate.clone();
        let page = self.page;
        let title_size = UnifiedSize::Medium.text_size();
        let text_size = UnifiedSize::Small.text_size();

        let root = build_node(app, window, pages, |tree, pages| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:monitored"))
                .style(page_style())
                .bind_style(visible_if(move |read| {
                    *read.read(&page) == DialogPage::Monitored
                }))
                .build(tree, pages)
        })?;

        let header = build_node(app, window, root, |tree, root| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:monitored:header"))
                .style(header_style())
                .build(tree, root)
        })?;
        self.heading_node(app, window, header, &key, title_size)?;
        let add_holder = build_node(app, window, header, |tree, header| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:monitored:add"))
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, header)
        })?;
        self.add_account_parent = Some(add_holder);
        self.build_button(app, window, add_holder, StaticButton::AddAccount)?;
        let refresh_holder = build_node(app, window, header, |tree, header| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:monitored:refresh"))
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, header)
        })?;
        self.refresh_parent = Some(refresh_holder);
        self.build_button(app, window, refresh_holder, StaticButton::Refresh)?;

        let toolbar = build_node(app, window, root, |tree, root| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:monitored:toolbar"))
                .style(toolbar_style())
                .build(tree, root)
        })?;
        let toolbar = build_node(app, window, toolbar, |tree, toolbar| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:monitored:toolbar-signed-in"))
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .min_width_0()
                        .build(),
                )
                .bind_style(visible_if({
                    let signed_in = self.signed_in;
                    move |read| *read.read(&signed_in)
                }))
                .build(tree, toolbar)
        })?;
        let filter_holder = build_node(app, window, toolbar, |tree, toolbar| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:monitored:filter"))
                .style(StyleBuilder::new().flex_1().min_width_0().build())
                .build(tree, toolbar)
        })?;
        let spec = Arc::new(InputSpec {
            name: self.keys.filter.clone(),
            label: Some(t_smol!(translate, "widget.x.filter_placeholder")),
            placeholder: t_smol!(translate, "widget.x.filter_placeholder"),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            visible_lines: None,
            size: ControlSize::Medium,
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
                &self.filter_state,
                &self.filter_placeholder,
            )
        };
        filter.install(app);
        self.filter = Some(filter);

        let area = build_node(app, window, root, |tree, root| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:monitored:area"))
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .flex_1()
                        .min_width_0()
                        .min_height_0()
                        .relative()
                        .build(),
                )
                .build(tree, root)
        })?;
        let frame = build_node(app, window, area, |tree, area| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:monitored:frame"))
                .style(
                    StyleBuilder::new()
                        .absolute()
                        .top(px(0.0))
                        .right(px(0.0))
                        .bottom(px(0.0))
                        .left(px(0.0))
                        .min_width_0()
                        .min_height_0()
                        .overflow_hidden()
                        .build(),
                )
                .bind_style(themed_background(theme.clone(), Role::SurfaceRaised))
                .build(tree, area)
        })?;
        let list_holder = build_node(app, window, frame, |tree, frame| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:monitored:list"))
                .style(
                    StyleBuilder::new()
                        .size_full()
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .bind_style(visible_if({
                    let signed_in = self.signed_in;
                    let known = self.known;
                    let shown = self.shown_rows;
                    move |read| {
                        *read.read(&signed_in) && *read.read(&known) && *read.read(&shown) > 0
                    }
                }))
                .build(tree, frame)
        })?;
        let Some(list) = self.accounts.as_ref() else {
            return Ok(());
        };
        list.mount(app, window, list_holder)?;

        self.build_monitored_messages(app, window, area, &key, text_size)?;
        Ok(())
    }

    fn build_monitored_messages(
        &mut self,
        app: &mut App,
        window: WindowId,
        area: NodeId,
        key: &str,
        text_size: LogicalPx,
    ) -> Result<(), EngineError> {
        let theme = self.theme.clone();
        let translate = self.translate.clone();
        let language = self.language;
        let signed_in = self.signed_in;
        let known = self.known;
        let count = self.count;
        let shown = self.shown_rows;

        let signed_out = message_block(app, window, area, format_smolstr!("{key}:signed-out"), {
            let signed_in = self.signed_in;
            move |read| !*read.read(&signed_in)
        })?;
        let sentence = translate.clone();
        let language_read = language;
        sentence_node(
            app,
            window,
            signed_out,
            format_smolstr!("{key}:signed-out:text"),
            text_size,
            theme.clone(),
            Role::FgMuted,
            move |read| {
                let _language = read.read(&language_read);
                t_smol!(sentence, "widget.x.logged_out")
            },
        )?;

        let loading = message_block(app, window, area, format_smolstr!("{key}:loading"), {
            let operation = self.operation;
            let known = self.known;
            let signed_in = self.signed_in;
            move |read| {
                *read.read(&signed_in)
                    && !*read.read(&known)
                    && read
                        .read(&operation)
                        .is_some_and(SelectionOperation::is_read)
            }
        })?;
        let sentence = translate.clone();
        let language_read = language;
        sentence_node(
            app,
            window,
            loading,
            format_smolstr!("{key}:loading:text"),
            text_size,
            theme.clone(),
            Role::FgMuted,
            move |read| {
                let _language = read.read(&language_read);
                t_smol!(sentence, "widget.x.loading_accounts")
            },
        )?;

        let unavailable =
            message_block(app, window, area, format_smolstr!("{key}:unavailable"), {
                let operation = self.operation;
                let known = self.known;
                let ready = self.ready;
                let signed_in = self.signed_in;
                move |read| {
                    *read.read(&signed_in)
                        && !*read.read(&known)
                        && !read
                            .read(&operation)
                            .is_some_and(SelectionOperation::is_read)
                        && !*read.read(&ready)
                }
            })?;
        let sentence = translate.clone();
        let language_read = language;
        sentence_node(
            app,
            window,
            unavailable,
            format_smolstr!("{key}:unavailable:text"),
            text_size,
            theme.clone(),
            Role::FgMuted,
            move |read| {
                let _language = read.read(&language_read);
                t_smol!(sentence, "widget.x.stream_unavailable")
            },
        )?;
        let recover_holder = build_node(app, window, unavailable, |tree, unavailable| {
            NodeBuilder::boxed()
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, unavailable)
        })?;
        self.recover_parent = Some(recover_holder);
        self.build_button(app, window, recover_holder, StaticButton::Recover)?;

        let failed = message_block(app, window, area, format_smolstr!("{key}:failed"), {
            let operation = self.operation;
            let known = self.known;
            let ready = self.ready;
            let signed_in = self.signed_in;
            move |read| {
                *read.read(&signed_in)
                    && !*read.read(&known)
                    && !read
                        .read(&operation)
                        .is_some_and(SelectionOperation::is_read)
                    && *read.read(&ready)
            }
        })?;
        let sentence = translate.clone();
        let language_read = language;
        sentence_node(
            app,
            window,
            failed,
            format_smolstr!("{key}:failed:text"),
            text_size,
            theme.clone(),
            Role::FgMuted,
            move |read| {
                let _language = read.read(&language_read);
                t_smol!(sentence, "widget.x.load_failed")
            },
        )?;
        let retry_holder = build_node(app, window, failed, |tree, failed| {
            NodeBuilder::boxed()
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, failed)
        })?;
        self.retry_parent = Some(retry_holder);
        self.build_button(app, window, retry_holder, StaticButton::Retry)?;

        let empty = message_block(app, window, area, format_smolstr!("{key}:empty"), {
            let signed_in = self.signed_in;
            let known = self.known;
            move |read| *read.read(&signed_in) && *read.read(&known) && *read.read(&count) == 0
        })?;
        let sentence = translate.clone();
        let language_read = language;
        sentence_node(
            app,
            window,
            empty,
            format_smolstr!("{key}:empty:text"),
            text_size,
            theme.clone(),
            Role::FgMuted,
            move |read| {
                let _language = read.read(&language_read);
                t_smol!(sentence, "widget.x.empty_accounts")
            },
        )?;

        let no_matches = message_block(
            app,
            window,
            area,
            format_smolstr!("{key}:no-matches"),
            move |read| {
                *read.read(&signed_in)
                    && *read.read(&known)
                    && *read.read(&count) > 0
                    && *read.read(&shown) == 0
            },
        )?;
        let sentence = translate;
        let language_read = language;
        sentence_node(
            app,
            window,
            no_matches,
            format_smolstr!("{key}:no-matches:text"),
            text_size,
            theme,
            Role::FgMuted,
            move |read| {
                let _language = read.read(&language_read);
                t_smol!(sentence, "common.no_matches")
            },
        )?;
        Ok(())
    }

    fn build_add_page(
        &mut self,
        app: &mut App,
        window: WindowId,
        pages: NodeId,
    ) -> Result<(), EngineError> {
        let key = self.keys.dialog.clone();
        let theme = self.theme.clone();
        let translate = self.translate.clone();
        let page = self.page;
        let title_size = UnifiedSize::Medium.text_size();
        let text_size = UnifiedSize::Small.text_size();

        let root = build_node(app, window, pages, |tree, pages| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:add"))
                .style(page_style())
                .bind_style(visible_if(move |read| *read.read(&page) == DialogPage::Add))
                .build(tree, pages)
        })?;

        let header = build_node(app, window, root, |tree, root| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:add:header"))
                .style(header_style())
                .build(tree, root)
        })?;
        let back_holder = build_node(app, window, header, |tree, header| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:add:back"))
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, header)
        })?;
        self.back_parent = Some(back_holder);
        self.build_button(app, window, back_holder, StaticButton::Back)?;
        self.heading_node(app, window, header, &key, title_size)?;

        let toolbar = build_node(app, window, root, |tree, root| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:add:toolbar"))
                .style(toolbar_style())
                .bind_style(visible_if({
                    let signed_in = self.signed_in;
                    move |read| *read.read(&signed_in)
                }))
                .build(tree, root)
        })?;
        let query_holder = build_node(app, window, toolbar, |tree, toolbar| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:add:query"))
                .style(StyleBuilder::new().flex_1().min_width_0().build())
                .build(tree, toolbar)
        })?;
        let submit = Action::Custom(format_smolstr!("{}.submit", self.keys.query));
        let submit_ready = self.ready;
        let submit_operation = self.operation;
        let submit_searching = self.searching;
        let submit_search_request = self.search_request;
        let submit_slot = Rc::new(RefCell::new(None::<InputNode>));
        {
            let slot = Rc::clone(&submit_slot);
            app.on_action(submit.clone(), move |ctx| {
                let Some(input) = slot.borrow().clone() else {
                    return;
                };
                let text = input.value(ctx.app_mut());
                offer_search(
                    ctx,
                    text.as_str(),
                    &submit_ready,
                    &submit_operation,
                    &submit_searching,
                    &submit_search_request,
                );
            });
        }
        self.query_submit = Some(submit.clone());
        let spec = Arc::new(InputSpec {
            name: self.keys.query.clone(),
            label: Some(t_smol!(translate, "widget.x.search_placeholder")),
            placeholder: t_smol!(translate, "widget.x.search_placeholder"),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            visible_lines: None,
            size: ControlSize::Medium,
            submit: Some(submit),
        });
        let query = {
            let tree = app
                .window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut();
            input::build(
                tree,
                query_holder,
                spec,
                &theme,
                &self.query_state,
                &self.query_placeholder,
            )
        };
        query.install(app);
        *submit_slot.borrow_mut() = Some(query.clone());
        self.query = Some(query);
        let search_holder = build_node(app, window, toolbar, |tree, toolbar| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:add:search"))
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, toolbar)
        })?;
        self.search_parent = Some(search_holder);
        self.build_button(app, window, search_holder, StaticButton::Search)?;

        let area = build_node(app, window, root, |tree, root| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:add:area"))
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .flex_1()
                        .min_width_0()
                        .min_height_0()
                        .relative()
                        .build(),
                )
                .build(tree, root)
        })?;

        let frame = build_node(app, window, area, |tree, area| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:add:frame"))
                .style(
                    StyleBuilder::new()
                        .absolute()
                        .top(px(0.0))
                        .right(px(0.0))
                        .bottom(px(0.0))
                        .left(px(0.0))
                        .min_width_0()
                        .min_height_0()
                        .overflow_hidden()
                        .build(),
                )
                .bind_style(themed_background(theme.clone(), Role::SurfaceRaised))
                .build(tree, area)
        })?;
        let list_holder = build_node(app, window, frame, |tree, frame| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:add:list"))
                .style(
                    StyleBuilder::new()
                        .size_full()
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .bind_style(visible_if({
                    let signing = self.signed_in;
                    let searching = self.searching;
                    let phase = self.search_phase;
                    move |read| {
                        *read.read(&signing)
                            && !*read.read(&searching)
                            && *read.read(&phase) == SearchPhase::Found
                    }
                }))
                .build(tree, frame)
        })?;
        let Some(list) = self.results.as_ref() else {
            return Ok(());
        };
        list.mount(app, window, list_holder)?;

        self.build_add_messages(app, window, area, &key, text_size)?;
        Ok(())
    }

    fn build_add_messages(
        &mut self,
        app: &mut App,
        window: WindowId,
        area: NodeId,
        key: &str,
        text_size: LogicalPx,
    ) -> Result<(), EngineError> {
        let theme = self.theme.clone();
        let translate = self.translate.clone();
        let language = self.language;

        let signed_out = message_block(
            app,
            window,
            area,
            format_smolstr!("{key}:add:signed-out"),
            {
                let signed_in = self.signed_in;
                move |read| !*read.read(&signed_in)
            },
        )?;
        let sentence = translate.clone();
        let language_read = language;
        sentence_node(
            app,
            window,
            signed_out,
            format_smolstr!("{key}:add:signed-out:text"),
            text_size,
            theme.clone(),
            Role::FgMuted,
            move |read| {
                let _language = read.read(&language_read);
                t_smol!(sentence, "widget.x.logged_out")
            },
        )?;

        let searching =
            message_block(app, window, area, format_smolstr!("{key}:add:searching"), {
                let searching = self.searching;
                let signed_in = self.signed_in;
                move |read| *read.read(&signed_in) && *read.read(&searching)
            })?;
        let spinner_holder = build_node(app, window, searching, |tree, searching| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:add:spinner"))
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, searching)
        })?;
        self.spinner_parent = Some(spinner_holder);
        let sentence = translate.clone();
        let language_read = language;
        sentence_node(
            app,
            window,
            searching,
            format_smolstr!("{key}:add:searching:text"),
            text_size,
            theme.clone(),
            Role::FgMuted,
            move |read| {
                let _language = read.read(&language_read);
                t_smol!(sentence, "widget.x.searching_users")
            },
        )?;

        let failed = message_block(app, window, area, format_smolstr!("{key}:add:failed"), {
            let searching = self.searching;
            let phase = self.search_phase;
            let signed_in = self.signed_in;
            move |read| {
                *read.read(&signed_in)
                    && !*read.read(&searching)
                    && *read.read(&phase) == SearchPhase::Failed
            }
        })?;
        let error = self.search_error;
        let sentence = translate.clone();
        let language_read = language;
        sentence_node(
            app,
            window,
            failed,
            format_smolstr!("{key}:add:failed:text"),
            text_size,
            theme.clone(),
            Role::StatusDangerBase,
            move |read| {
                let _language = read.read(&language_read);
                let failure = read.read(&error);
                match &*failure {
                    Some(error) => error.translated_message(&sentence),
                    None => SmolStr::default(),
                }
            },
        )?;

        let no_results = message_block(app, window, area, format_smolstr!("{key}:add:empty"), {
            let searching = self.searching;
            let phase = self.search_phase;
            let signed_in = self.signed_in;
            move |read| {
                *read.read(&signed_in)
                    && !*read.read(&searching)
                    && *read.read(&phase) == SearchPhase::Empty
            }
        })?;
        let sentence = translate;
        let language_read = language;
        sentence_node(
            app,
            window,
            no_results,
            format_smolstr!("{key}:add:empty:text"),
            text_size,
            theme,
            Role::FgMuted,
            move |read| {
                let _language = read.read(&language_read);
                t_smol!(sentence, "widget.x.no_search_results")
            },
        )?;
        Ok(())
    }

    fn heading_node(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        key: &str,
        title_size: LogicalPx,
    ) -> Result<NodeId, EngineError> {
        let count = self.count;
        let limit = self.limit;
        let known = self.known;
        let language = self.language;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        build_node(app, window, parent, |tree, parent| {
            NodeBuilder::text(
                SmolStr::default(),
                TextStyle::new(title_size).with_overflow(TextOverflow::Ellipsis, 1),
            )
            .id(format_smolstr!("{key}:heading"))
            .style(StyleBuilder::new().flex_1().min_width_0().build())
            .bind_style(themed_foreground(theme, Role::FgDefault))
            .bind_text(move |read| {
                let _language = read.read(&language);
                match (*read.read(&known), *read.read(&limit)) {
                    (true, Some(limit)) => t_smol!(
                        translate,
                        "widget.x.monitored_accounts",
                        current = *read.read(&count),
                        limit = limit
                    ),
                    _ => t_smol!(translate, "widget.x.dialog_title"),
                }
            })
            .build(tree, parent)
        })
    }

    fn build_footer(
        &mut self,
        app: &mut App,
        window: WindowId,
        pages: NodeId,
    ) -> Result<(), EngineError> {
        let key = self.keys.dialog.clone();
        let theme = self.theme.clone();
        let footer = build_node(app, window, pages, |tree, pages| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:footer"))
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .flex_none()
                        .flex_col()
                        .gap(px(8.0))
                        .build(),
                )
                .build(tree, pages)
        })?;
        build_node(app, window, footer, |tree, footer| {
            Separator::horizontal(theme).build(tree, footer)
        })?;
        let close_holder = build_node(app, window, footer, |tree, footer| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:footer:close"))
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .build(),
                )
                .build(tree, footer)
        })?;
        self.close_parent = Some(close_holder);
        self.build_button(app, window, close_holder, StaticButton::Close)
    }

    fn take_components(&mut self) -> DialogComponents {
        let mut row_controls: Vec<Button> = std::mem::take(&mut self.delete_buttons)
            .into_values()
            .map(|entry| entry.button)
            .collect();
        row_controls.extend(
            std::mem::take(&mut self.monitor_buttons)
                .into_values()
                .map(|entry| entry.button),
        );
        DialogComponents {
            dialog: self.dialog.take(),
            alert: self.alert.take(),
            filter: self.filter.take(),
            query: self.query.take(),
            query_submit: self.query_submit.take(),
            accounts: self.accounts.take(),
            results: self.results.take(),
            spinner: self.spinner.take(),
            add_account: self.add_account.take(),
            refresh: self.refresh.take(),
            back: self.back.take(),
            search: self.search.take(),
            close: self.close.take(),
            retry: self.retry.take(),
            recover: self.recover.take(),
            row_controls,
        }
    }
}

fn page_style() -> ui_core::paint::style::Style {
    StyleBuilder::new()
        .absolute()
        .top(px(0.0))
        .right(px(0.0))
        .bottom(px(0.0))
        .left(px(0.0))
        .flex_col()
        .gap(px(8.0))
        .min_width_0()
        .min_height_0()
        .build()
}

fn header_style() -> ui_core::paint::style::Style {
    StyleBuilder::new()
        .width_full()
        .flex_none()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .min_width_0()
        .build()
}

fn toolbar_style() -> ui_core::paint::style::Style {
    StyleBuilder::new()
        .width_full()
        .flex_none()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .min_width_0()
        .build()
}

fn typed_text(signal: &Signal<InputState>, cx: &WidgetCx<'_>) -> Option<SmolStr> {
    let state = cx.read(signal);
    let typed = state.text().trim();
    (!typed.is_empty()).then(|| SmolStr::new(typed))
}

fn searchable_now(
    ctx: &ActionCtx<'_>,
    typed_text: &str,
    ready: &Signal<bool>,
    operation: &Signal<Option<SelectionOperation>>,
    searching: &Signal<bool>,
) -> Option<SmolStr> {
    let typed = typed_text.trim();
    if !valid_search_query(typed) {
        return None;
    }
    if !(*ctx.read(ready) && ctx.read(operation).is_none() && !*ctx.read(searching)) {
        return None;
    }
    Some(SmolStr::new(typed))
}

fn offer_search(
    ctx: &mut ActionCtx<'_>,
    typed_text: &str,
    ready: &Signal<bool>,
    operation: &Signal<Option<SelectionOperation>>,
    searching: &Signal<bool>,
    search_request: &Signal<Option<SmolStr>>,
) {
    let Some(query) = searchable_now(ctx, typed_text, ready, operation, searching) else {
        return;
    };
    ctx.write(search_request, Some(query));
    let window = ctx.window();
    let _ = ctx.app_mut().request_repaint(window);
}

fn report_text(notice: &SelectionNotice, translate: &Translate) -> (SmolStr, SmolStr) {
    match notice {
        SelectionNotice::Added => (
            t_smol!(translate, "widget.x.success_title"),
            t_smol!(translate, "widget.x.add_success"),
        ),
        SelectionNotice::Problem(failure) => (
            t_smol!(translate, "widget.x.error_title"),
            failure.translated_message(translate),
        ),
    }
}

struct SearchRows<'a> {
    monitored: Option<&'a SelectionData>,
    hints: &'a HashMap<SmolStr, SmolStr>,
    quota_full: bool,
    enabled: bool,
}

impl SearchRows<'_> {
    fn action(&self, username: &str) -> SearchProfileAction {
        let monitored = self
            .monitored
            .is_some_and(|data| data.contains_username(username, self.hints));
        SearchProfileAction::new(monitored, !self.enabled || self.quota_full)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchProfileAction {
    Add { disabled: bool },
    Selected,
}

impl SearchProfileAction {
    const fn new(selected: bool, disabled: bool) -> Self {
        if selected {
            Self::Selected
        } else {
            Self::Add { disabled }
        }
    }

    const fn disabled(self) -> bool {
        match self {
            Self::Add { disabled } => disabled,
            Self::Selected => true,
        }
    }

    const fn icon(self) -> IconKind {
        match self {
            Self::Add { .. } => IconKind::Plus,
            Self::Selected => IconKind::Check,
        }
    }
}

fn filtered_selection_indices(
    selections: &[XSelection],
    hints: &HashMap<SmolStr, SmolStr>,
    filter: Option<&str>,
) -> Vec<usize> {
    let Some(filter) = filter.map(str::trim).filter(|filter| !filter.is_empty()) else {
        return (0..selections.len()).collect();
    };
    let filter = filter.to_lowercase();
    let filter = filter.strip_prefix('@').unwrap_or(&filter);

    selections
        .iter()
        .enumerate()
        .filter(|(_, selection)| {
            let mut fields = vec![selection.x_user_id.as_str()];
            if let Some(profile) = selection.profile.as_ref() {
                fields.push(profile.username.as_str());
                fields.push(profile.display_name.as_str());
            }
            if let Some(name) = selection_name(selection, hints) {
                fields.push(name);
            }
            fields
                .into_iter()
                .any(|field| field.to_lowercase().contains(filter))
        })
        .map(|(index, _)| index)
        .collect()
}

fn account_name(selection: &XSelection, hints: &HashMap<SmolStr, SmolStr>) -> SmolStr {
    match selection_name(selection, hints) {
        Some(name) => format_smolstr!("@{name}"),
        None => selection.x_user_id.clone(),
    }
}

fn format_follower_count(count: u64) -> SmolStr {
    const UNITS: [(u64, &str); 3] = [(1_000, "K"), (1_000_000, "M"), (1_000_000_000, "B")];
    let mut formatted: Option<SmolStr> = None;
    for (index, (scale, suffix)) in UNITS.iter().enumerate() {
        if count < *scale {
            break;
        }
        let tenths = (count % scale) * 10 + scale / 2;
        if tenths >= scale * 10 {
            formatted = match UNITS.get(index + 1) {
                Some((next_scale, next_suffix)) => Some(format_smolstr!(
                    "{}{next_suffix}",
                    (count + next_scale / 2) / next_scale
                )),
                None => Some(format_smolstr!("{}{suffix}", count / scale + 1)),
            };
            continue;
        }
        let whole = count / scale;
        let tenths = tenths / scale;
        formatted = Some(if tenths == 0 {
            format_smolstr!("{whole}{suffix}")
        } else {
            format_smolstr!("{whole}.{tenths}{suffix}")
        });
    }
    formatted.unwrap_or_else(|| format_smolstr!("{count}"))
}

struct AccountRows {
    key: SmolStr,
    held: Rc<RefCell<Arc<[XSelection]>>>,
    rows: Signal<Arc<[XSelection]>>,
    hints: Signal<Arc<HashMap<SmolStr, SmolStr>>>,
    language: Signal<LanguageCode>,
    theme: ThemeHandle,
    translate: Translate,
    media: Rc<RefCell<MediaStates>>,
    images: Rc<RefCell<HashMap<SmolStr, ImageSource>>>,
    verified: ImageSource,
    slots: Rc<RefCell<BTreeMap<usize, NodeId>>>,
}

struct ResultRows {
    key: SmolStr,
    held: Rc<RefCell<Arc<[XProfile]>>>,
    rows: Signal<Arc<[XProfile]>>,
    language: Signal<LanguageCode>,
    theme: ThemeHandle,
    translate: Translate,
    media: Rc<RefCell<MediaStates>>,
    images: Rc<RefCell<HashMap<SmolStr, ImageSource>>>,
    verified: ImageSource,
    slots: Rc<RefCell<BTreeMap<usize, NodeId>>>,
}

fn account_row_shape(rows: &[XSelection]) -> Vec<AccountRowShape> {
    rows.iter()
        .map(|selection| {
            let profile = selection.profile.as_ref();
            AccountRowShape {
                avatar_url: profile
                    .map_or_else(SmolStr::default, |profile| profile.avatar_url.clone()),
                profiled: profile.is_some(),
                status: selection.status,
                verified: profile.is_some_and(|profile| profile.verified),
                protected: profile.is_some_and(|profile| profile.protected),
            }
        })
        .collect()
}

fn build_account_row(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    rows: &AccountRows,
) -> Result<(), EngineError> {
    let key = &rows.key;
    let selection = rows.held.borrow().get(index).cloned();
    let Some(selection) = selection else {
        return Ok(());
    };
    let row = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:row:{index}"))
        .style(row_style())
        .build(tree, parent)?;

    let avatar_url = selection
        .profile
        .as_ref()
        .map_or_else(SmolStr::default, |profile| profile.avatar_url.clone());
    build_avatar(
        tree,
        row,
        format_smolstr!("{key}:row:{index}:face"),
        &avatar_url,
        &rows.media.borrow(),
        &rows.images.borrow(),
        &rows.theme,
    )?;

    let column = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:row:{index}:lines"))
        .style(
            StyleBuilder::new()
                .flex_1()
                .min_width_0()
                .flex_col()
                .gap(px(4.0))
                .build(),
        )
        .build(tree, row)?;
    match selection.profile.as_ref() {
        Some(profile) => {
            let names = rows.rows;
            let handles = rows.rows;
            profile_line(
                tree,
                column,
                &rows.key,
                index,
                profile,
                rows.verified,
                &rows.theme,
                &rows.translate,
                rows.language,
                move |read| {
                    read.read(&names)
                        .get(index)
                        .and_then(|selection| selection.profile.as_ref())
                        .map_or_else(SmolStr::default, |profile| profile.display_name.clone())
                },
                move |read| {
                    read.read(&handles)
                        .get(index)
                        .and_then(|selection| selection.profile.as_ref())
                        .map_or_else(SmolStr::default, |profile| {
                            format_smolstr!("@{}", profile.username.as_str())
                        })
                },
            )?;
            build_description(
                tree,
                column,
                format_smolstr!("{key}:row:{index}:description"),
                rows.theme.clone(),
                {
                    let rows = rows.rows;
                    move |read| {
                        read.read(&rows)
                            .get(index)
                            .and_then(|selection| selection.profile.as_ref())
                            .map_or_else(SmolStr::default, |profile| profile.description.clone())
                    }
                },
            )?;
        }
        None => {
            let rows_signal = rows.rows;
            let hints = rows.hints;
            let theme = rows.theme.clone();
            NodeBuilder::text(
                SmolStr::default(),
                TextStyle::new(UnifiedSize::Medium.text_size())
                    .with_weight(600)
                    .with_overflow(TextOverflow::Ellipsis, 1),
            )
            .id(format_smolstr!("{key}:row:{index}:pending-name"))
            .style(StyleBuilder::new().min_width_0().build())
            .bind_style(themed_foreground(theme.clone(), Role::FgDefault))
            .bind_text(move |read| {
                read.read(&rows_signal)
                    .get(index)
                    .map_or_else(SmolStr::default, |selection| {
                        account_name(selection, &read.read(&hints))
                    })
            })
            .build(tree, column)?;
            let rows_signal = rows.rows;
            let translate = rows.translate.clone();
            let language = rows.language;
            NodeBuilder::text(
                SmolStr::default(),
                TextStyle::new(UnifiedSize::Small.text_size())
                    .with_overflow(TextOverflow::Ellipsis, 1),
            )
            .id(format_smolstr!("{key}:row:{index}:pending-status"))
            .style(StyleBuilder::new().min_width_0().build())
            .bind_style(themed_foreground(theme.clone(), Role::FgMuted))
            .bind_text(move |read| {
                let _language = read.read(&language);
                read.read(&rows_signal)
                    .get(index)
                    .map_or_else(SmolStr::default, |selection| {
                        status_label(selection.status, &translate)
                    })
            })
            .build(tree, column)?;
            let rows_signal = rows.rows;
            let translate = rows.translate.clone();
            let language = rows.language;
            NodeBuilder::text(
                SmolStr::default(),
                TextStyle::new(UnifiedSize::Small.text_size())
                    .with_overflow(TextOverflow::Ellipsis, 1),
            )
            .id(format_smolstr!("{key}:row:{index}:id"))
            .style(StyleBuilder::new().min_width_0().build())
            .bind_style(themed_foreground(theme, Role::FgMuted))
            .bind_text(move |read| {
                let _language = read.read(&language);
                read.read(&rows_signal)
                    .get(index)
                    .map_or_else(SmolStr::default, |selection| {
                        t_smol!(
                            translate,
                            "widget.x.account_id",
                            id = selection.x_user_id.clone()
                        )
                    })
            })
            .build(tree, column)?;
        }
    }

    build_status_badge(tree, row, index, rows)?;
    build_row_slot(
        tree,
        row,
        index,
        &rows.key,
        &rows.slots,
        UnifiedSize::Small.h(),
    )?;
    Ok(())
}

fn build_result_row(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    rows: &ResultRows,
) -> Result<(), EngineError> {
    let key = &rows.key;
    let profile = rows.held.borrow().get(index).cloned();
    let Some(profile) = profile else {
        return Ok(());
    };
    let row = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:row:{index}"))
        .style(row_style())
        .build(tree, parent)?;

    build_avatar(
        tree,
        row,
        format_smolstr!("{key}:row:{index}:face"),
        &profile.avatar_url,
        &rows.media.borrow(),
        &rows.images.borrow(),
        &rows.theme,
    )?;

    let column = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:row:{index}:lines"))
        .style(
            StyleBuilder::new()
                .flex_1()
                .min_width_0()
                .flex_col()
                .gap(px(4.0))
                .build(),
        )
        .build(tree, row)?;
    let names = rows.rows;
    let handles = rows.rows;
    profile_line(
        tree,
        column,
        &rows.key,
        index,
        &profile,
        rows.verified,
        &rows.theme,
        &rows.translate,
        rows.language,
        move |read| {
            read.read(&names)
                .get(index)
                .map_or_else(SmolStr::default, |profile| profile.display_name.clone())
        },
        move |read| {
            read.read(&handles)
                .get(index)
                .map_or_else(SmolStr::default, |profile| {
                    format_smolstr!("@{}", profile.username.as_str())
                })
        },
    )?;
    if !profile.description.is_empty() {
        build_description(
            tree,
            column,
            format_smolstr!("{key}:row:{index}:description"),
            rows.theme.clone(),
            {
                let rows = rows.rows;
                move |read| {
                    read.read(&rows)
                        .get(index)
                        .map_or_else(SmolStr::default, |profile| profile.description.clone())
                }
            },
        )?;
    }
    if profile.followers_count > 0 || profile.following_count > 0 {
        let rows_signal = rows.rows;
        let translate = rows.translate.clone();
        let language = rows.language;
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()).with_overflow(TextOverflow::Ellipsis, 1),
        )
        .id(format_smolstr!("{key}:row:{index}:followers"))
        .style(StyleBuilder::new().min_width_0().build())
        .bind_style(themed_foreground(rows.theme.clone(), Role::FgMuted))
        .bind_text(move |read| {
            let _language = read.read(&language);
            let profiles = read.read(&rows_signal);
            let (followers, following) = profiles.get(index).map_or((0, 0), |profile| {
                (profile.followers_count, profile.following_count)
            });
            if followers == 0 && following == 0 {
                return SmolStr::default();
            }
            let formatted = format_follower_count(followers);
            if followers == 1 {
                t_smol!(
                    translate,
                    "widget.x.followers_following_one_follower",
                    followers = formatted,
                    following = following
                )
            } else {
                t_smol!(
                    translate,
                    "widget.x.followers_following",
                    followers = formatted,
                    following = following
                )
            }
        })
        .build(tree, column)?;
    }
    build_row_slot(
        tree,
        row,
        index,
        &rows.key,
        &rows.slots,
        UnifiedSize::Small.h(),
    )?;
    Ok(())
}

fn row_style() -> ui_core::paint::style::Style {
    StyleBuilder::new()
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .width_full()
        .height_full()
        .min_width_0()
        .padding_x(px(8.0))
        .build()
}

fn profile_line(
    tree: &mut NodeTree,
    parent: NodeId,
    key: &SmolStr,
    index: usize,
    profile: &XProfile,
    verified: ImageSource,
    theme: &ThemeHandle,
    translate: &Translate,
    language: Signal<LanguageCode>,
    name: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static,
    handle: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static,
) -> Result<(), EngineError> {
    let line = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:row:{index}:name-line"))
        .style(
            StyleBuilder::new()
                .width_full()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .min_width_0()
                .build(),
        )
        .build(tree, parent)?;

    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(UnifiedSize::Medium.text_size())
            .with_weight(600)
            .with_overflow(TextOverflow::Ellipsis, 1),
    )
    .id(format_smolstr!("{key}:row:{index}:name"))
    .style(StyleBuilder::new().min_width_0().build())
    .bind_style(themed_foreground(theme.clone(), Role::FgDefault))
    .bind_text(name)
    .build(tree, line)?;

    if profile.verified {
        build_icon(
            tree,
            line,
            verified,
            VERIFIED_ICON_SIZE,
            theme.clone(),
            Role::BrandBase,
        )?;
    }
    if profile.protected {
        let protected_theme = theme.clone();
        let protected_translate = translate.clone();
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()).with_overflow(TextOverflow::Ellipsis, 1),
        )
        .id(format_smolstr!("{key}:row:{index}:protected"))
        .style(StyleBuilder::new().flex_none().build())
        .bind_style(themed_foreground(protected_theme, Role::FgMuted))
        .bind_text(move |read| {
            let _language = read.read(&language);
            t_smol!(protected_translate, "widget.x.protected")
        })
        .build(tree, line)?;
    }

    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(UnifiedSize::Small.text_size()).with_overflow(TextOverflow::Ellipsis, 1),
    )
    .id(format_smolstr!("{key}:row:{index}:handle"))
    .style(StyleBuilder::new().min_width_0().flex_1().build())
    .bind_style(themed_foreground(theme.clone(), Role::FgMuted))
    .bind_text(handle)
    .build(tree, line)?;
    Ok(())
}

fn build_description(
    tree: &mut NodeTree,
    parent: NodeId,
    id: SmolStr,
    theme: ThemeHandle,
    text: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static,
) -> Result<(), EngineError> {
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(UnifiedSize::Small.text_size()).with_overflow(TextOverflow::Ellipsis, 1),
    )
    .id(id)
    .style(StyleBuilder::new().min_width_0().build())
    .bind_style(themed_foreground(theme, Role::FgMuted))
    .bind_text(text)
    .build(tree, parent)?;
    Ok(())
}

fn build_status_badge(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    rows: &AccountRows,
) -> Result<(), EngineError> {
    let status = rows
        .held
        .borrow()
        .get(index)
        .map_or(XSelectionStatus::Active, |selection| selection.status);
    let label = status_label(status, &rows.translate);
    Badge::new(rows.theme.clone())
        .small()
        .text(label)
        .build(tree, parent)?;
    Ok(())
}

fn build_row_slot(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    key: &str,
    slots: &Rc<RefCell<BTreeMap<usize, NodeId>>>,
    size: LogicalPx,
) -> Result<(), EngineError> {
    let slot = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:row:{index}:actions"))
        .style(
            StyleBuilder::new()
                .width(size)
                .height_full()
                .flex_none()
                .items_center()
                .justify_center()
                .build(),
        )
        .build(tree, parent)?;
    slots.borrow_mut().insert(index, slot);
    Ok(())
}

fn build_avatar(
    tree: &mut NodeTree,
    parent: NodeId,
    id: SmolStr,
    url: &SmolStr,
    media: &MediaStates,
    images: &HashMap<SmolStr, ImageSource>,
    theme: &ThemeHandle,
) -> Result<(), EngineError> {
    let style = StyleBuilder::new()
        .width(px(ROW_AVATAR_SIZE))
        .height(px(ROW_AVATAR_SIZE))
        .flex_none()
        .build();
    let ready = !url.is_empty()
        && matches!(media.get(url), Some(MediaState::Ready(_)))
        && images.contains_key(url);
    let image = ready.then(|| images.get(url).copied()).flatten();
    let Some(image) = image else {
        let placeholder = NodeBuilder::boxed()
            .id(id)
            .style(style)
            .bind_style(themed_background(theme.clone(), Role::SurfaceRaised))
            .a11y(A11yOverride {
                hidden: true,
                ..A11yOverride::default()
            })
            .build(tree, parent)?;
        let mut text_style = TextStyle::new(UnifiedSize::Small.text_size());
        text_style.alignment = TextAlign::Center;
        text_style.color = Color::WHITE;
        NodeBuilder::text(SmolStr::new_static("@"), text_style)
            .style(StyleBuilder::new().size_full().build())
            .bind_style(themed_foreground(theme.clone(), Role::FgMuted))
            .a11y(A11yOverride {
                hidden: true,
                ..A11yOverride::default()
            })
            .build(tree, placeholder)?;
        return Ok(());
    };
    let node = match image {
        ImageSource::Ready(resource) => NodeBuilder::image(resource, ImageFit::Cover),
        ImageSource::Pending(pending) => NodeBuilder::image_pending(pending, ImageFit::Cover),
    };
    node.style(style)
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, parent)?;
    Ok(())
}

fn build_icon(
    tree: &mut NodeTree,
    parent: NodeId,
    icon: ImageSource,
    size: f32,
    theme: ThemeHandle,
    role: Role,
) -> Result<(), EngineError> {
    let node = match icon {
        ImageSource::Ready(resource) => NodeBuilder::image(resource, ImageFit::Contain),
        ImageSource::Pending(pending) => NodeBuilder::image_pending(pending, ImageFit::Contain),
    };
    node.style(
        StyleBuilder::new()
            .width(px(size))
            .height(px(size))
            .flex_none()
            .build(),
    )
    .bind_style(themed_foreground(theme, role))
    .a11y(A11yOverride {
        hidden: true,
        ..A11yOverride::default()
    })
    .build(tree, parent)?;
    Ok(())
}

fn message_block(
    app: &mut App,
    window: WindowId,
    area: NodeId,
    id: SmolStr,
    visible: impl Fn(&mut ReadCtx<'_>) -> bool + 'static,
) -> Result<NodeId, EngineError> {
    build_node(app, window, area, |tree, area| {
        NodeBuilder::boxed()
            .id(id)
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .left(px(0.0))
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(8.0))
                    .padding(px(8.0))
                    .build(),
            )
            .bind_style(visible_if(visible))
            .build(tree, area)
    })
}

fn sentence_node(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    id: SmolStr,
    text_size: LogicalPx,
    theme: ThemeHandle,
    role: Role,
    text: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static,
) -> Result<NodeId, EngineError> {
    build_node(app, window, parent, |tree, parent| {
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(text_size).with_overflow(TextOverflow::Ellipsis, 2),
        )
        .id(id)
        .style(
            StyleBuilder::new()
                .max_width(px(MESSAGE_WIDTH))
                .min_width_0()
                .build(),
        )
        .bind_style(themed_foreground(theme, role))
        .bind_text(text)
        .build(tree, parent)
    })
}

fn visible_if(
    visible: impl Fn(&mut ReadCtx<'_>) -> bool + 'static,
) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static {
    move |read| StylePatch {
        visible: Some(visible(read)),
        ..StylePatch::default()
    }
}

fn themed_foreground(
    theme: ThemeHandle,
    role: Role,
) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static {
    move |read| StylePatch {
        foreground: Some(theme.role(read, role)),
        ..StylePatch::default()
    }
}

fn themed_background(
    theme: ThemeHandle,
    role: Role,
) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static {
    move |read| StylePatch {
        background: Some(theme.role(read, role)),
        ..StylePatch::default()
    }
}

fn remove_button_node(app: &mut App, window: WindowId, button: &Button) -> Result<(), EngineError> {
    let Some(node) = button.node() else {
        return Ok(());
    };
    if app
        .window(window)
        .is_none_or(|target| target.tree().get(node).is_err())
    {
        return Ok(());
    }
    app.remove_node(window, node).map(|_| ())
}

fn release_components(app: &mut App, components: DialogComponents) {
    if let Some(dialog) = components.dialog {
        dialog.release(app);
    }
    if let Some(alert) = components.alert {
        alert.release(app);
    }
    if let Some(input) = components.filter {
        input.release(app);
    }
    if let Some(input) = components.query {
        input.release(app);
    }
    if let Some(action) = components.query_submit {
        app.off_action(&action);
    }
    if let Some(list) = components.accounts {
        list.release(app);
    }
    if let Some(list) = components.results {
        list.release(app);
    }
    if let Some(spinner) = components.spinner {
        spinner.release(app);
    }
    for button in [
        components.add_account,
        components.refresh,
        components.back,
        components.search,
        components.close,
        components.retry,
        components.recover,
    ]
    .into_iter()
    .flatten()
    {
        button.release(app);
    }
    for button in components.row_controls {
        button.release(app);
    }
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use i18n::language::codes::LanguageCode;
    use kytrix_service_provider::service::x::{
        XApiError, XProfile, XSelection, XSelectionStatus, XSelectionsResponse,
    };
    use ui_core::event::{ActionSource, Event, ImeEvent, InputEvent};
    use widget::test_support::WidgetTestApp;

    use super::*;
    use crate::view::SelectionFailure;

    const SETTLE_FRAMES: usize = 8;

    fn translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales should load")),
            language,
        )
    }

    fn harness() -> WidgetTestApp {
        WidgetTestApp::with_translate(translate(LanguageCode::En))
    }

    fn open(harness: &mut WidgetTestApp) -> TweetDialog {
        harness
            .with_cx(TweetDialog::show)
            .expect("the dialog opens")
    }

    fn sync(harness: &mut WidgetTestApp, dialog: &mut TweetDialog, view: &DialogView) {
        harness
            .with_cx(|cx| dialog.sync(cx, view))
            .expect("the dialog syncs");
    }

    fn settle_frames(harness: &mut WidgetTestApp, dialog: &mut TweetDialog, view: &DialogView) {
        for _ in 0..SETTLE_FRAMES {
            sync(harness, dialog, view);
            harness.drain();
        }
    }

    fn requests(harness: &mut WidgetTestApp, dialog: &mut TweetDialog) -> Vec<DialogRequest> {
        harness.with_cx(|cx| dialog.take_requests(cx))
    }

    fn click(harness: &mut WidgetTestApp, button: &Button) {
        let node = button.node().expect("the control is built");
        let action = button
            .action_identity()
            .expect("the control registered a click handler");
        let window = harness.window();
        assert!(
            harness
                .app_mut()
                .dispatch_action(window, node, action, ActionSource::Programmatic),
            "the control is registered"
        );
    }

    fn set_query(harness: &mut WidgetTestApp, dialog: &TweetDialog, text: &str) {
        let input = dialog.inner.borrow().query.clone();
        harness.with_cx(|cx| {
            if let Some(input) = input.as_ref() {
                input.update_value(cx.app_mut(), text);
            }
        });
    }

    fn monitored(x_user_id: &str, username: Option<&str>) -> XSelection {
        XSelection {
            x_user_id: SmolStr::new(x_user_id),
            status: XSelectionStatus::Active,
            created_at: 1,
            updated_at: 2,
            profile: username.map(|username| profile(x_user_id, username)),
        }
    }

    fn profile(x_user_id: &str, username: &str) -> XProfile {
        XProfile {
            x_user_id: SmolStr::new(x_user_id),
            username: SmolStr::new(username),
            display_name: format_smolstr!("{username} display"),
            avatar_url: format_smolstr!("https://pbs.twimg.com/{username}.jpg"),
            description: SmolStr::new_static(""),
            followers_count: 0,
            following_count: 0,
            protected: false,
            verified: false,
        }
    }

    fn data(selections: Vec<XSelection>, limit: usize) -> SelectionData {
        SelectionData::new(XSelectionsResponse {
            selections,
            x_account_limit: limit,
        })
    }

    fn view(signed_in: bool, ready: bool, selections: Option<SelectionData>) -> DialogView {
        DialogView {
            signed_in,
            ready,
            selections,
            searching: None,
            operation: None,
            notice: None,
            hints: Arc::new(HashMap::default()),
            media: Arc::new(HashMap::default()),
            images: Rc::new(RefCell::new(HashMap::default())),
            recovery: WidgetAction::new("common.retry", |_| {}),
        }
    }

    #[test]
    fn a_report_should_be_written_in_both_languages() {
        for (language, error_title, success_title) in [
            (LanguageCode::En, "Error", "Success"),
            (LanguageCode::ZhCn, "错误", "成功"),
        ] {
            let translate = translate(language);
            let (problem, reason) = report_text(
                &SelectionNotice::Problem(SelectionFailure::Rejected(XApiError::QuotaExceeded)),
                &translate,
            );
            let (success, joined) = report_text(&SelectionNotice::Added, &translate);

            assert_eq!(
                problem.as_str(),
                error_title,
                "a change that was refused should be titled in {language:?}"
            );
            assert_eq!(
                success.as_str(),
                success_title,
                "an account that joined the list should be titled in {language:?}"
            );
            assert!(
                !reason.is_empty() && !reason.contains("%{"),
                "the reason should be written in {language:?}: {reason}"
            );
            assert!(
                !joined.is_empty() && !joined.contains("%{"),
                "the account that joined the list should be named in {language:?}: {joined}"
            );
        }
    }

    #[test]
    fn a_report_should_be_raised_once_over_the_dialog() {
        let mut harness = harness();
        let held = SelectionNotice::Problem(SelectionFailure::Rejected(XApiError::QuotaExceeded));
        let mut dialog = open(&mut harness);
        let mut pane_view = view(
            true,
            true,
            Some(data(vec![monitored("42", Some("kytrix"))], 4)),
        );
        pane_view.notice = Some(held);
        settle_frames(&mut harness, &mut dialog, &pane_view);
        assert!(
            dialog.inner.borrow().alert.is_none(),
            "a report the pane was already holding is not raised"
        );

        pane_view.notice = Some(SelectionNotice::Added);
        settle_frames(&mut harness, &mut dialog, &pane_view);
        let alert = dialog
            .inner
            .borrow()
            .alert
            .clone()
            .expect("the change is raised in an alert over the dialog");
        assert!(alert.is_open(harness.app()), "the alert is open");

        let confirm = alert
            .with_alert(|alert| alert.ok_button().clone())
            .expect("the alert draws the one button that says it was read");
        click(&mut harness, &confirm);
        for _ in 0..SETTLE_FRAMES {
            harness.advance(Duration::from_millis(40));
            sync(&mut harness, &mut dialog, &pane_view);
            if dialog.inner.borrow().alert.is_none() {
                break;
            }
        }
        assert!(
            dialog.inner.borrow().alert.is_none(),
            "the alert goes when it is read, and the report is not raised again"
        );
    }

    #[test]
    fn the_filter_should_match_what_a_reader_can_see() {
        let selections = [
            monitored("42", Some("Kytrix")),
            monitored("43", None),
            monitored("9876543210", Some("someone")),
        ];
        let hints = HashMap::from([(SmolStr::new_static("43"), SmolStr::new_static("pending"))]);

        assert_eq!(
            filtered_selection_indices(&selections, &hints, None),
            [0, 1, 2]
        );
        assert_eq!(
            filtered_selection_indices(&selections, &hints, Some("   ")),
            [0, 1, 2]
        );
        assert!(filtered_selection_indices(&selections, &hints, Some("nobody")).is_empty());

        assert_eq!(
            filtered_selection_indices(&selections, &hints, Some("kytrix")),
            [0]
        );
        assert_eq!(
            filtered_selection_indices(&selections, &hints, Some("Kytrix display")),
            [0]
        );
        assert_eq!(
            filtered_selection_indices(&selections, &hints, Some("@pending")),
            [1]
        );
        assert_eq!(
            filtered_selection_indices(&selections, &hints, Some("65432")),
            [2]
        );
    }

    #[test]
    fn a_follower_count_should_read_at_the_width_a_row_has() {
        for (count, expected) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1K"),
            (1_250, "1.3K"),
            (1_000_000, "1M"),
            (1_250_000, "1.3M"),
            (1_000_000_000, "1B"),
            (999_950, "1M"),
            (999_999_950, "1B"),
        ] {
            assert_eq!(format_follower_count(count), expected, "{count} followers");
        }
    }

    #[test]
    fn a_search_result_should_offer_to_monitor_it_unless_it_already_is() {
        let hints = HashMap::from([
            (SmolStr::new_static("42"), SmolStr::new_static("stale")),
            (SmolStr::new_static("43"), SmolStr::new_static("pending")),
        ]);
        let data = data(
            vec![
                monitored("42", Some("kytrix")),
                monitored("43", None),
                monitored("44", None),
            ],
            4,
        );
        let rows = SearchRows {
            monitored: Some(&data),
            hints: &hints,
            quota_full: false,
            enabled: true,
        };

        assert_eq!(
            rows.action("kytrix"),
            SearchProfileAction::Selected,
            "an account already monitored is not one to monitor again"
        );
        assert_eq!(
            rows.action("Kytrix"),
            SearchProfileAction::Selected,
            "a username is the same name whatever its case"
        );
        assert_eq!(
            rows.action("pending"),
            SearchProfileAction::Selected,
            "a row waiting to be activated is monitored under the name it was added with"
        );
        assert_eq!(
            rows.action("stale"),
            SearchProfileAction::Add { disabled: false },
            "a row a profile names is not also named by what is remembered for its id"
        );
        assert_eq!(
            rows.action("44"),
            SearchProfileAction::Add { disabled: false },
            "a row nothing names is not one a name can read as monitored"
        );
        assert_eq!(
            rows.action("someone"),
            SearchProfileAction::Add { disabled: false }
        );
        assert!(!rows.action("someone").disabled());

        let full = SearchRows {
            quota_full: true,
            ..rows
        };
        assert_eq!(
            full.action("someone"),
            SearchProfileAction::Add { disabled: true }
        );
        assert!(full.action("someone").disabled());

        let busy = SearchRows {
            enabled: false,
            ..full
        };
        assert_eq!(
            busy.action("someone"),
            SearchProfileAction::Add { disabled: true }
        );
    }

    #[test]
    fn a_monitored_account_should_be_named_by_what_is_known_about_it() {
        let hints = HashMap::from([(SmolStr::new_static("43"), SmolStr::new_static("pending"))]);

        assert_eq!(
            account_name(&monitored("42", Some("kytrix")), &hints),
            "@kytrix"
        );
        assert_eq!(
            account_name(&monitored("43", None), &hints),
            "@pending",
            "an account waiting to be activated is named by what was asked for"
        );
        assert_eq!(
            account_name(&monitored("44", None), &hints),
            "44",
            "nothing but the id is known, so the id is what a row says"
        );
    }

    #[test]
    fn the_close_control_should_finish_the_dialog() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let view = view(
            true,
            true,
            Some(data(vec![monitored("42", Some("kytrix"))], 4)),
        );
        settle_frames(&mut harness, &mut dialog, &view);
        assert!(!dialog.finished(), "the dialog is open");

        let close = dialog
            .inner
            .borrow()
            .close
            .clone()
            .expect("the footer draws the way out");
        click(&mut harness, &close);
        for _ in 0..SETTLE_FRAMES {
            harness.advance(Duration::from_millis(40));
            sync(&mut harness, &mut dialog, &view);
            if dialog.finished() {
                break;
            }
        }

        assert!(dialog.finished(), "the close control ends the dialog");
    }

    #[test]
    fn the_requests_the_controls_write_should_reach_the_pane() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let view = view(
            true,
            true,
            Some(data(
                vec![monitored("42", Some("kytrix")), monitored("43", None)],
                4,
            )),
        );
        settle_frames(&mut harness, &mut dialog, &view);

        let refresh = dialog
            .inner
            .borrow()
            .refresh
            .clone()
            .expect("the header draws the read-again control");
        click(&mut harness, &refresh);
        assert_eq!(
            requests(&mut harness, &mut dialog),
            vec![DialogRequest::Reload],
            "the control asks the pane to read again"
        );

        let delete = dialog
            .inner
            .borrow()
            .delete_buttons
            .get(&0)
            .map(|entry| entry.button.clone())
            .expect("the row carries the control that stops monitoring it");
        click(&mut harness, &delete);
        assert_eq!(
            requests(&mut harness, &mut dialog),
            vec![DialogRequest::Delete(SmolStr::new_static("42"))],
            "the row that was clicked is the account that leaves"
        );

        set_query(&mut harness, &dialog, "kytrix");
        settle_frames(&mut harness, &mut dialog, &view);
        let search = dialog
            .inner
            .borrow()
            .search
            .clone()
            .expect("the search page draws the control that sends a search");
        click(&mut harness, &search);
        assert_eq!(
            requests(&mut harness, &mut dialog),
            vec![DialogRequest::Search(SmolStr::new_static("kytrix"))],
            "the query the field holds is the one that was asked for"
        );

        let answer = SearchAnswer::Found(Arc::from(vec![profile("99", "someone")]));
        harness.with_cx(|cx| {
            dialog.answered_search(cx, &SmolStr::new_static("kytrix"), answer);
        });
        settle_frames(&mut harness, &mut dialog, &view);
        let monitor = dialog
            .inner
            .borrow()
            .monitor_buttons
            .get(&0)
            .map(|entry| entry.button.clone())
            .expect("the result row carries the control that monitors it");
        click(&mut harness, &monitor);
        assert_eq!(
            requests(&mut harness, &mut dialog),
            vec![DialogRequest::Add(SmolStr::new_static("someone"))],
            "the account beside the control is the one that is asked for"
        );
    }

    #[test]
    fn releasing_the_dialog_should_hand_its_values_back() {
        let mut harness = harness();
        let baseline = harness.signal_slot_stats();
        let view = view(
            true,
            true,
            Some(data(vec![monitored("42", Some("kytrix"))], 4)),
        );
        {
            let mut dialog = open(&mut harness);
            settle_frames(&mut harness, &mut dialog, &view);
            assert!(
                harness.signal_slot_stats().live > baseline.live,
                "the dialog mints its own values"
            );

            harness.with_cx(|cx| dialog.release(cx));
            assert_eq!(
                harness.signal_slot_stats().live,
                baseline.live,
                "releasing the dialog reclaims its values"
            );
            harness.with_cx(|cx| dialog.release(cx));
            assert_eq!(
                harness.signal_slot_stats().live,
                baseline.live,
                "a second release takes nothing more"
            );
            assert!(dialog.finished(), "a released dialog is finished");
        }

        let mut capacity = None;
        for _ in 0..8 {
            let mut dialog = open(&mut harness);
            harness.with_cx(|cx| dialog.release(cx));
            let stats = harness.signal_slot_stats();
            assert_eq!(
                stats.live, baseline.live,
                "a released dialog leaves no value behind"
            );
            match capacity {
                None => capacity = Some(stats.slots),
                Some(slots) => assert_eq!(
                    stats.slots, slots,
                    "the value slots are reused rather than grown"
                ),
            }
        }
    }

    fn node(dialog: &TweetDialog, harness: &WidgetTestApp, suffix: &str) -> NodeId {
        let key = dialog.inner.borrow().keys.dialog.clone();
        let prefix = format_smolstr!("{key}:{suffix}");
        harness
            .find_by_debug_prefix(harness.root(), &prefix)
            .unwrap_or_else(|| panic!("the dialog draws {suffix}"))
    }

    fn typed(harness: &WidgetTestApp, dialog: &TweetDialog, query: bool) -> SmolStr {
        let state = {
            let state = dialog.inner.borrow();
            if query {
                state.query_state
            } else {
                state.filter_state
            }
        };
        SmolStr::new(harness.app().read(&state).text())
    }

    #[test]
    fn the_dialog_should_show_one_page_at_a_time() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let view = view(
            true,
            true,
            Some(data(vec![monitored("42", Some("kytrix"))], 4)),
        );
        settle_frames(&mut harness, &mut dialog, &view);
        let managed = node(&dialog, &harness, "monitored");
        let search = node(&dialog, &harness, "add");
        assert!(harness.visible(managed), "the dialog opens on its accounts");
        assert!(!harness.visible(search));

        let add = dialog
            .inner
            .borrow()
            .add_account
            .clone()
            .expect("the managed page draws the control that opens the search page");
        click(&mut harness, &add);
        settle_frames(&mut harness, &mut dialog, &view);
        assert!(
            !harness.visible(managed),
            "the accounts page is not drawn under the search page"
        );
        assert!(harness.visible(search), "the search page is showing");

        let back = dialog
            .inner
            .borrow()
            .back
            .clone()
            .expect("the search page draws the way back");
        click(&mut harness, &back);
        settle_frames(&mut harness, &mut dialog, &view);
        assert!(
            harness.visible(managed),
            "the way back pops the search page"
        );
        assert!(!harness.visible(search));
    }

    #[test]
    fn a_search_that_is_out_should_be_drawn_as_a_spinner() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let mut pane_view = view(
            true,
            true,
            Some(data(vec![monitored("42", Some("kytrix"))], 4)),
        );
        settle_frames(&mut harness, &mut dialog, &pane_view);
        let searching = node(&dialog, &harness, "add:searching");
        assert!(!harness.visible(searching));
        assert!(
            dialog.inner.borrow().spinner.is_none(),
            "nothing is being waited for yet"
        );

        pane_view.searching = Some(SmolStr::new_static("kytrix"));
        settle_frames(&mut harness, &mut dialog, &pane_view);
        assert!(
            dialog.inner.borrow().spinner.is_some(),
            "the searching window draws a spinner"
        );
        assert!(
            harness.visible(searching),
            "the searching window is showing"
        );
        assert!(
            harness.has_canvas(searching),
            "the spinner is what the searching window draws"
        );

        pane_view.searching = None;
        settle_frames(&mut harness, &mut dialog, &pane_view);
        assert!(
            dialog.inner.borrow().spinner.is_none(),
            "the spinner is released when the search answers"
        );
        assert!(!harness.visible(searching));
    }

    #[test]
    fn the_dialog_s_static_copy_should_follow_the_language() {
        let mut harness = harness();
        harness.enable_a11y_capture();
        let mut dialog = open(&mut harness);
        let view = view(
            true,
            true,
            Some(data(vec![monitored("42", Some("kytrix"))], 4)),
        );
        settle_frames(&mut harness, &mut dialog, &view);
        let english = translate(LanguageCode::En);
        let add = dialog
            .inner
            .borrow()
            .add_account
            .clone()
            .expect("the managed page draws the control that opens the search page");
        assert_eq!(
            add.a11y_label_text(),
            Some(t_smol!(english, "widget.x.add_account").as_str()),
            "the control is named in the language it was built in"
        );
        let filter = dialog
            .inner
            .borrow()
            .filter
            .clone()
            .expect("the managed page draws its filter");
        assert!(
            harness
                .app()
                .read(&filter.placeholder_state)
                .contains(t_smol!(english, "widget.x.filter_placeholder").as_str()),
            "the field is placeheld in the language it was built in: {}",
            harness.app().read(&filter.placeholder_state)
        );
        assert_eq!(
            harness.a11y_label(filter.editable),
            Some(t_smol!(english, "widget.x.filter_placeholder").to_string()),
            "the field is named in the language it was built in"
        );

        harness.switch_language(LanguageCode::ZhCn);
        settle_frames(&mut harness, &mut dialog, &view);

        let chinese = translate(LanguageCode::ZhCn);
        let rebuilt = dialog
            .inner
            .borrow()
            .add_account
            .clone()
            .expect("the control is rebuilt for the language");
        assert_eq!(
            rebuilt.a11y_label_text(),
            Some(t_smol!(chinese, "widget.x.add_account").as_str()),
            "the control's copy follows the language"
        );
        assert!(
            harness
                .app()
                .read(&filter.placeholder_state)
                .contains(t_smol!(chinese, "widget.x.filter_placeholder").as_str()),
            "the field's placeholder follows the language: {}",
            harness.app().read(&filter.placeholder_state)
        );
        assert_eq!(
            harness.a11y_label(filter.editable),
            Some(t_smol!(chinese, "widget.x.filter_placeholder").to_string()),
            "the field's name follows the language"
        );
    }

    #[test]
    fn typing_and_ime_should_reach_the_dialog_s_fields() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let view = view(
            true,
            true,
            Some(data(vec![monitored("42", Some("kytrix"))], 4)),
        );
        settle_frames(&mut harness, &mut dialog, &view);
        let filter = dialog
            .inner
            .borrow()
            .filter
            .as_ref()
            .map(|input| input.editable)
            .expect("the managed page draws its filter");
        let window = harness.window();
        assert!(
            harness.app_mut().set_focus(window, Some(filter)),
            "the field takes the focus"
        );

        harness.dispatch_event(Event::Input(InputEvent::Text {
            text: SmolStr::new_static("ky"),
            timestamp: Instant::now(),
        }));
        sync(&mut harness, &mut dialog, &view);
        assert_eq!(
            typed(&harness, &dialog, false).as_str(),
            "ky",
            "the focused field takes the typed text"
        );

        harness.dispatch_event(Event::Ime(ImeEvent::Commit {
            text: SmolStr::new_static("中"),
            replace: None,
        }));
        sync(&mut harness, &mut dialog, &view);
        assert_eq!(
            typed(&harness, &dialog, false).as_str(),
            "ky中",
            "an IME commit reaches the field the keyboard is on"
        );

        assert_eq!(
            typed(&harness, &dialog, true).as_str(),
            "",
            "text goes to the field the reader is in, not to both"
        );
    }

    #[test]
    fn a_search_answer_should_belong_to_the_query_that_asked_for_it() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let view = view(
            true,
            true,
            Some(data(vec![monitored("42", Some("kytrix"))], 4)),
        );
        set_query(&mut harness, &dialog, "kytrix");
        settle_frames(&mut harness, &mut dialog, &view);
        assert!(!harness.visible(node(&dialog, &harness, "add:list")));

        let found = SearchAnswer::Found(Arc::from(vec![profile("99", "someone")]));
        harness.with_cx(|cx| {
            dialog.answered_search(cx, &SmolStr::new_static("kytrix"), found);
        });
        settle_frames(&mut harness, &mut dialog, &view);
        assert!(harness.visible(node(&dialog, &harness, "add:list")));

        set_query(&mut harness, &dialog, "someone-else");
        settle_frames(&mut harness, &mut dialog, &view);
        assert!(
            !harness.visible(node(&dialog, &harness, "add:list")),
            "an answer does not stand under a query it was not asked with"
        );

        harness.with_cx(|cx| {
            dialog.answered_search(
                cx,
                &SmolStr::new_static("kytrix"),
                SearchAnswer::Found(Arc::from(vec![profile("99", "someone")])),
            );
        });
        settle_frames(&mut harness, &mut dialog, &view);
        assert!(!harness.visible(node(&dialog, &harness, "add:list")));

        harness.with_cx(|cx| {
            dialog.answered_search(
                cx,
                &SmolStr::new_static("someone-else"),
                SearchAnswer::Found(Arc::from(vec![profile("99", "someone")])),
            );
        });
        settle_frames(&mut harness, &mut dialog, &view);
        assert!(harness.visible(node(&dialog, &harness, "add:list")));
    }

    #[test]
    fn an_empty_search_answer_should_say_so_on_the_page_that_asked() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let view = view(
            true,
            true,
            Some(data(vec![monitored("42", Some("kytrix"))], 4)),
        );
        set_query(&mut harness, &dialog, "nobody");
        settle_frames(&mut harness, &mut dialog, &view);

        harness.with_cx(|cx| {
            dialog.answered_search(
                cx,
                &SmolStr::new_static("nobody"),
                SearchAnswer::Found(Arc::from(Vec::new())),
            );
        });
        settle_frames(&mut harness, &mut dialog, &view);

        let message = node(&dialog, &harness, "add:empty");
        assert!(
            harness.visible(message),
            "the answer is drawn where it was asked"
        );
        let translate = harness.services().translate.clone();
        assert!(
            harness
                .text_of(message)
                .contains(t_smol!(translate, "widget.x.no_search_results").as_str()),
            "an answer that found nothing says so"
        );
        assert!(dialog.inner.borrow().alert.is_none(), "it is not a failure");
    }

    #[test]
    fn a_search_that_could_not_be_answered_should_be_drawn_where_it_was_asked() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let view = view(
            true,
            true,
            Some(data(vec![monitored("42", Some("kytrix"))], 4)),
        );
        set_query(&mut harness, &dialog, "kytrix");
        settle_frames(&mut harness, &mut dialog, &view);

        let error = RpcCallError::DeadlineExceeded {
            request_id: widget::protocol::RequestId::new(4),
        };
        let expected = error
            .translated_message(&translate(LanguageCode::En))
            .to_string();
        harness.with_cx(|cx| {
            dialog.answered_search(
                cx,
                &SmolStr::new_static("kytrix"),
                SearchAnswer::Failed(error),
            );
        });
        settle_frames(&mut harness, &mut dialog, &view);

        let message = node(&dialog, &harness, "add:failed");
        assert!(
            harness.visible(message),
            "the failure is drawn where it was asked"
        );
        assert!(
            harness.text_of(message).contains(expected.as_str()),
            "the words are the failure's own: {}",
            harness.text_of(message)
        );
        assert!(
            dialog.inner.borrow().alert.is_none(),
            "a search that failed is not a change to the accounts, so there is nothing to alert"
        );
    }
}

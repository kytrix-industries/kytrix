//! The dialog a pane opens to manage the accounts its timeline is read from.
//!
//! # Two pages, one list of accounts
//!
//! The dialog shows the accounts being monitored and, behind a control of its own, a search of
//! X for one more. The two pages draw different state. What is monitored, what the last change
//! said and which request is outstanding belong to the pane, because the service answering them
//! is the pane's and the accounts outlive any one dialog: a dialog that held them would lose
//! them on the next open. The search belongs to the dialog: the query is typed in its field and
//! the request is made for it, so what a search found is the page's own and goes when the dialog
//! goes — which is the point, because an answer drawn under a query a later dialog never asked
//! is worse than no answer at all.
//!
//! # One value channel into the dialog
//!
//! The pane is not reachable from the dialog: a kit control's callback owns no widget, and the
//! engine has no entity the two could hold each other through. So the dialog is built over
//! **signals**: the pane publishes what it holds into them every frame ([`TweetDialog::sync`]),
//! and every dynamic value the dialog draws is a `bind_*` closure over one of them. What the
//! user does travels back as requests the pane applies ([`DialogRequest`]) — a read, a search, a
//! new account and a cancelled one are all the pane's own operations, sent over the session the
//! pane's stream is holding.
//!
//! # The pages are toggled, not slid over one another
//!
//! **Registered degradation**: the old dialog pushed the search page onto a [`NavStack`], so one
//! page slid in over the other and each kept its own place. The engine has no navigation-stack
//! transition (no presence phases, no per-page progress), so the two pages are two containers in
//! one column area, each bound to a [`Signal<DialogPage>`] through `bind_style.visible`. The
//! search page appears and disappears in place: the change is instant rather than animated, and
//! there is no depth drift. Everything else a page holds — its field, its list, its place in that
//! list — is still the page's own, because each page's list is a component of its own.
//!
//! # The row controls are built into rows the list owns
//!
//! A kit list's row factory is handed a [`NodeTree`] and nothing else: no application context,
//! and therefore no `Button`. The delete control of an account row and the monitor control of a
//! search result are kit buttons all the same, so a row leaves an empty slot for its control and
//! the dialog builds the button into that slot from [`DialogState::sync_row_controls`], after the
//! list has mounted its rows. A list that rebuilds its visible rows (a new account count, a
//! language switch, a picture that arrived) leaves new slots behind; the sync pairs the buttons
//! with the slots showing now and releases the ones whose row is gone.
//!
//! # Static copy is rebuilt, not re-bound
//!
//! The kit components this dialog builds choose their labels when they are built, and a language
//! switch would otherwise leave them in the language the dialog was opened in. The buttons the
//! dialog holds (add / refresh / back / search / retry / recover / close) are therefore released
//! and built again into the same parent when the language moves — the same four steps
//! `crates/widgets/AGENTS.md` §15.1 freezes for static copy — the fields' placeholders are
//! re-written through `InputNode::update_placeholder`, and the visible rows are rebuilt so their
//! status badges are in the language showing now. The kit dialog's own title and its close
//! button's accessible name are the pieces chosen at open; a language switch while the dialog is
//! open leaves those two in the language it was opened in, which is the same known difference the
//! W8a solana dialog registered.
//!
//! # The spinner exists only while it spins
//!
//! A kit spinner animates for as long as it is synced, and the engine has no "hidden animation"
//! state: a spinner that merely sits in a hidden block would keep asking for frames (I3). It is
//! therefore built when the search page enters its searching window and released when that window
//! closes, which is the framework's own placeholder pattern.
//!
//! # What a change says is reported in an alert
//!
//! An account that joined the list and a change that was refused are the two things a change has
//! to say, and either of them is one alert over the dialog with the one button that says it was
//! read: a service's answer is not a line of a page, which a reader looks past. What the alert is
//! raised for is the report the pane is holding and not the frames it is held for, so a change is
//! one alert; a change the dialog was not there to hear — it was dismissed before the answer
//! arrived, or the pane was already holding the report when this dialog opened — is not raised.
//! A search is the search page's own procedure rather than a change to the accounts, so what came
//! of one is drawn where it was asked for and never as an alert.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::{Rc, Weak},
    sync::Arc,
};

use ahash::AHashMap as HashMap;
use base_ui::{
    kit::{
        alert::AlertBuilder,
        badge::Badge,
        button::Button,
        dialog::{Dialog, DialogBuilder, DialogClose, DialogError},
        icon::{IconKind, RasterTier, raster},
        input::{self, InputNode, InputSpec, InputState},
        list::{List, ListOptions, SelectionMode},
        overlay::OverlayError,
        separator::Separator,
        size::{UnifiedSizable, UnifiedSize},
        spinner::Spinner,
    },
    style_builder::StyleBuilder,
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::x::{XApiError, XProfile, XSelection, XSelectionStatus};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch},
    app::App,
    error::EngineError,
    event::{Action, Event, InputEvent},
    interaction::ActionCtx,
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        id::NodeId,
        image::{ImageFit, ImageSource},
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::{color::Color, style::StylePatch},
    text::{TextAlign, TextOverflow, TextStyle},
    window::WindowId,
};
use widget::{cx::WidgetCx, error::TranslatedError, rpc::RpcCallError, status::WidgetAction};

use super::{
    model::{MediaState, MediaStates, SelectionData, selection_name},
    view::{SelectionNotice, SelectionOperation, status_label, valid_search_query},
};

/// The width of the dialog's panel.
const DIALOG_WIDTH: f32 = 580.0;

/// The height of the panel's content column (the old dialog's 640 px panel less the title row
/// and padding the kit dialog's panel brings).
const CONTENT_HEIGHT: f32 = 520.0;

/// The widest a sentence standing in for one of the lists is drawn.
const MESSAGE_WIDTH: f32 = 420.0;

/// The size of the face one row of either list is drawn with.
const ROW_AVATAR_SIZE: f32 = 42.0;

/// The height one row of either list is drawn at.
///
/// A list measures the first row it draws and lays every other one out at that height, so what a
/// row is worth has to be the row's own height rather than whatever its content asks for: the row
/// is given one here, which is the face, the lines beside it and a little room around them. A
/// control's own size is not one — a control has the height of a control, and a row drawn at it
/// would be laid out at that height and overflow it.
const ROW_HEIGHT: f32 = 72.0;

/// The size of the check beside a name X vouches for.
const VERIFIED_ICON_SIZE: f32 = 12.0;

/// Which of the dialog's two pages is showing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DialogPage {
    /// The accounts being monitored, which is the page the dialog opens on.
    Monitored,
    /// A search of X for one more.
    Add,
}

/// What one frame hands the dialog: the pane's selection state, as the dialog draws it.
#[derive(Clone)]
pub(super) struct DialogView {
    /// Whether anybody is signed in.
    pub signed_in: bool,
    /// Whether the stream carrying the account is live.
    pub ready: bool,
    /// The accounts the account monitors, once a read has answered.
    pub selections: Option<SelectionData>,
    /// The query the pane is carrying a search for, while it is carrying one.
    pub searching: Option<SmolStr>,
    /// What the pane is waiting for, if anything.
    pub operation: Option<SelectionOperation>,
    /// What the pane says about the last change.
    pub notice: Option<SelectionNotice>,
    /// What is remembered about the accounts no profile names yet, keyed by their id.
    pub hints: Arc<HashMap<SmolStr, SmolStr>>,
    /// What is known about every picture the pane wants.
    pub media: MediaStates,
    /// The engine picture resources the pane has registered, keyed by the URL they came from.
    ///
    /// A row builder has no `App` and cannot register a picture itself, so the pane registers
    /// them and the dialog draws what is there: a face with no resource yet draws as its
    /// stand-in and is rebuilt when the resource arrives.
    pub images: Rc<RefCell<HashMap<SmolStr, ImageSource>>>,
    /// The recovery the pane itself offers, for a dialog with no stream.
    pub recovery: WidgetAction,
}

/// What the user asked the pane for through the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DialogRequest {
    /// Read the accounts again.
    Reload,
    /// Search X for the account the query names.
    Search(SmolStr),
    /// Monitor one more account.
    Add(SmolStr),
    /// Stop monitoring one account.
    Delete(SmolStr),
}

/// What one search of X answered.
///
/// A search is a procedure of the page that runs it rather than a change to the accounts, so what
/// it answered is that page's state rather than the pane's: it is handed to the dialog that asked
/// for it and dies with it. "Found nothing" is [`SearchAnswer::Found`] with no profile in it,
/// which is an answer; a query nothing has been searched with is not one at all.
pub(super) enum SearchAnswer {
    /// The accounts the search found, which may be none of them.
    Found(Arc<[XProfile]>),
    /// Why the search could not be answered.
    Failed(RpcCallError<XApiError>),
}

/// The dialog a pane opens to manage the accounts its timeline is read from.
pub(super) struct TweetDialog {
    inner: Rc<RefCell<DialogState>>,
}

impl TweetDialog {
    /// Builds every component and opens the kit dialog over the pane's window.
    ///
    /// The content is built against signals, so nothing about the pane is needed here: what the
    /// dialog draws is published by the first frame.
    ///
    /// # Errors
    /// The kit dialog could not be built or opened (the window is gone, or the content subtree
    /// could not be created).
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
            return Err(engine_error_of(error, window));
        }

        let _ = cx.request_frame();
        Ok(Self { inner })
    }

    /// One frame of the dialog: follow the language, publish the pane's state, sync the
    /// components, pair the row slots with their controls and raise what the pane reports.
    ///
    /// # Errors
    /// The kit dialog, a list or one of the controls failed to sync; the caller closes the
    /// dialog rather than leaving dead nodes behind.
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

    /// Takes the requests the user made through the dialog since the last frame.
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

    /// Hands a search answer to the page that asked for it.
    ///
    /// An answer belongs to the query that asked for it: a reader who edited the query while the
    /// search was out asked about something else, and an answer to a question they no longer have
    /// is not one this page may draw. So the answer is dropped rather than shown under a query it
    /// does not stand for.
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

    /// Whether the dialog closed (its close button / footer / Esc or a dismissal).
    pub(super) fn finished(&self) -> bool {
        let state = self.inner.borrow();
        state.released || state.closed.get()
    }

    /// Releases every kit component and the dialog's own signal scope. Idempotent.
    pub(super) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        let (components, scope) = {
            let mut state = self.inner.borrow_mut();
            if state.released {
                return;
            }
            state.released = true;
            (state.take_components(), state.scope)
        };
        // The dialog unmounts the content subtree, so it goes first; the controls that own
        // nodes go before the ones that do not (E16); the scope is the last thing handed back.
        release_components(cx.app_mut(), components);
        let _ = cx.app_mut().release_scope(scope);
    }
}

/// The kit keys one dialog mints from its instance identity.
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

/// One mounted row's control, and what it was built with.
struct RowControl {
    /// The slot the row left for the control, which a rebuilt row replaces.
    slot: NodeId,
    /// The language the control's accessible name is written in.
    language: LanguageCode,
    button: Button,
}

/// The controls whose copy is chosen when they are built and rebuilt when the language moves.
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

/// The components one dialog owns, taken as one set on release.
struct DialogComponents {
    dialog: Option<Dialog>,
    alert: Option<Dialog>,
    filter: Option<InputNode>,
    query: Option<InputNode>,
    /// The query field's submit action handler, which is the dialog's own registration.
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

/// The parts one open dialog owns.
struct DialogState {
    window: WindowId,
    scope: SignalScope,
    theme: ThemeHandle,
    translate: Translate,
    language: Signal<LanguageCode>,
    keys: DialogKeys,
    /// The language the static copy was built in.
    built_language: Option<LanguageCode>,
    /// The recovery control's label and enabled state, which a change rebuilds it for.
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
    /// The controls of the mounted monitored rows, keyed by the row's place in the filtered list.
    delete_buttons: BTreeMap<usize, RowControl>,
    /// The monitor controls of the mounted search results, on the same terms.
    monitor_buttons: BTreeMap<usize, RowControl>,
    /// The slot each mounted monitored row left for its delete control.
    account_slots: Rc<RefCell<BTreeMap<usize, NodeId>>>,
    /// The slot each mounted result row left for its monitor control.
    result_slots: Rc<RefCell<BTreeMap<usize, NodeId>>>,
    add_account_parent: Option<NodeId>,
    refresh_parent: Option<NodeId>,
    back_parent: Option<NodeId>,
    search_parent: Option<NodeId>,
    close_parent: Option<NodeId>,
    retry_parent: Option<NodeId>,
    recover_parent: Option<NodeId>,
    /// The rows the monitored list is drawing, which its row builders read for structure.
    account_rows_held: Rc<RefCell<Arc<[XSelection]>>>,
    /// The results the search list is drawing, on the same terms.
    result_rows_held: Rc<RefCell<Arc<[XProfile]>>>,
    /// The hints the row builders and the row controls name unresolved accounts by.
    hints_held: Rc<RefCell<Arc<HashMap<SmolStr, SmolStr>>>>,
    /// What the pane's picture resources hold, mirrored for the row builders (which have no
    /// `App` to read a signal with). Adopted from the pane's own map on every frame.
    images: Rc<RefCell<HashMap<SmolStr, ImageSource>>>,
    images_source: Option<Rc<RefCell<HashMap<SmolStr, ImageSource>>>>,
    /// What is known about every picture, mirrored for the row builders on the same terms.
    media: Rc<RefCell<MediaStates>>,
    /// The check beside a name X vouches for, registered once for every row to draw.
    verified: Option<ImageSource>,
    /// Which of the pane's rows the filter left the last time, in the order they are held.
    shown_indices: Vec<usize>,
    /// Whether another set of rows is showing now, which is what puts the reader back at the top.
    rows_moved: bool,
    /// The structure of the monitored rows the list was last built with, and what a rebuild would
    /// have to be told about besides it.
    account_shape: Vec<AccountRowShape>,
    account_language: Option<LanguageCode>,
    account_images: usize,
    shown_accounts: Option<usize>,
    /// The profiles the search list was last built with, and the same two extras.
    result_rows: Option<Arc<[XProfile]>>,
    result_language: Option<LanguageCode>,
    result_images: usize,
    /// The pane's state as the last frame published it.
    last_view: Option<DialogView>,
    /// The query a search answer stands for, while one stands.
    answered_query: Option<SmolStr>,
    /// What the pane was holding the last time the dialog reported it.
    ///
    /// `None` is the record not being taken yet: the first frame records what the pane already
    /// held, because a report made before the dialog opened was made to a dialog that is gone (or
    /// to none at all), and raising it would report something the reader did in a session that is
    /// over.
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

/// Which part of a search's life the search page is drawing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SearchPhase {
    /// Nothing has been searched for — or the query was edited since — so there is nothing to
    /// draw above the field.
    #[default]
    Idle,
    /// The search found these accounts, of which there is at least one.
    Found,
    /// The search was answered and found nothing.
    Empty,
    /// The search could not be answered.
    Failed,
}

/// What one monitored row's structure has to be told about before it can be built.
///
/// A row's text is bound to the signals the frame publishes, so text changes are not a reason to
/// rebuild; a face, a badge and the parts of the line a profile decides are built once, and this
/// is the record of what they were built for.
#[derive(Clone, Debug, Eq, PartialEq)]
struct AccountRowShape {
    avatar_url: SmolStr,
    profiled: bool,
    status: XSelectionStatus,
    verified: bool,
    protected: bool,
}

impl DialogState {
    /// Builds the dialog's keys and signals. No nodes and no kit components yet.
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

    /// Builds the two lists the dialog draws rows with, and the icons those rows draw. Called
    /// before the dialog is opened, because the dialog's content mounts them.
    fn create_kit(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let verified = {
            let image = cx
                .app_mut()
                .resources_mut()
                .insert(raster(IconKind::CheckCircle, RasterTier::Small48))?;
            ImageSource::Ready(image.id)
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
                .padding(0.0, 0.0),
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
                .padding(0.0, 0.0),
            move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                build_result_row(tree, parent, index, &results)
            },
        );
        list.set_initial_count(cx.app_mut(), 0);
        self.results = Some(list);
        Ok(())
    }

    /// Builds the kit dialog around this dialog's content.
    fn build_dialog(inner: &Weak<RefCell<DialogState>>, cx: &mut WidgetCx<'_>) -> Dialog {
        let (theme, layer, animation, key, title, close_label, closed) = {
            let state = inner
                .upgrade()
                .expect("the dialog is built while its state is alive");
            let state = state.borrow();
            (
                state.theme.clone(),
                cx.layer().clone(),
                cx.animation().clone(),
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
            .build(cx.app_mut(), theme, layer, animation, key)
    }

    /// One frame: rebuild the static copy, publish the pane's state, feed the lists, sync every
    /// component, and report what the pane says about the last change.
    fn frame(&mut self, cx: &mut WidgetCx<'_>, view: &DialogView) -> Result<(), EngineError> {
        if self.released || self.closed.get() {
            return Ok(());
        }
        self.rebuild_static_copy(cx)?;
        self.sync_recover(cx)?;
        self.publish(cx, view);
        self.dispatch_text_events(cx);
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

    /// Publishes the pane's state to the signals the dialog's nodes read.
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

        // The pane's picture resources are the pane's own map: the row builders hold a mirror of
        // it, which is adopted whenever the pane hands over a different map or grows the one it
        // has. A mirror that is a copy rather than the same `Rc` is what lets the rows see what
        // the pane registered after the dialog was built.
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

        // The rows the monitored list draws: the pane's accounts the filter leaves, in the order
        // they are held. The rows themselves read text off the signals, so what is written here
        // is the identity of the set — another set is another place in the list.
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

    /// Feeds this dialog's own typing to its two fields.
    ///
    /// A dialog owns **the typed text and key presses of its own window, and only while it is
    /// that window's modal scope**: the engine has already dispatched the interaction events, so
    /// what is left in the observation queue is what a text input can consume. An alert over the
    /// dialog is the modal scope while it is up, so the fields are left alone then.
    fn dispatch_text_events(&mut self, cx: &mut WidgetCx<'_>) {
        if !self.owns_modal(cx.app()) {
            return;
        }
        let window = self.window;
        let events = cx
            .app_mut()
            .drain_events_where(|target, event| target == window && is_text_input(event));
        for (_, event) in events {
            if let Some(input) = self.query.as_ref()
                && input.dispatch(cx.app_mut(), &event)
            {
                continue;
            }
            if let Some(input) = self.filter.as_ref() {
                let _ = input.dispatch(cx.app_mut(), &event);
            }
        }
    }

    /// Whether this dialog is the window's modal scope, and therefore the keyboard's owner.
    fn owns_modal(&self, app: &App) -> bool {
        let Some(dialog) = self.dialog.as_ref() else {
            return false;
        };
        matches!(
            (app.modal_scope(self.window), dialog.overlay_node()),
            (Some(top), Some(own)) if top == own
        )
    }

    /// Clears what a search found once the query it was asked with has moved.
    ///
    /// The query is what asks for a search, so an answer stands for the query it was asked with
    /// and for no other: editing the query takes the answer away rather than leaving it under a
    /// query it does not stand for.
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

    /// Shows what a search answered, for the query it was asked with.
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

    /// The query the field holds, once it is one a search could be asked with.
    fn typed_query(&self, cx: &WidgetCx<'_>) -> Option<SmolStr> {
        typed_text(&self.query_state, cx)
    }

    /// The filter the field holds.
    fn filter_text(&self, cx: &WidgetCx<'_>) -> Option<SmolStr> {
        typed_text(&self.filter_state, cx)
    }

    /// Whether the pane may change its account list right now.
    fn may_change(&self) -> bool {
        self.last_view.as_ref().is_some_and(|view| {
            view.ready
                && view.selections.is_some()
                && view.operation.is_none()
                && view.searching.is_none()
        })
    }

    /// What one search result's control does, from the state the last frame published.
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

    /// The language the dialog was built in is not the one showing now: release the static copy
    /// and build it again.
    fn rebuild_static_copy(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        if self.built_language == Some(language) {
            return Ok(());
        }
        let window = self.window;
        let filter_placeholder = t_smol!(self.translate, "widget.x.filter_placeholder");
        let query_placeholder = t_smol!(self.translate, "widget.x.search_placeholder");
        if let Some(input) = self.filter.as_ref() {
            input.update_placeholder(cx.app_mut(), filter_placeholder.clone());
            let _ = cx.app_mut().set_a11y_patch(
                window,
                input.root,
                A11yPatch {
                    label: Some(filter_placeholder),
                    ..A11yPatch::default()
                },
            );
        }
        if let Some(input) = self.query.as_ref() {
            input.update_placeholder(cx.app_mut(), query_placeholder.clone());
            let _ = cx.app_mut().set_a11y_patch(
                window,
                input.root,
                A11yPatch {
                    label: Some(query_placeholder),
                    ..A11yPatch::default()
                },
            );
        }
        // A spinner's accessible name is chosen when it is built as well: a language switch while
        // the search is out rebuilds it in the language showing now.
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

    /// Rebuilds the recovery control when the recovery the pane offers moved.
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

    /// Rebuilds one static-copy control with the language showing now.
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

    /// Builds one static-copy control into `parent`.
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
                let query_state = self.query_state;
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
                        offer_search(
                            ctx,
                            &query_state,
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

    /// Takes the control of one kind out of its field.
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

    /// Tells each list how many rows it draws, and rebuilds the visible ones when what a row has
    /// to be built for moved.
    fn sync_lists(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.sync_account_list(cx)?;
        self.sync_result_list(cx)?;
        Ok(())
    }

    /// The monitored list's rows: their number, their badges' language, and their faces.
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
        // Another set of rows is another place in the list: being given them is what puts the
        // reader back at the top. A rebuild of the rows themselves keeps the place they were at.
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

    /// The search list's rows, on the same terms.
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

    /// Syncs every component the dialog owns, in the order the engine expects, and pairs the row
    /// slots with the controls they are for.
    fn sync_components(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let window = self.window;
        if let Some(dialog) = self.dialog.as_ref() {
            dialog
                .sync(cx.app_mut())
                .map_err(|error| engine_error_of(error, window))?;
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

    /// Builds the search spinner when the page enters its searching window and releases it when
    /// that window closes, so a hidden spinner never keeps asking for frames (I3).
    fn sync_spinner(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if *cx.read(&self.searching) {
            if let Some(spinner) = self.spinner.as_ref() {
                spinner.sync(cx.app_mut())?;
            } else if let Some(parent) = self.spinner_parent {
                let key = format_smolstr!("{}:spinner", self.keys.dialog);
                // A spinner with no name is a decorative shape to assistive technology: the name
                // is what makes the searching window audible.
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

    /// Keeps the controls' enabled state in step with what the pane may do now.
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

    /// Pairs every mounted row with the control its slot is for, and releases the controls of
    /// the rows that are gone.
    fn sync_row_controls(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        let may_change = self.may_change();
        let window = self.window;
        let theme = self.theme.clone();
        let translate = self.translate.clone();

        // ── the monitored list's delete controls ─────────────────────────────
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

        // ── the search list's monitor controls ───────────────────────────────
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

    /// Reports what the last change said, in an alert over the dialog.
    ///
    /// What the pane holds is reported once: an alert is opened at the end of the frame that
    /// opens it and asks for a frame of its own, so a dialog that reported the same thing on the
    /// frame after that would be drawing frames for an alert rather than for the accounts it is
    /// there for. What clears the record is the pane holding nothing, which is what makes the
    /// next change reported whether or not it says what the last one did.
    fn report_notice(
        &mut self,
        cx: &mut WidgetCx<'_>,
        view: &DialogView,
    ) -> Result<(), EngineError> {
        match &self.reported {
            // The first frame records what the pane was already holding: it was said before this
            // dialog opened, so it is already reported as far as this dialog is concerned.
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

    /// Opens the alert one report is read in.
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
        let layer = cx.layer().clone();
        let animation = cx.animation().clone();
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
        .build(cx.app_mut(), theme, layer, animation, key);
        let window = self.window;
        alert
            .open(cx.app_mut(), window)
            .map_err(|error| engine_error_of(error, window))?;
        if let Some(previous) = self.alert.replace(alert) {
            previous.release(cx.app_mut());
        }
        Ok(())
    }

    /// Syncs the alert, and releases it once it has gone.
    fn sync_alert(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(alert) = self.alert.clone() else {
            return Ok(());
        };
        let window = self.window;
        alert
            .sync(cx.app_mut())
            .map_err(|error| engine_error_of(error, window))?;
        if !alert.is_open(cx.app()) {
            self.alert = None;
            alert.release(cx.app_mut());
        }
        Ok(())
    }

    /// Builds the dialog's own panel: the two pages over one another, and the way out.
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
                        .height(CONTENT_HEIGHT)
                        .flex_col()
                        .gap(8.0)
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

    /// The page that manages the accounts: the count and the controls over them, the filter, the
    /// list, and the sentence that stands in for it when there is nothing to list.
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

        // The field the accounts are filtered by, on the page that manages them.
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
                        .gap(8.0)
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
            placeholder: t_smol!(translate, "widget.x.filter_placeholder"),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
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
        let _ = app.set_a11y_patch(
            window,
            filter.root,
            A11yPatch {
                label: Some(t_smol!(translate, "widget.x.filter_placeholder")),
                ..A11yPatch::default()
            },
        );
        self.filter = Some(filter);

        // The accounts, inside the frame the page draws them in.
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
                        .top(0.0)
                        .right(0.0)
                        .bottom(0.0)
                        .left(0.0)
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

    /// The five sentences that stand in for the monitored list, each live while its own situation
    /// holds and none of them drawn over a list that has rows.
    fn build_monitored_messages(
        &mut self,
        app: &mut App,
        window: WindowId,
        area: NodeId,
        key: &str,
        text_size: f32,
    ) -> Result<(), EngineError> {
        let theme = self.theme.clone();
        let translate = self.translate.clone();
        let language = self.language;
        let signed_in = self.signed_in;
        let known = self.known;
        let count = self.count;
        let shown = self.shown_rows;

        // Nobody is signed in.
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

        // A read is on its way, whether it is out or waiting for the stream.
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

        // Nothing is holding the account, so a read has nothing to answer it: the way out is the
        // recovery the pane itself offers, which is the same control the pane shows behind this
        // dialog.
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

        // The read was answered with a failure, which the alert over the dialog names: what is
        // left to draw here is the way to read the accounts again.
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

        // The account monitors no account at all.
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

        // The filter leaves nothing of what the account does monitor.
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

    /// The page that searches X for one more account: the way back, the query and the control
    /// that sends it, the results, and what became of the search instead.
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
        // Enter in the field is the search control's keyboard path: the kit input dispatches the
        // action it is given, and the action re-checks at entry what the control checks, because
        // a disabled control is drawn disabled rather than enforced (AGENTS §4.2).
        let submit = Action::Custom(format_smolstr!("{}.submit", self.keys.query));
        let submit_query_state = self.query_state;
        let submit_ready = self.ready;
        let submit_operation = self.operation;
        let submit_searching = self.searching;
        let submit_search_request = self.search_request;
        app.on_action(submit.clone(), move |ctx| {
            offer_search(
                ctx,
                &submit_query_state,
                &submit_ready,
                &submit_operation,
                &submit_searching,
                &submit_search_request,
            );
        });
        self.query_submit = Some(submit.clone());
        let spec = Arc::new(InputSpec {
            name: self.keys.query.clone(),
            placeholder: t_smol!(translate, "widget.x.search_placeholder"),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
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
        let _ = app.set_a11y_patch(
            window,
            query.root,
            A11yPatch {
                label: Some(t_smol!(translate, "widget.x.search_placeholder")),
                ..A11yPatch::default()
            },
        );
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

        // The results, inside the frame the page draws them in.
        let frame = build_node(app, window, area, |tree, area| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:add:frame"))
                .style(
                    StyleBuilder::new()
                        .absolute()
                        .top(0.0)
                        .right(0.0)
                        .bottom(0.0)
                        .left(0.0)
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

    /// What the search page says instead of a list: the searching window, the words of a failure,
    /// and the answer that found nothing.
    fn build_add_messages(
        &mut self,
        app: &mut App,
        window: WindowId,
        area: NodeId,
        key: &str,
        text_size: f32,
    ) -> Result<(), EngineError> {
        let theme = self.theme.clone();
        let translate = self.translate.clone();
        let language = self.language;

        // Nobody is signed in: there is nothing either page's field could be asked about.
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

        // The searching window: a spinner, which exists only while it spins, and the sentence
        // that says what is being waited for.
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

        // A search that could not be answered is drawn in the words the failure itself gives.
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

        // The search was answered and found nothing: an answer, drawn where it was asked for.
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

    /// The count line both pages carry: how many accounts the account monitors, once it is known.
    fn heading_node(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        key: &str,
        title_size: f32,
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

    /// The way out of the dialog, which belongs to both pages and so is drawn once under them.
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
                        .gap(8.0)
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

    /// Takes the components out of the state, leaving the nodes to whoever releases them first
    /// (the dialog's overlay owns the content subtree).
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

/// The layout both pages share: a column filling the page area.
fn page_style() -> ui_core::paint::style::Style {
    StyleBuilder::new()
        .absolute()
        .top(0.0)
        .right(0.0)
        .bottom(0.0)
        .left(0.0)
        .flex_col()
        .gap(8.0)
        .min_width_0()
        .min_height_0()
        .build()
}

/// The layout a page's header row is drawn in.
fn header_style() -> ui_core::paint::style::Style {
    StyleBuilder::new()
        .width_full()
        .flex_none()
        .flex_row()
        .items_center()
        .gap(8.0)
        .min_width_0()
        .build()
}

/// The layout a page's toolbar row is drawn in.
fn toolbar_style() -> ui_core::paint::style::Style {
    StyleBuilder::new()
        .width_full()
        .flex_none()
        .flex_row()
        .items_center()
        .gap(8.0)
        .min_width_0()
        .build()
}

/// The query one field holds, once it is one a search could be asked with.
fn typed_text(signal: &Signal<InputState>, cx: &WidgetCx<'_>) -> Option<SmolStr> {
    let state = cx.read(signal);
    let typed = state.text().trim();
    (!typed.is_empty()).then(|| SmolStr::new(typed))
}

/// Whether a control may send a search, from the signals showing on this frame.
fn searchable_now(
    ctx: &ActionCtx<'_>,
    query_state: &Signal<InputState>,
    ready: &Signal<bool>,
    operation: &Signal<Option<SelectionOperation>>,
    searching: &Signal<bool>,
) -> Option<SmolStr> {
    let state = ctx.read(query_state);
    let typed = state.text().trim();
    if !valid_search_query(typed) {
        return None;
    }
    if !(*ctx.read(ready) && ctx.read(operation).is_none() && !*ctx.read(searching)) {
        return None;
    }
    Some(SmolStr::new(typed))
}

/// Offers the query the field holds to the pane, when the pane may search with it now.
///
/// The search control and Enter in the field both come through here: what the control is drawn
/// with is a hint, and the judgement that keeps a request from leaving is this action-entry check
/// (AGENTS §4.2).
fn offer_search(
    ctx: &mut ActionCtx<'_>,
    query_state: &Signal<InputState>,
    ready: &Signal<bool>,
    operation: &Signal<Option<SelectionOperation>>,
    searching: &Signal<bool>,
    search_request: &Signal<Option<SmolStr>>,
) {
    let Some(query) = searchable_now(ctx, query_state, ready, operation, searching) else {
        return;
    };
    ctx.write(search_request, Some(query));
    let window = ctx.window();
    let _ = ctx.app_mut().request_repaint(window);
}

/// What one report says: the title of the alert it is raised in, and what that alert is about.
///
/// Which way the change went is the title, and what the service answered is the message: a
/// failure is read in the words the failure itself gives, rather than in a sentence written
/// around it here.
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

/// What one search-result row needs to decide what its control does.
struct SearchRows<'a> {
    /// The accounts being monitored, which is what says whether this one already is.
    monitored: Option<&'a SelectionData>,
    /// The names those accounts are known by, which is what they are asked about by name.
    hints: &'a HashMap<SmolStr, SmolStr>,
    quota_full: bool,
    enabled: bool,
}

impl SearchRows<'_> {
    /// What the control beside one search result does.
    ///
    /// Whether an account is already monitored is the question the rows of the accounts list
    /// answer, asked of the same set by the same rule: there is no second list of names here that
    /// could say something the list itself would not.
    fn action(&self, username: &str) -> SearchProfileAction {
        let monitored = self
            .monitored
            .is_some_and(|data| data.contains_username(username, self.hints));
        SearchProfileAction::new(monitored, !self.enabled || self.quota_full)
    }
}

/// What the control beside one search result is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchProfileAction {
    /// The account may be monitored.
    Add { disabled: bool },
    /// The account is already monitored.
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

/// Which of the monitored accounts a filter leaves, in the order they are held.
///
/// The filter is a convenience over what is already on screen: an account is matched by its
/// username, its display name, the username it is waiting to be activated under, or any part of
/// its numeric id.
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
            // A row waiting to be activated is matched by the name it is drawn with, which is
            // the only one it has.
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

/// What one monitored account is called, and what it is called while it waits.
///
/// A row is named by what is known about it — its profile, or the username it was added under
/// while it waits to be activated — which is the same rule that decides whether an account is
/// already monitored, asked in the same place.
fn account_name(selection: &XSelection, hints: &HashMap<SmolStr, SmolStr>) -> SmolStr {
    match selection_name(selection, hints) {
        Some(name) => format_smolstr!("@{name}"),
        None => selection.x_user_id.clone(),
    }
}

/// How many followers an account has, at the width a row has for it.
///
/// A row is one line, so a count is read in the largest unit that keeps it under a thousand, to
/// one tenth: 1250 is "1.3K". A count whose tenth rounds up to the next unit reads as that unit —
/// 999_950 is "1M" rather than "1000.0K" — which is what makes a row's width depend on the number
/// rather than on the unit it happens to be near.
fn format_follower_count(count: u64) -> SmolStr {
    const UNITS: [(u64, &str); 3] = [(1_000, "K"), (1_000_000, "M"), (1_000_000_000, "B")];
    let mut formatted: Option<SmolStr> = None;
    for (index, (scale, suffix)) in UNITS.iter().enumerate() {
        if count < *scale {
            break;
        }
        let tenths = (count % scale) * 10 + scale / 2;
        if tenths >= scale * 10 {
            // The tenth carried into the unit above this one, and a larger unit that also fits
            // replaces whatever this turn would have written.
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

/// The signal a row builder reads: the accounts, and what they are named by.
struct AccountRows {
    key: SmolStr,
    /// The rows themselves, for the structure a row is built with.
    held: Rc<RefCell<Arc<[XSelection]>>>,
    /// The same rows as a signal, for the text the row's nodes follow.
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

/// The signal a search-result row builder reads: the profiles a search found.
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

/// What one monitored row's structure has to be told about, row by row.
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

/// One monitored account: which account it is, what is known about it, and the slot its delete
/// control goes in.
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
                .gap(4.0)
                .build(),
        )
        .build(tree, row)?;
    match selection.profile.as_ref() {
        // The profile line already names the account, so the name is not repeated above it.
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
        // An account that is waiting to be activated has no profile yet, so what is known about
        // it is its name, its state and its id.
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

    // The state of the account, which the header line of a profiled row does not carry.
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

/// One account a search found: who they are, and the slot the control that monitors them goes in.
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
                .gap(4.0)
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
    // Counts a search did not carry are both zero, and a line of zeros says nothing about the
    // account.
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
            // A count of one is the singular word: "1 follower".
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

/// The layout one row of either list is drawn in.
fn row_style() -> ui_core::paint::style::Style {
    StyleBuilder::new()
        .flex_row()
        .items_center()
        .gap(12.0)
        .width_full()
        .height_full()
        .min_width_0()
        .padding_x(8.0)
        .build()
}

/// The name line of one account: what it is called, and whether X vouches for it.
///
/// Both lists draw the same line over different rows, so the shape is built here once and what
/// the two texts read is each list's own.
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
                .gap(4.0)
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

/// The one line of secondary text under a name.
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

/// The state of one monitored account, as a badge.
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

/// The slot a row leaves for the control the dialog builds into it.
fn build_row_slot(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    key: &str,
    slots: &Rc<RefCell<BTreeMap<usize, NodeId>>>,
    size: f32,
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

/// A face, or the stand-in that says there is one to come.
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
        .width(ROW_AVATAR_SIZE)
        .height(ROW_AVATAR_SIZE)
        .flex_none()
        .build();
    // What can be drawn is what the pane registered; what the pane knows about the picture says
    // whether there is anything to wait for at all.
    let ready = !url.is_empty()
        && matches!(media.get(url), Some(MediaState::Ready(_)))
        && images.contains_key(url);
    let image = ready.then(|| images.get(url).copied()).flatten();
    let Some(image) = image else {
        // No bytes yet (or no face to fetch): a surface-toned tile with an `@` is what the old
        // interface drew for a face that had not arrived.
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

/// Draws one icon image at `size`, toned by a theme role.
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
            .width(size)
            .height(size)
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

/// One absolute block in a page's list area, centred, drawn while `visible` holds.
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
                    .top(0.0)
                    .right(0.0)
                    .bottom(0.0)
                    .left(0.0)
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(8.0)
                    .padding(8.0)
                    .build(),
            )
            .bind_style(visible_if(visible))
            .build(tree, area)
    })
}

/// One plain sentence standing in a message block.
fn sentence_node(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    id: SmolStr,
    text_size: f32,
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
                .max_width(MESSAGE_WIDTH)
                .min_width_0()
                .build(),
        )
        .bind_style(themed_foreground(theme, role))
        .bind_text(text)
        .build(tree, parent)
    })
}

/// A `bind_style` that only decides whether the node is drawn.
fn visible_if(
    visible: impl Fn(&mut ReadCtx<'_>) -> bool + 'static,
) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static {
    move |read| StylePatch {
        visible: Some(visible(read)),
        ..StylePatch::default()
    }
}

/// A dynamic foreground colour read through the theme handle.
fn themed_foreground(
    theme: ThemeHandle,
    role: Role,
) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static {
    move |read| StylePatch {
        foreground: Some(theme.role(read, role)),
        ..StylePatch::default()
    }
}

/// A dynamic background colour read through the theme handle.
fn themed_background(
    theme: ThemeHandle,
    role: Role,
) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static {
    move |read| StylePatch {
        background: Some(theme.role(read, role)),
        ..StylePatch::default()
    }
}

/// Removes one control's node, if it is still there.
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

/// Releases the components one dialog owns, the node-owning ones first (E16).
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

/// Whether `event` is one only a dialog's own text inputs can consume.
fn is_text_input(event: &Event) -> bool {
    matches!(
        event,
        Event::Input(InputEvent::Text { .. } | InputEvent::Key { .. }) | Event::Ime(_)
    )
}

/// Maps a kit dialog error onto the engine error the widget contract speaks.
fn engine_error_of(error: DialogError, window: WindowId) -> EngineError {
    match error {
        DialogError::Engine(error) | DialogError::Overlay(OverlayError::Engine(error)) => error,
        DialogError::Overlay(OverlayError::PlacementConfig { .. }) | DialogError::NotOpen => {
            EngineError::UnknownWindow { window }
        }
    }
}

/// Builds one node under `parent` with a window's tree in hand.
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
    use ui_core::event::{ActionSource, ImeEvent};
    use widget::test_support::WidgetTestApp;

    use super::*;
    use crate::view::SelectionFailure;

    /// How many production-shaped frames a test lets run: nodes built by the frame that created
    /// them are laid out and painted by the frames after.
    const SETTLE_FRAMES: usize = 8;

    fn translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::global::embedded::load().expect("the embedded locales should load")),
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

    /// The click a reader makes on one of the dialog's controls.
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

    /// Puts `text` in the dialog's search field.
    fn set_query(harness: &mut WidgetTestApp, dialog: &TweetDialog, text: &str) {
        let state = dialog.inner.borrow().query_state;
        harness.with_cx(|cx| {
            cx.write(&state, InputState::new(text));
        });
    }

    /// One monitored account, under an id and a name of the test's choosing.
    fn monitored(x_user_id: &str, username: Option<&str>) -> XSelection {
        XSelection {
            x_user_id: SmolStr::new(x_user_id),
            status: XSelectionStatus::Active,
            created_at: 1,
            updated_at: 2,
            profile: username.map(|username| profile(x_user_id, username)),
        }
    }

    /// One account a search could have found.
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

    /// The alert a change is reported in says which way it went, in either language.
    ///
    /// The title and the message are all an alert is made of, and a key a locale does not define
    /// translates to an empty string rather than failing: a typo in one of them would otherwise
    /// reach the user as an alert with a blank panel, which is what both are read for here.
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

    /// A change the pane reports while the dialog is open is raised once, in an alert over it,
    /// and a report the pane was already holding when the dialog opened is not raised at all.
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

        // Nothing typed is everything on screen, and a filter that matches nothing is nothing.
        assert_eq!(
            filtered_selection_indices(&selections, &hints, None),
            [0, 1, 2]
        );
        assert_eq!(
            filtered_selection_indices(&selections, &hints, Some("   ")),
            [0, 1, 2]
        );
        assert!(filtered_selection_indices(&selections, &hints, Some("nobody")).is_empty());

        // A username, a display name, a hint and a numeric id are all things a reader reads.
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

    /// Whether a search result is one already monitored is the question the monitored list
    /// answers, asked of the same rows by the same rule: the control beside a result never reads
    /// as monitored for a name the list itself would not show.
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

    /// The way out of the footer ends the dialog, and a dialog that has gone says so.
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

    /// Every control the dialog holds writes the request the pane reads: a read, a search, an
    /// account to monitor and one to stop monitoring.
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

    /// Releasing the dialog hands every value slot it minted back, and releasing it twice does
    /// not take anything else with it.
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

        // Opening and releasing again reuses the pool the first dialog sized: the capacity the
        // engine keeps is not the leak, growth across cycles is.
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

    /// The dialog's node with the given debug suffix (the page containers, the message
    /// blocks and the fields are addressed by the dialog's own key).
    fn node(dialog: &TweetDialog, harness: &WidgetTestApp, suffix: &str) -> NodeId {
        let key = dialog.inner.borrow().keys.dialog.clone();
        let prefix = format_smolstr!("{key}:{suffix}");
        harness
            .find_by_debug_prefix(harness.root(), &prefix)
            .unwrap_or_else(|| panic!("the dialog draws {suffix}"))
    }

    /// The text one of the two fields holds.
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

    /// The pages are one under the other with only the one being worked on drawn: the
    /// control on the managed page pushes the search page, and the way back pops it.
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

    /// A search that is out is drawn as a spinner, and the spinner exists only while the
    /// search is: a hidden spinner is released rather than spinning off screen (I3).
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

    /// The copy chosen when a control is built follows the language: the controls are
    /// rebuilt, and the fields' placeholders and accessible names with them.
    #[test]
    fn the_dialog_s_static_copy_should_follow_the_language() {
        let mut harness = harness();
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
            .as_ref()
            .map(|input| input.root)
            .expect("the managed page draws its filter");
        assert!(
            harness
                .text_of(filter)
                .contains(t_smol!(english, "widget.x.filter_placeholder").as_str()),
            "the field is placeheld in the language it was built in: {}",
            harness.text_of(filter)
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
                .text_of(filter)
                .contains(t_smol!(chinese, "widget.x.filter_placeholder").as_str()),
            "the field's placeholder follows the language: {}",
            harness.text_of(filter)
        );
    }

    /// The typed text and an IME commit reach the dialog's own fields, and only while the
    /// dialog is the window's modal scope: the fields are fed from the observation queue
    /// the engine leaves to a modal dialog's owner.
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
            .map(|input| input.root)
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

        // The other field is the search page's, and it is not the focused one.
        assert_eq!(
            typed(&harness, &dialog, true).as_str(),
            "",
            "text goes to the field the reader is in, not to both"
        );
    }

    /// An answer to a question the reader no longer has is not drawn: editing the query
    /// takes the answer away, and the answer to the query showing now is drawn.
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

        // The reader edits the query while an answer for the old one is what is drawn.
        set_query(&mut harness, &dialog, "someone-else");
        settle_frames(&mut harness, &mut dialog, &view);
        assert!(
            !harness.visible(node(&dialog, &harness, "add:list")),
            "an answer does not stand under a query it was not asked with"
        );

        // The answer to the old query, arriving late, is not drawn either.
        harness.with_cx(|cx| {
            dialog.answered_search(
                cx,
                &SmolStr::new_static("kytrix"),
                SearchAnswer::Found(Arc::from(vec![profile("99", "someone")])),
            );
        });
        settle_frames(&mut harness, &mut dialog, &view);
        assert!(!harness.visible(node(&dialog, &harness, "add:list")));

        // The answer to the query showing now is.
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

    /// A search that was answered and found nothing says so on the page that asked, rather
    /// than leaving the page blank or opening an alert.
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

    /// A search that could not be answered is drawn in the words the failure itself gives,
    /// on the page that asked — not as an alert over the dialog.
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

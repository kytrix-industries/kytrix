//! The dialog one pane opens to manage the wallet addresses it monitors.
//!
//! # What the dialog owns, and what it does not
//!
//! It owns what the user is typing, the sentence the last attempt produced, and the place a
//! reader is in the list — and the components those are drawn through. Everything else
//! belongs to the pane: which wallets are followed, whether one more may be added, and the
//! service that reads them. The dialog draws the pane's list and asks the pane to change it
//! rather than holding a copy of its own.
//!
//! # Why there is nothing to choose but an address
//!
//! Which of the venue's partitions a wallet is read from is the venue's answer rather than
//! the user's choice, so the dialog offers one decision — add this address, or stop
//! following that one — and the pane's list of addresses is its whole content.
//!
//! # One value channel into the dialog
//!
//! The pane is not reachable from the dialog: a kit control's callback owns no widget, and
//! the engine has no entity the two could hold each other through. So the dialog is built
//! over **signals**: the pane publishes what it holds into them every frame
//! ([`WalletAddressDialog::sync`]), and every dynamic value the dialog draws is a `bind_*`
//! closure over one of them. What the user does travels back as requests the pane applies
//! ([`DialogRequest`]) — adding and removing are the pane's own operations on its stored
//! address list.
//!
//! # The old dialog had no stream state to show
//!
//! A Hyperliquid wallet pane's addresses are its own persisted configuration, not a list a
//! service answers for, so the dialog has nothing to read and nothing that can be down: the
//! list the pane holds is drawn as it is, and the sentence about the last attempt is the
//! only state besides the typed text. The add control is disabled exactly when what the
//! field holds could not be monitored (an address that does not parse, one already on the
//! list, or one past the limit), which is the old `addable()` rule.
//!
//! # The row controls are built into rows the list owns
//!
//! A kit list's row factory is handed a [`NodeTree`] and nothing else: no application
//! context, and therefore no `Button`. The remove control of an address row is a kit button
//! all the same, so the row leaves an empty slot for it and the dialog builds the button
//! into that slot from [`WalletAddressDialog::sync`], after the list has mounted its rows.
//! A list that rebuilds its visible rows (a new address count) leaves new slots behind; the
//! sync pairs the buttons with the slots showing now and releases the ones whose row is
//! gone.
//!
//! # Static copy is rebuilt, not re-bound
//!
//! The kit components this dialog builds choose their labels when they are built, and a
//! language switch would otherwise leave them in the language the dialog was opened in.
//! The buttons the dialog holds (add and close) are therefore released and built again into
//! the same parent when the language moves — the same four steps
//! `crates/widgets/AGENTS.md` §15.1 freezes for static copy. The dialog's own title is the
//! one piece chosen at open (it is the kit dialog's accessible name); a language switch
//! while it is open leaves that name in the language it was opened in, which is registered
//! as a known difference in the W9a ledger.
//!
//! # The invalid-address sentence is the dialog's own
//!
//! The pane decides what a change did ([`AddressNotice`] is what it reports), but whether
//! what the user typed names a wallet at all is something this dialog answers itself: the
//! old dialog did the same, and a sentence about the field's own contents belongs to the
//! field. That refusal is [`AddressDialogError::Invalid`], shown on the notice line while
//! the pane has nothing to say, and cleared as soon as the field moves.
//!
//! # Why adding does not clear the field
//!
//! The address field is the user's, and nothing is lost by leaving it: the new address
//! appears in the list under the field, which disables the control that would add it again.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    rc::{Rc, Weak},
    sync::Arc,
};

use base_ui::{
    kit::{
        button::Button,
        dialog::{Dialog, DialogBuilder, DialogError},
        empty_state::EmptyState,
        icon::IconKind,
        input::{self, InputNode, InputSpec, InputState},
        list::{List, ListOptions, SelectionMode},
        overlay::OverlayError,
        separator::Separator,
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::StyleBuilder,
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_model::identifier::WalletAddress;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, Event, InputEvent},
    interaction::ActionCtx,
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::style::StylePatch,
    text::{TextOverflow, TextStyle},
    window::WindowId,
};
use widget::{cx::WidgetCx, error::TranslatedError};

/// The width of the address dialog's panel, as the old dialog's 620 px.
const DIALOG_WIDTH: f32 = 620.0;

/// The height of the panel's content column (the old dialog's 480 px panel less the title
/// row and padding the kit dialog's panel brings).
const CONTENT_HEIGHT: f32 = 400.0;

/// Why an address was not monitored.
///
/// The pane maps its own list rules onto the first two ([`crate::watch::WatchListError`]);
/// [`AddressDialogError::Invalid`] belongs to the dialog's field and never reaches the pane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AddressDialogError {
    /// What was typed is not a wallet address.
    Invalid,
    /// The address is on the list already.
    Duplicate,
    /// The list holds as many addresses as it may.
    Limit,
}

impl TranslatedError for AddressDialogError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Invalid => t_smol!(translate, "widget.hyperliquid_wallet.invalid_address"),
            Self::Duplicate => t_smol!(translate, "widget.hyperliquid_wallet.already_monitored"),
            Self::Limit => t_smol!(translate, "widget.hyperliquid_wallet.limit_reached"),
        }
    }
}

/// What the pane says about the last change to the list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum AddressNotice {
    /// Why the last change did not happen.
    Problem(AddressDialogError),
}

/// What one frame hands the dialog: the pane's list, as the dialog draws it.
#[derive(Clone)]
pub(super) struct DialogView<'a> {
    /// The addresses the pane follows, in the order it added them.
    pub addresses: &'a [WalletAddress],
    /// How many addresses the pane may follow.
    pub limit: usize,
    /// What the pane says about the last change, if anything.
    pub notice: Option<&'a AddressNotice>,
}

/// What the user asked the pane for through the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DialogRequest {
    /// Monitor one more address.
    ///
    /// The address is already the one the pane stores: the dialog parses what was typed,
    /// folds it to the venue's case and only asks for addresses it could monitor, so the
    /// pane applies the request as it stands.
    Add(WalletAddress),
    /// Stop monitoring one address.
    Delete(WalletAddress),
}

/// The dialog a pane opens to manage the wallets it monitors.
pub(super) struct WalletAddressDialog {
    inner: Rc<RefCell<DialogState>>,
}

impl WalletAddressDialog {
    /// Builds the dialog over the pane's window and opens it.
    ///
    /// The content is built against signals, so nothing about the pane is needed here: what
    /// the dialog draws is published by the first frame.
    ///
    /// # Errors
    /// The kit dialog could not be built or opened (the window is gone, or the content
    /// subtree could not be created).
    pub(super) fn open(cx: &mut WidgetCx<'_>) -> Result<Self, EngineError> {
        let inner = Rc::new(RefCell::new(DialogState::new(cx)));
        inner.borrow_mut().create_kit(cx);

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

    /// One frame of the dialog: follow the language, publish the pane's list, feed the field
    /// its typed events, and sync every component the dialog owns.
    ///
    /// What the user asked for since the last frame comes back as requests; the caller
    /// applies them to the pane and asks again next frame. Once the dialog has closed itself
    /// the frame draws nothing and reports nothing, and [`Self::is_open`] answers `false`.
    ///
    /// # Errors
    /// The kit dialog or one of its components failed to sync; the dialog has been released
    /// rather than left with dead nodes behind, and the caller may drop the handle.
    pub(super) fn sync(
        &mut self,
        cx: &mut WidgetCx<'_>,
        view: DialogView<'_>,
    ) -> Result<Vec<DialogRequest>, EngineError> {
        if !self.is_open(cx.app()) {
            return Ok(Vec::new());
        }
        let result = self.inner.borrow_mut().frame(cx, &view);
        if let Err(error) = result {
            log::warn!("the hyperliquid wallet address dialog could not be synced: {error}");
            self.release(cx);
            return Ok(Vec::new());
        }
        let requests = {
            let mut state = self.inner.borrow_mut();
            state.take_requests(cx)
        };
        Ok(requests)
    }

    /// Whether the dialog is on screen (opened, not closing, not released).
    ///
    /// A dialog closes itself — the close control, Esc, or its window going away — and the
    /// pane learns it here on the frame after the one that closed it.
    pub(super) fn is_open(&self, app: &App) -> bool {
        let state = self.inner.borrow();
        !state.released
            && !state.closed.get()
            && state
                .dialog
                .as_ref()
                .is_some_and(|dialog| dialog.is_open(app))
    }

    /// Releases the dialog's components and its signal scope. Idempotent.
    pub(super) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        let (components, scope) = {
            let mut state = self.inner.borrow_mut();
            if state.released {
                return;
            }
            state.released = true;
            (state.take_components(), state.scope)
        };
        // Nodes first (the dialog unmounts the content subtree), components second (E16).
        release_components(cx.app_mut(), components);
        let _ = cx.app_mut().release_scope(scope);
    }
}

/// The handles the crate's tests read a dialog's components through.
#[cfg(test)]
impl WalletAddressDialog {
    pub(super) fn test_dialog(&self) -> Option<Dialog> {
        self.inner.borrow().dialog.clone()
    }

    pub(super) fn test_input(&self) -> Option<InputNode> {
        self.inner.borrow().input.clone()
    }

    pub(super) fn test_add_button(&self) -> Option<Button> {
        self.inner.borrow().add.clone()
    }

    pub(super) fn test_close_button(&self) -> Option<Button> {
        self.inner.borrow().close.clone()
    }

    pub(super) fn test_delete_button(&self, index: usize) -> Option<Button> {
        self.inner
            .borrow()
            .delete_buttons
            .get(&index)
            .map(|entry| entry.button.clone())
    }

    pub(super) fn test_submit_action(&self) -> Action {
        self.inner.borrow().submit.clone()
    }
}

/// The parts one open dialog owns.
struct DialogState {
    window: WindowId,
    scope: SignalScope,
    theme: ThemeHandle,
    translate: Translate,
    language: Signal<LanguageCode>,
    keys: DialogKeys,
    /// The action the address field dispatches when Enter is pressed in it.
    submit: Action,
    /// The language the kit components (the buttons and the field's placeholder) were
    /// built in.
    built_language: Option<LanguageCode>,
    dialog: Option<Dialog>,
    list: Option<List>,
    input: Option<InputNode>,
    add: Option<Button>,
    close: Option<Button>,
    /// The remove control of each mounted address row, keyed by row index.
    delete_buttons: BTreeMap<usize, DeleteButton>,
    /// The slot each mounted address row left for its remove control.
    slots: Rc<RefCell<BTreeMap<usize, NodeId>>>,
    add_parent: Option<NodeId>,
    close_parent: Option<NodeId>,
    /// The number of address rows the list was last told to draw.
    shown_addresses: Option<usize>,
    /// The text the field held on the last frame, so the dialog can hear the user type and
    /// take back the sentence about the attempt before it.
    seen_text: Option<SmolStr>,
    addresses: Signal<Arc<[WalletAddress]>>,
    limit: Signal<usize>,
    notice: Signal<Option<AddressNotice>>,
    /// The dialog's own refusal of what the field holds, shown while the pane says nothing.
    refusal: Signal<Option<AddressDialogError>>,
    input_state: Signal<InputState>,
    input_placeholder: Signal<SmolStr>,
    /// The empty list's sentence, which a language switch rewrites in place.
    empty_text: Signal<SmolStr>,
    add_request: Signal<Option<WalletAddress>>,
    delete_request: Signal<Option<WalletAddress>>,
    released: bool,
    closed: Rc<Cell<bool>>,
}

/// The kit keys one dialog mints from its instance identity.
#[derive(Clone)]
struct DialogKeys {
    dialog: SmolStr,
    list: SmolStr,
    input: SmolStr,
    submit: SmolStr,
    add: SmolStr,
    close: SmolStr,
    empty: SmolStr,
}

/// One mounted row's remove control.
struct DeleteButton {
    slot: NodeId,
    language: LanguageCode,
    button: Button,
}

/// The components one dialog owns, taken as one set on release.
struct DialogComponents {
    dialog: Option<Dialog>,
    input: Option<InputNode>,
    /// The field's submit action handler, which is the dialog's own registration.
    submit: Option<Action>,
    list: Option<List>,
    add: Option<Button>,
    close: Option<Button>,
    delete_buttons: Vec<Button>,
}

/// The static-copy controls the dialog rebuilds when the language moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogButtonKind {
    Add,
    Close,
}

/// What the field holds, as the add control makes it out.
#[derive(Clone, Debug, Eq, PartialEq)]
enum TypedAddress {
    /// The field holds nothing, so there is nothing to refuse.
    Empty,
    /// Monitor this address.
    Offer(WalletAddress),
    /// The address could not be monitored, and this is why.
    Refused(AddressDialogError),
}

impl DialogState {
    /// Builds the dialog's signals and keys. No nodes and no kit components yet.
    fn new(cx: &mut WidgetCx<'_>) -> Self {
        let scope = cx.app_mut().signal_scope();
        let language = cx.language();
        let theme = cx.theme().clone();
        let translate = cx.translate().clone();
        let keys = DialogKeys {
            dialog: cx.unique_key("hyperliquid_wallet.dialog"),
            list: cx.unique_key("hyperliquid_wallet.dialog.addresses"),
            input: cx.unique_key("hyperliquid_wallet.dialog.field"),
            submit: cx.unique_key("hyperliquid_wallet.dialog.field.submit"),
            add: cx.unique_key("hyperliquid_wallet.dialog.add"),
            close: cx.unique_key("hyperliquid_wallet.dialog.close"),
            empty: cx.unique_key("hyperliquid_wallet.dialog.empty"),
        };
        let placeholder = t_smol!(translate, "widget.hyperliquid_wallet.placeholder");
        let empty_text = t_smol!(translate, "widget.hyperliquid_wallet.empty_monitored");
        let submit = Action::Custom(format_smolstr!("{}.submit", keys.submit));
        let addresses: Arc<[WalletAddress]> = Arc::from([]);
        Self {
            window: cx.window(),
            scope,
            theme,
            translate,
            language,
            keys,
            submit,
            built_language: Some(*cx.read(&language)),
            dialog: None,
            list: None,
            input: None,
            add: None,
            close: None,
            delete_buttons: BTreeMap::new(),
            slots: Rc::new(RefCell::new(BTreeMap::new())),
            add_parent: None,
            close_parent: None,
            shown_addresses: None,
            seen_text: None,
            addresses: cx.app_mut().signal_scoped(scope, addresses),
            limit: cx.app_mut().signal_scoped(scope, 0usize),
            notice: cx.app_mut().signal_scoped(scope, None),
            refusal: cx.app_mut().signal_scoped(scope, None),
            input_state: cx.app_mut().signal_scoped(scope, InputState::new("")),
            input_placeholder: cx.app_mut().signal_scoped(scope, placeholder),
            empty_text: cx.app_mut().signal_scoped(scope, empty_text),
            add_request: cx.app_mut().signal_scoped(scope, None),
            delete_request: cx.app_mut().signal_scoped(scope, None),
            released: false,
            closed: Rc::new(Cell::new(false)),
        }
    }

    /// Builds the list the dialog draws its addresses with, and registers the action its
    /// address field dispatches on Enter. Called before the dialog is opened, because the
    /// dialog's content mounts the list.
    fn create_kit(&mut self, cx: &mut WidgetCx<'_>) {
        let addresses = self.addresses;
        let slots = Rc::clone(&self.slots);
        let theme = self.theme.clone();
        let key = self.keys.list.clone();
        let row_height = UnifiedSize::Medium.h();
        let list = List::new(
            cx.app_mut(),
            theme.clone(),
            key.clone(),
            ListOptions::fixed(row_height)
                .selection(SelectionMode::None)
                .padding(0.0, 0.0),
            move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                build_address_row(tree, parent, index, &key, &addresses, &slots, &theme)
            },
        );
        list.set_initial_count(cx.app_mut(), 0);
        self.shown_addresses = Some(0);
        self.list = Some(list);

        // Enter in the address field is the add control by another route: the same
        // judgement, made again at the action entry rather than trusting the control's
        // disabled state (AGENTS §4.2).
        let input_state = self.input_state;
        let addresses = self.addresses;
        let limit = self.limit;
        let add_request = self.add_request;
        let refusal = self.refusal;
        cx.app_mut().on_action(self.submit.clone(), move |ctx| {
            offer_typed_address(
                ctx,
                &input_state,
                &addresses,
                &limit,
                &add_request,
                &refusal,
            );
        });
    }

    /// Builds the kit dialog around this dialog's content.
    fn build_dialog(inner: &Weak<RefCell<DialogState>>, cx: &mut WidgetCx<'_>) -> Dialog {
        let (theme, layer, animation, key, title, closed) = {
            let state = inner
                .upgrade()
                .expect("the dialog is built while its state is alive");
            let state = state.borrow();
            (
                state.theme.clone(),
                cx.layer().clone(),
                cx.animation().clone(),
                state.keys.dialog.clone(),
                t_smol!(
                    state.translate,
                    "widget.hyperliquid_wallet.manage_addresses"
                ),
                Rc::clone(&state.closed),
            )
        };
        let content_weak = Weak::clone(inner);
        DialogBuilder::new(title)
            .width(DIALOG_WIDTH)
            .close_button(false)
            .content(move |app, window, body| {
                let Some(state) = content_weak.upgrade() else {
                    return Ok(());
                };
                state.borrow_mut().build_content(app, window, body)
            })
            .on_close(move |_reason, _app| closed.set(true))
            .build(cx.app_mut(), theme, layer, animation, key)
    }

    /// Publishes the pane's list to the signals the dialog's nodes read.
    fn publish(&mut self, cx: &mut WidgetCx<'_>, view: &DialogView<'_>) {
        let addresses: Arc<[WalletAddress]> = Arc::from(view.addresses);
        let _ = cx.write(&self.addresses, addresses);
        let _ = cx.write(&self.limit, view.limit);
        let _ = cx.write(&self.notice, view.notice.cloned());
    }

    /// Tells the list how many addresses the pane holds.
    fn show_addresses(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let count = cx.read(&self.addresses).len();
        if self.shown_addresses == Some(count) {
            return Ok(());
        }
        let Some(list) = self.list.as_ref() else {
            return Ok(());
        };
        list.set_items(cx.app_mut(), count)?;
        self.shown_addresses = Some(count);
        let _ = cx.request_frame();
        Ok(())
    }

    /// The language the dialog was built in is not the one showing now: release the static
    /// copy and build it again.
    fn rebuild_static_copy(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        if self.built_language == Some(language) {
            return Ok(());
        }
        let placeholder = t_smol!(self.translate, "widget.hyperliquid_wallet.placeholder");
        if let Some(input) = self.input.as_ref() {
            input.update_placeholder(cx.app_mut(), placeholder);
        }
        let window = cx.window();
        let app = cx.app_mut();
        for (parent, kind) in [
            (self.add_parent, DialogButtonKind::Add),
            (self.close_parent, DialogButtonKind::Close),
        ] {
            let Some(parent) = parent else {
                continue;
            };
            if app
                .window(window)
                .is_none_or(|target| target.tree().get(parent).is_err())
            {
                continue;
            }
            if let Some(button) = self.take_button(kind) {
                remove_button_node(app, window, &button)?;
                let _ = button.release(app);
            }
            self.build_button(app, window, parent, kind)?;
        }
        let empty_text = t_smol!(self.translate, "widget.hyperliquid_wallet.empty_monitored");
        let _ = cx.write(&self.empty_text, empty_text);
        self.built_language = Some(language);
        Ok(())
    }

    /// Builds one static-copy control into `parent`.
    fn build_button(
        &mut self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        kind: DialogButtonKind,
    ) -> Result<(), EngineError> {
        let theme = self.theme.clone();
        let translate = self.translate.clone();
        match kind {
            DialogButtonKind::Add => {
                let label = t_smol!(translate, "widget.hyperliquid_wallet.add");
                let input_state = self.input_state;
                let addresses = self.addresses;
                let limit = self.limit;
                let add_request = self.add_request;
                let refusal = self.refusal;
                let button = Button::new(app, theme, self.keys.add.clone())
                    .primary()
                    .small()
                    .icon(IconKind::Plus)
                    .a11y_label(label.clone())
                    .tooltip_text(label)
                    .on_click(move |ctx| {
                        offer_typed_address(
                            ctx,
                            &input_state,
                            &addresses,
                            &limit,
                            &add_request,
                            &refusal,
                        );
                    });
                button.build(app, window, parent)?;
                self.add = Some(button);
            }
            DialogButtonKind::Close => {
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
        }
        Ok(())
    }

    /// Takes the control of one kind out of its field.
    fn take_button(&mut self, kind: DialogButtonKind) -> Option<Button> {
        match kind {
            DialogButtonKind::Add => self.add.take(),
            DialogButtonKind::Close => self.close.take(),
        }
    }

    /// The address the add control would carry, from the signals showing this frame.
    fn addable_now(&self, cx: &WidgetCx<'_>) -> bool {
        let typed = SmolStr::new(cx.read(&self.input_state).text().trim());
        let addresses = cx.read(&self.addresses);
        matches!(
            judge_typed_address(&typed, *cx.read(&self.limit), &addresses[..]),
            TypedAddress::Offer(_)
        )
    }

    /// One frame: rebuild static copy, publish, feed the field, sync the components.
    fn frame(&mut self, cx: &mut WidgetCx<'_>, view: &DialogView<'_>) -> Result<(), EngineError> {
        if self.released || self.closed.get() {
            return Ok(());
        }
        self.rebuild_static_copy(cx)?;
        self.publish(cx, view);
        self.clear_stale_refusal(cx);
        self.dispatch_text_events(cx);
        self.show_addresses(cx)?;
        self.sync_components(cx)?;
        if !self
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.is_open(cx.app()))
        {
            self.closed.set(true);
        }
        Ok(())
    }

    /// Takes back the dialog's own refusal as soon as the user types something else: the
    /// sentence is about what the field held, not about the field.
    fn clear_stale_refusal(&mut self, cx: &mut WidgetCx<'_>) {
        let text = SmolStr::new(cx.read(&self.input_state).text());
        if self.seen_text.as_ref() == Some(&text) {
            return;
        }
        self.seen_text = Some(text);
        if cx.read(&self.refusal).is_some() {
            let _ = cx.write(&self.refusal, None);
        }
    }

    /// Feeds this dialog's own typing to its address field.
    ///
    /// A dialog owns **the typed text and key presses of its own window, and only while it
    /// is that window's modal scope**: the engine has already dispatched the interaction
    /// events, so what is left in the observation queue is what a text input can consume.
    fn dispatch_text_events(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(input) = self.input.as_ref() else {
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
            let _ = input.dispatch(cx.app_mut(), &event);
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

    /// Syncs every component the dialog owns, in the order the engine expects.
    fn sync_components(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if let Some(dialog) = self.dialog.as_ref() {
            dialog
                .sync(cx.app_mut())
                .map_err(|error| engine_error_of(error, self.window))?;
        }
        if let Some(list) = self.list.as_ref() {
            list.sync(cx.app_mut())?;
        }
        if let Some(input) = self.input.as_ref() {
            input.sync(cx.app_mut());
        }
        self.sync_delete_buttons(cx)?;
        self.sync_add_button(cx);
        Ok(())
    }

    /// Keeps the add control's enabled state in step with what the field holds.
    fn sync_add_button(&self, cx: &mut WidgetCx<'_>) {
        if let Some(add) = self.add.as_ref() {
            let disabled = !self.addable_now(cx);
            let _ = add.set_disabled(cx.app_mut(), disabled);
        }
    }

    /// Pairs every mounted address row with its remove control.
    fn sync_delete_buttons(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(list) = self.list.as_ref() else {
            return Ok(());
        };
        let mounted: BTreeSet<usize> = list
            .mounted_rows(cx.app())
            .into_iter()
            .map(|(index, _)| index)
            .collect();
        let language = *cx.read(&self.language);

        let stale: Vec<usize> = self
            .delete_buttons
            .keys()
            .copied()
            .filter(|index| !mounted.contains(index))
            .collect();
        for index in stale {
            if let Some(entry) = self.delete_buttons.remove(&index) {
                let window = cx.window();
                remove_button_node(cx.app_mut(), window, &entry.button)?;
                let _ = entry.button.release(cx.app_mut());
            }
        }

        let addresses = self.addresses;
        let delete_request = self.delete_request;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        let window = cx.window();
        for index in mounted {
            let slot = { self.slots.borrow().get(&index).copied() };
            let Some(slot) = slot else {
                continue;
            };
            let replace = self
                .delete_buttons
                .get(&index)
                .is_none_or(|entry| entry.slot != slot || entry.language != language);
            if !replace {
                continue;
            }
            if let Some(entry) = self.delete_buttons.remove(&index) {
                remove_button_node(cx.app_mut(), window, &entry.button)?;
                let _ = entry.button.release(cx.app_mut());
            }
            let address = cx
                .read(&addresses)
                .get(index)
                .map_or_else(SmolStr::default, |address| SmolStr::new(address.as_str()));
            let label = t_smol!(
                translate,
                "widget.hyperliquid_wallet.remove_address",
                address = address
            );
            let key = cx.unique_key(&format_smolstr!("hyperliquid_wallet.dialog.delete.{index}"));
            let button = Button::new(cx.app_mut(), theme.clone(), key)
                .ghost()
                .small()
                .icon(IconKind::Trash)
                .a11y_label(label.clone())
                .tooltip_text(label)
                .on_click(move |ctx| {
                    let address = ctx.read(&addresses).get(index).cloned();
                    if let Some(address) = address {
                        ctx.write(&delete_request, Some(address));
                        let window = ctx.window();
                        let _ = ctx.app_mut().request_repaint(window);
                    }
                });
            button.build(cx.app_mut(), window, slot)?;
            self.delete_buttons.insert(
                index,
                DeleteButton {
                    slot,
                    language,
                    button,
                },
            );
        }
        Ok(())
    }

    /// Builds the dialog's own panel: the count row, the address field and its add control,
    /// the sentence about the last attempt, the address list and the footer.
    fn build_content(
        &mut self,
        app: &mut App,
        window: WindowId,
        body: NodeId,
    ) -> Result<(), EngineError> {
        let Some(list) = self.list.clone() else {
            // The dialog is being torn down; there is nothing to mount into.
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
                        .height(CONTENT_HEIGHT)
                        .flex_col()
                        .gap(8.0)
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .build(tree, body)
        })?;

        // The count row: what this dialog manages, and how many of them the pane holds.
        let header = build_node(app, window, panel, |tree, panel| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:header"))
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .flex_none()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap(8.0)
                        .min_width_0()
                        .build(),
                )
                .build(tree, panel)
        })?;
        self.heading_nodes(app, window, header, &key, text_size)?;

        // The address field and the control that monitors what it holds.
        let toolbar = build_node(app, window, panel, |tree, panel| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:toolbar"))
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .flex_none()
                        .flex_row()
                        .items_center()
                        .gap(8.0)
                        .min_width_0()
                        .build(),
                )
                .build(tree, panel)
        })?;
        let field_holder = build_node(app, window, toolbar, |tree, toolbar| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:field-holder"))
                .style(StyleBuilder::new().flex_1().min_width_0().build())
                .build(tree, toolbar)
        })?;
        let spec = Arc::new(InputSpec {
            name: self.keys.input.clone(),
            placeholder: SmolStr::default(),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            size: ControlSize::Medium,
            submit: Some(self.submit.clone()),
        });
        let input = {
            let tree = app
                .window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut();
            input::build(
                tree,
                field_holder,
                spec,
                &theme,
                &self.input_state,
                &self.input_placeholder,
            )
        };
        input.install(app);
        self.input = Some(input);

        let add_holder = build_node(app, window, toolbar, |tree, toolbar| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:add"))
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, toolbar)
        })?;
        self.add_parent = Some(add_holder);
        self.build_button(app, window, add_holder, DialogButtonKind::Add)?;

        // The line the last attempt is reported on: the pane's answer, or the dialog's own
        // refusal of what the field holds.
        let notice = self.notice;
        let refusal = self.refusal;
        let notice_language = self.language;
        let notice_translate = translate;
        let notice_theme = theme.clone();
        build_node(app, window, panel, |tree, panel| {
            NodeBuilder::text(
                SmolStr::default(),
                TextStyle::new(text_size).with_overflow(TextOverflow::Ellipsis, 1),
            )
            .id(format_smolstr!("{key}:notice"))
            .style(
                StyleBuilder::new()
                    .width_full()
                    .flex_none()
                    .min_width_0()
                    .build(),
            )
            .bind_style(move |cx| {
                let pane_notice = cx.read(&notice);
                let own_refusal = *cx.read(&refusal);
                let (visible, foreground) = match (pane_notice.as_ref(), own_refusal) {
                    (Some(AddressNotice::Problem(_)), _) | (None, Some(_)) => {
                        (true, Some(notice_theme.role(cx, Role::StatusDangerBase)))
                    }
                    (None, None) => (false, Some(notice_theme.role(cx, Role::FgMuted))),
                };
                StylePatch {
                    foreground,
                    visible: Some(visible),
                    ..StylePatch::default()
                }
            })
            .bind_text(move |cx| {
                let _language = cx.read(&notice_language);
                let pane_notice = cx.read(&notice);
                match (pane_notice.as_ref(), *cx.read(&refusal)) {
                    (Some(AddressNotice::Problem(failure)), _) => {
                        failure.translated_message(&notice_translate)
                    }
                    (None, Some(failure)) => failure.translated_message(&notice_translate),
                    (None, None) => SmolStr::default(),
                }
            })
            .build(tree, panel)
        })?;

        // The addresses, or the sentence that stands in for them.
        let area = build_node(app, window, panel, |tree, panel| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:area"))
                .style(
                    StyleBuilder::new()
                        .flex_1()
                        .min_width_0()
                        .min_height_0()
                        .relative()
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, panel)
        })?;

        let language = self.language;
        let a11y_translate = self.translate.clone();
        let list_holder = build_node(app, window, area, |tree, area| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:addresses"))
                .a11y(A11yOverride {
                    role: Some(A11yRole::ScrollView),
                    ..A11yOverride::default()
                })
                .style(
                    StyleBuilder::new()
                        .size_full()
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .bind_a11y(move |cx| {
                    let _language = cx.read(&language);
                    A11yPatch {
                        label: Some(t_smol!(
                            a11y_translate,
                            "widget.hyperliquid_wallet.scroll_addresses"
                        )),
                        ..A11yPatch::default()
                    }
                })
                .build(tree, area)
        })?;
        list.mount(app, window, list_holder)?;

        self.build_empty_block(app, window, area, &key)?;

        // The way out of the dialog.
        let footer = build_node(app, window, panel, |tree, panel| {
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
                .build(tree, panel)
        })?;
        build_node(app, window, footer, |tree, footer| {
            Separator::horizontal(theme.clone()).build(tree, footer)
        })?;
        let close_holder = build_node(app, window, footer, |tree, footer| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:close"))
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
        self.build_button(app, window, close_holder, DialogButtonKind::Close)?;
        Ok(())
    }

    /// The two static lines of the count row: what the dialog manages, and how many
    /// addresses the pane holds against the limit.
    fn heading_nodes(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        key: &str,
        text_size: f32,
    ) -> Result<(), EngineError> {
        let language = self.language;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        build_node(app, window, parent, |tree, parent| {
            NodeBuilder::text(
                SmolStr::default(),
                TextStyle::new(text_size).with_overflow(TextOverflow::Ellipsis, 1),
            )
            .id(format_smolstr!("{key}:heading"))
            .style(StyleBuilder::new().flex_1().min_width_0().build())
            .bind_style(move |cx| StylePatch {
                foreground: Some(theme.role(cx, Role::FgDefault)),
                ..StylePatch::default()
            })
            .bind_text(move |cx| {
                let _language = cx.read(&language);
                t_smol!(translate, "widget.hyperliquid_wallet.manage_addresses")
            })
            .build(tree, parent)
        })?;

        let addresses = self.addresses;
        let limit = self.limit;
        let language = self.language;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        build_node(app, window, parent, |tree, parent| {
            NodeBuilder::text(
                SmolStr::default(),
                TextStyle::new(text_size).with_overflow(TextOverflow::Ellipsis, 1),
            )
            .id(format_smolstr!("{key}:count"))
            .style(StyleBuilder::new().flex_none().build())
            .bind_style(move |cx| StylePatch {
                foreground: Some(theme.role(cx, Role::FgMuted)),
                ..StylePatch::default()
            })
            .bind_text(move |cx| {
                let _language = cx.read(&language);
                let count = cx.read(&addresses).len();
                let limit = *cx.read(&limit);
                t_smol!(
                    translate,
                    "widget.hyperliquid_wallet.monitored_count",
                    current = count,
                    limit = limit
                )
            })
            .build(tree, parent)
        })?;
        Ok(())
    }

    /// The sentence standing in for an empty address list.
    ///
    /// The pane's own placeholder uses `empty_addresses`, and that copy points at the
    /// settings control in the top-left corner — which is the control that opened *this*
    /// dialog. Reusing it here would tell a reader who is already inside that dialog to go
    /// and open it, so the dialog says `empty_monitored` instead, as the old one did.
    fn build_empty_block(
        &self,
        app: &mut App,
        window: WindowId,
        area: NodeId,
        key: &str,
    ) -> Result<(), EngineError> {
        let addresses = self.addresses;
        let empty = message_block(
            app,
            window,
            area,
            format_smolstr!("{key}:empty"),
            move |cx| StylePatch {
                visible: Some(cx.read(&addresses).is_empty()),
                ..StylePatch::default()
            },
        )?;
        let empty_state =
            EmptyState::new(self.theme.clone(), self.keys.empty.clone()).bind_text(self.empty_text);
        let tree = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut();
        empty_state.build(tree, empty)?;
        Ok(())
    }

    /// Takes the components out of the state, leaving the nodes to whoever releases them
    /// first (the dialog's overlay owns the content subtree).
    fn take_components(&mut self) -> DialogComponents {
        let delete_buttons = std::mem::take(&mut self.delete_buttons)
            .into_values()
            .map(|entry| entry.button)
            .collect();
        DialogComponents {
            dialog: self.dialog.take(),
            input: self.input.take(),
            submit: Some(self.submit.clone()),
            list: self.list.take(),
            add: self.add.take(),
            close: self.close.take(),
            delete_buttons,
        }
    }

    /// Takes the requests the user made since the last call, each exactly once.
    fn take_requests(&mut self, cx: &mut WidgetCx<'_>) -> Vec<DialogRequest> {
        let mut requests = Vec::with_capacity(1);
        let add = cx.read(&self.add_request).clone();
        if let Some(address) = add {
            cx.write(&self.add_request, None);
            requests.push(DialogRequest::Add(address));
        }
        let delete = cx.read(&self.delete_request).clone();
        if let Some(address) = delete {
            cx.write(&self.delete_request, None);
            requests.push(DialogRequest::Delete(address));
        }
        requests
    }
}

/// A wallet address as a person types it: forty hexadecimal digits, with or without the prefix.
///
/// The venue writes an address with `0x`, and a person pasting one may or may not: what is
/// refused is anything that is not forty hexadecimal digits, which is a shape rather than a
/// checksum. An address is the same address in either case, and the list the pane stores is
/// what the venue writes (lower case), so the digits are folded before they are read — a
/// reader pasting the address their wallet shows is pasting the address they meant.
fn parsed_address(typed: Option<&str>) -> Option<WalletAddress> {
    let typed = typed?.trim();
    let digits = typed
        .strip_prefix("0x")
        .or_else(|| typed.strip_prefix("0X"))
        .unwrap_or(typed);
    if digits.len() != 40 || !digits.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    format_smolstr!("0x{}", digits.to_ascii_lowercase())
        .parse()
        .ok()
}

/// What the field holds, as the add control makes it out: the address it would ask the pane
/// for, or why it would not ask.
fn judge_typed_address(typed: &str, limit: usize, addresses: &[WalletAddress]) -> TypedAddress {
    let typed = typed.trim();
    if typed.is_empty() {
        return TypedAddress::Empty;
    }
    let Some(address) = parsed_address(Some(typed)) else {
        return TypedAddress::Refused(AddressDialogError::Invalid);
    };
    if addresses.contains(&address) {
        return TypedAddress::Refused(AddressDialogError::Duplicate);
    }
    if addresses.len() >= limit {
        return TypedAddress::Refused(AddressDialogError::Limit);
    }
    TypedAddress::Offer(address)
}

/// Offers the address the field holds to the pane, when the pane may monitor it now.
///
/// The add control and Enter in the field both come through here: what the control is drawn
/// with (`addable_now`) is a hint, and the judgement that keeps a request from leaving is the
/// action-entry check (`judge_typed_address`, AGENTS §4.2). What is refused is said on the
/// notice line rather than swallowed: the control is drawn disabled, and a disabled kit
/// control does not stop its handler from running.
fn offer_typed_address(
    ctx: &mut ActionCtx<'_>,
    input_state: &Signal<InputState>,
    addresses: &Signal<Arc<[WalletAddress]>>,
    limit: &Signal<usize>,
    add_request: &Signal<Option<WalletAddress>>,
    refusal: &Signal<Option<AddressDialogError>>,
) {
    let text = SmolStr::new(ctx.read(input_state).text().trim());
    let addresses = ctx.read(addresses);
    let judged = judge_typed_address(&text, *ctx.read(limit), &addresses[..]);
    match judged {
        TypedAddress::Empty => {}
        TypedAddress::Offer(address) => {
            ctx.write(refusal, None);
            ctx.write(add_request, Some(address));
            let window = ctx.window();
            let _ = ctx.app_mut().request_repaint(window);
        }
        TypedAddress::Refused(reason) => {
            ctx.write(refusal, Some(reason));
            let window = ctx.window();
            let _ = ctx.app_mut().request_repaint(window);
        }
    }
}

/// One address row of the list: the address, and the slot its remove control goes in.
fn build_address_row(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    key: &str,
    addresses: &Signal<Arc<[WalletAddress]>>,
    slots: &Rc<RefCell<BTreeMap<usize, NodeId>>>,
    theme: &ThemeHandle,
) -> Result<(), EngineError> {
    let row_theme = theme.clone();
    // The kit list's own row container is the list option this content is drawn in, so the
    // content root declares no role of its own: `A11yRole::Row` belongs to real tables and
    // grids only (ui_core `a11y.rs`), and the old address row declared none (W8a ledger §6.8).
    let row = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:row:{index}"))
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(8.0)
                .width_full()
                .min_width_0()
                .build(),
        )
        .build(tree, parent)?;

    let text_addresses = *addresses;
    let text_theme = row_theme;
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(UnifiedSize::Medium.text_size()).with_overflow(TextOverflow::Ellipsis, 1),
    )
    .id(format_smolstr!("{key}:row:{index}:address"))
    .style(StyleBuilder::new().flex_1().min_width_0().build())
    .bind_text(move |cx| {
        cx.read(&text_addresses)
            .get(index)
            .map_or_else(SmolStr::default, |address| SmolStr::new(address.as_str()))
    })
    .bind_style(move |cx| StylePatch {
        foreground: Some(text_theme.role(cx, Role::FgDefault)),
        ..StylePatch::default()
    })
    .build(tree, row)?;

    let slot = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:row:{index}:actions"))
        .style(
            StyleBuilder::new()
                .width(UnifiedSize::Small.h())
                .height_full()
                .flex_none()
                .items_center()
                .justify_center()
                .build(),
        )
        .build(tree, row)?;
    slots.borrow_mut().insert(index, slot);

    Ok(())
}

/// One absolute overlay in the list area, centered.
fn message_block(
    app: &mut App,
    window: WindowId,
    area: NodeId,
    id: SmolStr,
    visible: impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static,
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
            .bind_style(visible)
            .build(tree, area)
    })
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

/// Releases the components one dialog owns, node-owning component first (E16).
fn release_components(app: &mut App, components: DialogComponents) {
    if let Some(dialog) = components.dialog {
        dialog.release(app);
    }
    if let Some(input) = components.input {
        input.release(app);
    }
    if let Some(action) = components.submit {
        app.off_action(&action);
    }
    if let Some(list) = components.list {
        list.release(app);
    }
    for button in [components.add, components.close].into_iter().flatten() {
        button.release(app);
    }
    for button in components.delete_buttons {
        button.release(app);
    }
}

/// Whether `event` is one only a dialog's own text input can consume.
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

    use i18n::{language::codes::LanguageCode, t_smol};
    use ui_core::event::{ActionSource, KeyCode, Modifiers};
    use widget::test_support::WidgetTestApp;

    use super::*;

    /// Two wallets written the way the venue writes them (lower-case digits).
    const FIRST: &str = "0x1111111111111111111111111111111111111111";
    const SECOND: &str = "0x2222222222222222222222222222222222222222";
    /// One address in the two cases a wallet may show it in.
    const CHECKSUMMED: &str = "0x52908400098527886E0F7030069857D2E4169EE7";
    const CANONICAL: &str = "0x52908400098527886e0f7030069857d2e4169ee7";
    const SETTLE_FRAMES: usize = 8;

    fn translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    fn harness() -> WidgetTestApp {
        WidgetTestApp::with_translate(translate(LanguageCode::En))
    }

    fn address(digits: &str) -> WalletAddress {
        format_smolstr!("0x{digits}")
            .parse()
            .expect("the test address is a wallet address")
    }

    fn wallet(text: &str) -> WalletAddress {
        text.parse().expect("the test address is a wallet address")
    }

    fn dialog_view(addresses: &[WalletAddress], limit: usize) -> DialogView<'_> {
        DialogView {
            addresses,
            limit,
            notice: None,
        }
    }

    fn open(harness: &mut WidgetTestApp) -> WalletAddressDialog {
        harness
            .with_cx(WalletAddressDialog::open)
            .expect("the dialog opens")
    }

    fn sync(
        harness: &mut WidgetTestApp,
        dialog: &mut WalletAddressDialog,
        view: DialogView<'_>,
    ) -> Vec<DialogRequest> {
        harness
            .with_cx(|cx| dialog.sync(cx, view))
            .expect("the dialog syncs")
    }

    /// Lets a few production-shaped frames run: nodes built by the frame that created them
    /// are laid out and painted by the frames after.
    fn settle_frames(
        harness: &mut WidgetTestApp,
        dialog: &mut WalletAddressDialog,
        view: &DialogView<'_>,
    ) {
        for _ in 0..SETTLE_FRAMES {
            let _ = sync(harness, dialog, view.clone());
            harness.drain();
        }
    }

    fn overlay(dialog: &WalletAddressDialog) -> NodeId {
        dialog
            .test_dialog()
            .expect("the kit dialog is built")
            .overlay_node()
            .expect("the dialog is open")
    }

    fn key(dialog: &WalletAddressDialog) -> SmolStr {
        SmolStr::new(dialog.test_dialog().expect("the kit dialog is built").key())
    }

    /// The key the address list's rows are drawn under.
    fn list_key(dialog: &WalletAddressDialog) -> SmolStr {
        dialog.inner.borrow().keys.list.clone()
    }

    fn find(harness: &WidgetTestApp, dialog: &WalletAddressDialog, suffix: &str) -> Option<NodeId> {
        harness.find_by_debug_prefix(
            overlay(dialog),
            &format_smolstr!("{}:{suffix}", key(dialog)),
        )
    }

    fn find_row(
        harness: &WidgetTestApp,
        dialog: &WalletAddressDialog,
        suffix: &str,
    ) -> Option<NodeId> {
        harness.find_by_debug_prefix(
            overlay(dialog),
            &format_smolstr!("{}:{suffix}", list_key(dialog)),
        )
    }

    fn text_at(harness: &WidgetTestApp, dialog: &WalletAddressDialog, suffix: &str) -> String {
        let node = find(harness, dialog, suffix)
            .unwrap_or_else(|| panic!("the dialog draws its {suffix} node"));
        harness.text_of(node)
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

    fn requests(
        harness: &mut WidgetTestApp,
        dialog: &mut WalletAddressDialog,
        view: DialogView<'_>,
    ) -> Vec<DialogRequest> {
        harness.with_cx(|cx| dialog.sync(cx, view).expect("the dialog syncs"))
    }

    fn type_address(harness: &mut WidgetTestApp, dialog: &WalletAddressDialog, text: &str) {
        let input = dialog.test_input().expect("the address field is built");
        let window = harness.window();
        let _ = harness.app_mut().set_focus(window, Some(input.root));
        let event = Event::Input(InputEvent::Text {
            text: SmolStr::new(text),
            timestamp: Instant::now(),
        });
        assert!(
            input.dispatch(harness.app_mut(), &event),
            "the focused field takes the text"
        );
    }

    /// Puts exactly `text` in the address field, replacing whatever it held.
    fn set_address(harness: &mut WidgetTestApp, dialog: &WalletAddressDialog, text: &str) {
        let state = dialog.inner.borrow().input_state;
        harness.with_cx(|cx| {
            cx.write(&state, InputState::new(text));
        });
    }

    /// Presses Enter in the address field: the field dispatches its own submit action.
    fn press_enter(harness: &mut WidgetTestApp, dialog: &WalletAddressDialog) {
        let input = dialog.test_input().expect("the address field is built");
        let window = harness.window();
        let _ = harness.app_mut().set_focus(window, Some(input.root));
        let event = Event::Input(InputEvent::key_down(KeyCode::Enter, Modifiers::empty()));
        assert!(
            input.dispatch(harness.app_mut(), &event),
            "the focused field takes Enter"
        );
    }

    /// A wallet address is forty hexadecimal digits with or without the prefix, and the
    /// space a paste can bring along is not part of one.
    #[test]
    fn an_address_is_read_with_or_without_its_prefix() {
        let digits = "1111111111111111111111111111111111111111";
        let expected = address(digits);

        assert_eq!(parsed_address(Some(digits)), Some(expected.clone()));
        assert_eq!(
            parsed_address(Some(&format!("0x{digits}"))),
            Some(expected.clone())
        );
        assert_eq!(parsed_address(Some(&format!("0X{digits}"))), Some(expected));
        assert_eq!(
            parsed_address(Some(&format!("  {digits}  "))),
            parsed_address(Some(digits))
        );
    }

    #[test]
    fn anything_that_is_not_a_whole_address_is_refused() {
        assert_eq!(parsed_address(None), None);
        assert_eq!(parsed_address(Some("")), None);
        assert_eq!(parsed_address(Some("0x")), None);
        assert_eq!(parsed_address(Some("1111")), None);
        assert_eq!(
            parsed_address(Some("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz")),
            None
        );
        assert_eq!(
            parsed_address(Some("11111111111111111111111111111111111111111")),
            None
        );
    }

    /// A checksummed address and the lower-case one are the same wallet, and the list the
    /// pane stores is what the venue writes, so what the field holds is folded before it is
    /// read rather than being added a second time under another case.
    #[test]
    fn an_address_should_be_read_in_one_case_however_it_is_written() {
        assert_eq!(
            parsed_address(Some(CHECKSUMMED)),
            Some(wallet(CANONICAL)),
            "an address is the address it names in either case"
        );
        assert_eq!(
            parsed_address(Some(CANONICAL)),
            Some(wallet(CANONICAL)),
            "an address already written the way the venue writes it is left alone"
        );
        assert_eq!(
            parsed_address(Some("  0X52908400098527886E0F7030069857D2E4169EE7  ")),
            Some(wallet(CANONICAL)),
            "the prefix's case and the whitespace around an address are not part of it"
        );
    }

    #[test]
    fn a_dialog_should_draw_the_heading_and_the_addresses_the_pane_holds() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let addresses = vec![wallet(FIRST)];
        let view = dialog_view(&addresses, 3);
        settle_frames(&mut harness, &mut dialog, &view);

        assert!(
            text_at(&harness, &dialog, "heading").contains("Manage monitored addresses"),
            "the heading says what the dialog is for"
        );
        assert!(
            text_at(&harness, &dialog, "count").contains("1/3"),
            "the count line reads the addresses against the quota: {}",
            text_at(&harness, &dialog, "count")
        );
        let row = find_row(&harness, &dialog, "row:0:address")
            .expect("the list draws the address the pane holds");
        assert_eq!(harness.text_of(row), FIRST);
        assert!(
            dialog.test_delete_button(0).is_some(),
            "the row carries the control that stops monitoring it"
        );
    }

    #[test]
    fn an_empty_address_list_should_say_it_is_empty() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let addresses: Vec<WalletAddress> = Vec::new();
        let view = dialog_view(&addresses, 3);
        settle_frames(&mut harness, &mut dialog, &view);

        let empty = find(&harness, &dialog, "empty").expect("the dialog draws the empty block");
        assert!(harness.visible(empty), "an empty list says so");
        assert!(
            harness.text_of(empty).contains("No monitored wallets yet"),
            "the sentence is the dialog's own, not the pane's settings hint: {}",
            harness.text_of(empty)
        );
    }

    /// Typing an address offers it to the pane; an address the account already monitors is
    /// not offered, and neither is one past the quota.
    #[test]
    fn typing_an_address_should_offer_it_to_the_pane() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let addresses = vec![wallet(FIRST)];
        let view = dialog_view(&addresses, 3);
        settle_frames(&mut harness, &mut dialog, &view);

        type_address(&mut harness, &dialog, SECOND);
        settle_frames(&mut harness, &mut dialog, &view);
        let add = dialog
            .test_add_button()
            .expect("the toolbar draws the add control");
        click(&mut harness, &add);
        assert_eq!(
            requests(&mut harness, &mut dialog, view.clone()),
            vec![DialogRequest::Add(wallet(SECOND))],
            "the address the field holds is the one that was asked for"
        );

        // An address already on the list is not a change: the control refuses it rather than
        // putting a question the pane would answer with a conflict.
        set_address(&mut harness, &dialog, FIRST);
        settle_frames(&mut harness, &mut dialog, &view);
        click(&mut harness, &add);
        assert!(
            requests(&mut harness, &mut dialog, view.clone()).is_empty(),
            "an address already monitored is not offered"
        );
    }

    /// Enter in the address field is the add control by another route: it offers what the
    /// field holds, and it refuses what the control would refuse.
    #[test]
    fn pressing_enter_in_the_address_field_should_offer_the_typed_address() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let addresses = vec![wallet(FIRST)];
        let view = dialog_view(&addresses, 3);
        settle_frames(&mut harness, &mut dialog, &view);

        press_enter(&mut harness, &dialog);
        assert!(
            requests(&mut harness, &mut dialog, view.clone()).is_empty(),
            "an empty field has nothing to offer"
        );

        type_address(&mut harness, &dialog, SECOND);
        settle_frames(&mut harness, &mut dialog, &view);
        press_enter(&mut harness, &dialog);
        assert_eq!(
            requests(&mut harness, &mut dialog, view.clone()),
            vec![DialogRequest::Add(wallet(SECOND))],
            "Enter offers the address the field holds, as the add control does"
        );

        // The action entry check refuses what the pane may not monitor.
        set_address(&mut harness, &dialog, FIRST);
        press_enter(&mut harness, &dialog);
        assert!(
            requests(&mut harness, &mut dialog, view.clone()).is_empty(),
            "an address the pane already monitors is not offered"
        );
    }

    #[test]
    fn a_full_quota_should_refuse_another_address() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let addresses = vec![wallet(FIRST)];
        let view = dialog_view(&addresses, 1);
        settle_frames(&mut harness, &mut dialog, &view);

        type_address(&mut harness, &dialog, SECOND);
        settle_frames(&mut harness, &mut dialog, &view);
        let add = dialog
            .test_add_button()
            .expect("the toolbar draws the add control");
        click(&mut harness, &add);

        assert!(
            requests(&mut harness, &mut dialog, view.clone()).is_empty(),
            "the address past the quota is not one the pane may monitor"
        );
        assert!(
            dialog.test_delete_button(0).is_some(),
            "a full quota is not a reason to refuse leaving one"
        );
    }

    /// What the field holds and the pane would not take is said on the notice line instead
    /// of being swallowed: the old dialog said `invalid_address` for it, and the sentence is
    /// taken back when the user types something else.
    #[test]
    fn an_address_that_is_not_one_should_say_why_it_was_not_offered() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let addresses = vec![wallet(FIRST)];
        let view = dialog_view(&addresses, 3);
        settle_frames(&mut harness, &mut dialog, &view);

        set_address(&mut harness, &dialog, "not an address");
        press_enter(&mut harness, &dialog);
        harness.drain();

        let notice = find(&harness, &dialog, "notice").expect("the dialog draws its notice line");
        assert!(harness.visible(notice), "the refusal is said");
        assert_eq!(
            harness.text_of(notice),
            t_smol!(
                translate(LanguageCode::En),
                "widget.hyperliquid_wallet.invalid_address"
            )
            .as_str(),
            "the sentence is the catalogue's invalid-address copy"
        );
        assert!(
            requests(&mut harness, &mut dialog, view.clone()).is_empty(),
            "an address that is not one is never offered"
        );

        set_address(&mut harness, &dialog, "0x");
        harness.drain();
        assert!(
            !harness.visible(notice),
            "typing something else takes the sentence about the field back"
        );
    }

    /// A row's trash control is built into the row the list mounts, and asks the pane to
    /// stop monitoring that row's address.
    #[test]
    fn removing_an_address_should_ask_the_pane() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let addresses = vec![wallet(FIRST), wallet(SECOND)];
        let view = dialog_view(&addresses, 3);
        settle_frames(&mut harness, &mut dialog, &view);

        let delete = dialog
            .test_delete_button(0)
            .expect("the first row carries its control");
        click(&mut harness, &delete);

        assert_eq!(
            requests(&mut harness, &mut dialog, view.clone()),
            vec![DialogRequest::Delete(wallet(FIRST))],
            "the address of the row that was clicked is the one that was asked to leave"
        );
    }

    /// A row that leaves the visible window takes its remove control with it: the slot is
    /// released and the control's values go back, rather than the pairing being left behind
    /// for the next row (the leak the W8a review's M6 mutation found).
    #[test]
    fn a_shrunk_address_list_should_release_the_row_controls_it_no_longer_shows() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let two = vec![wallet(FIRST), wallet(SECOND)];
        let view = dialog_view(&two, 3);
        settle_frames(&mut harness, &mut dialog, &view);
        assert!(
            dialog.test_delete_button(1).is_some(),
            "each row carries the control that stops monitoring its address"
        );
        let both = harness.signal_slot_stats();

        let one = vec![wallet(FIRST)];
        let shrunk = dialog_view(&one, 3);
        settle_frames(&mut harness, &mut dialog, &shrunk);

        assert!(
            dialog.test_delete_button(1).is_none(),
            "the control of the row that went is released"
        );
        assert!(
            dialog.test_delete_button(0).is_some(),
            "the row that is still shown keeps a control"
        );
        assert!(
            harness.signal_slot_stats().live < both.live,
            "the released control's values went with it"
        );
    }

    /// The remove control's accessible name is chosen when it is built: a language switch
    /// rebuilds the row controls, not only the dialog's buttons (the W8a review's M7
    /// mutation found the pairing was never rebuilt).
    #[test]
    fn the_row_control_should_follow_the_language() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let addresses = vec![wallet(FIRST)];
        let view = dialog_view(&addresses, 3);
        settle_frames(&mut harness, &mut dialog, &view);
        let english = t_smol!(
            translate(LanguageCode::En),
            "widget.hyperliquid_wallet.remove_address",
            address = FIRST
        );
        assert_eq!(
            dialog
                .test_delete_button(0)
                .expect("the row carries its control")
                .a11y_label_text(),
            Some(english.as_str()),
            "the row control names the address it removes"
        );

        harness.switch_language(LanguageCode::ZhCn);
        settle_frames(&mut harness, &mut dialog, &view);

        let chinese = t_smol!(
            translate(LanguageCode::ZhCn),
            "widget.hyperliquid_wallet.remove_address",
            address = FIRST
        );
        assert_eq!(
            dialog
                .test_delete_button(0)
                .expect("the row control is rebuilt")
                .a11y_label_text(),
            Some(chinese.as_str()),
            "the control is rebuilt in the language showing now"
        );
    }

    /// The dialog's static copy follows the language: the buttons are rebuilt and the count
    /// row is read again in the language showing now.
    #[test]
    fn a_dialog_should_follow_the_language() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let addresses = vec![wallet(FIRST)];
        let view = dialog_view(&addresses, 3);
        settle_frames(&mut harness, &mut dialog, &view);

        let add = dialog.test_add_button().expect("the toolbar is built");
        assert_eq!(
            add.a11y_label_text(),
            Some(t_smol!(translate(LanguageCode::En), "widget.hyperliquid_wallet.add").as_str())
        );

        harness.switch_language(LanguageCode::ZhCn);
        settle_frames(&mut harness, &mut dialog, &view);

        let add = dialog.test_add_button().expect("the toolbar is rebuilt");
        assert_eq!(
            add.a11y_label_text(),
            Some(
                t_smol!(
                    translate(LanguageCode::ZhCn),
                    "widget.hyperliquid_wallet.add"
                )
                .as_str()
            ),
            "the control is rebuilt in the language showing now"
        );
        assert!(
            text_at(&harness, &dialog, "heading").contains("管理监控地址"),
            "the heading is read in the language showing now: {}",
            text_at(&harness, &dialog, "heading")
        );
    }

    /// The pane's sentence about the last change is drawn as it is, in the danger colour:
    /// the pane reports refusals (`watch::with` / `without` are the typed answer), so there
    /// is no success notice to draw.
    #[test]
    fn a_change_the_pane_reported_should_be_drawn() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let addresses = vec![wallet(FIRST)];
        let limit = AddressNotice::Problem(AddressDialogError::Limit);
        let view = DialogView {
            addresses: &addresses,
            limit: 3,
            notice: Some(&limit),
        };
        settle_frames(&mut harness, &mut dialog, &view);
        let notice = find(&harness, &dialog, "notice").expect("the notice line is built");
        assert!(harness.visible(notice), "the pane's refusal is said");
        assert_eq!(
            harness.text_of(notice),
            t_smol!(
                translate(LanguageCode::En),
                "widget.hyperliquid_wallet.limit_reached"
            )
            .as_str()
        );
    }

    /// The close control is what ends the dialog, and the pane learns it from `is_open`:
    /// it drops the handle and releases what the dialog owned.
    #[test]
    fn closing_the_dialog_should_release_it() {
        let mut harness = harness();
        let baseline = harness.signal_slot_stats();
        let mut dialog = open(&mut harness);
        assert!(
            dialog.is_open(harness.app()),
            "a freshly opened dialog is open"
        );
        let addresses = vec![wallet(FIRST)];
        let view = dialog_view(&addresses, 3);
        settle_frames(&mut harness, &mut dialog, &view);
        assert!(
            harness.signal_slot_stats().live > baseline.live,
            "the dialog mints its own values"
        );
        let submit = dialog.test_submit_action();
        assert!(
            harness.app().has_action_handler(&submit),
            "the address field's submit action is registered"
        );
        let opened_slots = harness.signal_slot_stats().slots;

        let close = dialog.test_close_button().expect("the footer is built");
        click(&mut harness, &close);
        for _ in 0..SETTLE_FRAMES {
            harness.advance(Duration::from_millis(40));
            let _ = harness.with_cx(|cx| dialog.sync(cx, view.clone()));
            if !dialog.is_open(harness.app()) {
                break;
            }
        }
        assert!(
            !dialog.is_open(harness.app()),
            "the close control ends the dialog"
        );

        harness.with_cx(|cx| dialog.release(cx));
        assert_eq!(
            harness.signal_slot_stats().live,
            baseline.live,
            "releasing the dialog reclaims its values"
        );
        assert!(
            !harness.app().has_action_handler(&submit),
            "releasing the dialog unregisters the address field's submit action"
        );
        assert_eq!(
            harness.signal_slot_stats().slots,
            opened_slots,
            "releasing the dialog takes no values of its own"
        );
    }

    /// Opening and releasing a dialog many times neither leaks value slots nor keeps growing
    /// the slot table: the pane may open one per address in a long session.
    #[test]
    fn releasing_the_dialog_should_reclaim_its_value_slots() {
        let mut harness = harness();
        let baseline = harness.signal_slot_stats();
        let addresses = vec![wallet(FIRST), wallet(SECOND)];
        let view = dialog_view(&addresses, 3);
        // The first dialog is the one that makes the slot table big enough for a dialog; a
        // later one drawing the same content has to reuse that table rather than grow it.
        let mut warmed_slots = None;

        for _ in 0..8 {
            let mut dialog = open(&mut harness);
            settle_frames(&mut harness, &mut dialog, &view);
            harness.with_cx(|cx| dialog.release(cx));
            harness.drain();
            assert_eq!(
                harness.signal_slot_stats().live,
                baseline.live,
                "each dialog gives its values back"
            );
            match warmed_slots {
                None => warmed_slots = Some(harness.signal_slot_stats().slots),
                Some(expected) => assert_eq!(
                    harness.signal_slot_stats().slots,
                    expected,
                    "the slot table is reused rather than growing"
                ),
            }
        }
    }
}

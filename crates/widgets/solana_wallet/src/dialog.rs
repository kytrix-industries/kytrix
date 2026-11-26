use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    rc::{Rc, Weak},
    sync::Arc,
};

use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::solana_wallet::{
    SolanaWalletSelection, solana_wallet_address,
};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    event::Action,
    interaction::ActionCtx,
    math::{LogicalPx, px},
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::{style::StylePatch, style_builder::StyleBuilder},
    text::{TextOverflow, TextStyle},
    window::WindowId,
};
use ui_kit::{
    kit::{
        assets,
        button::Button,
        dialog::{Dialog, DialogBuilder, DialogError},
        icon::IconKind,
        input::{self, InputNode, InputSpec, InputState},
        list::{List, ListOptions, SelectionMode},
        markdown::{parse::parse_document, source::MarkdownAssets, view::MarkdownNode},
        overlay::OverlayError,
        separator::Separator,
        size::{UnifiedSizable, UnifiedSize},
        spinner::Spinner,
    },
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
};
use widget::{cx::WidgetCx, error::TranslatedError, status::WidgetAction};

use super::{
    model::SelectionData,
    view::{SelectionNotice, SelectionOperation},
};

const DIALOG_WIDTH: f32 = 620.0;

const CONTENT_HEIGHT: f32 = 400.0;

const MESSAGE_WIDTH: f32 = 420.0;

#[derive(Clone)]
pub(super) struct DialogView {
    pub signed_in: bool,
    pub ready: bool,
    pub selections: Option<SelectionData>,
    pub operation: Option<SelectionOperation>,
    pub notice: Option<SelectionNotice>,
    pub recovery: WidgetAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DialogRequest {
    Reload,
    Add(SmolStr),
    Delete(SmolStr),
}

pub(super) struct SolanaWalletDialog {
    inner: Rc<RefCell<DialogState>>,
}

impl SolanaWalletDialog {
    pub(super) fn open(cx: &mut WidgetCx<'_>) -> Result<Self, EngineError> {
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

    pub(super) fn sync(
        &mut self,
        cx: &mut WidgetCx<'_>,
        view: &DialogView,
    ) -> Result<(), EngineError> {
        if self.finished() {
            return Ok(());
        }
        let result = self.inner.borrow_mut().frame(cx, view);
        if let Err(error) = result {
            log::warn!("the solana wallet address dialog could not be synced: {error}");
            self.release(cx);
        }
        Ok(())
    }

    pub(super) fn take_requests(&mut self, cx: &mut WidgetCx<'_>) -> Vec<DialogRequest> {
        if self.finished() {
            return Vec::new();
        }
        let (add_request, delete_request, reload_request, seen_reload) = {
            let state = self.inner.borrow();
            (
                state.add_request,
                state.delete_request,
                state.reload_request,
                state.seen_reload,
            )
        };

        let mut requests = Vec::new();
        let add = cx.read(&add_request).clone();
        if let Some(address) = add {
            cx.write(&add_request, None);
            requests.push(DialogRequest::Add(address));
        }
        let delete = cx.read(&delete_request).clone();
        if let Some(address) = delete {
            cx.write(&delete_request, None);
            requests.push(DialogRequest::Delete(address));
        }
        let reload = *cx.read(&reload_request);
        if reload != seen_reload {
            self.inner.borrow_mut().seen_reload = reload;
            requests.push(DialogRequest::Reload);
        }

        requests
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

#[cfg(test)]
impl SolanaWalletDialog {
    pub(super) fn test_dialog(&self) -> Option<Dialog> {
        self.inner.borrow().dialog.clone()
    }

    pub(super) fn test_input(&self) -> Option<InputNode> {
        self.inner.borrow().input.clone()
    }

    pub(super) fn test_close_button(&self) -> Option<Button> {
        self.inner.borrow().close.clone()
    }

    pub(super) fn test_add_button(&self) -> Option<Button> {
        self.inner.borrow().add.clone()
    }

    pub(super) fn test_recover_button(&self) -> Option<Button> {
        self.inner.borrow().recover.clone()
    }

    pub(super) fn test_retry_button(&self) -> Option<Button> {
        self.inner.borrow().retry.clone()
    }

    pub(super) fn test_delete_button(&self, index: usize) -> Option<Button> {
        self.inner
            .borrow()
            .delete_buttons
            .get(&index)
            .map(|entry| entry.button.clone())
    }
}

struct DialogState {
    window: WindowId,
    scope: SignalScope,
    theme: ThemeHandle,
    translate: Translate,
    language: Signal<LanguageCode>,
    keys: DialogKeys,
    built_language: Option<LanguageCode>,
    dialog: Option<Dialog>,
    list: Option<List>,
    input: Option<InputNode>,
    input_submit: Option<Action>,
    spinner: Option<Spinner>,
    spinner_parent: Option<NodeId>,
    refresh: Option<Button>,
    add: Option<Button>,
    retry: Option<Button>,
    recover: Option<Button>,
    close: Option<Button>,
    delete_buttons: BTreeMap<usize, DeleteButton>,
    slots: Rc<RefCell<BTreeMap<usize, NodeId>>>,
    refresh_parent: Option<NodeId>,
    add_parent: Option<NodeId>,
    retry_parent: Option<NodeId>,
    recover_parent: Option<NodeId>,
    close_parent: Option<NodeId>,
    signed_out_parent: Option<NodeId>,
    empty_parent: Option<NodeId>,
    signed_out_markdown: Option<MarkdownMessage>,
    empty_markdown: Option<MarkdownMessage>,
    shown_addresses: Option<usize>,
    recover_built: Option<WidgetAction>,
    last_view: Option<DialogView>,
    addresses: Signal<Arc<[SolanaWalletSelection]>>,
    limit: Signal<Option<usize>>,
    signed_in: Signal<bool>,
    ready: Signal<bool>,
    known: Signal<bool>,
    operation: Signal<Option<SelectionOperation>>,
    notice: Signal<Option<SelectionNotice>>,
    input_state: Signal<InputState>,
    input_placeholder: Signal<SmolStr>,
    add_request: Signal<Option<SmolStr>>,
    delete_request: Signal<Option<SmolStr>>,
    reload_request: Signal<u64>,
    seen_reload: u64,
    released: bool,
    closed: Rc<Cell<bool>>,
}

#[derive(Clone)]
struct DialogKeys {
    dialog: SmolStr,
    list: SmolStr,
    input: SmolStr,
    refresh: SmolStr,
    add: SmolStr,
    retry: SmolStr,
    recover: SmolStr,
    close: SmolStr,
}

struct DeleteButton {
    slot: NodeId,
    language: LanguageCode,
    button: Button,
}

struct MarkdownMessage {
    node: MarkdownNode,
}

struct DialogComponents {
    dialog: Option<Dialog>,
    input: Option<InputNode>,
    input_submit: Option<Action>,
    list: Option<List>,
    spinner: Option<Spinner>,
    refresh: Option<Button>,
    add: Option<Button>,
    retry: Option<Button>,
    recover: Option<Button>,
    close: Option<Button>,
    delete_buttons: Vec<Button>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogButtonKind {
    Refresh,
    Add,
    Retry,
    Recover,
    Close,
}

impl DialogState {
    fn new(cx: &mut WidgetCx<'_>) -> Self {
        let scope = cx.app_mut().signal_scope();
        let language = cx.language();
        let theme = cx.theme().clone();
        let translate = cx.translate().clone();
        let keys = DialogKeys {
            dialog: cx.unique_key("solana_wallet.dialog"),
            list: cx.unique_key("solana_wallet.dialog.addresses"),
            input: cx.unique_key("solana_wallet.dialog.field"),
            refresh: cx.unique_key("solana_wallet.dialog.refresh"),
            add: cx.unique_key("solana_wallet.dialog.add"),
            retry: cx.unique_key("solana_wallet.dialog.retry"),
            recover: cx.unique_key("solana_wallet.dialog.recover"),
            close: cx.unique_key("solana_wallet.dialog.close"),
        };
        let placeholder = t_smol!(translate, "widget.solana_wallet.address_placeholder");
        let addresses: Arc<[SolanaWalletSelection]> = Arc::from([]);
        Self {
            window: cx.window(),
            scope,
            theme,
            translate,
            language,
            keys,
            built_language: Some(*cx.read(&language)),
            dialog: None,
            list: None,
            input: None,
            input_submit: None,
            spinner: None,
            spinner_parent: None,
            refresh: None,
            add: None,
            retry: None,
            recover: None,
            close: None,
            delete_buttons: BTreeMap::new(),
            slots: Rc::new(RefCell::new(BTreeMap::new())),
            refresh_parent: None,
            add_parent: None,
            retry_parent: None,
            recover_parent: None,
            close_parent: None,
            signed_out_parent: None,
            empty_parent: None,
            signed_out_markdown: None,
            empty_markdown: None,
            shown_addresses: None,
            recover_built: None,
            last_view: None,
            addresses: cx.app_mut().signal_scoped(scope, addresses),
            limit: cx.app_mut().signal_scoped(scope, None),
            signed_in: cx.app_mut().signal_scoped(scope, false),
            ready: cx.app_mut().signal_scoped(scope, false),
            known: cx.app_mut().signal_scoped(scope, false),
            operation: cx.app_mut().signal_scoped(scope, None),
            notice: cx.app_mut().signal_scoped(scope, None),
            input_state: cx.app_mut().signal_scoped(scope, InputState::new("")),
            input_placeholder: cx.app_mut().signal_scoped(scope, placeholder),
            add_request: cx.app_mut().signal_scoped(scope, None),
            delete_request: cx.app_mut().signal_scoped(scope, None),
            reload_request: cx.app_mut().signal_scoped(scope, 0_u64),
            seen_reload: 0,
            released: false,
            closed: Rc::new(Cell::new(false)),
        }
    }

    fn create_kit(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
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
                .padding(LogicalPx::ZERO, LogicalPx::ZERO),
            move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                build_address_row(tree, parent, index, &key, &addresses, &slots, &theme)
            },
        );
        let count = cx.read(&addresses).len();
        list.set_initial_count(cx.app_mut(), count);
        self.shown_addresses = Some(count);
        self.list = Some(list);
        Ok(())
    }

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
                t_smol!(state.translate, "widget.solana_wallet.dialog_title"),
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

    fn publish(&mut self, cx: &mut WidgetCx<'_>, view: &DialogView) {
        let addresses = view
            .selections
            .as_ref()
            .map(SelectionData::shared_selections)
            .unwrap_or_default();
        let limit = view.selections.as_ref().map(SelectionData::limit);
        let _ = cx.write(&self.signed_in, view.signed_in);
        let _ = cx.write(&self.ready, view.ready);
        let _ = cx.write(&self.known, view.selections.is_some());
        let _ = cx.write(&self.operation, view.operation);
        let _ = cx.write(&self.notice, view.notice.clone());
        let _ = cx.write(&self.addresses, addresses);
        let _ = cx.write(&self.limit, limit);
        self.last_view = Some(view.clone());
    }

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

    fn rebuild_static_copy(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        if self.built_language == Some(language) {
            return Ok(());
        }
        let placeholder = t_smol!(self.translate, "widget.solana_wallet.address_placeholder");
        if let Some(input) = self.input.as_ref() {
            input.update_placeholder(cx.app_mut(), placeholder);
        }
        if let Some(spinner) = self.spinner.take() {
            spinner.release(cx.app_mut());
        }
        let window = cx.window();
        let app = cx.app_mut();
        for (parent, kind) in [
            (self.refresh_parent, DialogButtonKind::Refresh),
            (self.add_parent, DialogButtonKind::Add),
            (self.retry_parent, DialogButtonKind::Retry),
            (self.recover_parent, DialogButtonKind::Recover),
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
        self.rebuild_markdown(cx)?;
        self.built_language = Some(language);
        Ok(())
    }

    fn rebuild_button(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        kind: DialogButtonKind,
    ) -> Result<(), EngineError> {
        if cx.tree().get(parent).is_err() {
            return Ok(());
        }
        if let Some(button) = self.take_button(kind) {
            let window = cx.window();
            remove_button_node(cx.app_mut(), window, &button)?;
            let _ = button.release(cx.app_mut());
        }
        let window = cx.window();
        self.build_button(cx.app_mut(), window, parent, kind)
    }

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
            DialogButtonKind::Refresh => {
                let label = t_smol!(translate, "widget.solana_wallet.refresh");
                let reload = self.reload_request;
                let ready = self.ready;
                let operation = self.operation;
                let button = Button::new(app, theme, self.keys.refresh.clone())
                    .ghost()
                    .small()
                    .icon(IconKind::ArrowsClockwise)
                    .a11y_label(label.clone())
                    .tooltip_text(label)
                    .on_click(move |ctx| {
                        if *ctx.read(&ready) && ctx.read(&operation).is_none() {
                            ctx.update(&reload, |requests| *requests += 1);
                            let window = ctx.window();
                            let _ = ctx.app_mut().request_repaint(window);
                        }
                    });
                button.build(app, window, parent)?;
                self.refresh = Some(button);
            }
            DialogButtonKind::Add => {
                let label = t_smol!(translate, "widget.solana_wallet.add_address");
                let add_input = self.input.clone();
                let addresses = self.addresses;
                let limit = self.limit;
                let ready = self.ready;
                let known = self.known;
                let operation = self.operation;
                let add_request = self.add_request;
                let button = Button::new(app, theme, self.keys.add.clone())
                    .primary()
                    .small()
                    .icon(IconKind::Plus)
                    .a11y_label(label.clone())
                    .tooltip_text(label)
                    .on_click(move |ctx| {
                        let Some(input) = add_input.as_ref() else {
                            return;
                        };
                        offer_typed_address(
                            ctx,
                            input,
                            &addresses,
                            &limit,
                            &ready,
                            &known,
                            &operation,
                            &add_request,
                        );
                    });
                button.build(app, window, parent)?;
                self.add = Some(button);
            }
            DialogButtonKind::Retry => {
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
            DialogButtonKind::Recover => {
                let action = self
                    .last_view
                    .as_ref()
                    .map(|view| view.recovery.clone())
                    .unwrap_or_else(|| WidgetAction::new("common.retry", |_| {}));
                let label = action.label(&translate);
                let button = Button::new(app, theme, self.keys.recover.clone())
                    .outline()
                    .small()
                    .label(label)
                    .disabled(!action.is_enabled())
                    .on_click(move |ctx| action.run(ctx.app_mut()));
                button.build(app, window, parent)?;
                self.recover = Some(button);
                self.recover_built = self.last_view.as_ref().map(|view| view.recovery.clone());
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

    fn take_button(&mut self, kind: DialogButtonKind) -> Option<Button> {
        match kind {
            DialogButtonKind::Refresh => self.refresh.take(),
            DialogButtonKind::Add => self.add.take(),
            DialogButtonKind::Retry => self.retry.take(),
            DialogButtonKind::Recover => self.recover.take(),
            DialogButtonKind::Close => self.close.take(),
        }
    }

    fn addable_now(&self, cx: &WidgetCx<'_>) -> Option<SmolStr> {
        let text = SmolStr::new(cx.read(&self.input_state).text().trim());
        let typed = canonical_address(&text)?;
        solana_wallet_address(&typed)?;
        if !(*cx.read(&self.ready) && *cx.read(&self.known) && cx.read(&self.operation).is_none()) {
            return None;
        }
        let limit = (*cx.read(&self.limit))?;
        let addresses = cx.read(&self.addresses);
        if addresses.len() >= limit || addresses.iter().any(|current| current.address == typed) {
            return None;
        }
        Some(typed)
    }

    fn may_change(&self) -> bool {
        self.last_view
            .as_ref()
            .is_some_and(|view| view.ready && view.selections.is_some() && view.operation.is_none())
    }

    fn frame(&mut self, cx: &mut WidgetCx<'_>, view: &DialogView) -> Result<(), EngineError> {
        if self.released || self.closed.get() {
            return Ok(());
        }
        if let Some(input) = &self.input {
            let _ = input.sync(cx.app_mut());
        }
        self.rebuild_static_copy(cx)?;
        self.publish(cx, view);
        self.sync_recover_button(cx)?;
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

    fn sync_recover_button(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let current = self.last_view.as_ref().map(|view| view.recovery.clone());
        if self.recover_built == current {
            return Ok(());
        }
        self.recover_built = current;
        let Some(parent) = self.recover_parent else {
            return Ok(());
        };
        self.rebuild_button(cx, parent, DialogButtonKind::Recover)
    }

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
        self.sync_spinner(cx)?;
        self.sync_delete_buttons(cx)?;
        self.sync_button_states(cx);
        self.sync_markdown(cx);
        Ok(())
    }

    fn sync_spinner(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let loading = self.last_view.as_ref().is_some_and(|view| {
            view.signed_in
                && view.selections.is_none()
                && view.operation.is_some_and(SelectionOperation::is_read)
        });
        if loading {
            if let Some(spinner) = self.spinner.as_ref() {
                spinner.sync(cx.app_mut())?;
            } else if let Some(parent) = self.spinner_parent {
                let key = format_smolstr!("{}:spinner", self.keys.dialog);
                let label = t_smol!(self.translate, "widget.solana_wallet.loading_addresses");
                let spinner = Spinner::new(cx.app_mut(), self.theme.clone(), key).with_label(label);
                let window = cx.window();
                spinner.build(cx.app_mut(), window, parent)?;
                self.spinner = Some(spinner);
            }
        } else if let Some(spinner) = self.spinner.take() {
            spinner.release(cx.app_mut());
        }
        Ok(())
    }

    fn sync_button_states(&self, cx: &mut WidgetCx<'_>) {
        let operation = self.last_view.as_ref().and_then(|view| view.operation);
        let ready = self.last_view.as_ref().is_some_and(|view| view.ready);
        if let Some(refresh) = self.refresh.as_ref() {
            let _ = refresh.set_disabled(cx.app_mut(), !ready || operation.is_some());
        }
        if let Some(add) = self.add.as_ref() {
            let enabled = self.addable_now(cx);
            let _ = add.set_disabled(cx.app_mut(), enabled.is_none());
        }
        if let Some(recover) = self.recover.as_ref() {
            let enabled = self
                .last_view
                .as_ref()
                .is_none_or(|view| view.recovery.is_enabled());
            let _ = recover.set_disabled(cx.app_mut(), !enabled);
        }
    }

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
        let enabled = self.may_change();

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
            if replace {
                if let Some(entry) = self.delete_buttons.remove(&index) {
                    remove_button_node(cx.app_mut(), window, &entry.button)?;
                    let _ = entry.button.release(cx.app_mut());
                }
                let label = t_smol!(translate, "widget.solana_wallet.delete_address");
                let key = cx.unique_key(&format_smolstr!("solana_wallet.dialog.delete.{index}"));
                let button = Button::new(cx.app_mut(), theme.clone(), key)
                    .ghost()
                    .small()
                    .icon(IconKind::Trash)
                    .a11y_label(label.clone())
                    .tooltip_text(label)
                    .disabled(!enabled)
                    .on_click(move |ctx| {
                        let address = ctx
                            .read(&addresses)
                            .get(index)
                            .map(|selection| selection.address.clone());
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
            } else if let Some(entry) = self.delete_buttons.get(&index) {
                let _ = entry.button.set_disabled(cx.app_mut(), !enabled);
            }
        }
        Ok(())
    }

    fn rebuild_markdown(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if let Some(message) = self.signed_out_markdown.take() {
            remove_markdown(cx, message)?;
        }
        if let Some(message) = self.empty_markdown.take() {
            remove_markdown(cx, message)?;
        }
        if let Some(parent) = self.signed_out_parent {
            let text = t_smol!(self.translate, "widget.solana_wallet.logged_out");
            self.signed_out_markdown = Some(mount_markdown(cx, parent, &self.theme, &text)?);
        }
        if let Some(parent) = self.empty_parent {
            let text = t_smol!(self.translate, "widget.solana_wallet.empty_addresses");
            self.empty_markdown = Some(mount_markdown(cx, parent, &self.theme, &text)?);
        }
        Ok(())
    }

    fn sync_markdown(&mut self, cx: &mut WidgetCx<'_>) {
        if let Some(message) = self.signed_out_markdown.as_mut() {
            message.node.sync(cx.app_mut());
        }
        if let Some(message) = self.empty_markdown.as_mut() {
            message.node.sync(cx.app_mut());
        }
    }

    fn build_content(
        &mut self,
        app: &mut App,
        window: WindowId,
        body: NodeId,
    ) -> Result<(), EngineError> {
        let Some(list) = self.list.clone() else {
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
                        .height(px(CONTENT_HEIGHT))
                        .flex_col()
                        .gap(px(8.0))
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .build(tree, body)
        })?;

        let header = build_node(app, window, panel, |tree, panel| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:header"))
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .flex_none()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .min_width_0()
                        .build(),
                )
                .build(tree, panel)
        })?;
        self.heading_node(app, window, header, &key, text_size)?;
        let refresh_holder = build_node(app, window, header, |tree, header| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:refresh"))
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, header)
        })?;
        self.refresh_parent = Some(refresh_holder);
        self.build_button(app, window, refresh_holder, DialogButtonKind::Refresh)?;

        let toolbar = build_node(app, window, panel, |tree, panel| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:toolbar"))
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .flex_none()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
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
        let submit = Action::Custom(format_smolstr!("{}.submit", self.keys.input));
        self.input_submit = Some(submit.clone());
        let spec = Arc::new(InputSpec {
            name: self.keys.input.clone(),
            label: None,
            placeholder: SmolStr::default(),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            visible_lines: None,
            size: ControlSize::Medium,
            submit: Some(submit.clone()),
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
        input.install(app)?;
        let submit_input = input.clone();
        let submit_addresses = self.addresses;
        let submit_limit = self.limit;
        let submit_ready = self.ready;
        let submit_known = self.known;
        let submit_operation = self.operation;
        let submit_add_request = self.add_request;
        app.on_action(submit, move |ctx| {
            offer_typed_address(
                ctx,
                &submit_input,
                &submit_addresses,
                &submit_limit,
                &submit_ready,
                &submit_known,
                &submit_operation,
                &submit_add_request,
            );
        });
        self.input = Some(input);

        let add_holder = build_node(app, window, toolbar, |tree, toolbar| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:add"))
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, toolbar)
        })?;
        self.add_parent = Some(add_holder);
        self.build_button(app, window, add_holder, DialogButtonKind::Add)?;

        let notice = self.notice;
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
                let notice = cx.read(&notice);
                let (visible, foreground) = match notice.as_ref() {
                    Some(SelectionNotice::Added) => {
                        (true, Some(notice_theme.role(cx, Role::StatusSuccessBase)))
                    }
                    Some(SelectionNotice::Problem(_)) => {
                        (true, Some(notice_theme.role(cx, Role::StatusDangerBase)))
                    }
                    None => (false, Some(notice_theme.role(cx, Role::FgMuted))),
                };
                StylePatch {
                    foreground,
                    visible: Some(visible),
                    ..StylePatch::default()
                }
            })
            .bind_text(move |cx| {
                let _language = cx.read(&notice_language);
                match cx.read(&notice).as_ref() {
                    Some(SelectionNotice::Added) => {
                        t_smol!(notice_translate, "widget.solana_wallet.add_success")
                    }
                    Some(SelectionNotice::Problem(failure)) => {
                        failure.translated_message(&notice_translate)
                    }
                    None => SmolStr::default(),
                }
            })
            .build(tree, panel)
        })?;

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

        let signed_in = self.signed_in;
        let known = self.known;
        let addresses = self.addresses;
        let list_holder = build_node(app, window, area, |tree, area| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:addresses"))
                .style(
                    StyleBuilder::new()
                        .size_full()
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .bind_style(move |cx| StylePatch {
                    visible: Some(
                        *cx.read(&signed_in) && *cx.read(&known) && !cx.read(&addresses).is_empty(),
                    ),
                    ..StylePatch::default()
                })
                .build(tree, area)
        })?;
        list.mount(app, window, list_holder)?;

        self.build_message_blocks(app, window, area, &key, text_size)?;

        let footer = build_node(app, window, panel, |tree, panel| {
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

    fn heading_node(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        key: &str,
        text_size: LogicalPx,
    ) -> Result<NodeId, EngineError> {
        let addresses = self.addresses;
        let limit = self.limit;
        let known = self.known;
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
                let limit = *cx.read(&limit);
                let count = cx.read(&addresses).len();
                match (limit, *cx.read(&known)) {
                    (Some(limit), true) => t_smol!(
                        translate,
                        "widget.solana_wallet.monitored_addresses",
                        current = count,
                        limit = limit
                    ),
                    _ => t_smol!(translate, "widget.solana_wallet.dialog_title"),
                }
            })
            .build(tree, parent)
        })
    }

    fn build_message_blocks(
        &mut self,
        app: &mut App,
        window: WindowId,
        area: NodeId,
        key: &str,
        text_size: LogicalPx,
    ) -> Result<(), EngineError> {
        let signed_in = self.signed_in;
        let ready = self.ready;
        let known = self.known;
        let operation = self.operation;
        let addresses = self.addresses;

        let signed_out = message_block(
            app,
            window,
            area,
            format_smolstr!("{key}:signed-out"),
            move |cx| StylePatch {
                visible: Some(!*cx.read(&signed_in)),
                ..StylePatch::default()
            },
        )?;
        self.signed_out_parent = Some(signed_out);
        let signed_out_text = t_smol!(self.translate, "widget.solana_wallet.logged_out");
        self.signed_out_markdown = Some(mount_markdown_from(
            app,
            window,
            signed_out,
            &self.theme,
            &signed_out_text,
        )?);

        let loading = message_block(
            app,
            window,
            area,
            format_smolstr!("{key}:loading"),
            move |cx| StylePatch {
                visible: Some(
                    *cx.read(&signed_in)
                        && !*cx.read(&known)
                        && cx.read(&operation).is_some_and(SelectionOperation::is_read),
                ),
                ..StylePatch::default()
            },
        )?;
        let spinner_holder = build_node(app, window, loading, |tree, loading| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:spinner-slot"))
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, loading)
        })?;
        self.spinner_parent = Some(spinner_holder);
        self.message_text(
            app,
            window,
            loading,
            text_size,
            "widget.solana_wallet.loading_addresses",
        )?;

        let unavailable = message_block(
            app,
            window,
            area,
            format_smolstr!("{key}:unavailable"),
            move |cx| StylePatch {
                visible: Some(
                    *cx.read(&signed_in)
                        && !*cx.read(&known)
                        && !cx.read(&operation).is_some_and(SelectionOperation::is_read)
                        && !*cx.read(&ready),
                ),
                ..StylePatch::default()
            },
        )?;
        self.message_text(
            app,
            window,
            unavailable,
            text_size,
            "widget.solana_wallet.stream_unavailable",
        )?;
        let recover_holder = build_node(app, window, unavailable, |tree, parent| {
            NodeBuilder::boxed()
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, parent)
        })?;
        self.recover_parent = Some(recover_holder);
        self.build_button(app, window, recover_holder, DialogButtonKind::Recover)?;

        let failed = message_block(
            app,
            window,
            area,
            format_smolstr!("{key}:failed"),
            move |cx| StylePatch {
                visible: Some(
                    *cx.read(&signed_in)
                        && !*cx.read(&known)
                        && !cx.read(&operation).is_some_and(SelectionOperation::is_read)
                        && *cx.read(&ready),
                ),
                ..StylePatch::default()
            },
        )?;
        self.message_text(
            app,
            window,
            failed,
            text_size,
            "widget.solana_wallet.load_failed",
        )?;
        let retry_holder = build_node(app, window, failed, |tree, parent| {
            NodeBuilder::boxed()
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, parent)
        })?;
        self.retry_parent = Some(retry_holder);
        self.build_button(app, window, retry_holder, DialogButtonKind::Retry)?;

        let empty = message_block(
            app,
            window,
            area,
            format_smolstr!("{key}:empty"),
            move |cx| StylePatch {
                visible: Some(
                    *cx.read(&signed_in) && *cx.read(&known) && cx.read(&addresses).is_empty(),
                ),
                ..StylePatch::default()
            },
        )?;
        self.empty_parent = Some(empty);
        let empty_text = t_smol!(self.translate, "widget.solana_wallet.empty_addresses");
        self.empty_markdown = Some(mount_markdown_from(
            app,
            window,
            empty,
            &self.theme,
            &empty_text,
        )?);

        Ok(())
    }

    fn message_text(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        text_size: LogicalPx,
        key: &'static str,
    ) -> Result<NodeId, EngineError> {
        let language = self.language;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        build_node(app, window, parent, |tree, parent| {
            NodeBuilder::text(
                SmolStr::default(),
                TextStyle::new(text_size).with_overflow(TextOverflow::Ellipsis, 2),
            )
            .style(
                StyleBuilder::new()
                    .max_width(px(MESSAGE_WIDTH))
                    .min_width_0()
                    .build(),
            )
            .bind_style(move |cx| StylePatch {
                foreground: Some(theme.role(cx, Role::FgMuted)),
                ..StylePatch::default()
            })
            .bind_text(move |cx| {
                let _language = cx.read(&language);
                t_smol!(translate, key)
            })
            .build(tree, parent)
        })
    }

    fn take_components(&mut self) -> DialogComponents {
        let delete_buttons = std::mem::take(&mut self.delete_buttons)
            .into_values()
            .map(|entry| entry.button)
            .collect();
        DialogComponents {
            dialog: self.dialog.take(),
            input: self.input.take(),
            input_submit: self.input_submit.take(),
            list: self.list.take(),
            spinner: self.spinner.take(),
            refresh: self.refresh.take(),
            add: self.add.take(),
            retry: self.retry.take(),
            recover: self.recover.take(),
            close: self.close.take(),
            delete_buttons,
        }
    }
}

fn canonical_address(typed: &SmolStr) -> Option<SmolStr> {
    let typed = typed.trim();
    (!typed.is_empty()).then(|| SmolStr::new(typed))
}

fn addable_from(
    ctx: &ActionCtx<'_>,
    typed_text: &str,
    addresses: &Signal<Arc<[SolanaWalletSelection]>>,
    limit: &Signal<Option<usize>>,
    ready: &Signal<bool>,
    known: &Signal<bool>,
    operation: &Signal<Option<SelectionOperation>>,
) -> Option<SmolStr> {
    let typed = canonical_address(&SmolStr::new(typed_text.trim()))?;
    solana_wallet_address(&typed)?;
    if !(*ctx.read(ready) && *ctx.read(known) && ctx.read(operation).is_none()) {
        return None;
    }
    let limit = (*ctx.read(limit))?;
    let addresses = ctx.read(addresses);
    if addresses.len() >= limit || addresses.iter().any(|current| current.address == typed) {
        return None;
    }
    Some(typed)
}

fn offer_typed_address(
    ctx: &mut ActionCtx<'_>,
    input: &InputNode,
    addresses: &Signal<Arc<[SolanaWalletSelection]>>,
    limit: &Signal<Option<usize>>,
    ready: &Signal<bool>,
    known: &Signal<bool>,
    operation: &Signal<Option<SelectionOperation>>,
    add_request: &Signal<Option<SmolStr>>,
) {
    let text = input.value(ctx.app_mut());
    let Some(address) = addable_from(
        ctx,
        text.as_str(),
        addresses,
        limit,
        ready,
        known,
        operation,
    ) else {
        return;
    };
    ctx.write(add_request, Some(address));
    let window = ctx.window();
    let _ = ctx.app_mut().request_repaint(window);
}

fn build_address_row(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    key: &str,
    addresses: &Signal<Arc<[SolanaWalletSelection]>>,
    slots: &Rc<RefCell<BTreeMap<usize, NodeId>>>,
    theme: &ThemeHandle,
) -> Result<(), EngineError> {
    let row_theme = theme.clone();
    let row = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:row:{index}"))
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(px(8.0))
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
            .map_or_else(SmolStr::default, |selection| selection.address.clone())
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
            .bind_style(visible)
            .build(tree, area)
    })
}

fn mount_markdown(
    cx: &mut WidgetCx<'_>,
    parent: NodeId,
    theme: &ThemeHandle,
    source: &str,
) -> Result<MarkdownMessage, EngineError> {
    let window = cx.window();
    mount_markdown_from(cx.app_mut(), window, parent, theme, source)
}

fn mount_markdown_from(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    theme: &ThemeHandle,
    source: &str,
) -> Result<MarkdownMessage, EngineError> {
    let document = parse_document(source, &EmbeddedAssets);
    let node = MarkdownNode::mount(app, window, parent, theme.clone(), Arc::new(document))?;
    Ok(MarkdownMessage { node })
}

fn remove_markdown(cx: &mut WidgetCx<'_>, message: MarkdownMessage) -> Result<(), EngineError> {
    let root = message.node.root();
    if cx.tree().get(root).is_err() {
        return Ok(());
    }
    let window = cx.window();
    cx.app_mut().remove_node(window, root).map(|_| ())
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
    if let Some(input) = components.input {
        input.release(app);
    }
    if let Some(action) = components.input_submit {
        app.off_action(&action);
    }
    if let Some(list) = components.list {
        list.release(app);
    }
    if let Some(spinner) = components.spinner {
        spinner.release(app);
    }
    for button in [
        components.refresh,
        components.add,
        components.retry,
        components.recover,
        components.close,
    ]
    .into_iter()
    .flatten()
    {
        button.release(app);
    }
    for button in components.delete_buttons {
        button.release(app);
    }
}

struct EmbeddedAssets;

impl MarkdownAssets for EmbeddedAssets {
    fn load(&self, path: &str) -> Option<std::borrow::Cow<'static, [u8]>> {
        assets::bytes(path).map(std::borrow::Cow::Borrowed)
    }
}

fn engine_error_of(error: DialogError, window: WindowId) -> EngineError {
    match error {
        DialogError::Engine(error) | DialogError::Overlay(OverlayError::Engine(error)) => error,
        DialogError::Overlay(OverlayError::PlacementConfig { .. }) | DialogError::NotOpen => {
            EngineError::UnknownWindow { window }
        }
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

    use i18n::{language::codes::LanguageCode, t_smol};
    use kytrix_service_provider::service::solana_wallet::{
        SolanaWalletSelection, SolanaWalletSelectionsResponse,
    };
    use ui_core::event::{ActionSource, Event, InputEvent, KeyCode, KeyState, Modifiers};
    use widget::{protocol::RequestId, rpc::RpcCallError, test_support::WidgetTestApp};

    use super::*;
    use crate::view::{SelectionFailure, SelectionNotice};

    const FIRST: &str = "So11111111111111111111111111111111111111112";
    const SECOND: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
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

    fn address(address: &str) -> SolanaWalletSelection {
        SolanaWalletSelection {
            address: SmolStr::new(address),
            created_at: 1,
        }
    }

    fn data(addresses: Vec<SolanaWalletSelection>, limit: usize) -> SelectionData {
        SelectionData::new(SolanaWalletSelectionsResponse {
            selections: addresses,
            solana_wallet_monitor_address_limit: limit,
        })
    }

    fn view(signed_in: bool, ready: bool, selections: Option<SelectionData>) -> DialogView {
        DialogView {
            signed_in,
            ready,
            selections,
            operation: None,
            notice: None,
            recovery: WidgetAction::new("common.retry", |_| {}),
        }
    }

    fn open(harness: &mut WidgetTestApp) -> SolanaWalletDialog {
        harness
            .with_cx(SolanaWalletDialog::open)
            .expect("the dialog opens")
    }

    fn sync(harness: &mut WidgetTestApp, dialog: &mut SolanaWalletDialog, view: &DialogView) {
        harness
            .with_cx(|cx| dialog.sync(cx, view))
            .expect("the dialog syncs");
    }

    fn settle_frames(
        harness: &mut WidgetTestApp,
        dialog: &mut SolanaWalletDialog,
        view: &DialogView,
    ) {
        for _ in 0..SETTLE_FRAMES {
            sync(harness, dialog, view);
            harness.drain();
        }
    }

    fn overlay(dialog: &SolanaWalletDialog) -> NodeId {
        dialog
            .test_dialog()
            .expect("the kit dialog is built")
            .overlay_node()
            .expect("the dialog is open")
    }

    fn key(dialog: &SolanaWalletDialog) -> SmolStr {
        SmolStr::new(dialog.test_dialog().expect("the kit dialog is built").key())
    }

    fn list_key(dialog: &SolanaWalletDialog) -> SmolStr {
        dialog.inner.borrow().keys.list.clone()
    }

    fn find(harness: &WidgetTestApp, dialog: &SolanaWalletDialog, suffix: &str) -> Option<NodeId> {
        harness.find_by_debug_prefix(
            overlay(dialog),
            &format_smolstr!("{}:{suffix}", key(dialog)),
        )
    }

    fn find_row(
        harness: &WidgetTestApp,
        dialog: &SolanaWalletDialog,
        suffix: &str,
    ) -> Option<NodeId> {
        harness.find_by_debug_prefix(
            overlay(dialog),
            &format_smolstr!("{}:{suffix}", list_key(dialog)),
        )
    }

    fn text_at(harness: &WidgetTestApp, dialog: &SolanaWalletDialog, suffix: &str) -> String {
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
        dialog: &mut SolanaWalletDialog,
    ) -> Vec<DialogRequest> {
        harness.with_cx(|cx| dialog.take_requests(cx))
    }

    fn type_address(harness: &mut WidgetTestApp, dialog: &SolanaWalletDialog, text: &str) {
        let input = dialog.test_input().expect("the address field is built");
        let window = harness.window();
        let _ = harness.app_mut().set_focus(window, Some(input.editable));
        harness.dispatch_event(Event::Input(InputEvent::Text {
            text: SmolStr::new(text),
            timestamp: Instant::now(),
        }));
    }

    fn set_address(harness: &mut WidgetTestApp, dialog: &SolanaWalletDialog, text: &str) {
        let input = dialog.test_input().expect("the address field is built");
        input.update_value(harness.app_mut(), text);
    }

    fn press_enter(harness: &mut WidgetTestApp, dialog: &SolanaWalletDialog) {
        let input = dialog.test_input().expect("the address field is built");
        let window = harness.window();
        let _ = harness.app_mut().set_focus(window, Some(input.editable));
        harness.dispatch_event(Event::Input(InputEvent::Key {
            code: KeyCode::Enter,
            state: KeyState::Down,
            repeat: false,
            modifiers: Modifiers::empty(),
            timestamp: Instant::now(),
        }));
    }

    #[test]
    fn an_address_should_be_read_as_the_address_it_names() {
        assert_eq!(
            canonical_address(&SmolStr::new_static(FIRST)),
            Some(SmolStr::new_static(FIRST))
        );
        assert_eq!(
            canonical_address(&SmolStr::new_static(
                "  TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA  "
            )),
            Some(SmolStr::new_static(SECOND))
        );
        assert_eq!(canonical_address(&SmolStr::new_static("")), None);
        assert_eq!(canonical_address(&SmolStr::new_static("   ")), None);
        assert_eq!(
            solana_wallet_address("0x52908400098527886E0F7030069857D2E4169EE7"),
            None,
            "an Ethereum address is not a Solana one"
        );
    }

    #[test]
    fn a_dialog_should_draw_the_heading_and_the_addresses_the_pane_holds() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let view = view(true, true, Some(data(vec![address(FIRST)], 3)));
        settle_frames(&mut harness, &mut dialog, &view);

        assert!(
            text_at(&harness, &dialog, "heading").contains("Selected addresses: 1/3"),
            "the heading reads the addresses against the quota"
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
    fn typing_an_address_should_offer_it_to_the_pane() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let view = view(true, true, Some(data(vec![address(FIRST)], 3)));
        settle_frames(&mut harness, &mut dialog, &view);

        type_address(&mut harness, &dialog, SECOND);
        settle_frames(&mut harness, &mut dialog, &view);
        let add = dialog
            .test_add_button()
            .expect("the toolbar draws the add control");
        click(&mut harness, &add);
        assert_eq!(
            requests(&mut harness, &mut dialog),
            vec![DialogRequest::Add(SmolStr::new_static(SECOND))],
            "the address the field holds is the one that was asked for"
        );

        type_address(&mut harness, &dialog, FIRST);
        settle_frames(&mut harness, &mut dialog, &view);
        click(&mut harness, &add);
        assert!(
            requests(&mut harness, &mut dialog).is_empty(),
            "an address already monitored is not offered"
        );
    }

    #[test]
    fn pressing_enter_in_the_address_field_should_offer_the_typed_address() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let view = view(true, true, Some(data(vec![address(FIRST)], 3)));
        settle_frames(&mut harness, &mut dialog, &view);

        type_address(&mut harness, &dialog, SECOND);
        settle_frames(&mut harness, &mut dialog, &view);
        assert!(
            requests(&mut harness, &mut dialog).is_empty(),
            "typing alone offers nothing"
        );
        press_enter(&mut harness, &dialog);
        assert_eq!(
            requests(&mut harness, &mut dialog),
            vec![DialogRequest::Add(SmolStr::new_static(SECOND))],
            "Enter offers the address the field holds, exactly as the add control does"
        );

        set_address(&mut harness, &dialog, FIRST);
        press_enter(&mut harness, &dialog);
        assert!(
            requests(&mut harness, &mut dialog).is_empty(),
            "an address the account already monitors is not offered"
        );
    }

    #[test]
    fn a_full_quota_should_refuse_another_address() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let view = view(true, true, Some(data(vec![address(FIRST)], 1)));
        settle_frames(&mut harness, &mut dialog, &view);

        type_address(&mut harness, &dialog, SECOND);
        settle_frames(&mut harness, &mut dialog, &view);
        let add = dialog
            .test_add_button()
            .expect("the toolbar draws the add control");
        click(&mut harness, &add);

        assert!(
            requests(&mut harness, &mut dialog).is_empty(),
            "the address past the quota is not one the account may monitor"
        );
        assert!(
            dialog.test_delete_button(0).is_some(),
            "a full quota is not a reason to refuse leaving one"
        );
    }

    #[test]
    fn removing_an_address_should_ask_the_pane() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let view = view(
            true,
            true,
            Some(data(vec![address(FIRST), address(SECOND)], 3)),
        );
        settle_frames(&mut harness, &mut dialog, &view);

        let delete = dialog
            .test_delete_button(0)
            .expect("the first row carries its control");
        click(&mut harness, &delete);

        assert_eq!(
            requests(&mut harness, &mut dialog),
            vec![DialogRequest::Delete(SmolStr::new_static(FIRST))],
            "the address of the row that was clicked is the one that was asked to leave"
        );
    }

    #[test]
    fn a_shrunk_address_list_should_release_the_row_controls_it_no_longer_shows() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let two = view(
            true,
            true,
            Some(data(vec![address(FIRST), address(SECOND)], 3)),
        );
        settle_frames(&mut harness, &mut dialog, &two);
        assert!(
            dialog.test_delete_button(1).is_some(),
            "each row carries the control that stops monitoring its address"
        );
        let both = harness.signal_slot_stats();

        let one = view(true, true, Some(data(vec![address(FIRST)], 3)));
        settle_frames(&mut harness, &mut dialog, &one);

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

    #[test]
    fn the_row_control_should_follow_the_language() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let view = view(true, true, Some(data(vec![address(FIRST)], 3)));
        settle_frames(&mut harness, &mut dialog, &view);
        let english = dialog
            .test_delete_button(0)
            .expect("the row carries its control")
            .a11y_label_text()
            .map(str::to_owned);
        assert_eq!(
            english.as_deref(),
            Some(
                t_smol!(
                    translate(LanguageCode::En),
                    "widget.solana_wallet.delete_address"
                )
                .as_str()
            )
        );

        harness.switch_language(LanguageCode::ZhCn);
        settle_frames(&mut harness, &mut dialog, &view);

        let chinese = dialog
            .test_delete_button(0)
            .expect("the row is rebuilt")
            .a11y_label_text()
            .map(str::to_owned);
        assert_eq!(
            chinese.as_deref(),
            Some(
                t_smol!(
                    translate(LanguageCode::ZhCn),
                    "widget.solana_wallet.delete_address"
                )
                .as_str()
            ),
            "the control is rebuilt in the language showing now"
        );
    }

    #[test]
    fn a_dialog_should_follow_the_language() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let view = view(false, false, None);
        settle_frames(&mut harness, &mut dialog, &view);

        let add = dialog.test_add_button().expect("the toolbar is built");
        assert_eq!(
            add.a11y_label_text(),
            Some(
                t_smol!(
                    translate(LanguageCode::En),
                    "widget.solana_wallet.add_address"
                )
                .as_str()
            )
        );
        let markdown = harness
            .find_by_debug_prefix(overlay(&dialog), "MARKDOWN")
            .expect("the signed-out sentence is drawn as rich text");
        let english = harness.text_of(markdown);
        assert!(
            english.contains("register or log in"),
            "the sentence is resolved and drawn: {english}"
        );
        assert!(
            !english.contains("<p"),
            "the markup never reaches the user: {english}"
        );

        harness.switch_language(LanguageCode::ZhCn);
        settle_frames(&mut harness, &mut dialog, &view);

        let add = dialog.test_add_button().expect("the toolbar is rebuilt");
        assert_eq!(
            add.a11y_label_text(),
            Some(
                t_smol!(
                    translate(LanguageCode::ZhCn),
                    "widget.solana_wallet.add_address"
                )
                .as_str()
            ),
            "the control is rebuilt in the language showing now"
        );
        let markdown = harness
            .find_by_debug_prefix(overlay(&dialog), "MARKDOWN")
            .expect("the sentence is parsed again");
        let chinese = harness.text_of(markdown);
        assert!(
            chinese.contains("需要注册"),
            "the sentence is in the language showing now: {chinese}"
        );
        assert!(
            !chinese.contains("<p"),
            "the markup never reaches the user: {chinese}"
        );
    }

    #[test]
    fn a_dialog_without_a_stream_should_offer_the_panes_recovery() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let ran = Rc::new(Cell::new(false));
        let ran_in_action = Rc::clone(&ran);
        let mut view = view(true, false, None);
        view.recovery = WidgetAction::new("common.retry", move |_| ran_in_action.set(true));
        settle_frames(&mut harness, &mut dialog, &view);

        assert!(
            text_at(&harness, &dialog, "unavailable").contains("not connected"),
            "the dialog says the stream is not connected"
        );
        let recover = dialog
            .test_recover_button()
            .expect("the unavailable block draws the pane's recovery control");
        click(&mut harness, &recover);
        assert!(ran.get(), "the pane's recovery runs");
    }

    #[test]
    fn a_loading_dialog_should_announce_what_it_is_reading() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let mut view = view(true, true, None);
        view.operation = Some(SelectionOperation::Loading);
        settle_frames(&mut harness, &mut dialog, &view);
        let spinning = {
            let state = dialog.inner.borrow();
            state
                .spinner
                .as_ref()
                .expect("the loading window spins")
                .node()
                .expect("the spinner is built")
        };

        harness.enable_a11y_capture();
        harness.drain();
        assert!(
            !harness.a11y_hidden(spinning),
            "a named spinner is announced rather than hidden as decoration"
        );

        harness.switch_language(LanguageCode::ZhCn);
        sync(&mut harness, &mut dialog, &view);
        harness.drain();
        let rebuilt = {
            let state = dialog.inner.borrow();
            state
                .spinner
                .as_ref()
                .expect("the loading window still spins")
                .node()
                .expect("the spinner is built again")
        };
        assert_ne!(
            rebuilt, spinning,
            "the language rebuilds the spinner's name"
        );
        assert!(
            !harness.a11y_hidden(rebuilt),
            "the rebuilt spinner is announced as well"
        );
    }

    #[test]
    fn a_read_that_failed_should_offer_another_read() {
        let mut harness = harness();
        let mut dialog = open(&mut harness);
        let mut view = view(true, true, None);
        view.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(
            RpcCallError::ServiceClosed {
                request_id: RequestId::new(2),
            },
        )));
        settle_frames(&mut harness, &mut dialog, &view);

        assert!(
            text_at(&harness, &dialog, "failed").contains("Could not load wallet addresses"),
            "the dialog says the read failed"
        );
        let retry = dialog
            .test_retry_button()
            .expect("the failed read offers another read");
        let node = retry.node().expect("the retry control is built");
        assert!(harness.visible(node), "the retry control is on screen");
        click(&mut harness, &retry);
        assert_eq!(
            requests(&mut harness, &mut dialog),
            vec![DialogRequest::Reload],
            "the control asks the pane to read again"
        );
    }

    #[test]
    fn closing_the_dialog_should_release_it() {
        let mut harness = harness();
        let baseline = harness.signal_slot_stats();
        let mut dialog = open(&mut harness);
        let view = view(true, true, Some(data(vec![address(FIRST)], 3)));
        settle_frames(&mut harness, &mut dialog, &view);
        assert!(
            harness.signal_slot_stats().live > baseline.live,
            "the dialog mints its own values"
        );

        let close = dialog.test_close_button().expect("the footer is built");
        click(&mut harness, &close);
        for _ in 0..SETTLE_FRAMES {
            harness.advance(Duration::from_millis(40));
            sync(&mut harness, &mut dialog, &view);
            if dialog.finished() {
                break;
            }
        }
        assert!(dialog.finished(), "the close control ends the dialog");

        harness.with_cx(|cx| dialog.release(cx));
        assert_eq!(
            harness.signal_slot_stats().live,
            baseline.live,
            "releasing the dialog reclaims its values"
        );
    }
}

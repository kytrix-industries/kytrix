use std::sync::Arc;

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::{
    engine::KytrixServiceError,
    solana_wallet::{
        SolanaWalletApiError, SolanaWalletMutationOutcome, SolanaWalletMutationUnknown,
        solana_wallet_address,
    },
};
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::style::StylePatch,
    text::{TextOverflow, TextStyle},
};
use ui_kit::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        icon::IconKind,
        list::{List, ListOptions, SelectionMode},
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::TranslatedError,
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    time_zone::data_time_zone,
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use super::{
    dialog::{DialogRequest, DialogView, SolanaWalletDialog},
    model::{MAX_TRANSFERS, SelectionData, SolanaWalletDataError, TransferSnapshot, unshown},
    service::{
        SolanaWalletProtocol, SolanaWalletRequest, SolanaWalletResponse, SolanaWalletScope,
        SolanaWalletService, SolanaWalletSource, SolanaWalletStatus, SolanaWalletStreamError,
        SolanaWalletStreamMessage,
    },
};

const ROW_ESTIMATE: f32 = 68.0;

const ROW_OVERSCAN: usize = 2;

const ADDRESS_LABEL_WIDTH: f32 = 32.0;

const TRANSFER_TEXT_SIZE: UnifiedSize = UnifiedSize::Medium;

const TRANSFER_TIME_SIZE: UnifiedSize = UnifiedSize::Small;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SolanaWalletConfig;

impl WidgetConfig for SolanaWalletConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SolanaWalletFailure {
    Stream(SolanaWalletStreamError),
    Rpc(RpcCallError<SolanaWalletApiError>),
}

impl TranslatedError for SolanaWalletFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Stream(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    Start,
    Stop,
    Read,
    Add,
    Delete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SelectionOperation {
    Loading,
    Refreshing,
    Adding,
    Deleting,
}

impl SelectionOperation {
    pub(super) const fn is_read(self) -> bool {
        matches!(self, Self::Loading | Self::Refreshing)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectionPending {
    operation: SelectionOperation,
    address: Option<SmolStr>,
    sent: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SelectionNotice {
    Added,
    Problem(SelectionFailure),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SelectionFailure {
    Rejected(SolanaWalletApiError),
    Unknown(SolanaWalletMutationUnknown),
    Inconsistent(SolanaWalletDataError),
    Rpc(RpcCallError<SolanaWalletApiError>),
}

impl TranslatedError for SelectionFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Rejected(error) => error.translated_message(translate),
            Self::Unknown(error) => error.translated_message(translate),
            Self::Inconsistent(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum SelectionRead {
    #[default]
    NotRead,
    Known,
    Failed(RpcCallError<SolanaWalletApiError>),
}

pub struct SolanaWalletWidget {
    binding: Option<WidgetBinding<SolanaWalletProtocol>>,
    pending: Pending<PendingCommand>,
    status: WidgetStatus,
    recovery: WidgetAction,
    account_id: Option<SmolStr>,
    snapshot: Option<TransferSnapshot>,
    ready: bool,
    items: Signal<TransferSnapshot>,
    list: Option<List>,
    body_root: Option<NodeId>,
    empty: Option<NodeId>,
    empty_text: Signal<SmolStr>,
    selections: Option<SelectionData>,
    selection_pending: Option<SelectionPending>,
    notice: Option<SelectionNotice>,
    selection_read: SelectionRead,
    dialog: Option<SolanaWalletDialog>,
    manage_button: Option<Button>,
    manage_parent: Option<NodeId>,
    manage_language: Option<LanguageCode>,
    manage_requests: Signal<u64>,
    seen_manage_requests: u64,
}

impl WidgetView for SolanaWalletWidget {
    type Config = SolanaWalletConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.solana_wallet");
    const DISPLAY_NAME: &'static str = "widget.names.solana_wallet";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.solana_wallet");
    const ICON: Option<&'static str> = Some("widgets/solana_wallet.png");

    fn new(
        _instance_id: WidgetInstanceId,
        _config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        let source = cx.services().kytrix_service().cloned();
        Ok(Self::assemble(source, status, recovery, cx))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let retry = self.status.is_failed(cx.app());
        self.account_id = cx.account();
        if let Err(error) = self.start(retry, cx) {
            log::error!("the solana wallet pane could not start: {error}");
        }
    }

    fn has_content(&self) -> bool {
        self.account_has_addresses() && self.snapshot.is_some()
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let root = NodeBuilder::boxed()
            .id("TEST_SOLANA_WALLET_BODY")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .relative()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        self.body_root = Some(root);

        let language = cx.language();
        let translate = cx.translate().clone();
        let list_holder = NodeBuilder::boxed()
            .id("TEST_SOLANA_WALLET_TRANSFER_LIST")
            .a11y(A11yOverride {
                role: Some(A11yRole::ScrollView),
                ..A11yOverride::default()
            })
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .bind_a11y(move |cx| {
                let _language = cx.read(&language);
                A11yPatch {
                    label: Some(t_smol!(translate, "widget.solana_wallet.scroll_transfers")),
                    ..A11yPatch::default()
                }
            })
            .build(cx.tree_mut(), root)?;
        self.list = Some(self.make_list(cx, list_holder)?);
        self.sync_empty_state(cx)?;
        let _ = cx.request_frame();

        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let account = cx.account();
        if account != self.account_id {
            self.account_id = account;
            self.start(false, cx)?;
        }

        let messages = self
            .binding
            .as_ref()
            .map(WidgetBinding::take)
            .unwrap_or_default();
        for message in messages {
            match message {
                Inbound::Reliable { key, message } => {
                    self.apply_reliable(key, message, cx);
                }
                Inbound::Latest { key, message } => {
                    self.apply_latest(key, message, cx);
                }
                Inbound::Rpc { request_id, result } => {
                    self.apply_rpc(request_id, result, cx);
                }
            }
        }

        let requests = *cx.read(&self.manage_requests);
        if requests != self.seen_manage_requests {
            self.seen_manage_requests = requests;
            self.open_dialog(cx)?;
        }

        self.drive_dialog(cx)?;
        self.sync_manage_button(cx)?;

        if let Some(list) = &self.list {
            list.sync(cx.app_mut())?;
        }
        self.sync_empty_state(cx)?;

        if self.has_content() && self.body_root.is_none() {
            cx.request_frame()?;
        }

        Ok(())
    }

    fn header(
        &mut self,
        group: HeaderGroup,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        if group != HeaderGroup::Left {
            return Ok(());
        }
        self.release_manage_button(cx, true)?;
        self.build_manage_button(parent, cx)
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        self.binding = None;
        let _ = self.release_manage_button(cx, false);
        if let Some(mut dialog) = self.dialog.take() {
            dialog.release(cx);
        }
        if let Some(root) = self.body_root.take() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, root);
        }
        if let Some(list) = self.list.take() {
            let _ = list.release(cx.app_mut());
        }
        self.empty = None;
        self.snapshot = None;
    }
}

impl SolanaWalletWidget {
    fn assemble<S: SolanaWalletSource>(
        source: Option<S>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let binding =
            source.map(|source| WidgetBinding::spawn(cx, SolanaWalletService::new(source)));
        let items = cx.signal(TransferSnapshot::default());
        let empty_text = cx.signal(t_smol!(cx.translate(), "common.no_data"));
        let manage_requests = cx.signal(0_u64);
        let account_id = cx.account();

        Self {
            binding,
            pending: Pending::new(),
            status,
            recovery,
            account_id,
            snapshot: None,
            ready: false,
            items,
            list: None,
            body_root: None,
            empty: None,
            empty_text,
            selections: None,
            selection_pending: None,
            notice: None,
            selection_read: SelectionRead::default(),
            dialog: None,
            manage_button: None,
            manage_parent: None,
            manage_language: None,
            manage_requests,
            seen_manage_requests: 0,
        }
    }

    #[cfg(test)]
    fn new_with_source<S: SolanaWalletSource>(
        source: S,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        Self::assemble(Some(source), status, recovery, cx)
    }

    fn current_scope(&self) -> Option<SolanaWalletScope> {
        SolanaWalletScope::new(self.account_id.as_ref())
    }

    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(scope) = self.current_scope() else {
            self.clear(cx)?;
            self.report_no_content_notice(cx);
            self.call(cx, SolanaWalletRequest::Stop, PendingCommand::Stop);
            cx.request_frame()?;
            return Ok(());
        };
        self.ready = false;
        if !retry {
            self.clear(cx)?;
        }
        self.selection_pending = None;
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        if self.binding.is_none() {
            self.fail(
                SolanaWalletStreamError::Service(KytrixServiceError::Closed),
                cx,
            );
            return Ok(());
        }
        let request = if retry {
            SolanaWalletRequest::Retry(scope)
        } else {
            SolanaWalletRequest::Load(scope)
        };
        self.call(cx, request, PendingCommand::Start);
        cx.request_frame()?;

        Ok(())
    }

    fn call(
        &mut self,
        cx: &mut WidgetCx<'_>,
        request: SolanaWalletRequest,
        command: PendingCommand,
    ) {
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        let request_id = binding.call(cx, request);
        self.pending.track(request_id, command);
    }

    fn clear(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.snapshot = None;
        self.ready = false;
        cx.write(&self.items, TransferSnapshot::default());
        if let Some(list) = &self.list {
            list.set_items(cx.app_mut(), 0)?;
        }
        self.sync_empty_state(cx)?;
        self.selections = None;
        self.selection_pending = None;
        self.notice = None;
        cx.request_frame()?;

        Ok(())
    }

    fn report_no_content_notice(&self, cx: &mut WidgetCx<'_>) {
        let notice = if self.current_scope().is_none() {
            Notice::message("widget.solana_wallet.logged_out")
        } else if !self.account_has_addresses() {
            Notice::message("widget.solana_wallet.empty_addresses")
        } else {
            return;
        };
        self.status
            .report(cx.app_mut(), WidgetState::waiting(notice));
    }

    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<SolanaWalletProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        match self.pending.take(request_id) {
            Some(PendingCommand::Start) => self.apply_load_result(result, cx),
            Some(PendingCommand::Stop) => {}
            Some(PendingCommand::Read) => self.apply_selections_result(result, cx),
            Some(PendingCommand::Add) => self.apply_add_result(result, cx),
            Some(PendingCommand::Delete) => self.apply_delete_result(result, cx),
            None => log::debug!(
                "the solana wallet pane ignored reply {request_id}: no command is waiting for it"
            ),
        }
    }

    fn apply_load_result(
        &mut self,
        result: WidgetRpcResult<SolanaWalletProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.status.is_failed(cx.app()) {
            return;
        }
        if let Err(error) = result {
            self.ready = false;
            self.selection_pending = None;
            let recovery = self.recovery.clone();
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(Arc::new(SolanaWalletFailure::Rpc(error)), recovery),
            );
        }
    }

    fn apply_latest(
        &mut self,
        key: SolanaWalletScope,
        message: Arc<SolanaWalletStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key)
            || !self.may_report()
            || self.status.is_failed(cx.app())
        {
            return false;
        }
        let SolanaWalletStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return false;
        };

        if let Err(error) = self.accept(snapshot.clone(), cx) {
            log::error!("the solana wallet pane could not take the feed it was published: {error}");
            return false;
        }
        if self.ready && self.may_report() {
            self.report(cx);
        }
        true
    }

    fn may_report(&self) -> bool {
        !matches!(self.selection_read, SelectionRead::Failed(_))
    }

    fn report(&mut self, cx: &mut WidgetCx<'_>) {
        match self.selection_read.clone() {
            SelectionRead::NotRead => {
                self.status.report(cx.app_mut(), WidgetState::Loading);
            }
            SelectionRead::Failed(error) => {
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(Arc::new(SolanaWalletFailure::Rpc(error)), recovery),
                );
            }
            SelectionRead::Known => {
                if !self.account_has_addresses() {
                    self.report_no_content_notice(cx);
                } else if self.has_content() {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                } else {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
                }
            }
        }
    }

    fn account_has_addresses(&self) -> bool {
        !self
            .selections
            .as_ref()
            .is_some_and(|data| data.selections().is_empty())
    }

    fn apply_reliable(
        &mut self,
        key: SolanaWalletScope,
        message: Arc<SolanaWalletStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key)
            || !self.may_report()
            || self.status.is_failed(cx.app())
        {
            return false;
        }
        let SolanaWalletStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };

        match status {
            SolanaWalletStatus::Connecting => {
                self.ready = false;
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            SolanaWalletStatus::Ready => {
                self.ready = true;
                if self.selection_pending.is_some() {
                    self.send_pending_selections(cx);
                } else if self.selections.is_none() || self.dialog.is_some() {
                    self.load_selections(cx);
                }
                self.report(cx);
            }
            SolanaWalletStatus::Failed(error) => {
                self.ready = false;
                if self
                    .selection_pending
                    .as_ref()
                    .is_some_and(|pending| !pending.sent)
                {
                    self.selection_pending = None;
                }
                self.fail(error.clone(), cx);
            }
        }

        false
    }

    fn fail(&mut self, error: SolanaWalletStreamError, cx: &mut WidgetCx<'_>) {
        let failure = SolanaWalletFailure::Stream(error);
        let recovery = self.recovery.clone();
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(Arc::new(failure), recovery),
        );
    }

    fn accept(
        &mut self,
        snapshot: TransferSnapshot,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let unshown = unshown(
            self.snapshot.as_ref().map_or(&[], TransferSnapshot::items),
            snapshot.items(),
        );
        cx.write(&self.items, snapshot.clone());
        let count = snapshot.items().len();
        match (unshown, self.list.as_ref()) {
            (Some(pushed), Some(list)) => {
                for _ in 0..pushed {
                    list.push(cx.app_mut())?;
                }
            }
            (None, Some(list)) => list.set_items(cx.app_mut(), count)?,
            _ => {}
        }
        self.snapshot = Some(snapshot);
        cx.request_frame()?;

        Ok(())
    }

    fn make_list(&self, cx: &mut WidgetCx<'_>, parent: NodeId) -> Result<List, EngineError> {
        let context = TransferRowContext {
            items: self.items,
            language: cx.language(),
            config: cx.services().config,
            theme: cx.theme().clone(),
            translate: cx.translate().clone(),
        };
        let count = cx.read(&self.items).items().len();
        let key = cx.unique_key("solana_wallet.transfers");
        let row_context = context.clone();

        let list = List::new(
            cx.app_mut(),
            context.theme,
            key,
            ListOptions::variable(ROW_ESTIMATE)
                .deque(MAX_TRANSFERS)
                .reverse()
                .keep_on_newest()
                .overscan(ROW_OVERSCAN)
                .separator()
                .selection(SelectionMode::None)
                .padding(0.0, 0.0),
            move |tree, row_parent, index| {
                build_transfer_row(tree, row_parent, index, &row_context)
            },
        );
        list.set_initial_count(cx.app_mut(), count);
        let window = cx.window();
        list.mount(cx.app_mut(), window, parent)?;

        Ok(list)
    }

    fn sync_empty_state(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let empty = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.items().is_empty());
        if !empty {
            if let Some(node) = self.empty.take() {
                remove_own_node(cx, node)?;
            }
            return Ok(());
        }
        let text = t_smol!(cx.translate(), "common.no_data");
        cx.write(&self.empty_text, text);
        if self.empty.is_some() {
            return Ok(());
        }
        let Some(root) = self.body_root else {
            return Ok(());
        };

        let overlay = NodeBuilder::boxed()
            .id("TEST_SOLANA_WALLET_EMPTY")
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
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        EmptyState::new(cx.theme().clone(), "SolanaWalletEmpty")
            .bind_text(self.empty_text)
            .build(cx.tree_mut(), overlay)?;
        self.empty = Some(overlay);

        Ok(())
    }

    fn build_manage_button(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let wrapper = NodeBuilder::boxed()
            .id("TEST_SOLANA_WALLET_MONITOR_MANAGE")
            .style(StyleBuilder::new().flex_none().build())
            .build(cx.tree_mut(), parent)?;
        self.manage_parent = Some(wrapper);
        self.build_manage_control(cx, wrapper)
    }

    fn build_manage_control(
        &mut self,
        cx: &mut WidgetCx<'_>,
        wrapper: NodeId,
    ) -> Result<(), EngineError> {
        let language = *cx.read(&cx.language());
        let label = t_smol!(cx.translate(), "widget.solana_wallet.manage");
        let key = cx.unique_key("solana_wallet.header.manage");
        let theme = cx.theme().clone();
        let window = cx.window();
        let manage_requests = self.manage_requests;
        let enabled = self.account_id.is_some();

        let button = Button::new(cx.app_mut(), theme, key)
            .primary()
            .small()
            .icon(IconKind::Faders)
            .a11y_label(label.clone())
            .tooltip_text(label)
            .disabled(!enabled)
            .on_click(move |ctx| {
                ctx.update(&manage_requests, |requests| *requests += 1);
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        button.build(cx.app_mut(), window, wrapper)?;

        self.manage_button = Some(button);
        self.manage_language = Some(language);

        Ok(())
    }

    fn sync_manage_button(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(button) = self.manage_button.as_ref() else {
            return Ok(());
        };
        let _ = button.set_disabled(cx.app_mut(), self.account_id.is_none());

        let language = *cx.read(&cx.language());
        if self.manage_language == Some(language) {
            return Ok(());
        }
        let Some(wrapper) = self.manage_parent else {
            return Ok(());
        };
        if cx.tree().get(wrapper).is_err() {
            self.release_manage_button(cx, false)?;
            return Ok(());
        }

        if let Some(button) = self.manage_button.take() {
            if let Some(node) = button.node() {
                remove_own_node(cx, node)?;
            }
            let _ = button.release(cx.app_mut());
        }
        self.manage_language = None;
        self.build_manage_control(cx, wrapper)
    }

    fn release_manage_button(
        &mut self,
        cx: &mut WidgetCx<'_>,
        remove_node: bool,
    ) -> Result<(), EngineError> {
        let Some(button) = self.manage_button.take() else {
            return Ok(());
        };
        if remove_node {
            if let Some(node) = button.node() {
                remove_own_node(cx, node)?;
            }
            if let Some(wrapper) = self.manage_parent.take() {
                remove_own_node(cx, wrapper)?;
            }
        }
        let _ = button.release(cx.app_mut());
        self.manage_parent = None;
        self.manage_language = None;

        Ok(())
    }

    fn open_dialog(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.dialog.is_some() || self.current_scope().is_none() {
            return Ok(());
        }
        let mut dialog = SolanaWalletDialog::open(cx)?;
        self.load_selections(cx);
        let requests = dialog.take_requests(cx);
        for request in requests {
            self.apply_dialog_request(request, cx)?;
        }
        let view = self.dialog_view();
        dialog.sync(cx, &view)?;
        if dialog.finished() {
            dialog.release(cx);
        } else {
            self.dialog = Some(dialog);
        }
        cx.request_frame()?;
        Ok(())
    }

    fn drive_dialog(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(mut dialog) = self.dialog.take() else {
            return Ok(());
        };
        let requests = dialog.take_requests(cx);
        for request in requests {
            self.apply_dialog_request(request, cx)?;
        }
        let view = self.dialog_view();
        dialog.sync(cx, &view)?;
        if dialog.finished() {
            dialog.release(cx);
        } else {
            self.dialog = Some(dialog);
        }
        Ok(())
    }

    fn apply_dialog_request(
        &mut self,
        request: DialogRequest,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        match request {
            DialogRequest::Reload => {
                self.load_selections(cx);
            }
            DialogRequest::Add(address) => {
                self.add_selection(address, cx);
            }
            DialogRequest::Delete(address) => {
                self.delete_selection(address, cx);
            }
        }
        Ok(())
    }

    fn dialog_view(&self) -> DialogView {
        DialogView {
            signed_in: self.account_id.is_some(),
            ready: self.ready,
            selections: self.selections.clone(),
            operation: self
                .selection_pending
                .as_ref()
                .map(|pending| pending.operation),
            notice: self.notice.clone(),
            recovery: self.recovery.clone(),
        }
    }

    fn load_selections(&mut self, cx: &mut WidgetCx<'_>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        if self.selection_pending.is_some() {
            return false;
        }
        self.selection_pending = Some(SelectionPending {
            operation: if self.selections.is_some() {
                SelectionOperation::Refreshing
            } else {
                SelectionOperation::Loading
            },
            address: None,
            sent: false,
        });
        self.notice = None;
        self.selection_read = SelectionRead::NotRead;
        if self.ready {
            self.send_selections(scope, cx);
        }
        true
    }

    fn send_pending_selections(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(scope) = self.current_scope() else {
            return;
        };
        if self
            .selection_pending
            .as_ref()
            .is_some_and(|pending| !pending.sent && pending.operation.is_read())
        {
            self.send_selections(scope, cx);
        }
    }

    fn send_selections(&mut self, scope: SolanaWalletScope, cx: &mut WidgetCx<'_>) {
        if let Some(pending) = self.selection_pending.as_mut() {
            pending.sent = true;
        }
        self.call(
            cx,
            SolanaWalletRequest::FetchSelections(scope),
            PendingCommand::Read,
        );
    }

    fn add_selection(&mut self, address: SmolStr, cx: &mut WidgetCx<'_>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        if !self.may_change(cx) || solana_wallet_address(&address).is_none() {
            return false;
        }
        self.begin_change(SelectionOperation::Adding, Some(address.clone()));
        self.call(
            cx,
            SolanaWalletRequest::AddSelection { scope, address },
            PendingCommand::Add,
        );
        true
    }

    fn delete_selection(&mut self, address: SmolStr, cx: &mut WidgetCx<'_>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        if !self.may_change(cx) {
            return false;
        }
        self.begin_change(SelectionOperation::Deleting, Some(address.clone()));
        self.call(
            cx,
            SolanaWalletRequest::DeleteSelection { scope, address },
            PendingCommand::Delete,
        );
        true
    }

    fn may_change(&self, cx: &WidgetCx<'_>) -> bool {
        self.ready
            && self.selections.is_some()
            && self.selection_pending.is_none()
            && !self.status.is_failed(cx.app())
    }

    fn begin_change(&mut self, operation: SelectionOperation, address: Option<SmolStr>) {
        self.selection_pending = Some(SelectionPending {
            operation,
            address,
            sent: true,
        });
        self.notice = None;
    }

    fn apply_selections_result(
        &mut self,
        result: WidgetRpcResult<SolanaWalletProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        self.selection_pending = None;
        match result {
            Ok(SolanaWalletResponse::Selections(data)) => {
                self.selections = Some(data);
                self.notice = None;
                self.selection_read = SelectionRead::Known;
            }
            Ok(_) => {}
            Err(error) => {
                self.selection_read = SelectionRead::Failed(error.clone());
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
            }
        }
        if self.ready {
            self.report(cx);
        }
    }

    fn apply_add_result(
        &mut self,
        result: WidgetRpcResult<SolanaWalletProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        let requested = self
            .selection_pending
            .take()
            .and_then(|pending| pending.address);
        let outcome = match result {
            Ok(SolanaWalletResponse::Added(outcome)) => outcome,
            Ok(_) => return,
            Err(error) => {
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
                return;
            }
        };
        self.notice = match outcome {
            SolanaWalletMutationOutcome::Confirmed(selection) => {
                let grown = requested.as_ref().and_then(|address| {
                    self.selections
                        .as_ref()?
                        .with_added(address, selection)
                        .ok()
                });
                match grown {
                    Some(data) => {
                        self.selections = Some(data);
                        Some(SelectionNotice::Added)
                    }
                    None => Some(inconsistent()),
                }
            }
            SolanaWalletMutationOutcome::Rejected(error) => {
                Some(SelectionNotice::Problem(SelectionFailure::Rejected(error)))
            }
            SolanaWalletMutationOutcome::Unknown(unknown) => {
                Some(SelectionNotice::Problem(SelectionFailure::Unknown(unknown)))
            }
        };
        let _ = cx.request_frame();
    }

    fn apply_delete_result(
        &mut self,
        result: WidgetRpcResult<SolanaWalletProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        let requested = self
            .selection_pending
            .take()
            .and_then(|pending| pending.address);
        let outcome = match result {
            Ok(SolanaWalletResponse::Deleted(outcome)) => outcome,
            Ok(_) => return,
            Err(error) => {
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
                return;
            }
        };
        self.notice = match outcome {
            SolanaWalletMutationOutcome::Confirmed(()) => {
                let shrunk = requested.as_ref().and_then(|address| {
                    self.selections
                        .as_ref()
                        .map(|data| data.with_deleted(address))
                });
                match shrunk {
                    Some(data) => {
                        self.selections = Some(data);
                        None
                    }
                    None => Some(inconsistent()),
                }
            }
            SolanaWalletMutationOutcome::Rejected(error) => {
                Some(SelectionNotice::Problem(SelectionFailure::Rejected(error)))
            }
            SolanaWalletMutationOutcome::Unknown(unknown) => {
                Some(SelectionNotice::Problem(SelectionFailure::Unknown(unknown)))
            }
        };
        let _ = cx.request_frame();
    }
}

fn inconsistent() -> SelectionNotice {
    SelectionNotice::Problem(SelectionFailure::Inconsistent(
        SolanaWalletDataError::InconsistentSelections,
    ))
}

fn remove_own_node(cx: &mut WidgetCx<'_>, node: NodeId) -> Result<(), EngineError> {
    if cx.tree().get(node).is_err() {
        return Ok(());
    }
    let window = cx.window();
    cx.app_mut().remove_node(window, node).map(|_| ())
}

#[derive(Clone)]
struct TransferRowContext {
    items: Signal<TransferSnapshot>,
    language: Signal<LanguageCode>,
    config: Signal<app_config::settings::Config>,
    theme: ThemeHandle,
    translate: Translate,
}

fn build_transfer_row(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    context: &TransferRowContext,
) -> Result<(), EngineError> {
    let a11y_items = context.items;
    let a11y_language = context.language;
    let a11y_config = context.config;
    let a11y_translate = context.translate.clone();
    let row = NodeBuilder::boxed()
        .id(format_smolstr!("TEST_SOLANA_WALLET_TRANSFER_ROW:{index}"))
        .style(
            StyleBuilder::new()
                .flex_col()
                .gap(4.0)
                .width_full()
                .min_width_0()
                .padding(8.0)
                .build(),
        )
        .bind_a11y(move |cx| {
            let _language: ui_core::node::signal::SignalRef<LanguageCode> = cx.read(&a11y_language);
            let snapshot = cx.read(&a11y_items);
            let settings = cx.read(&a11y_config);
            let Some(item) = snapshot.items().get(index) else {
                return A11yPatch::default();
            };
            let zone = data_time_zone(&settings);
            let display = item.display(zone);
            A11yPatch {
                label: Some(t_smol!(
                    a11y_translate,
                    "widget.solana_wallet.transfer",
                    amount = display.amount.clone(),
                    time = display.time.clone()
                )),
                ..A11yPatch::default()
            }
        })
        .build(tree, parent)?;

    let amount = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(8.0)
                .width_full()
                .min_width_0()
                .build(),
        )
        .build(tree, row)?;

    let amount_items = context.items;
    let amount_config = context.config;
    let amount_theme = context.theme.clone();
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(TRANSFER_TEXT_SIZE.text_size())
            .with_overflow(TextOverflow::Ellipsis, 1)
            .with_weight(600),
    )
    .id(format_smolstr!(
        "TEST_SOLANA_WALLET_TRANSFER_AMOUNT:{index}"
    ))
    .style(StyleBuilder::new().flex_1().min_width_0().build())
    .bind_text(move |cx| {
        let snapshot = cx.read(&amount_items);
        let settings = cx.read(&amount_config);
        let zone = data_time_zone(&settings);
        snapshot
            .items()
            .get(index)
            .map_or_else(SmolStr::default, |item| item.display(zone).amount.clone())
    })
    .bind_style(move |cx| StylePatch {
        foreground: Some(amount_theme.role(cx, Role::FgDefault)),
        ..StylePatch::default()
    })
    .build(tree, amount)?;

    let time_items = context.items;
    let time_config = context.config;
    let time_theme = context.theme.clone();
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(TRANSFER_TIME_SIZE.text_size()),
    )
    .id(format_smolstr!("TEST_SOLANA_WALLET_TRANSFER_TIME:{index}"))
    .style(StyleBuilder::new().flex_none().build())
    .bind_text(move |cx| {
        let snapshot = cx.read(&time_items);
        let settings = cx.read(&time_config);
        let zone = data_time_zone(&settings);
        snapshot
            .items()
            .get(index)
            .map_or_else(SmolStr::default, |item| item.display(zone).time.clone())
    })
    .bind_style(move |cx| StylePatch {
        foreground: Some(time_theme.role(cx, Role::FgMuted)),
        ..StylePatch::default()
    })
    .build(tree, amount)?;

    build_address_line(
        tree,
        row,
        index,
        "widget.solana_wallet.from",
        AddressEnd::From,
        context,
    )?;
    build_address_line(
        tree,
        row,
        index,
        "widget.solana_wallet.to",
        AddressEnd::To,
        context,
    )?;

    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddressEnd {
    From,
    To,
}

fn build_address_line(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    label_key: &'static str,
    end: AddressEnd,
    context: &TransferRowContext,
) -> Result<(), EngineError> {
    let line = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(8.0)
                .min_width_0()
                .build(),
        )
        .build(tree, parent)?;

    let label_language = context.language;
    let label_translate = context.translate.clone();
    let label_theme = context.theme.clone();
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(TRANSFER_TIME_SIZE.text_size()),
    )
    .id(format_smolstr!("TEST_SOLANA_WALLET_TRANSFER_LABEL:{index}"))
    .style(
        StyleBuilder::new()
            .width(ADDRESS_LABEL_WIDTH)
            .flex_none()
            .build(),
    )
    .bind_text(move |cx| {
        let _language = cx.read(&label_language);
        t_smol!(label_translate, label_key)
    })
    .bind_style(move |cx| StylePatch {
        foreground: Some(label_theme.role(cx, Role::FgMuted)),
        ..StylePatch::default()
    })
    .build(tree, line)?;

    let address_items = context.items;
    let address_theme = context.theme.clone();
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(TRANSFER_TIME_SIZE.text_size()).with_overflow(TextOverflow::Ellipsis, 1),
    )
    .id(format_smolstr!(
        "TEST_SOLANA_WALLET_TRANSFER_ADDRESS:{index}"
    ))
    .style(StyleBuilder::new().flex_1().min_width_0().build())
    .bind_text(move |cx| {
        let snapshot = cx.read(&address_items);
        snapshot
            .items()
            .get(index)
            .map_or_else(SmolStr::default, |item| match end {
                AddressEnd::From => item.origin().clone(),
                AddressEnd::To => item.destination().clone(),
            })
    })
    .bind_style(move |cx| StylePatch {
        foreground: Some(address_theme.role(cx, Role::FgDefault)),
        ..StylePatch::default()
    })
    .build(tree, line)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{rc::Rc, time::Duration};

    use app_config::{
        settings::{AccountConfig, AccountWalletKind, Config},
        time_zone::DataTimeZone,
        widget::{VersionedWidgetConfig, decode_or_default},
    };
    use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
    use kytrix_service_provider::service::solana_wallet::{
        SolanaWalletMutationOutcome, SolanaWalletSelection,
    };
    use network::transport::error::TransportError;
    use ui_core::event::{Action, ActionSource};
    use ui_kit::kit::button::Button;
    use widget::{protocol::RequestId, test_support::WidgetTestApp};

    use super::*;
    use crate::test_support::{
        ACCOUNT, FakeFeed, FakeSession, FakeSource, selection, selection_at, selections,
    };

    const FIRST: &str = "So11111111111111111111111111111111111111112";
    const SECOND: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    const OTHER_ACCOUNT: &str = "someone-else";

    const SETTLE_TURNS: usize = 64;

    fn translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    fn harness() -> WidgetTestApp {
        WidgetTestApp::with_translate(translate(LanguageCode::En))
    }

    fn current_config(harness: &WidgetTestApp) -> Config {
        let signal = harness.services().config;
        (*harness.app().read(&signal)).clone()
    }

    fn write_config(harness: &mut WidgetTestApp, config: Config) -> bool {
        let signal = harness.services().config;
        harness.app_mut().write(&signal, config)
    }

    fn sign_in(harness: &mut WidgetTestApp, user_id: &str) {
        let mut config = current_config(harness);
        config.account = Some(AccountConfig {
            user_id: user_id.to_owned(),
            wallet_address: FIRST.to_owned(),
            wallet_kind: AccountWalletKind::Solana,
        });
        assert!(
            write_config(harness, config),
            "signing in is a settings change"
        );
    }

    fn sign_out(harness: &mut WidgetTestApp) {
        let mut config = current_config(harness);
        config.account = None;
        assert!(
            write_config(harness, config),
            "signing out is a settings change"
        );
    }

    struct Pane {
        widget: SolanaWalletWidget,
        status: WidgetStatus,
        parent: NodeId,
    }

    fn pane_with(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
        let scope = harness.scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let parent = harness.placeholder_parent();
        let widget = harness.with_cx(|cx| {
            SolanaWalletWidget::new_with_source(
                source,
                status,
                WidgetAction::new("common.retry", |_| {}),
                cx,
            )
        });

        Pane {
            widget,
            status,
            parent,
        }
    }

    fn pane(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
        sign_in(harness, ACCOUNT);
        pane_with(harness, source)
    }

    fn signed_out_pane(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
        pane_with(harness, source)
    }

    fn quiet_pane(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
        let mut pane = pane(harness, source);
        if let Some(binding) = pane.widget.binding.as_mut() {
            binding.cancel();
        }
        pane
    }

    fn live_pane(
        harness: &mut WidgetTestApp,
        source: &FakeSource,
        session: &FakeSession,
        addresses: Vec<SolanaWalletSelection>,
        limit: usize,
    ) -> Pane {
        session.answer_selections(selections(addresses, limit));
        let mut pane = pane(harness, source.clone());
        load(harness, &mut pane);
        settle(harness, &mut pane, "the addresses to arrive", |_, pane| {
            pane.widget.selections.is_some()
        });

        pane
    }

    fn load(harness: &mut WidgetTestApp, pane: &mut Pane) {
        harness.with_cx(|cx| pane.widget.load(cx));
    }

    fn sync(harness: &mut WidgetTestApp, pane: &mut Pane) {
        harness
            .with_cx(|cx| pane.widget.sync(cx))
            .expect("the widget syncs");
    }

    fn build_body(harness: &mut WidgetTestApp, pane: &mut Pane) {
        let parent = pane.parent;
        harness
            .with_cx(|cx| pane.widget.body(parent, cx))
            .expect("the body builds");
    }

    fn frame(harness: &mut WidgetTestApp, pane: &mut Pane) {
        sync(harness, pane);
        harness.drain();
        harness.drain();
    }

    fn settle(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        what: &str,
        ready: impl Fn(&WidgetTestApp, &Pane) -> bool,
    ) {
        for _ in 0..SETTLE_TURNS {
            harness.drain_runtime();
            sync(harness, pane);
            harness.drain();
            if ready(harness, pane) {
                return;
            }
        }
        panic!("{what} should happen");
    }

    fn report_feed(harness: &mut WidgetTestApp, pane: &mut Pane, feed: TransferSnapshot) -> bool {
        let scope = pane
            .widget
            .current_scope()
            .expect("the pane follows an account");
        if let Some(binding) = pane.widget.binding.as_mut() {
            binding.cancel();
        }
        harness.with_cx(|cx| {
            pane.widget.apply_latest(
                scope,
                Arc::new(SolanaWalletStreamMessage::Snapshot(feed)),
                cx,
            )
        })
    }

    fn report_status(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        status: SolanaWalletStatus,
    ) -> bool {
        let scope = pane
            .widget
            .current_scope()
            .expect("the pane follows an account");
        if let Some(binding) = pane.widget.binding.as_mut() {
            binding.cancel();
        }
        harness.with_cx(|cx| {
            pane.widget.apply_reliable(
                scope,
                Arc::new(SolanaWalletStreamMessage::Status(status)),
                cx,
            )
        })
    }

    fn state_of(harness: &WidgetTestApp, pane: &Pane) -> WidgetState {
        pane.status.state(harness.app())
    }

    fn report_status_live(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        status: SolanaWalletStatus,
    ) -> bool {
        let scope = pane
            .widget
            .current_scope()
            .expect("the pane follows an account");
        harness.with_cx(|cx| {
            pane.widget.apply_reliable(
                scope,
                Arc::new(SolanaWalletStreamMessage::Status(status)),
                cx,
            )
        })
    }

    fn failure_text(harness: &WidgetTestApp, pane: &Pane) -> String {
        state_of(harness, pane)
            .failure()
            .expect("the pane reported a failure")
            .text(&harness.services().translate)
            .to_string()
    }

    fn recovery_label(harness: &WidgetTestApp, pane: &Pane) -> String {
        state_of(harness, pane)
            .failure()
            .expect("the pane reported a failure")
            .action()
            .label(&harness.services().translate)
            .to_string()
    }

    fn event_ids_of(widget: &SolanaWalletWidget) -> Vec<String> {
        widget.snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
            snapshot
                .items()
                .iter()
                .map(|item| item.event_id().to_owned())
                .collect()
        })
    }

    fn addresses_of(widget: &SolanaWalletWidget) -> Vec<String> {
        widget.selections.as_ref().map_or_else(Vec::new, |data| {
            data.selections()
                .iter()
                .map(|selection| selection.address.to_string())
                .collect()
        })
    }

    fn click_button(harness: &mut WidgetTestApp, button: &Button) {
        let node = button.node().expect("the control is built");
        let action = button
            .action_identity()
            .expect("the control registered a click handler");
        click_node(harness, node, action);
    }

    fn click_node(harness: &mut WidgetTestApp, node: NodeId, action: Action) {
        let window = harness.window();
        assert!(
            harness
                .app_mut()
                .dispatch_action(window, node, action, ActionSource::Programmatic),
            "the control is registered"
        );
    }

    fn build_header(harness: &mut WidgetTestApp, pane: &mut Pane) {
        let root = harness.root();
        harness
            .with_cx(|cx| pane.widget.header(HeaderGroup::Left, root, cx))
            .expect("the header control builds");
    }

    #[test]
    fn every_message_this_pane_shows_should_resolve_to_text() {
        let translate = translate(LanguageCode::En);

        for key in [
            "widget.names.solana_wallet",
            "widget.descriptions.solana_wallet",
            "widget.solana_wallet.add_address",
            "widget.solana_wallet.add_success",
            "widget.solana_wallet.address_placeholder",
            "widget.solana_wallet.delete_address",
            "widget.solana_wallet.dialog_title",
            "widget.solana_wallet.empty_addresses",
            "widget.solana_wallet.from",
            "widget.solana_wallet.load_failed",
            "widget.solana_wallet.loading_addresses",
            "widget.solana_wallet.logged_out",
            "widget.solana_wallet.manage",
            "widget.solana_wallet.refresh",
            "widget.solana_wallet.scroll_transfers",
            "widget.solana_wallet.stream_unavailable",
            "widget.solana_wallet.to",
            "widget.solana_wallet.transfer",
            "widget.solana_wallet.inconsistent_selections",
        ] {
            assert!(!t_smol!(translate, key).is_empty(), "{key} has no copy");
        }

        for failure in [
            SelectionFailure::Rejected(SolanaWalletApiError::Conflict),
            SelectionFailure::Unknown(SolanaWalletMutationUnknown::Timeout),
            SelectionFailure::Inconsistent(SolanaWalletDataError::InconsistentSelections),
            SelectionFailure::Rpc(RpcCallError::DeadlineExceeded {
                request_id: RequestId::new(2),
            }),
        ] {
            assert!(
                !failure.translated_message(&translate).is_empty(),
                "{failure:?} has no copy"
            );
        }

        for failure in [
            SolanaWalletFailure::Stream(SolanaWalletStreamError::Service(
                KytrixServiceError::Closed,
            )),
            SolanaWalletFailure::Stream(SolanaWalletStreamError::Transport(TransportError::Closed)),
            SolanaWalletFailure::Rpc(RpcCallError::ServiceClosed {
                request_id: RequestId::new(3),
            }),
        ] {
            assert!(
                !failure.translated_message(&translate).is_empty(),
                "{failure:?} has no copy"
            );
        }
    }

    #[test]
    fn the_widget_should_have_a_stable_identity() {
        assert_eq!(SolanaWalletWidget::ID.as_str(), "kytrix.solana_wallet");
        assert_eq!(
            SolanaWalletWidget::DISPLAY_NAME,
            "widget.names.solana_wallet"
        );
        assert_eq!(
            <SolanaWalletConfig as WidgetConfig>::SCHEMA_VERSION,
            WidgetConfigSchemaVersion::V1
        );
    }

    #[test]
    fn the_config_should_round_trip_through_its_envelope() {
        let stored = VersionedWidgetConfig::encode(&SolanaWalletConfig)
            .expect("a solana_wallet monitor config should encode");

        assert_eq!(stored.schema_version(), WidgetConfigSchemaVersion::V1);
        assert_eq!(stored.decode::<SolanaWalletConfig>(), SolanaWalletConfig);
        assert_eq!(
            decode_or_default::<SolanaWalletConfig>(None),
            SolanaWalletConfig
        );
    }

    #[test]
    fn a_pane_should_persist_no_config() {
        let mut harness = harness();
        let pane = pane(&mut harness, FakeSource::default());

        assert!(
            pane.widget.config().is_none(),
            "a pane with no options writes no envelope"
        );
    }

    #[test]
    fn a_signed_out_pane_should_say_what_it_is_waiting_for() {
        let mut harness = harness();
        let mut pane = signed_out_pane(&mut harness, FakeSource::default());

        load(&mut harness, &mut pane);

        assert!(!pane.widget.has_content());
        assert_eq!(pane.widget.current_scope(), None);
        assert_eq!(
            state_of(&harness, &pane).notice(),
            Some(&Notice::message("widget.solana_wallet.logged_out")),
            "a pane nobody signed in for is not a failure"
        );
        assert!(!state_of(&harness, &pane).is_failed());
    }

    #[test]
    fn a_signed_out_pane_should_wait_for_a_login_in_the_language_showing_now() {
        let mut harness = harness();
        let mut pane = signed_out_pane(&mut harness, FakeSource::default());
        load(&mut harness, &mut pane);

        let notice = state_of(&harness, &pane)
            .notice()
            .cloned()
            .expect("the pane waits");
        assert_eq!(notice, Notice::message("widget.solana_wallet.logged_out"));
        let english = notice.resolve(&translate(LanguageCode::En));
        let chinese = notice.resolve(&translate(LanguageCode::ZhCn));
        assert_eq!(
            english,
            t_smol!(
                translate(LanguageCode::En),
                "widget.solana_wallet.logged_out"
            )
        );
        assert_ne!(
            chinese, english,
            "the same reference resolves in the new language"
        );
    }

    #[test]
    fn an_account_with_no_addresses_should_wait_in_the_language_showing_now() {
        let mut harness = harness();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(&mut harness, &source, &session, Vec::new(), 3);

        assert_eq!(
            state_of(&harness, &pane).notice(),
            Some(&Notice::message("widget.solana_wallet.empty_addresses")),
            "an account that monitors nothing waits with its own sentence"
        );
        assert!(!pane.widget.has_content());

        let notice = state_of(&harness, &pane)
            .notice()
            .cloned()
            .expect("the pane waits");
        assert_ne!(
            notice.resolve(&translate(LanguageCode::ZhCn)),
            notice.resolve(&translate(LanguageCode::En)),
            "the reference is resolved in the language showing now"
        );
    }

    #[test]
    fn a_feed_should_become_content_and_readiness_should_follow_it() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();
        let live = feed.push("one", 1);

        report_status(&mut harness, &mut pane, SolanaWalletStatus::Ready);
        assert!(report_feed(&mut harness, &mut pane, live));
        assert!(pane.widget.has_content(), "an empty feed is still a feed");
        assert_eq!(event_ids_of(&pane.widget), ["one"]);
        assert_eq!(
            state_of(&harness, &pane),
            WidgetState::Loading,
            "the pane follows an account it has not been told the addresses of yet"
        );

        harness.with_cx(|cx| {
            pane.widget.apply_selections_result(
                Ok(SolanaWalletResponse::Selections(SelectionData::new(
                    selections(vec![selection(FIRST)], 3),
                ))),
                cx,
            );
        });
        harness.drain();
        assert_eq!(state_of(&harness, &pane), WidgetState::Ready);
        assert_eq!(event_ids_of(&pane.widget), ["one"]);
    }

    #[test]
    fn a_live_subscription_without_transfers_should_still_be_content() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        harness.with_cx(|cx| {
            pane.widget.apply_selections_result(
                Ok(SolanaWalletResponse::Selections(SelectionData::new(
                    selections(vec![selection(FIRST)], 3),
                ))),
                cx,
            );
        });
        report_feed(&mut harness, &mut pane, FakeFeed::new().snapshot());
        report_status(&mut harness, &mut pane, SolanaWalletStatus::Ready);

        assert!(pane.widget.has_content());
        assert_eq!(state_of(&harness, &pane), WidgetState::Ready);
        assert!(!state_of(&harness, &pane).is_failed());
    }

    #[test]
    fn an_account_with_no_addresses_should_say_it_has_none() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        pane.widget.ready = true;
        harness.with_cx(|cx| {
            pane.widget.apply_selections_result(
                Ok(SolanaWalletResponse::Selections(SelectionData::new(
                    selections(Vec::new(), 3),
                ))),
                cx,
            );
        });

        assert_eq!(
            state_of(&harness, &pane),
            WidgetState::waiting(Notice::message("widget.solana_wallet.empty_addresses"))
        );
        assert!(!state_of(&harness, &pane).is_failed());
        assert!(
            !pane.widget.has_content(),
            "with nothing to follow the pane has nothing to draw"
        );
    }

    #[test]
    fn a_pane_reading_its_addresses_should_be_loading() {
        let mut harness = harness();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let block = session.block_selections();
        session.answer_selections(selections(vec![selection(FIRST)], 3));
        let mut pane = pane(&mut harness, source);
        load(&mut harness, &mut pane);
        settle(&mut harness, &mut pane, "the read to be sent", |_, _| {
            session.state().fetches == 1
        });

        assert!(pane.widget.has_content());
        assert!(pane.widget.selections.is_none());
        assert_eq!(
            state_of(&harness, &pane),
            WidgetState::Loading,
            "a pane that does not yet know what it follows is loading"
        );

        block.release();
        settle(
            &mut harness,
            &mut pane,
            "the addresses to arrive",
            |_, pane| pane.widget.selections.is_some(),
        );
        assert_eq!(state_of(&harness, &pane), WidgetState::Ready);
    }

    #[test]
    fn a_read_that_failed_should_be_reported_rather_than_swallowed() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        pane.widget.ready = true;
        harness.with_cx(|cx| {
            pane.widget.apply_selections_result(
                Err(RpcCallError::ServiceClosed {
                    request_id: RequestId::new(2),
                }),
                cx,
            );
        });

        let state = state_of(&harness, &pane);
        assert!(state.is_failed());
        assert!(state.notice().is_none());
        assert_eq!(recovery_label(&harness, &pane), "Retry");
        assert!(pane.widget.selections.is_none());
    }

    #[test]
    fn a_pane_without_a_dialog_should_read_the_addresses() {
        let mut harness = harness();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(&mut harness, &source, &session, vec![selection(FIRST)], 3);

        assert_eq!(addresses_of(&pane.widget), [FIRST.to_owned()]);
        assert_eq!(
            session.state().fetches,
            1,
            "the pane asks for the addresses on its own behalf"
        );
    }

    #[test]
    fn a_failure_should_be_reported_with_the_panes_own_recovery() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();
        let live = feed.push("one", 1);
        report_feed(&mut harness, &mut pane, live);
        report_status(
            &mut harness,
            &mut pane,
            SolanaWalletStatus::Failed(SolanaWalletStreamError::Transport(TransportError::Closed)),
        );

        assert!(state_of(&harness, &pane).is_failed());
        assert_eq!(
            failure_text(&harness, &pane),
            failure_text_of(
                &harness,
                &SolanaWalletFailure::Stream(SolanaWalletStreamError::Transport(
                    TransportError::Closed
                ))
            )
        );
        assert_eq!(recovery_label(&harness, &pane), "Retry");
        assert!(
            pane.widget.has_content(),
            "transfers that were read before a failure are not dropped by it"
        );
    }

    fn failure_text_of(harness: &WidgetTestApp, failure: &SolanaWalletFailure) -> String {
        failure
            .translated_message(&harness.services().translate)
            .to_string()
    }

    #[test]
    fn a_message_after_a_failure_should_be_refused() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        report_status(
            &mut harness,
            &mut pane,
            SolanaWalletStatus::Failed(SolanaWalletStreamError::Transport(TransportError::Closed)),
        );

        let mut feed = FakeFeed::new();
        let live = feed.push("one", 1);
        assert!(
            !report_feed(&mut harness, &mut pane, live),
            "a failure is terminal until the user recovers it"
        );
        assert_eq!(event_ids_of(&pane.widget), Vec::<String>::new());
    }

    #[test]
    fn a_message_for_another_account_should_be_refused() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();
        let live = feed.push("one", 1);
        let other = SolanaWalletScope::new(Some(&SmolStr::new_static(OTHER_ACCOUNT)))
            .expect("an account is a scope");

        let changed = harness.with_cx(|cx| {
            pane.widget.apply_latest(
                other,
                Arc::new(SolanaWalletStreamMessage::Snapshot(live)),
                cx,
            )
        });

        assert!(!changed, "another account's transfers are not this pane's");
        assert!(!pane.widget.has_content());
    }

    #[test]
    fn a_recovery_should_keep_the_transfers_and_an_account_change_should_not() {
        let mut harness = harness();
        let mut pane = pane(&mut harness, FakeSource::default());
        if let Some(binding) = pane.widget.binding.as_mut() {
            binding.cancel();
        }
        let mut feed = FakeFeed::new();
        let live = feed.push("one", 1);
        report_feed(&mut harness, &mut pane, live);

        harness.with_cx(|cx| {
            pane.widget
                .start(true, cx)
                .expect("a recovery starts again");
        });
        harness.drain();
        assert!(pane.widget.has_content());
        assert_eq!(state_of(&harness, &pane), WidgetState::Connecting);

        sign_in(&mut harness, OTHER_ACCOUNT);
        sync(&mut harness, &mut pane);
        assert!(!pane.widget.has_content());
        assert!(pane.widget.selections.is_none());
    }

    #[test]
    fn an_account_change_should_read_the_new_account() {
        let mut harness = harness();
        let source = FakeSource::default();
        let first = source.session(ACCOUNT);
        let _first_control = first.subscribe();
        let second = source.session(OTHER_ACCOUNT);
        let _second_control = second.subscribe();
        let mut pane = pane(&mut harness, source.clone());
        load(&mut harness, &mut pane);
        settle(
            &mut harness,
            &mut pane,
            "the stream to come up",
            |_, pane| pane.widget.ready,
        );

        sign_in(&mut harness, OTHER_ACCOUNT);
        sync(&mut harness, &mut pane);
        settle(
            &mut harness,
            &mut pane,
            "the replacement session",
            |_, _| second.state().subscribes == 1,
        );

        assert_eq!(pane.widget.account_id.as_deref(), Some(OTHER_ACCOUNT));
        assert_eq!(
            source.requested(),
            [
                SmolStr::new_static(ACCOUNT),
                SmolStr::new_static(OTHER_ACCOUNT),
            ],
            "the session the pane opened is the new account's"
        );
    }

    #[test]
    fn a_read_should_wait_for_the_stream_and_be_sent_once_it_is_live() {
        let mut harness = harness();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_selections(selections(vec![selection(FIRST)], 3));
        let mut pane = pane(&mut harness, source);
        load(&mut harness, &mut pane);
        assert!(harness.with_cx(|cx| pane.widget.load_selections(cx)));

        assert_eq!(
            session.state().fetches,
            0,
            "a read before the stream is live has no session to answer it"
        );
        settle(
            &mut harness,
            &mut pane,
            "the addresses to arrive",
            |_, pane| pane.widget.selections.is_some(),
        );
        assert_eq!(session.state().fetches, 1);
    }

    #[test]
    fn a_stream_that_came_back_should_read_the_addresses_again() {
        let mut harness = harness();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let mut pane = live_pane(&mut harness, &source, &session, vec![selection(FIRST)], 3);
        harness
            .with_cx(|cx| pane.widget.open_dialog(cx))
            .expect("the dialog opens");
        assert_eq!(session.state().fetches, 1);
        session.answer_selections(selections(vec![selection(FIRST), selection(SECOND)], 3));

        report_status_live(&mut harness, &mut pane, SolanaWalletStatus::Connecting);
        report_status_live(&mut harness, &mut pane, SolanaWalletStatus::Ready);

        settle(
            &mut harness,
            &mut pane,
            "the addresses to be read again",
            |_, pane| addresses_of(&pane.widget).len() == 2,
        );
        assert_eq!(
            session.state().fetches,
            2,
            "the list a pane shows is only as true as the last read over the live stream"
        );
    }

    #[test]
    fn a_read_that_failed_should_say_why_and_leave_nothing_behind() {
        let mut harness = harness();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.refuse_selections(SolanaWalletApiError::Unavailable);
        let mut pane = pane(&mut harness, source);
        load(&mut harness, &mut pane);
        settle(
            &mut harness,
            &mut pane,
            "the read to be refused",
            |_, pane| matches!(pane.widget.selection_read, SelectionRead::Failed(_)),
        );
        settle(
            &mut harness,
            &mut pane,
            "the refusal to arrive",
            |_, pane| pane.widget.selection_pending.is_none(),
        );

        assert!(pane.widget.selections.is_none());
        assert!(matches!(
            pane.widget.notice,
            Some(SelectionNotice::Problem(SelectionFailure::Rpc(
                RpcCallError::Service {
                    source: SolanaWalletApiError::Unavailable,
                    ..
                }
            )))
        ));
        assert!(state_of(&harness, &pane).is_failed());
    }

    #[test]
    fn an_address_the_service_confirmed_should_join_the_list() {
        let mut harness = harness();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let mut pane = live_pane(
            &mut harness,
            &source,
            &session,
            vec![selection_at(FIRST, 1)],
            3,
        );
        session
            .state()
            .adds
            .push_back(SolanaWalletMutationOutcome::Confirmed(selection_at(
                SECOND, 2,
            )));

        assert!(
            harness.with_cx(|cx| { pane.widget.add_selection(SmolStr::new_static(SECOND), cx) })
        );
        settle(
            &mut harness,
            &mut pane,
            "the address to be added",
            |_, pane| addresses_of(&pane.widget).len() == 2,
        );

        assert_eq!(
            addresses_of(&pane.widget),
            [SECOND.to_owned(), FIRST.to_owned()],
            "the confirmed address reads in the order a fresh answer would"
        );
        assert_eq!(pane.widget.notice, Some(SelectionNotice::Added));
        assert!(pane.widget.selection_pending.is_none());
    }

    #[test]
    fn an_address_the_service_refused_should_say_why() {
        let mut harness = harness();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let mut pane = live_pane(&mut harness, &source, &session, vec![selection(FIRST)], 3);
        session
            .state()
            .adds
            .push_back(SolanaWalletMutationOutcome::Rejected(
                SolanaWalletApiError::Conflict,
            ));

        harness.with_cx(|cx| {
            pane.widget.add_selection(SmolStr::new_static(SECOND), cx);
        });
        settle(
            &mut harness,
            &mut pane,
            "the refusal to arrive",
            |_, pane| pane.widget.selection_pending.is_none(),
        );

        assert_eq!(
            pane.widget.notice,
            Some(SelectionNotice::Problem(SelectionFailure::Rejected(
                SolanaWalletApiError::Conflict
            )))
        );
        assert_eq!(addresses_of(&pane.widget), [FIRST.to_owned()]);
    }

    #[test]
    fn a_change_of_an_unknown_outcome_should_say_so_and_leave_the_list_alone() {
        let mut harness = harness();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let mut pane = live_pane(&mut harness, &source, &session, vec![selection(FIRST)], 3);
        session
            .state()
            .adds
            .push_back(SolanaWalletMutationOutcome::Unknown(
                SolanaWalletMutationUnknown::Timeout,
            ));

        harness.with_cx(|cx| {
            pane.widget.add_selection(SmolStr::new_static(SECOND), cx);
        });
        settle(
            &mut harness,
            &mut pane,
            "the outcome to arrive",
            |_, pane| pane.widget.selection_pending.is_none(),
        );

        assert_eq!(
            pane.widget.notice,
            Some(SelectionNotice::Problem(SelectionFailure::Unknown(
                SolanaWalletMutationUnknown::Timeout
            )))
        );
        assert_eq!(addresses_of(&pane.widget), [FIRST.to_owned()]);
    }

    #[test]
    fn an_address_the_service_confirmed_removed_should_leave_the_list() {
        let mut harness = harness();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let mut pane = live_pane(
            &mut harness,
            &source,
            &session,
            vec![selection(FIRST), selection(SECOND)],
            3,
        );
        session
            .state()
            .deletes
            .push_back(SolanaWalletMutationOutcome::Confirmed(()));

        assert!(
            harness.with_cx(|cx| { pane.widget.delete_selection(SmolStr::new_static(FIRST), cx) })
        );
        settle(
            &mut harness,
            &mut pane,
            "the address to be left",
            |_, pane| addresses_of(&pane.widget) == [SECOND.to_owned()],
        );

        assert_eq!(
            pane.widget.notice, None,
            "a removal is visible in the list itself"
        );
        assert!(pane.widget.selection_pending.is_none());
    }

    #[test]
    fn a_change_should_be_refused_while_the_stream_is_not_live() {
        let mut harness = harness();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let mut pane = live_pane(&mut harness, &source, &session, vec![selection(FIRST)], 3);
        report_status(&mut harness, &mut pane, SolanaWalletStatus::Connecting);

        assert!(
            !harness.with_cx(|cx| { pane.widget.add_selection(SmolStr::new_static(SECOND), cx) }),
            "a change goes over the session the stream was opened with"
        );
        assert_eq!(
            session.state().add_calls,
            0,
            "a refused change never reaches the account"
        );
    }

    #[test]
    fn a_change_a_recovery_replaced_should_be_forgotten() {
        let mut harness = harness();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let mut pane = live_pane(&mut harness, &source, &session, vec![selection(FIRST)], 3);
        let block = session.block_mutation();
        harness.with_cx(|cx| {
            pane.widget.add_selection(SmolStr::new_static(SECOND), cx);
        });
        settle(
            &mut harness,
            &mut pane,
            "the change to reach the account",
            |_, _| session.state().add_calls == 1,
        );

        let recovered = source.session(ACCOUNT);
        let _recovered_control = recovered.subscribe();
        harness.with_cx(|cx| {
            pane.widget
                .start(true, cx)
                .expect("a recovery starts again");
        });

        assert!(pane.widget.selection_pending.is_none());
        assert!(pane.widget.notice.is_none());
        assert_eq!(
            session.state().add_calls,
            1,
            "a change of an unknown outcome is not asked again"
        );
        for _ in 0..SETTLE_TURNS {
            harness.drain_runtime();
            if block.is_dropped() {
                return;
            }
        }
        panic!("the abandoned change should be dropped");
    }

    #[test]
    fn the_answer_to_a_replaced_command_should_be_dropped() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        load(&mut harness, &mut pane);
        harness.with_cx(|cx| {
            pane.widget.load(cx);
        });

        harness.with_cx(|cx| {
            pane.widget.apply_rpc(
                RequestId::new(1),
                Err(RpcCallError::ServiceClosed {
                    request_id: RequestId::new(1),
                }),
                cx,
            );
        });

        assert!(
            !state_of(&harness, &pane).is_failed(),
            "the old failure is not the new command's"
        );
        assert_eq!(
            pane.widget.pending.dropped(),
            1,
            "the late answer is dropped and counted"
        );
    }

    #[test]
    fn a_failed_pane_should_not_repaint_its_failure_from_a_late_reply() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        load(&mut harness, &mut pane);
        let failed = SolanaWalletStreamError::Transport(TransportError::Closed);
        report_status(&mut harness, &mut pane, SolanaWalletStatus::Failed(failed));
        let before = failure_text(&harness, &pane);

        harness.with_cx(|cx| {
            pane.widget.apply_rpc(
                RequestId::new(1),
                Err(RpcCallError::ServiceClosed {
                    request_id: RequestId::new(1),
                }),
                cx,
            );
        });

        assert_eq!(
            failure_text(&harness, &pane),
            before,
            "a failure is terminal"
        );
    }

    #[test]
    fn a_pane_should_draw_its_feed() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();
        let live = feed.push("one", 1_700_000_000_000);
        report_feed(&mut harness, &mut pane, live);
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);

        let list = pane.widget.list.as_ref().expect("the body built the list");
        assert_eq!(list.count(harness.app()), 1);
        assert_eq!(list.mounted_rows(harness.app()).len(), 1);
        let body = pane.widget.body_root.expect("the body was built");
        let text = harness.text_of(body);
        assert!(text.contains("1.25 SOL"), "the feed's row is drawn: {text}");
        assert!(
            text.contains(FIRST),
            "the row names the account it came from: {text}"
        );
        assert!(
            text.contains(SECOND),
            "the row names the account it went to: {text}"
        );
        assert!(
            text.contains("2023-11-14 22:13:20"),
            "the row reads the time in the display zone: {text}"
        );
    }

    #[test]
    fn moving_the_display_zone_should_re_read_the_row_time() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();
        report_feed(
            &mut harness,
            &mut pane,
            feed.push("instant", 1_700_000_000_000),
        );
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        let body = pane.widget.body_root.expect("the body was built");
        assert!(harness.text_of(body).contains("2023-11-14 22:13:20"));

        let mut config = current_config(&harness);
        config.data_time_zone = DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset");
        assert!(write_config(&mut harness, config));
        frame(&mut harness, &mut pane);

        let text = harness.text_of(body);
        assert!(
            text.contains("2023-11-15 06:13:20"),
            "the row is drawn in the zone the settings display data in: {text}"
        );
        assert!(
            !text.contains("2023-11-14 22:13:20"),
            "the old clock is not left behind: {text}"
        );
    }

    #[test]
    fn a_feed_nothing_of_which_survives_should_be_held_by_a_list_built_again() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();
        report_feed(&mut harness, &mut pane, feed.push("one", 1));
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        report_feed(&mut harness, &mut pane, feed.push("two", 2));
        frame(&mut harness, &mut pane);
        assert_eq!(event_ids_of(&pane.widget), ["two", "one"]);

        let mut replacement = FakeFeed::new();
        report_feed(&mut harness, &mut pane, replacement.push("three", 3));
        frame(&mut harness, &mut pane);

        assert_eq!(event_ids_of(&pane.widget), ["three"]);
        let list = pane.widget.list.as_ref().expect("the body built the list");
        assert_eq!(list.count(harness.app()), 1);
        assert_eq!(list.mounted_rows(harness.app()).len(), 1);
        let body = pane.widget.body_root.expect("the body was built");
        let text = harness.text_of(body);
        assert!(
            text.contains("1.25 SOL"),
            "the surviving feed is drawn: {text}"
        );
    }

    #[test]
    fn a_feed_with_no_transfers_should_say_the_body_holds_nothing() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        pane.widget.selections = Some(SelectionData::new(selections(vec![selection(FIRST)], 3)));
        pane.widget.selection_read = SelectionRead::Known;
        report_feed(&mut harness, &mut pane, FakeFeed::new().snapshot());
        report_status(&mut harness, &mut pane, SolanaWalletStatus::Ready);
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);

        assert!(pane.widget.has_content());
        assert_eq!(state_of(&harness, &pane), WidgetState::Ready);
        let body = pane.widget.body_root.expect("the body was built");
        let empty = harness
            .find_by_debug_prefix(body, "TEST_SOLANA_WALLET_EMPTY")
            .expect("an empty feed draws its stand-in");
        assert!(
            harness.text_of(empty).contains("No data"),
            "the stand-in says the feed holds nothing"
        );
        assert!(
            harness
                .tree()
                .get(empty)
                .is_ok_and(|node| !node.children().is_empty()),
            "the stand-in draws its sentence"
        );
        let list = pane.widget.list.as_ref().expect("the body built the list");
        assert_eq!(list.count(harness.app()), 0);
    }

    #[test]
    fn the_rows_should_follow_the_language() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();
        report_feed(&mut harness, &mut pane, feed.push("one", 1));
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        let body = pane.widget.body_root.expect("the body was built");
        assert!(harness.text_of(body).contains("From"));
        assert!(harness.text_of(body).contains("To"));

        harness.switch_language(LanguageCode::ZhCn);
        frame(&mut harness, &mut pane);

        let text = harness.text_of(body);
        let zh = translate(LanguageCode::ZhCn);
        assert!(
            text.contains(t_smol!(zh, "widget.solana_wallet.from").as_str()),
            "the row names the sending end in the language showing now: {text}"
        );
        assert!(
            text.contains(t_smol!(zh, "widget.solana_wallet.to").as_str()),
            "the row names the receiving end in the language showing now: {text}"
        );
    }

    #[test]
    fn the_manage_control_should_open_the_dialog_and_its_close_control_should_close_it() {
        let mut harness = harness();
        let mut pane = pane(&mut harness, FakeSource::default());
        build_header(&mut harness, &mut pane);
        let manage = pane
            .widget
            .manage_button
            .as_ref()
            .expect("the header built its control")
            .clone();

        click_button(&mut harness, &manage);
        sync(&mut harness, &mut pane);
        assert!(pane.widget.dialog.is_some(), "the control opens the dialog");

        let close = pane
            .widget
            .dialog
            .as_ref()
            .and_then(SolanaWalletDialog::test_close_button)
            .expect("the dialog draws the control at its foot");
        click_button(&mut harness, &close);
        settle_close(&mut harness, &mut pane);

        assert!(
            pane.widget.dialog.is_none(),
            "the dialog closed itself and was released"
        );
    }

    fn settle_close(harness: &mut WidgetTestApp, pane: &mut Pane) -> bool {
        for _ in 0..SETTLE_TURNS {
            harness.advance(Duration::from_millis(40));
            sync(harness, pane);
            if pane.widget.dialog.is_none() {
                return true;
            }
        }
        pane.widget.dialog.is_none()
    }

    #[test]
    fn the_manage_control_should_follow_the_language() {
        let mut harness = harness();
        let mut pane = pane(&mut harness, FakeSource::default());
        build_header(&mut harness, &mut pane);
        assert_eq!(
            pane.widget.manage_language,
            Some(LanguageCode::En),
            "the control is built in the language showing now"
        );

        harness.switch_language(LanguageCode::ZhCn);
        sync(&mut harness, &mut pane);

        let rebound = pane
            .widget
            .manage_button
            .as_ref()
            .expect("the rebuilt control is held")
            .clone();
        assert_eq!(
            pane.widget.manage_language,
            Some(LanguageCode::ZhCn),
            "the control is rebuilt in the language showing now"
        );
        assert_eq!(
            rebound.a11y_label_text(),
            Some(t_smol!(translate(LanguageCode::ZhCn), "widget.solana_wallet.manage").as_str())
        );
        assert!(
            harness.app().has_action_handler(
                &rebound
                    .action_identity()
                    .expect("the rebuilt control registers a click")
            ),
            "the rebuilt control's handler is registered"
        );
    }

    #[test]
    fn two_panes_should_keep_their_own_header_handlers() {
        let mut harness = harness();
        let mut first = pane(&mut harness, FakeSource::default());
        build_header(&mut harness, &mut first);
        let first_button = first
            .widget
            .manage_button
            .as_ref()
            .expect("the first pane built its control")
            .clone();
        let first_action = first_button
            .action_identity()
            .expect("the first control registered a click");

        let services = Rc::clone(harness.services_rc());
        let window = harness.window();
        let root = harness.root();
        let scope = harness.app_mut().signal_scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let mut second = {
            let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
            SolanaWalletWidget::new_with_source(
                FakeSource::default(),
                status,
                WidgetAction::new("common.retry", |_| {}),
                &mut cx,
            )
        };
        {
            let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
            second
                .header(HeaderGroup::Left, root, &mut cx)
                .expect("the second pane builds its control");
        }
        let second_button = second
            .manage_button
            .as_ref()
            .expect("the second pane built its control")
            .clone();
        let second_action = second_button
            .action_identity()
            .expect("the second control registered a click");

        assert_ne!(
            first_action, second_action,
            "each pane's control is keyed by its own instance"
        );
        assert!(harness.app().has_action_handler(&first_action));
        assert!(harness.app().has_action_handler(&second_action));

        {
            let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
            let _ = first.widget.release_manage_button(&mut cx, true);
        }
        assert!(
            !harness.app().has_action_handler(&first_action),
            "the closed pane's handler is unregistered"
        );
        assert!(
            harness.app().has_action_handler(&second_action),
            "releasing one pane must not unregister another pane's handler"
        );
    }

    #[test]
    fn releasing_the_dialog_should_reclaim_its_value_slots() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        pane.widget.selections = Some(SelectionData::new(selections(vec![selection(FIRST)], 3)));
        pane.widget.selection_read = SelectionRead::Known;
        pane.widget.ready = true;
        let baseline = harness.signal_slot_stats();

        harness
            .with_cx(|cx| pane.widget.open_dialog(cx))
            .expect("the dialog opens");
        harness.drain();
        assert!(
            harness.signal_slot_stats().live > baseline.live,
            "the dialog mints its own values"
        );
        let mut dialog = pane.widget.dialog.take().expect("the dialog is open");
        harness.with_cx(|cx| dialog.release(cx));
        drop(dialog);
        let after_first = harness.signal_slot_stats();
        assert_eq!(
            after_first.live, baseline.live,
            "releasing the dialog returns every live value slot"
        );
        for _ in 0..4 {
            harness
                .with_cx(|cx| pane.widget.open_dialog(cx))
                .expect("the dialog opens again");
            harness.drain();
            let mut dialog = pane.widget.dialog.take().expect("the dialog is open");
            harness.with_cx(|cx| dialog.release(cx));
            drop(dialog);
        }
        assert_eq!(
            harness.signal_slot_stats().slots,
            after_first.slots,
            "the slots are reused rather than minted afresh"
        );
    }

    #[test]
    fn releasing_the_pane_should_reclaim_what_it_built() {
        let mut harness = harness();
        let baseline = harness.signal_slot_stats();
        let mut mounted_slots = None;
        sign_in(&mut harness, ACCOUNT);

        for _ in 0..3 {
            let scope = harness.app_mut().signal_scope();
            let services = Rc::clone(harness.services_rc());
            let window = harness.window();
            let parent = harness.placeholder_parent();
            let status = WidgetStatus::new(harness.app_mut(), scope);
            let source = FakeSource::default();
            let session = source.session(ACCOUNT);
            let _control = session.subscribe();
            let mut pane = {
                let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
                let widget = SolanaWalletWidget::new_with_source(
                    source,
                    status,
                    WidgetAction::new("common.retry", |_| {}),
                    &mut cx,
                );
                Pane {
                    widget,
                    status,
                    parent,
                }
            };

            pane.widget.selections =
                Some(SelectionData::new(selections(vec![selection(FIRST)], 3)));
            pane.widget.selection_read = SelectionRead::Known;
            pane.widget.ready = true;
            report_feed(&mut harness, &mut pane, FakeFeed::new().push("one", 1));
            build_body(&mut harness, &mut pane);
            build_header(&mut harness, &mut pane);
            harness
                .with_cx(|cx| pane.widget.open_dialog(cx))
                .expect("the dialog opens");
            harness.drain();
            assert!(
                harness.signal_slot_stats().live > baseline.live,
                "the pane mints its own values"
            );
            let body = pane.widget.body_root.expect("the body was built");
            assert!(pane.widget.list.is_some());
            assert!(pane.widget.dialog.is_some());

            harness.with_cx(|cx| pane.widget.release(cx));
            assert!(
                harness.tree().get(body).is_err(),
                "the pane takes its own body subtree down"
            );
            assert!(pane.widget.list.is_none(), "the list is released");
            assert!(pane.widget.dialog.is_none(), "the dialog is released");
            assert!(
                pane.widget.manage_button.is_none(),
                "the header control is released"
            );
            drop(pane);
            harness.app_mut().release_scope(scope);

            assert_eq!(
                harness.signal_slot_stats().live,
                baseline.live,
                "releasing the pane returns every live value slot"
            );
            let used = harness.signal_slot_stats().slots;
            assert_eq!(
                *mounted_slots.get_or_insert(used),
                used,
                "a mount reuses the slots it was given rather than minting afresh"
            );
        }
    }

    #[test]
    fn a_pane_mounted_without_the_kytrix_service_should_report_a_typed_failure() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let scope = harness.scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let mut widget = harness
            .with_cx(|cx| {
                SolanaWalletWidget::new(
                    WidgetInstanceId::new(),
                    SolanaWalletConfig,
                    status,
                    ReloadHandle::new(|_| {}),
                    WidgetConfigChange::default(),
                    WidgetAction::new("common.retry", |_| {}),
                    cx,
                )
            })
            .expect("a host without a service still builds a pane");
        assert!(widget.binding.is_none());

        harness.with_cx(|cx| widget.load(cx));

        let state = status.state(harness.app());
        let failure = state.failure().expect("the pane reports a typed failure");
        assert_eq!(
            failure.text(&harness.services().translate),
            SolanaWalletFailure::Stream(SolanaWalletStreamError::Service(
                KytrixServiceError::Closed
            ))
            .translated_message(&harness.services().translate)
        );
        assert_eq!(
            failure.action().label(&harness.services().translate),
            "Retry"
        );
    }

    #[test]
    fn a_steady_pane_should_not_ask_for_a_frame() {
        let mut harness = harness();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let mut pane = live_pane(&mut harness, &source, &session, vec![selection(FIRST)], 3);
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        let requests = harness.frame_requests();

        sync(&mut harness, &mut pane);
        harness.drain();
        sync(&mut harness, &mut pane);
        harness.drain();

        assert_eq!(
            harness.frame_requests(),
            requests,
            "a steady pane asks for no frame"
        );
        assert!(!harness.rendered(), "a steady pane paints nothing");
    }

    #[test]
    fn a_pane_waiting_for_its_first_address_should_not_keep_asking_for_frames() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        pane.widget.selections = Some(SelectionData::new(selections(Vec::new(), 3)));
        pane.widget.selection_read = SelectionRead::Known;
        pane.widget.ready = true;
        report_feed(&mut harness, &mut pane, FakeFeed::new().snapshot());
        harness.drain();
        let requests = harness.frame_requests();

        sync(&mut harness, &mut pane);
        harness.drain();
        sync(&mut harness, &mut pane);
        harness.drain();

        assert!(!pane.widget.has_content(), "there is nothing to follow");
        assert!(
            pane.widget.body_root.is_none(),
            "so the framework built no body"
        );
        assert_eq!(
            harness.frame_requests(),
            requests,
            "a pane with nothing to follow asks for no frame"
        );
    }

    #[test]
    fn signing_out_should_take_the_feed_with_it() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();
        report_feed(&mut harness, &mut pane, feed.push("one", 1));
        assert!(pane.widget.has_content());

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);

        assert!(!pane.widget.has_content());
        assert_eq!(
            state_of(&harness, &pane).notice(),
            Some(&Notice::message("widget.solana_wallet.logged_out"))
        );
    }
}

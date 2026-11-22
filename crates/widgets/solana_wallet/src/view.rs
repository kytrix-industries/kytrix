//! The Solana wallet monitor pane: the transfers it follows, and the addresses it manages.
//!
//! # What a pane is scoped to
//!
//! One account. The account is the pane's scope and the stream key at once, so the pane
//! never has to decide whether a transfer belongs to the account on screen: a message
//! published for an account the pane has left is a message the binding does not deliver,
//! and one that arrives by hand is refused by comparing the key.
//!
//! # The pane owns the addresses, the service owns the session
//!
//! Following transfers and managing the addresses they are watched for are two things the
//! same account is asked for, and the same service answers both with the same session. So
//! the pane is what waits for both, and the dialog it opens is a view of the pane's state
//! rather than an owner of its own: a dialog that held the list would lose it on the next
//! open, and one that took a session of its own would hold a second one for an account the
//! pane is already reading as.
//!
//! # What a change the pane stopped waiting for means
//!
//! A request the pane stops waiting for — because the user retried, or because the account
//! changed under it — is dropped rather than decided: whether the service made the change
//! is not something the pane can know, and it does not guess. What settles it is the next
//! read, which is why a dialog that is open reads the addresses again as soon as the stream
//! that answers reads is live. The commands in flight are tracked by the framework's
//! [`RequestId`](widget::protocol::RequestId) (AGENTS §8.2), so a reply to a command the
//! binding replaced is dropped rather than applied to the one that replaced it.
//!
//! # Why the list reads a signal rather than holding rows
//!
//! The body is built once, and a kit list row's content closure has no [`App`] to read
//! current data with. The rows therefore read the feed through a signal: every row's text
//! is a `bind_text` closure over the snapshot signal, the display-zone setting signal and
//! the language signal, and the list itself only holds the row count and where the reader
//! is. A newer snapshot is reported to the list as the transfers it has not been shown
//! rather than as a new set of rows: a list rebuilt from scratch would re-measure every row
//! on screen and, worse, move a reader who had scrolled into the feed back to the top.
//!
//! # What this pane does not have
//!
//! The interface this crate was written for has no balance cards, no table and no controls
//! that copy an address or open a block explorer, so none are migrated here: `open_url`
//! stays unused by this widget. Addresses are drawn by the row's own text node, with the
//! engine's ellipsis overflow — there is no grapheme-aware truncation helper on the frozen
//! contract (`widget::util` carries polling arithmetic only), which the W8a ledger
//! registers as the current state.

use std::sync::Arc;

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
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

/// The height a transfer row is estimated at until the list has measured one.
///
/// A row is four short lines (the amount, the time and two address lines), and the old
/// interface kept 150 px either side of the viewport; at this estimate that is two rows
/// (AGENTS §15.3: overscan is a row count, not a pixel buffer).
const ROW_ESTIMATE: f32 = 68.0;

/// How many rows the list mounts either side of the viewport.
const ROW_OVERSCAN: usize = 2;

/// The width of the "from"/"to" label column of a transfer row.
const ADDRESS_LABEL_WIDTH: f32 = 32.0;

/// The size a transfer's amount and address text are drawn at.
const TRANSFER_TEXT_SIZE: UnifiedSize = UnifiedSize::Medium;

/// The size a transfer's time is drawn at.
const TRANSFER_TIME_SIZE: UnifiedSize = UnifiedSize::Small;

/// A Solana wallet monitor carries no options.
///
/// What a pane follows is the account's own list of monitored addresses, which is read from
/// the service rather than configured here, so there is nothing to persist but the schema
/// version.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SolanaWalletConfig;

impl WidgetConfig for SolanaWalletConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// A failure a pane reports.
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

/// One command the pane sent and has not seen the answer to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    /// A read of the stream: `Load` or `Retry`.
    Start,
    /// The stop that lets go of the stream an account left behind.
    Stop,
    /// A read of the monitored addresses.
    Read,
    /// Monitoring one more address.
    Add,
    /// Leaving one address.
    Delete,
}

/// What the dialog is doing, or waiting for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SelectionOperation {
    /// The addresses are being read for a dialog that has none to show.
    Loading,
    /// The addresses are being read for a dialog that is already showing some.
    Refreshing,
    /// An address is being monitored.
    Adding,
    /// An address is being left.
    Deleting,
}

impl SelectionOperation {
    /// Whether this operation is a read rather than a change.
    pub(super) const fn is_read(self) -> bool {
        matches!(self, Self::Loading | Self::Refreshing)
    }
}

/// The change the pane is waiting for.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectionPending {
    operation: SelectionOperation,
    /// The address a change was asked about, which is what its confirmation is checked
    /// against and what a removal is projected from.
    address: Option<SmolStr>,
    /// Whether the request is out.
    ///
    /// A read asked for before the stream is live is held here instead: there is no session
    /// behind the account yet, and the readiness that arrives later is what makes the read
    /// possible.
    sent: bool,
}

/// What the dialog says about the last change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SelectionNotice {
    /// The address is now monitored.
    Added,
    /// Why the last change did not happen, or did not happen for certain.
    Problem(SelectionFailure),
}

/// Why a change to the monitored addresses did not happen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SelectionFailure {
    /// The service refused the change.
    Rejected(SolanaWalletApiError),
    /// The change may or may not have been made, which is what the service answers when it
    /// could not tell.
    Unknown(SolanaWalletMutationUnknown),
    /// The confirmed change is not one the list the pane holds can describe, which means the
    /// list and the account have gone out of step.
    Inconsistent(SolanaWalletDataError),
    /// The call itself failed.
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

/// How far the pane has got in reading the addresses it follows.
///
/// The addresses are a pane's scope rather than a list it happens to show, so not knowing
/// them is not the same as knowing there are none — and the three states are told apart
/// because they read differently to a user.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum SelectionRead {
    /// Nothing has answered yet: the pane does not know what it follows.
    #[default]
    NotRead,
    /// The list arrived, and it is what the pane follows.
    Known,
    /// The read failed, and this is why. A read that failed is not a list with nothing in it.
    Failed(RpcCallError<SolanaWalletApiError>),
}

/// One pane: the transfers it holds, and the addresses it manages.
pub struct SolanaWalletWidget {
    config: SolanaWalletConfig,
    /// The executor bridge, absent on a host that installed no Kytrix service.
    binding: Option<WidgetBinding<SolanaWalletProtocol>>,
    /// The commands in flight, keyed by the identity the binding returned.
    pending: Pending<PendingCommand>,
    status: WidgetStatus,
    recovery: WidgetAction,
    /// The account the pane reads for, which is its whole scope.
    account_id: Option<SmolStr>,
    /// The transfers the pane is showing, once a subscription has published them.
    snapshot: Option<TransferSnapshot>,
    /// Whether the subscription has reported itself live.
    ready: bool,
    /// The feed the rows read. Rows have no `App` of their own, so the state they draw is a
    /// signal watched by their `bind_*` closures.
    items: Signal<TransferSnapshot>,
    /// The list that draws the feed, built by [`WidgetView::body`] where its parent is known.
    list: Option<List>,
    /// The root of the body this pane built, so teardown can take its own subtree down
    /// before releasing the list component.
    body_root: Option<NodeId>,
    /// The stand-in a feed with nothing in it draws.
    empty: Option<NodeId>,
    /// The copy the stand-in draws, rewritten when the language moves.
    empty_text: Signal<SmolStr>,
    /// The addresses the account monitors, as the service last answered them.
    selections: Option<SelectionData>,
    /// What the dialog is waiting for, if anything.
    selection_pending: Option<SelectionPending>,
    /// What the dialog says about the last change.
    notice: Option<SelectionNotice>,
    /// How far the read of the addresses has got.
    selection_read: SelectionRead,
    /// The open dialog, which is the pane's only way of managing addresses.
    dialog: Option<SolanaWalletDialog>,
    /// The header control that opens the dialog, kept so it can be released.
    manage_button: Option<Button>,
    /// The header group the control lives in, so a language change can rebuild it in place.
    manage_parent: Option<NodeId>,
    /// The language the control was built in: a language switch rebuilds it because a kit
    /// button's label is chosen when it is built.
    manage_language: Option<LanguageCode>,
    /// Click requests from the header control: a button handler owns no `&mut self`, so it
    /// asks the next `sync` to open the dialog instead of opening it.
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
        config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        let source = cx.services().kytrix_service().cloned();
        Ok(Self::assemble(config, source, status, recovery, cx))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let retry = self.status.is_failed(cx.app());
        self.account_id = cx.account();
        if let Err(error) = self.start(retry, cx) {
            log::error!("the solana wallet pane could not start: {error}");
        }
    }

    /// A pane is content once a subscription has published transfers for it.
    ///
    /// An empty feed is content: an account whose addresses have not moved yet is still the
    /// account the pane is about, and the pane says so itself with the empty state its body
    /// draws. An account with no addresses at all is the different thing — a pane with
    /// nothing to follow rather than one that is quiet — so that is answered here as no
    /// content, which leaves the slot to the framework's placeholder and its own sentence.
    fn has_content(&self) -> bool {
        self.account_has_addresses() && self.snapshot.is_some()
    }

    /// The one option this pane owns: the schema version of an otherwise empty config.
    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
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
        // The settings are the pane's scope: signing in as someone else is another feed,
        // and signing out is no feed at all. A settings write reaches this hook on the
        // frame it schedules, and the comparison is what makes it a reload only when the
        // account really moved.
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

        // The body is built by the framework from the answer `has_content` gives, and it
        // asks that question before this hook runs: content that first appeared here is
        // one frame away, so ask for that frame.
        if self.snapshot.is_some() && self.body_root.is_none() {
            cx.request_frame()?;
        }

        Ok(())
    }

    /// The control that opens the address dialog.
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
        // Dropping the bridge aborts its forwarding tasks and cancels the service.
        self.binding = None;
        // The header's nodes belong to the pane, which removes its row: only the
        // component's signal scope and action handler are this widget's to release.
        let _ = self.release_manage_button(cx, false);
        if let Some(mut dialog) = self.dialog.take() {
            dialog.release(cx);
        }
        // The list documents its own teardown order: its nodes first, then the component.
        // The body subtree is the widget's, not the framework's shell, so removing it here
        // is the widget cleaning up after itself.
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
    /// The seam the framework and the tests build a pane through.
    ///
    /// The pane has no opinion about where its wallet comes from, so the source is an
    /// argument: the production constructor answers it with the injected Kytrix service
    /// connection, and a test with a fake session.
    fn assemble<S: SolanaWalletSource>(
        config: SolanaWalletConfig,
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
            config,
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

    /// A pane over a source the caller answers, for the crate's own tests.
    #[cfg(test)]
    fn new_with_source<S: SolanaWalletSource>(
        config: SolanaWalletConfig,
        source: S,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        Self::assemble(config, Some(source), status, recovery, cx)
    }

    /// The scope this pane follows, which is the account it reads as.
    fn current_scope(&self) -> Option<SolanaWalletScope> {
        SolanaWalletScope::new(self.account_id.as_ref())
    }

    /// Starts reading, or restarts for a pane the user is recovering.
    ///
    /// A recovery keeps the transfers the user is still reading — blanking a feed to
    /// reconnect would flash it away, and the addresses are read again once the stream this
    /// pane is recovering is live — while an account change starts over, because the pane
    /// cannot vouch for what it was showing.
    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(scope) = self.current_scope() else {
            self.clear(cx)?;
            self.report_no_content_notice(cx);
            // Nothing is followed, so nothing should stay subscribed on this pane's behalf:
            // an account that signed out must not keep a seat it is not using.
            self.call(cx, SolanaWalletRequest::Stop, PendingCommand::Stop);
            cx.request_frame()?;
            return Ok(());
        };
        self.ready = false;
        if !retry {
            self.clear(cx)?;
        }
        // Starting a stream replaces whatever the binding was awaiting, so a change that
        // was in flight is dropped rather than remembered: whether it was made is not
        // something this pane knows, and what settles it is the read that follows the
        // stream coming back.
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

    /// Sends one command through the binding, tracking the identity it was given.
    ///
    /// The binding holds one command at a time: a new call replaces the previous one and
    /// aborts it, so the command it replaced is forgotten here (its reply is then an
    /// unknown identity and is dropped, AGENTS §8.2).
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

    /// Drops what a pane is leaving behind.
    ///
    /// A change the pane was waiting for goes with it: the request replaces whatever the
    /// binding was awaiting, so its answer is never seen and nothing about it is remembered —
    /// the account's addresses are read again instead.
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

    /// Reports what a pane with nothing to follow waits for.
    ///
    /// This is the only place the pane's two waiting references are produced: the paths that
    /// enter either state call it rather than holding a copy of their own. What each one says
    /// is a reference into the catalogue, resolved by the framework on the frame that draws
    /// it, in the language showing then.
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

    /// Applies one RPC reply, once its identity says it answers the command in flight.
    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<SolanaWalletProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        match self.pending.take(request_id) {
            Some(PendingCommand::Start) => self.apply_load_result(result, cx),
            // The stop lets go of a stream the pane already left; what the service says
            // about it is nothing the pane reports.
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
        // A failure is terminal (AGENTS §4.4): the read's answer may not repaint the
        // failure the user is reading, and it is not the reason the pane failed.
        if self.status.is_failed(cx.app()) {
            return;
        }
        // The answer says the service took the account; what the subscription then reports
        // about itself arrives on the stream, which is where a failure to subscribe is drawn
        // from.
        if let Err(error) = result {
            self.ready = false;
            // A read the dialog is waiting for is answered by the account being taken, so it
            // does not outlive the failure of the request that would have taken it.
            self.selection_pending = None;
            let recovery = self.recovery.clone();
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(Arc::new(SolanaWalletFailure::Rpc(error)), recovery),
            );
        }
    }

    /// Applies the transfers a subscription published.
    ///
    /// Returns whether what a frame draws changed, which for a feed is always: the service
    /// publishes it when a transfer moved it.
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

    /// Whether a message may still publish a status.
    ///
    /// A read that failed is terminal until the user recovers it, and the status cell does not
    /// know that yet while the failure is only recorded here — so a feed that arrives in that
    /// window must not be allowed to report a pane that looks like it is working.
    fn may_report(&self) -> bool {
        !matches!(self.selection_read, SelectionRead::Failed(_))
    }

    /// Reports what the pane is showing.
    ///
    /// The account's addresses are the scope of everything here, so a pane whose account
    /// monitors none has nothing to follow and says so rather than drawing the empty feed of
    /// an account that is merely quiet.
    ///
    /// Until the list has been read the pane is loading, whatever the feed holds: the service
    /// publishes an empty feed before it publishes anything else, so a pane that reported what
    /// it had here would spend the read showing the blank list of an account that may well turn
    /// out to follow nothing at all.
    ///
    /// A read that *failed* is not that window. It is a failure, and it is reported as one with
    /// the framework's own recovery rather than left looking like a pane that is still working.
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

    /// Whether the account is one with addresses to follow.
    ///
    /// Only a list that answered *and held nothing* says no: a list nobody has asked for and one
    /// whose read failed both leave the pane without an answer, and neither of them is a list
    /// that holds nothing. Answering otherwise here would take the body away from a pane that
    /// has simply not been told yet.
    fn account_has_addresses(&self) -> bool {
        !self
            .selections
            .as_ref()
            .is_some_and(|data| data.selections().is_empty())
    }

    /// Applies a status the stream reported.
    ///
    /// Returns whether what a frame draws changed, which a status never does on its own: the
    /// framework draws the state around the body, and a row of transfers says nothing about
    /// the subscription it arrived on.
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
                // The transfers already read stay: a reconnecting feed is not a reason to
                // blank what a reader was looking at.
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            SolanaWalletStatus::Ready => {
                self.ready = true;
                // What a pane shows is only as true as the read it came from, and a change
                // the pane stopped waiting for was never read back. The stream coming up is
                // what makes reading the addresses possible again, so it is what reads them
                // — and the pane reads them once whether or not the dialog is open, because
                // an account with no addresses is a pane with nothing to follow rather than
                // a feed that has been quiet.
                if self.selection_pending.is_some() {
                    self.send_pending_selections(cx);
                } else if self.selections.is_none() || self.dialog.is_some() {
                    self.load_selections(cx);
                }
                self.report(cx);
            }
            SolanaWalletStatus::Failed(error) => {
                self.ready = false;
                // A read that never went out has nothing left to wait for: the stream it was
                // waiting for failed, and the dialog says so rather than spinning.
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

    /// Reports why a subscription stopped.
    fn fail(&mut self, error: SolanaWalletStreamError, cx: &mut WidgetCx<'_>) {
        let failure = SolanaWalletFailure::Stream(error);
        let recovery = self.recovery.clone();
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(Arc::new(failure), recovery),
        );
    }

    /// Tells the list about the transfers this feed changed.
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
            // The oldest of the transfers it has not been shown goes in first, so the newest
            // of them ends up where a list that draws its newest transfer first draws it.
            (Some(pushed), Some(list)) => {
                for _ in 0..pushed {
                    list.push(cx.app_mut())?;
                }
            }
            // Nothing the list holds survives, so there is no edit it can be told: every
            // transfer on screen is one the feed no longer has. The list is rebuilt for the
            // feed it has now.
            (None, Some(list)) => list.set_items(cx.app_mut(), count)?,
            _ => {}
        }
        self.snapshot = Some(snapshot);
        cx.request_frame()?;

        Ok(())
    }

    /// Builds the list the body draws its feed with.
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

    /// Shows `common.no_data` over a feed that holds nothing.
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

    /// Builds the header control that opens the dialog into `parent`.
    /// Builds the wrapper the header control is drawn in, and the control itself.
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

    /// Builds the header control into the wrapper it lives in.
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

    /// Keeps the header control in step with the account and the language.
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
        // The pane may have rebuilt the header row since: its group is not this widget's
        // node, so a stale wrapper means the control already went with it.
        if cx.tree().get(wrapper).is_err() {
            self.release_manage_button(cx, false)?;
            return Ok(());
        }

        // The wrapper stays: only the control built into it is replaced.
        if let Some(button) = self.manage_button.take() {
            if let Some(node) = button.node() {
                remove_own_node(cx, node)?;
            }
            let _ = button.release(cx.app_mut());
        }
        self.manage_language = None;
        self.build_manage_control(cx, wrapper)
    }

    /// Removes the control's node (when the widget owns the teardown) and releases its
    /// component scope and action handler.
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

    /// Opens the address dialog, when there is an account to manage addresses for.
    fn open_dialog(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.dialog.is_some() || self.current_scope().is_none() {
            return Ok(());
        }
        let mut dialog = SolanaWalletDialog::open(cx)?;
        // The read the dialog exists for: the pane reads the addresses on the dialog's
        // behalf as soon as it opens, and the stream's readiness is what sends it.
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

    /// One frame of the open dialog and the requests the user made through it.
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

    /// Applies one request the user made through the dialog.
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

    /// The pane's own selection state, as the dialog draws it.
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

    /// Reads the addresses the account monitors.
    ///
    /// A read goes over a live stream, because a live stream is what says the service is
    /// holding the account: before it there is no session to answer one. A read asked for too
    /// early waits here instead, and the readiness that arrives later is what sends it.
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
        // Asking again is what takes a failure back off the screen: the answer to this read is
        // what the pane will have to say, and until it arrives the pane does not know.
        self.selection_read = SelectionRead::NotRead;
        if self.ready {
            self.send_selections(scope, cx);
        }
        true
    }

    /// Sends the read a dialog is waiting for.
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

    /// Monitors one more address.
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

    /// Stops monitoring one address.
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

    /// Whether the pane is in a position to change what the account monitors.
    ///
    /// A change is sent over the same session the stream was opened with, so a pane whose
    /// stream is not live has no session to change anything through.
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
            // The protocol types the answer, so anything else is a service that answered the
            // wrong question rather than a shape a reader could describe: nothing changes.
            Ok(_) => {}
            Err(error) => {
                // The read failed, which is not the same as a list that holds nothing: what the
                // pane follows is unknown, and that is a failure rather than an empty account.
                // The dialog says so in its own words, and the pane says so as its status —
                // this is the only place either of them learns it, so it is recorded once and
                // reported from here.
                self.selection_read = SelectionRead::Failed(error.clone());
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
            }
        }
        // A pane that is following an account says what the answer means; a pane that is not
        // yet ready has no status to publish, and the readiness that arrives later reports for
        // it.
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
            // What the list becomes is the confirmation applied to it, and only a
            // confirmation of the address that was asked about is one of those.
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
            // The change may or may not have been made, so the list the pane holds is not one
            // it can change: it is left as it was, and the next read settles it.
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

/// What the dialog says about a confirmation the list cannot hold.
fn inconsistent() -> SelectionNotice {
    SelectionNotice::Problem(SelectionFailure::Inconsistent(
        SolanaWalletDataError::InconsistentSelections,
    ))
}

/// Removes a node this pane built, if it is still in the tree.
fn remove_own_node(cx: &mut WidgetCx<'_>, node: NodeId) -> Result<(), EngineError> {
    if cx.tree().get(node).is_err() {
        return Ok(());
    }
    let window = cx.window();
    cx.app_mut().remove_node(window, node).map(|_| ())
}

/// The signals and collaborators one transfer row draws through.
///
/// A row closure has no `App`, so everything a row reads or draws with is handed in once
/// when the list is built: the feed for the transfer, the settings for the display zone, the
/// language for the labels, and the theme / translator for the colours and the copy.
#[derive(Clone)]
struct TransferRowContext {
    items: Signal<TransferSnapshot>,
    language: Signal<LanguageCode>,
    config: Signal<app_config::settings::Config>,
    theme: ThemeHandle,
    translate: Translate,
}

/// Builds one row of the feed into the list's row container.
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
        .a11y(A11yOverride {
            role: Some(A11yRole::Row),
            ..A11yOverride::default()
        })
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

/// Which end of a transfer an address line names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddressEnd {
    From,
    To,
}

/// One end of a transfer: which Solana wallet, under the label that says which end it is.
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
    use base_ui::kit::button::Button;
    use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
    use kytrix_service_provider::service::solana_wallet::{
        SolanaWalletMutationOutcome, SolanaWalletSelection,
    };
    use network::transport::error::TransportError;
    use ui_core::event::{Action, ActionSource};
    use widget::{protocol::RequestId, test_support::WidgetTestApp};

    use super::*;
    use crate::test_support::{
        ACCOUNT, FakeFeed, FakeSession, FakeSource, selection, selection_at, selections,
    };

    const FIRST: &str = "So11111111111111111111111111111111111111112";
    const SECOND: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    const OTHER_ACCOUNT: &str = "someone-else";

    /// How many runtime / frame turns a settle loop may take before it calls the test a
    /// failure.
    const SETTLE_TURNS: usize = 64;

    /// The translator the product ships, in `language`.
    fn translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    /// The framework's headless host, over the product catalogue.
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

    /// Signs `user_id` in the way the composition root does: a settings write.
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

    /// Signs the pane out the way the composition root does: a settings write.
    fn sign_out(harness: &mut WidgetTestApp) {
        let mut config = current_config(harness);
        config.account = None;
        assert!(
            write_config(harness, config),
            "signing out is a settings change"
        );
    }

    /// A pane the framework would have built, over a source the test answers.
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
                SolanaWalletConfig,
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

    /// A pane over an account with the service it was given, as `load` would start it.
    fn pane(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
        sign_in(harness, ACCOUNT);
        pane_with(harness, source)
    }

    /// A pane nobody is signed in for.
    fn signed_out_pane(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
        pane_with(harness, source)
    }

    /// A pane no message of which arrives from the binding: the tests below report
    /// statuses and feeds by hand, and a live service would report its own.
    fn quiet_pane(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
        let mut pane = pane(harness, source);
        if let Some(binding) = pane.widget.binding.as_mut() {
            binding.cancel();
        }
        pane
    }

    /// A pane reading an account whose stream is live, with the addresses it read.
    ///
    /// The read is the stream's, not this fixture's: a pane that has answered readiness
    /// reads the addresses itself.
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

    /// Builds the body the framework would build once the pane has content.
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

    /// Lets the host's runtime deliver what the fake source published, and the pane the
    /// frames it asked for, until `ready` holds.
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

    /// Reports a feed as the stream would, with the binding taken out of the way.
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

    /// Reports a status as the stream would.
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

    /// Reports a status as the stream would, over the binding the pane is really reading:
    /// for a live pane whose stream is what answers the read the status asks for.
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

    /// Dispatches the click a kit control's action identity names.
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

    /// Builds the header control the pane would build into the window's header row.
    fn build_header(harness: &mut WidgetTestApp, pane: &mut Pane) {
        let root = harness.root();
        harness
            .with_cx(|cx| pane.widget.header(HeaderGroup::Left, root, cx))
            .expect("the header control builds");
    }

    /// A missing key is an empty string on screen rather than an error, so a gap in the
    /// interface is what a typo looks like: this is the test that turns it into a failure.
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

    /// The words a waiting pane shows are the ones the language resolves them in: the
    /// notice is a reference into the catalogue, so the pane reports nothing again.
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

        // The sentence is a catalogue reference, so the language it resolves in moves
        // without the pane reporting anything again.
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

    /// A feed that lands while the pane is still reading its addresses is content the
    /// moment it arrives: the pane holds the transfers even though what it is *reporting*
    /// is still the read of the addresses, and readiness follows once that answers.
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

    /// An account with no addresses is a pane with nothing to follow, and a feed with no
    /// transfers in it reads exactly the same as one whose addresses were never added.
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

    /// While the pane is reading the addresses it is loading, even though the live stream
    /// has already handed it a feed.
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

    /// A read that failed is a failure, and the pane reports it as one rather than sitting
    /// on it. It is *not* a list with nothing in it.
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

    /// The pane reads the addresses whether or not the dialog is open: they are its own
    /// scope, not the dialog's list.
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

    /// Another account is another feed, and the pane hears about it from the settings
    /// rather than from a reload: it opens that account's session.
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

    /// A change the service could not tell the outcome of is not one the pane can apply: it
    /// says so and leaves the list alone rather than guessing either way.
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

    /// A change the pane stopped waiting for — because the user recovered the stream — is
    /// dropped rather than decided, and the read that follows settles it.
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

    /// The answer to a command the binding replaced is a late one: its identity is not in
    /// the table, so it is dropped and counted rather than applied to the command that
    /// replaced it (AGENTS §8.2).
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

    /// A transfer's time is read in the zone the settings display data in, on the frame
    /// that draws it: moving the setting re-reads the row.
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

    /// An account that does follow addresses, none of which has moved, is the other
    /// emptiness: the pane is ready and its body says it holds nothing.
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

    /// The rows follow the language: their labels are `bind_text` closures over the
    /// language signal, so switching it re-reads them and reports nothing again.
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

    /// The header control opens the dialog, and the dialog's own close control closes it.
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

    /// Settles the dialog through its close: the kit fade needs frames and clock movement.
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

    /// The header control is static copy: a language switch rebuilds it in the language
    /// showing now (a kit button's label is chosen when it is built).
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

    /// Two panes of one application build the same control with different keys: releasing
    /// one pane's control leaves the other's handler registered (AGENTS §9).
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

        // The second pane is a second widget instance: a fresh signal scope, which is what
        // `cx.unique_key` prefixes a control's identity with.
        let services = Rc::clone(harness.services_rc());
        let window = harness.window();
        let root = harness.root();
        let scope = harness.app_mut().signal_scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let mut second = {
            let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
            SolanaWalletWidget::new_with_source(
                SolanaWalletConfig,
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

    /// The dialog owns its own value slots: opening and releasing it repeatedly returns
    /// them all, and no slot is minted afresh on each pass.
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

    /// A host that installed no Kytrix service is a pane that cannot follow its account:
    /// it reports a typed failure with its own recovery instead of panicking.
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

    /// A pane with nothing changing does not ask for a frame (I3).
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

    /// The pane is content for an account with addresses as soon as the feed arrives; the
    /// addresses are the scope that decides whether there is anything to follow.
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

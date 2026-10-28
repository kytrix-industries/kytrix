//! The Solana wallet monitor pane: the transfers it follows, and the addresses it manages.
//!
//! # What a pane is scoped to
//!
//! One account. The account is the pane's scope and the stream key at once, so the pane
//! never has to decide whether a transfer belongs to the account on screen: a message
//! published for an account the pane has left is a message the framework does not deliver,
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
//! that answers reads is live.
//!
//! # Why the list holds the transfers itself
//!
//! The body is a bounded list of rows of their own height which holds the newest transfers
//! and remembers which row a reader is looking at. A newer snapshot is therefore reported to
//! it as the transfers it has not been shown rather than as a new set of rows: a list rebuilt
//! from scratch would re-measure every row on screen and, worse, move a reader who had
//! scrolled into the feed back to the top. The one case that does move them is a transfer
//! that arrives while the newest row is the one they are reading — the newest transfer
//! belongs on screen, and that is exactly where they are looking.

use std::sync::Arc;

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use app_config_provider::{events::AccountChanged, manager::ConfigManager};
use base_ui::{
    button::Button,
    deque_unsized_list::DequeUnsizedList,
    empty_state::EmptyState,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    separator::Separator,
    size::UnifiedSizable,
    style::theme::{Theme, ThemeExt},
    tooltip::TooltipExt,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Entity, FontWeight, InteractiveElement,
    IntoElement, ListAlignment, ParentElement, Render, Role, SharedString,
    StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window, base::Disableable, div,
    px,
};
use i18n::t;
use kytrix_service_provider::service::{
    connector::KytrixServiceConnector,
    solana_wallet::{
        SolanaWalletApiError, SolanaWalletMutationOutcome, SolanaWalletMutationUnknown,
        solana_wallet_address,
    },
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use widget::{
    account::account_of,
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::LocalizedError,
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    time_zone::{data_time_zone, on_data_time_zone_change},
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use super::{
    dialog::{DialogState, SolanaWalletDialog},
    model::{
        MAX_TRANSFERS, SelectionData, SolanaWalletDataError, TransferItem, TransferSnapshot,
        unshown,
    },
    service::{
        SolanaWalletProtocol, SolanaWalletRequest, SolanaWalletResponse, SolanaWalletScope,
        SolanaWalletService, SolanaWalletSource, SolanaWalletStatus, SolanaWalletStreamError,
        SolanaWalletStreamMessage,
    },
};

/// How far above and below the viewport the list measures rows it cannot see, which is what
/// keeps a row from appearing a moment after it was scrolled to. A transfer is four short
/// lines, so a row is cheap to measure and a couple of them either side of the viewport is
/// enough.
const LIST_OVERDRAW: f32 = 150.0;

/// The width of the "from"/"to" label column of a transfer row.
const ADDRESS_LABEL_WIDTH: f32 = 32.0;

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

impl LocalizedError for SolanaWalletFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Stream(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
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

/// What the dialog is waiting for.
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

impl LocalizedError for SelectionFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Rejected(error) => error.localized_message(cx),
            Self::Unknown(error) => error.localized_message(cx),
            Self::Inconsistent(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
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
    binding: WidgetBinding<SolanaWalletProtocol>,
    status: Entity<WidgetStatus>,
    recovery: WidgetAction,
    /// The account the pane reads for, which is its whole scope.
    account_id: Option<SmolStr>,
    /// The transfers the pane is showing, once a subscription has published them.
    snapshot: Option<TransferSnapshot>,
    /// Whether the subscription has reported itself live.
    ///
    /// It is what keeps a read of the addresses from being sent before there is a session to
    /// answer it, and a change from being sent while the stream carrying the account is down.
    ready: bool,
    /// The list itself, which holds the newest transfers, measures their rows and remembers
    /// where a reader is.
    list: Entity<DequeUnsizedList<Arc<TransferItem>>>,
    /// The addresses the account monitors, as the service last answered them.
    selections: Option<SelectionData>,
    /// What the dialog is waiting for, if anything.
    pending: Option<SelectionPending>,
    /// What the dialog says about the last change.
    notice: Option<SelectionNotice>,
    /// How far the read of the addresses has got.
    ///
    /// A pane cannot say what it is following until the list has been read, so this is what
    /// keeps it from claiming to be ready before then — and what makes a read that failed a
    /// failure the user is told about rather than one the pane sits on.
    selection_read: SelectionRead,
    /// The open dialog, which is the pane's only way of managing addresses.
    dialog: Option<WeakEntity<SolanaWalletDialog>>,
    /// Keeps the pane following the account the settings hold.
    _account: Subscription,
    /// Keeps the pane, and the list that draws its rows, following the display time zone.
    _time_zone: Option<Subscription>,
}

impl WidgetView for SolanaWalletWidget {
    type Config = SolanaWalletConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.solana_wallet");
    const DISPLAY_NAME: &'static str = "widget.names.solana_wallet";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: &'static str = "Live transfers of the Solana addresses an account monitors";
    const ICON: Option<&'static str> = Some("widgets/solana_wallet.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = cx.global::<KytrixServiceConnector>().clone();
        Self::new_with_source(config, source, status, config_change, recovery, cx)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let retry = self.status.is_failed(cx);
        self.account_id = account_of(&cx.global::<ConfigManager>().read());
        self.start(retry, cx);
    }

    /// A pane is content once a subscription has published transfers for it.
    ///
    /// An empty feed is content: an account whose addresses have not moved yet is still the
    /// account the pane is about, and the pane says so itself with the empty state its body
    /// draws. An account with no addresses at all is the different thing — a pane with nothing
    /// to follow rather than one that is quiet — so that is answered here as no content, which
    /// is what leaves the slot to the framework's placeholder and its own sentence.
    fn has_content(&self) -> bool {
        self.account_has_addresses() && self.snapshot.is_some()
    }

    /// The control that opens the address dialog.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty().left(self.manage_button(cx))
    }
}

impl SolanaWalletWidget {
    /// Says what a pane with nothing to follow waits for.
    ///
    /// This is the only place the pane's two waiting references are produced: the paths that
    /// enter either state call it rather than holding a copy of their own. What each one says
    /// is a reference into the catalogue, resolved by the framework on the frame that draws
    /// it, in the language showing then.
    fn report_no_content_notice(&mut self, cx: &mut Context<Self>) {
        let notice = if self.current_scope().is_none() {
            Notice::message("widget.solana_wallet.logged_out")
        } else if !self.account_has_addresses() {
            Notice::message("widget.solana_wallet.empty_addresses")
        } else {
            return;
        };
        self.status.report(WidgetState::waiting(notice), cx);
    }

    /// The seam a test can answer through: `new` only picks the production source.
    fn new_with_source<S>(
        _config: SolanaWalletConfig,
        source: S,
        status: Entity<WidgetStatus>,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: SolanaWalletSource,
    {
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            SolanaWalletService::new(source),
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        // The account is the pane's scope, and the user can change it while the pane is
        // open: signing out has to take the transfers with it, and signing in as someone
        // else has to read again.
        let events = cx.global::<ConfigManager>().events().clone();
        let account = cx.subscribe(&events, |this, _, _: &AccountChanged, cx| {
            this.reconcile_account(account_of(&cx.global::<ConfigManager>().read()), cx);
        });

        // The list holds the feed itself, so what a reader is reading is the list's to keep:
        // it is told about the transfers a snapshot added rather than about the snapshot.
        let list = cx.new(|_| feed_list());

        Self {
            binding,
            status,
            recovery,
            // The pane records the account it was built for, but it starts nothing: the
            // framework calls `load` once construction is done.
            account_id: account_of(&cx.global::<ConfigManager>().read()),
            snapshot: None,
            ready: false,
            list,
            selections: None,
            pending: None,
            notice: None,
            selection_read: SelectionRead::default(),
            dialog: None,
            _account: account,
            // The rows are built by the list entity, which keeps what it drew until it
            // is told otherwise: moving the display zone has to wake the list itself.
            _time_zone: on_data_time_zone_change(cx, |this, _, cx| {
                this.list.update(cx, |_, cx| cx.notify());
                cx.notify();
            }),
        }
    }

    /// The scope this pane follows, which is the account it reads as.
    fn current_scope(&self) -> Option<SolanaWalletScope> {
        SolanaWalletScope::new(self.account_id.as_ref())
    }

    /// Follows the account the settings now hold.
    ///
    /// Another account is another feed and another list of addresses, so what the pane is
    /// showing is dropped rather than kept under a new name — and a change the pane was
    /// waiting for is dropped with it, because whether that change was made is not something
    /// the new account's list can answer.
    fn reconcile_account(&mut self, account: Option<SmolStr>, cx: &mut Context<Self>) {
        if self.account_id == account {
            return;
        }
        self.account_id = account;
        self.start(false, cx);
    }

    /// Starts reading, or restarts for a pane the user is recovering.
    ///
    /// A recovery keeps the transfers the user is still reading — blanking a feed to
    /// reconnect would flash it away, and the addresses are read again once the stream this
    /// pane is recovering is live — while an account change starts over, because the pane
    /// cannot vouch for what it was showing.
    fn start(&mut self, retry: bool, cx: &mut Context<Self>) {
        let Some(scope) = self.current_scope() else {
            self.clear(cx);
            self.report_no_content_notice(cx);
            // Nothing is followed, so nothing should stay subscribed on this pane's behalf:
            // an account that signed out must not keep a seat it is not using.
            self.binding
                .call(SolanaWalletRequest::Stop, cx, |_, _, _| {});
            cx.notify();
            return;
        };
        self.ready = false;
        if !retry {
            self.clear(cx);
        }
        // Starting a stream replaces whatever the binding was awaiting, so a change that
        // was in flight is dropped rather than remembered: whether it was made is not
        // something this pane knows, and what settles it is the read that follows the
        // stream coming back.
        self.pending = None;
        self.status.report(WidgetState::Connecting, cx);
        self.binding.call(
            if retry {
                SolanaWalletRequest::Retry(scope)
            } else {
                SolanaWalletRequest::Load(scope)
            },
            cx,
            Self::apply_load_result,
        );
        cx.notify();
    }

    /// Drops what a pane is leaving behind.
    ///
    /// A change the pane was waiting for goes with it: the request replaces whatever the
    /// binding was awaiting, so its answer is never seen and nothing about it is remembered —
    /// the account's addresses are read again instead.
    ///
    /// A list holds the transfers it was given and can only take more of them, so the feed it
    /// held is left behind by building the list again rather than by telling it about a feed it
    /// no longer has.
    fn clear(&mut self, cx: &mut Context<Self>) {
        self.snapshot = None;
        self.ready = false;
        self.list = cx.new(|_| feed_list());
        cx.notify();
        self.selections = None;
        self.pending = None;
        self.notice = None;
        self.notify_dialog(cx);
    }

    fn apply_load_result(
        &mut self,
        result: WidgetRpcResult<SolanaWalletProtocol>,
        cx: &mut Context<Self>,
    ) {
        // The answer says the service took the account; what the subscription then reports
        // about itself arrives on the stream, which is where a failure to subscribe is drawn
        // from.
        if let Err(error) = result {
            self.ready = false;
            // A read the dialog is waiting for is answered by the account being taken, so it
            // does not outlive the failure of the request that would have taken it.
            self.pending = None;
            self.notify_dialog(cx);
            let recovery = self.recovery.clone();
            self.status.report(
                WidgetState::failed(Arc::new(SolanaWalletFailure::Rpc(error)), recovery),
                cx,
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
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key)
            || !self.may_report()
            || self.status.is_failed(cx)
        {
            return false;
        }
        let SolanaWalletStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return false;
        };

        self.accept(snapshot.clone(), cx);
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
    /// out to follow nothing at all. Nothing is lost by that — the transfers are already in the
    /// list, and the body is drawn again the moment the read answers.
    ///
    /// A read that *failed* is not that window. It is a failure, and it is reported as one with
    /// the framework's own recovery rather than left looking like a pane that is still working.
    fn report(&mut self, cx: &mut Context<Self>) {
        match self.selection_read.clone() {
            SelectionRead::NotRead => {
                self.status.report(WidgetState::Loading, cx);
            }
            SelectionRead::Failed(error) => {
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(Arc::new(SolanaWalletFailure::Rpc(error)), recovery),
                    cx,
                );
            }
            SelectionRead::Known => {
                if !self.account_has_addresses() {
                    self.report_no_content_notice(cx);
                } else if self.has_content() {
                    self.status.report(WidgetState::Ready, cx);
                } else {
                    self.status.report(WidgetState::Loading, cx);
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
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key)
            || !self.may_report()
            || self.status.is_failed(cx)
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
                self.status.report(WidgetState::Connecting, cx);
                self.notify_dialog(cx);
            }
            SolanaWalletStatus::Ready => {
                self.ready = true;
                // What a pane shows is only as true as the read it came from, and a change
                // the pane stopped waiting for was never read back. The stream coming up is
                // what makes reading the addresses possible again, so it is what reads them
                // — and the pane reads them once whether or not the dialog is open, because
                // an account with no addresses is a pane with nothing to follow rather than
                // a feed that has been quiet.
                if self.pending.is_some() {
                    self.send_pending_selections(cx);
                } else if self.selections.is_none() || self.dialog_open() {
                    self.load_selections(cx);
                }
                self.report(cx);
            }
            SolanaWalletStatus::Failed(error) => {
                self.ready = false;
                // A read that never went out has nothing left to wait for: the stream it was
                // waiting for failed, and the dialog says so rather than spinning.
                if self.pending.as_ref().is_some_and(|pending| !pending.sent) {
                    self.pending = None;
                }
                self.notify_dialog(cx);
                self.fail(error, cx);
            }
        }

        false
    }

    /// Reports why a subscription stopped.
    fn fail(&mut self, error: &SolanaWalletStreamError, cx: &mut Context<Self>) {
        let failure = SolanaWalletFailure::Stream(error.clone());
        let recovery = self.recovery.clone();
        self.status
            .report(WidgetState::failed(Arc::new(failure), recovery), cx);
    }

    /// Tells the list about the transfers this feed changed.
    ///
    /// The list holds the newest transfers and remembers the one a reader is looking at, so
    /// it is told the transfers it has not been shown rather than the feed itself: a list that
    /// keeps the transfers it holds where they are is what keeps a reader on the transfer they
    /// are reading. The one transfer that does move them is one that arrives while the newest
    /// transfer is the one they are reading, which is where the newest transfer belongs — and
    /// a feed the list cannot be extended into is one it holds no transfer of, which is built
    /// for instead.
    fn accept(&mut self, snapshot: TransferSnapshot, cx: &mut Context<Self>) {
        let unshown = unshown(
            self.snapshot.as_ref().map_or(&[], TransferSnapshot::items),
            snapshot.items(),
        );

        match unshown {
            // The oldest of the transfers it has not been shown goes in first, so the newest
            // of them ends up where a list that draws its newest transfer first draws it.
            Some(count) => {
                self.list.update(cx, |list, cx| {
                    for item in snapshot.items()[..count].iter().rev() {
                        list.push(item.clone());
                    }
                    cx.notify();
                });
            }
            // Nothing the list holds survives, so there is no edit it can be told: every
            // transfer on screen is one the feed no longer has. A list holds the transfers it
            // was given and can only take more of them, so the one this pane draws is built
            // again for the feed it has now, and the reader starts over on the transfer that
            // feed draws first.
            None => {
                let transfers: Vec<Arc<TransferItem>> = snapshot.items().to_vec();
                self.list = cx.new(move |_| {
                    let mut list = feed_list();
                    for item in transfers.iter().rev() {
                        list.push(item.clone());
                    }
                    list
                });
                // The rows are drawn by the list built here rather than the one it replaced, so
                // the pane is what is told about it.
                cx.notify();
            }
        }

        self.snapshot = Some(snapshot);
    }

    /// The list, which draws the transfers the pane holds.
    fn list_body(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .id(ElementId::Name(SharedString::new_static(
                "SolanaWalletFrame",
            )))
            .debug_selector(|| "TEST_SOLANA_WALLET_TRANSFER_LIST".into())
            .aria_label(t!(cx, "widget.solana_wallet.scroll_transfers"))
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(self.list.clone())
            .into_any_element()
    }

    /// The control that opens the address dialog.
    ///
    /// It is disabled without an account: the addresses belong to one, so a reader the
    /// service does not know has nothing to manage — and a live control that can only fail
    /// reads as if the pane were broken.
    fn manage_button(&self, cx: &Context<Self>) -> AnyElement {
        let widget = cx.entity().downgrade();

        div()
            .debug_selector(|| "TEST_SOLANA_WALLET_MONITOR_MANAGE".into())
            .flex_none()
            // A control in the header row has to take its own clicks: the row is the pane's
            // drag area as well.
            .occlude()
            .child(
                Button::new(ElementId::Name(SharedString::new_static(
                    "SolanaWalletManage",
                )))
                .filled()
                .small()
                .disabled(self.account_id.is_none())
                .aria_label(t!(cx, "widget.solana_wallet.manage"))
                .with_tooltip(t!(cx, "widget.solana_wallet.manage"))
                .on_click(move |_, window, cx| {
                    SolanaWalletDialog::show(widget.clone(), window, cx);
                })
                .child(Icon::new(IconKind::Faders).size_3()),
            )
            .into_any_element()
    }

    /// Whether the address dialog is open.
    pub(super) fn dialog_open(&self) -> bool {
        self.dialog
            .as_ref()
            .is_some_and(|dialog| dialog.upgrade().is_some())
    }

    /// Takes over the dialog that was just opened over this pane.
    ///
    /// Opening the dialog is asking for the addresses: the service answers once the stream
    /// carrying the account is live, which is where a read waits. What the pane keeps is a
    /// weak handle, so the layer is still the only thing holding the dialog — and the dialog's
    /// own state goes with it when it closes.
    pub(super) fn attach_dialog(
        &mut self,
        dialog: WeakEntity<SolanaWalletDialog>,
        cx: &mut Context<Self>,
    ) {
        self.dialog = Some(dialog);
        self.load_selections(cx);
    }

    /// The pane's own selection state, read for the frame that draws it.
    ///
    /// The dialog draws what the pane holds rather than reading the pane for each of its
    /// fields, so this is the one place the two are joined.
    pub(super) fn dialog_state(&self) -> DialogState {
        DialogState::new(
            self.account_id.is_some(),
            self.ready,
            self.selections.clone(),
            self.pending.as_ref().map(|pending| pending.operation),
            self.notice.clone(),
            self.recovery.clone(),
        )
    }

    /// Repaints the dialog, which draws the state this pane holds.
    ///
    /// The dialog is told that it changed rather than asked to change: a pane is what answers
    /// the dialog's own requests, so this runs inside a frame that dialog is already being
    /// updated for — and an entity cannot be updated while it is being updated. Notifying it
    /// by id is what says the same thing without taking it.
    fn notify_dialog(&self, cx: &mut Context<Self>) {
        if let Some(dialog) = self.dialog.as_ref().and_then(WeakEntity::upgrade) {
            App::notify(cx, dialog.entity_id());
        }
    }

    /// Reads the addresses the account monitors.
    ///
    /// A read goes over a live stream, because a live stream is what says the service is
    /// holding the account: before it there is no session to answer one. A read asked for too
    /// early waits here instead, and the readiness that arrives later is what sends it.
    pub(super) fn load_selections(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        if self.pending.is_some() {
            return false;
        }
        self.pending = Some(SelectionPending {
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
        self.notify_dialog(cx);
        true
    }

    /// Sends the read a dialog is waiting for.
    fn send_pending_selections(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = self.current_scope() else {
            return;
        };
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| !pending.sent && pending.operation.is_read())
        {
            self.send_selections(scope, cx);
            self.notify_dialog(cx);
        }
    }

    fn send_selections(&mut self, scope: SolanaWalletScope, cx: &mut Context<Self>) {
        if let Some(pending) = self.pending.as_mut() {
            pending.sent = true;
        }
        self.binding.call(
            SolanaWalletRequest::FetchSelections(scope),
            cx,
            Self::apply_selections_result,
        );
    }

    /// Monitors one more address.
    pub(super) fn add_selection(&mut self, address: SmolStr, cx: &mut Context<Self>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        if !self.may_change(cx) || solana_wallet_address(&address).is_none() {
            return false;
        }
        self.begin_change(SelectionOperation::Adding, Some(address.clone()), cx);
        self.binding.call(
            SolanaWalletRequest::AddSelection { scope, address },
            cx,
            Self::apply_add_result,
        );
        true
    }

    /// Stops monitoring one address.
    pub(super) fn delete_selection(&mut self, address: SmolStr, cx: &mut Context<Self>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        if !self.may_change(cx) {
            return false;
        }
        self.begin_change(SelectionOperation::Deleting, Some(address.clone()), cx);
        self.binding.call(
            SolanaWalletRequest::DeleteSelection { scope, address },
            cx,
            Self::apply_delete_result,
        );
        true
    }

    /// Whether the pane is in a position to change what the account monitors.
    ///
    /// A change is sent over the same session the stream was opened with, so a pane whose
    /// stream is not live has no session to change anything through.
    fn may_change(&self, cx: &Context<Self>) -> bool {
        self.ready
            && self.selections.is_some()
            && self.pending.is_none()
            && !self.status.is_failed(cx)
    }

    fn begin_change(
        &mut self,
        operation: SelectionOperation,
        address: Option<SmolStr>,
        cx: &mut Context<Self>,
    ) {
        self.pending = Some(SelectionPending {
            operation,
            address,
            sent: true,
        });
        self.notice = None;
        self.notify_dialog(cx);
    }

    fn apply_selections_result(
        &mut self,
        result: WidgetRpcResult<SolanaWalletProtocol>,
        cx: &mut Context<Self>,
    ) {
        self.pending = None;
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
        self.notify_dialog(cx);
    }

    fn apply_add_result(
        &mut self,
        result: WidgetRpcResult<SolanaWalletProtocol>,
        cx: &mut Context<Self>,
    ) {
        let requested = self.pending.take().and_then(|pending| pending.address);
        let outcome = match result {
            Ok(SolanaWalletResponse::Added(outcome)) => outcome,
            Ok(_) => return,
            Err(error) => {
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
                self.notify_dialog(cx);
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
        self.notify_dialog(cx);
    }

    fn apply_delete_result(
        &mut self,
        result: WidgetRpcResult<SolanaWalletProtocol>,
        cx: &mut Context<Self>,
    ) {
        let requested = self.pending.take().and_then(|pending| pending.address);
        let outcome = match result {
            Ok(SolanaWalletResponse::Deleted(outcome)) => outcome,
            Ok(_) => return,
            Err(error) => {
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
                self.notify_dialog(cx);
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
        self.notify_dialog(cx);
    }
}

impl Render for SolanaWalletWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A snapshot that arrived and holds no transfer is the fact "nothing has moved", and it
        // reads exactly like a list that failed to load unless the pane says which it is. The
        // account having no addresses at all is the different thing, and it is not drawn here:
        // that is a pane with nothing to follow, which the framework draws from the waiting
        // status the pane reports. So this is reached only for an account that does follow
        // something.
        let body = match self.snapshot.as_ref() {
            Some(snapshot) if snapshot.items().is_empty() => div()
                .size_full()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyState::new(
                    "SolanaWalletEmpty",
                    t!(cx, "common.no_data"),
                ))
                .into_any_element(),
            _ => self.list_body(cx),
        };

        div().size_full().min_w_0().min_h_0().child(body)
    }
}

/// What the dialog says about a confirmation the list cannot hold.
fn inconsistent() -> SelectionNotice {
    SelectionNotice::Problem(SelectionFailure::Inconsistent(
        SolanaWalletDataError::InconsistentSelections,
    ))
}

/// The list a pane draws its feed with.
///
/// The list holds the feed itself, so what a reader is reading is the list's to keep: it is
/// told about the transfers a snapshot added rather than about the snapshot. What it was given
/// is what it holds, which is why a feed that is not one the list it holds can be extended into
/// is a list built again rather than a list told about it.
fn feed_list() -> DequeUnsizedList<Arc<TransferItem>> {
    DequeUnsizedList::new(
        ElementId::Name(SharedString::new_static("SolanaWalletTransferList")),
        ListAlignment::Top,
        MAX_TRANSFERS,
        px(LIST_OVERDRAW),
        |index, item: &Arc<TransferItem>, _window, cx| transfer_row(item, index, cx),
    )
    .separator(|_, _| Separator::horizontal().into_any_element())
    .render_reverse()
    .keep_on_newest()
}

/// One row of the feed: what moved, when, and between which Solana wallets.
///
/// A row is separated from the one above it rather than framed: a feed is read as a list, and
/// the newest transfer is the row a reader starts at.
fn transfer_row(item: &TransferItem, index: usize, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let display = item.display(data_time_zone(cx));
    let row = div()
        .id(ElementId::NamedInteger(
            SharedString::new_static("SolanaWalletTransferRow"),
            index as u64,
        ))
        .debug_selector(|| "TEST_SOLANA_WALLET_TRANSFER_ROW".into())
        .role(Role::ListItem)
        .aria_label(t!(
            cx,
            "widget.solana_wallet.transfer",
            amount = display.amount.clone(),
            time = display.time.clone()
        ))
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .min_w_0()
        .px_2()
        .py_2();

    row.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .w_full()
            .min_w_0()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.roles.fg.default.hsla)
                    .child(display.amount.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.roles.fg.muted.hsla)
                    .child(display.time.clone()),
            ),
    )
    .child(address_line(
        t!(cx, "widget.solana_wallet.from"),
        item.origin().clone(),
        theme,
    ))
    .child(address_line(
        t!(cx, "widget.solana_wallet.to"),
        item.destination().clone(),
        theme,
    ))
    .into_any_element()
}

/// One end of a transfer: which Solana wallet, under the label that says which end it is.
fn address_line(label: SharedString, address: SharedString, theme: &Theme) -> AnyElement {
    div()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .text_xs()
        .child(
            div()
                .w(px(ADDRESS_LABEL_WIDTH))
                .flex_none()
                .text_color(theme.roles.fg.muted.hsla)
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_color(theme.roles.fg.default.hsla)
                .child(address),
        )
        .into_any_element()
}

/// The pane's own tests, and the fixtures the dialog's tests build a pane with.
///
/// The dialog is drawn from a pane and asks it for every change, so a test of either side needs
/// a pane that reads an account: what a pane is, and what it is waiting for, are private to this
/// module, which is why the two fixtures the dialog's tests need are handed over from here
/// rather than built again next to them.
#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use app_config::{
        settings::{AccountConfig, AccountWalletKind, Config},
        widget::{VersionedWidgetConfig, decode_or_default},
    };
    use base_ui::{animation::ANIMATION_TICK, input::InputStateBuilder, layer::Layer};
    use gpui_kit::{
        AppContext, BorrowAppContext, IntoElement, Modifiers, TestAppContext, VisualTestContext,
    };
    use i18n::language::codes::LanguageCode;
    use kytrix_service_provider::service::{
        engine::KytrixServiceError, solana_wallet::SolanaWalletSelection,
    };
    use network::transport::error::TransportError;
    use widget::protocol::RequestId;

    use super::*;
    use crate::{
        dialog::ADDRESS_INPUT_ID,
        test_support::{
            ACCOUNT, FakeFeed, FakeSession, FakeSource, selection, selection_at, selections,
            settle_until,
        },
    };

    const FIRST: &str = "So11111111111111111111111111111111111111112";
    const SECOND: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

    /// Settings holding `account`, or holding nobody when it is `None`.
    fn settings(account: Option<&str>) -> Config {
        let mut config = Config::default();
        config.account = account.map(|user_id| AccountConfig {
            user_id: user_id.to_owned(),
            wallet_address: FIRST.to_owned(),
            wallet_kind: AccountWalletKind::Solana,
        });
        config
    }

    /// Installs everything a pane reads, with `account` signed in or nobody.
    fn install(cx: &mut App, account: Option<&str>) {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
        global::install(cx).expect("the test Tokio runtime should build");
        let manager = ConfigManager::in_memory(settings(account), cx);
        cx.set_global(manager);
    }

    /// A pane the framework would have built, driven by a fake Solana wallet.
    fn pane_with_source<S>(source: S, cx: &mut Context<SolanaWalletWidget>) -> SolanaWalletWidget
    where
        S: SolanaWalletSource,
    {
        let status = cx.new(|_| WidgetStatus::new());
        let weak = cx.weak_entity();
        let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
            let _ = weak.update(cx, |pane, cx| pane.load(cx));
        });
        SolanaWalletWidget::new_with_source(
            SolanaWalletConfig,
            source,
            status,
            WidgetConfigChange::default(),
            recovery,
            cx,
        )
    }

    /// A pane holding an account with nothing started, so what a test reports to it is the
    /// only input it has.
    fn quiet_pane(cx: &mut TestAppContext, source: FakeSource) -> Entity<SolanaWalletWidget> {
        cx.executor().allow_parking();
        let pane = cx.update(|cx| {
            install(cx, Some(ACCOUNT));
            cx.new(|cx| pane_with_source(source, cx))
        });
        pane.update(cx, |pane, _| {
            pane.account_id = Some(SmolStr::new_static(ACCOUNT));
        });
        pane
    }

    /// A pane nobody is signed in for.
    fn signed_out_pane(cx: &mut TestAppContext, source: FakeSource) -> Entity<SolanaWalletWidget> {
        cx.executor().allow_parking();
        let pane = cx.update(|cx| {
            install(cx, None);
            cx.new(|cx| pane_with_source(source, cx))
        });
        pane.update(cx, |pane, cx| pane.load(cx));
        pane
    }

    /// A pane reading an account, with the stream the framework starts left running.
    fn pane(cx: &mut TestAppContext, source: &FakeSource) -> Entity<SolanaWalletWidget> {
        cx.executor().allow_parking();
        let pane = cx.update(|cx| {
            install(cx, Some(ACCOUNT));
            cx.new(|cx| pane_with_source(source.clone(), cx))
        });
        pane.update(cx, |pane, cx| pane.load(cx));
        pane
    }

    /// A pane reading an account whose stream is live, with the addresses it read.
    ///
    /// The read is the stream's, not this fixture's: a pane that has answered readiness reads
    /// the addresses itself, because until it has them it does not know what it follows. Asking
    /// again here would put a second read on the wire whenever the first had already answered,
    /// and that read would be the one to consume whatever a test queued for the change it is
    /// actually about — so what this waits for is the answer to the read the pane sends.
    ///
    /// The dialog's own tests build their pane the same way: what a dialog draws and asks for is
    /// read from a pane, and this is the pane a test of either side needs.
    pub(crate) fn live_pane(
        cx: &mut TestAppContext,
        source: &FakeSource,
        session: &FakeSession,
        addresses: Vec<SolanaWalletSelection>,
        limit: usize,
    ) -> Entity<SolanaWalletWidget> {
        session.answer_selections(selections(addresses, limit));
        let pane = pane(cx, source);
        settle_until(cx, "the addresses to arrive", |cx| {
            pane.read(cx).selections.is_some()
        });

        pane
    }

    /// The dialog a pane opens, as the pane holds it.
    ///
    /// `show` itself needs the window layer the application installs, so a test attaches the
    /// same dialog the way the pane would hold it: the field is built here, under the id the
    /// dialog's own one is built with, and nothing is left holding the dialog but this handle.
    fn attach_dialog(
        pane: &Entity<SolanaWalletWidget>,
        cx: &mut TestAppContext,
    ) -> Entity<SolanaWalletDialog> {
        let window = cx.add_empty_window();
        let dialog = window.update(|window, cx| {
            let address = cx.new(|cx| InputStateBuilder::new(ADDRESS_INPUT_ID).build(window, cx));
            cx.new(|cx| SolanaWalletDialog::new(pane.downgrade(), address, cx))
        });
        pane.update(cx, |pane, _| pane.dialog = Some(dialog.downgrade()));
        dialog
    }

    /// Reports a feed as the stream would.
    fn report_feed(
        pane: &Entity<SolanaWalletWidget>,
        feed: TransferSnapshot,
        cx: &mut App,
    ) -> bool {
        let scope = pane
            .read(cx)
            .current_scope()
            .expect("the pane follows an account");
        pane.update(cx, |pane, cx| {
            pane.apply_latest(
                scope,
                Arc::new(SolanaWalletStreamMessage::Snapshot(feed)),
                cx,
            )
        })
    }

    /// Reports a status as the stream would.
    fn report_status(
        pane: &Entity<SolanaWalletWidget>,
        status: SolanaWalletStatus,
        cx: &mut App,
    ) -> bool {
        let scope = pane
            .read(cx)
            .current_scope()
            .expect("the pane follows an account");
        pane.update(cx, |pane, cx| {
            pane.apply_reliable(
                scope,
                Arc::new(SolanaWalletStreamMessage::Status(status)),
                cx,
            )
        })
    }

    /// What the transfers the pane holds say, which is what its list is drawn from.
    ///
    /// The list is not read: what it holds is the list's own, and what a reader sees of it is
    /// asserted by drawing the pane.
    fn event_ids_of(pane: &SolanaWalletWidget) -> Vec<String> {
        pane.snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
            snapshot
                .items()
                .iter()
                .map(|item| item.event_id().to_owned())
                .collect()
        })
    }

    /// The addresses the pane holds.
    fn addresses_of(pane: &SolanaWalletWidget) -> Vec<String> {
        pane.selections.as_ref().map_or_else(Vec::new, |data| {
            data.selections()
                .iter()
                .map(|selection| selection.address.to_string())
                .collect()
        })
    }

    /// The address of the change the pane is waiting for, if it is waiting for one.
    ///
    /// This is how a dialog's test reads back what the dialog asked the pane for, which is the
    /// pane's own state rather than the dialog's.
    pub(crate) fn asked_for(pane: &Entity<SolanaWalletWidget>, cx: &App) -> Option<String> {
        pane.read(cx)
            .pending
            .as_ref()
            .and_then(|pending| pending.address.as_ref())
            .map(ToString::to_string)
    }

    /// Takes back what the pane is waiting for and what it last said.
    ///
    /// A test that asks the pane for two changes in a row says with this that the first one was
    /// answered: a pane still waiting for one refuses the next, which is a rule of its own.
    pub(crate) fn forget_last_change(pane: &Entity<SolanaWalletWidget>, cx: &mut App) {
        pane.update(cx, |pane, _| {
            pane.pending = None;
            pane.notice = None;
        });
    }

    /// A missing key is an empty string on screen rather than an error, so a gap in the
    /// interface is what a typo looks like: this is the test that turns it into a failure.
    #[gpui_kit::test]
    fn every_message_this_pane_shows_should_resolve_to_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx, Some(ACCOUNT));

            for key in [
                "widget.names.solana_wallet",
                "widget.solana_wallet.active",
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
                "widget.solana_wallet.paused",
                "widget.solana_wallet.refresh",
                "widget.solana_wallet.scroll_transfers",
                "widget.solana_wallet.stream_unavailable",
                "widget.solana_wallet.to",
                "widget.solana_wallet.transfer",
                "widget.solana_wallet.inconsistent_selections",
            ] {
                assert!(!t!(cx, key).is_empty(), "{key} has no copy");
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
                    !failure.localized_message(cx).is_empty(),
                    "{failure:?} has no copy"
                );
            }

            for failure in [
                SolanaWalletFailure::Stream(SolanaWalletStreamError::Service(
                    KytrixServiceError::Closed,
                )),
                SolanaWalletFailure::Stream(SolanaWalletStreamError::Transport(
                    TransportError::Closed,
                )),
                SolanaWalletFailure::Rpc(RpcCallError::ServiceClosed {
                    request_id: RequestId::new(3),
                }),
            ] {
                assert!(
                    !failure.localized_message(cx).is_empty(),
                    "{failure:?} has no copy"
                );
            }
        });
    }

    #[test]
    fn the_widget_should_have_a_stable_identity() {
        assert_eq!(SolanaWalletWidget::ID.as_str(), "kytrix.solana_wallet");
        assert_eq!(
            SolanaWalletWidget::DISPLAY_NAME,
            "widget.names.solana_wallet"
        );
        assert_eq!(
            SolanaWalletConfig::SCHEMA_VERSION,
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

    #[gpui_kit::test]
    fn a_signed_out_pane_should_say_what_it_is_waiting_for(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let pane = signed_out_pane(cx, source.clone());

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(!pane.has_content());
            assert_eq!(pane.current_scope(), None);
            assert_eq!(
                pane.status.state(cx).notice(),
                Some(&Notice::message("widget.solana_wallet.logged_out")),
                "a pane nobody signed in for is not a failure"
            );
            assert!(!pane.status.state(cx).is_failed());
        });
        assert_eq!(
            source.sessions(),
            0,
            "a pane nobody reads for takes no session"
        );
    }

    /// Switches the language and asserts the pane's waiting copy followed it.
    ///
    /// The notice is a reference into the catalogue rather than a sentence, so the pane reports
    /// nothing again: the frame that draws it resolves the words in the language showing then.
    fn the_waiting_copy_should_follow_the_language(
        cx: &mut TestAppContext,
        pane: &Entity<SolanaWalletWidget>,
        key: &'static str,
    ) {
        let (notice, english) = cx.update(|cx| {
            let notice = pane
                .read(cx)
                .status
                .state(cx)
                .notice()
                .cloned()
                .expect("the pane is waiting");
            assert_eq!(
                notice,
                Notice::message(key),
                "the pane waits with its own catalogue reference"
            );
            let english = notice.resolve(cx);
            assert_eq!(
                english,
                t!(cx, key),
                "and it resolves into the sentence the catalogue holds"
            );

            (notice, english)
        });

        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));

        cx.update(|cx| {
            let chinese = notice.resolve(cx);
            assert_eq!(
                chinese,
                t!(cx, key),
                "the same reference resolves in the language showing now"
            );
            assert_ne!(
                chinese, english,
                "and it is not the copy of the language the user left"
            );
        });
    }

    /// The words a waiting pane shows are the ones the window's language resolves them in: the
    /// notice is a reference into the catalogue, so the pane reports nothing again.
    #[gpui_kit::test]
    fn a_signed_out_pane_should_wait_for_a_login_in_the_language_showing_now(
        cx: &mut TestAppContext,
    ) {
        let pane = signed_out_pane(cx, FakeSource::default());

        the_waiting_copy_should_follow_the_language(cx, &pane, "widget.solana_wallet.logged_out");
    }

    /// The same for the other sentence this pane can be waiting with: the account is signed
    /// in, and the list it read holds nothing.
    #[gpui_kit::test]
    fn an_account_with_no_addresses_should_wait_in_the_language_showing_now(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_selections(selections(Vec::new(), 3));
        let pane = pane(cx, &source);
        settle_until(cx, "the addresses to arrive", |cx| {
            pane.read(cx).selections.is_some()
        });

        the_waiting_copy_should_follow_the_language(
            cx,
            &pane,
            "widget.solana_wallet.empty_addresses",
        );
    }

    /// A feed that lands while the pane is still reading its addresses is content the moment it
    /// arrives: the pane holds the transfers even though what it is *reporting* is still the read
    /// of the addresses, and readiness follows once that answers.
    ///
    /// The read is held open, which is what makes the window this test is about a window rather
    /// than a race: a pane whose answer has already landed is ready, and there is nothing left of
    /// "the feed came first" to assert.
    #[gpui_kit::test]
    fn a_feed_should_become_content_and_readiness_should_follow_it(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_selections(selections(vec![selection(FIRST)], 3));
        let held = session.block_selections();
        let pane = pane(cx, &source);
        // The service takes the account and publishes the empty feed before it says the stream is
        // live, and readiness is what sends the read the block above holds.
        settle_until(cx, "the read of the addresses to be sent", |_| {
            session.state().fetches == 1
        });
        let mut feed = FakeFeed::new();
        let live = feed.push("one", 1);

        assert!(cx.update(|cx| report_feed(&pane, live.clone(), cx)));
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(pane.has_content(), "an empty feed is still a feed");
            assert_eq!(event_ids_of(pane), ["one"]);
            assert_eq!(
                pane.status.state(cx),
                WidgetState::Loading,
                "the pane follows an account it has not been told the addresses of yet"
            );
        });

        // And readiness follows both: once the read answers, the pane is idle and the transfers
        // it took are the ones it holds.
        held.release();
        settle_until(cx, "the addresses to arrive", |cx| {
            pane.read(cx).selections.is_some()
        });
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.status.state(cx), WidgetState::Ready);
            assert_eq!(event_ids_of(pane), ["one"]);
        });
    }

    #[gpui_kit::test]
    fn a_live_subscription_without_transfers_should_still_be_content(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_selections(selections(vec![selection(FIRST)], 3));
        let pane = pane(cx, &source);
        settle_until(cx, "the service to hold the account", |_| {
            source.sessions() == 1
        });
        let feed = FakeFeed::new();

        cx.update(|cx| {
            report_feed(&pane, feed.snapshot(), cx);
            report_status(&pane, SolanaWalletStatus::Ready, cx);
        });
        settle_until(cx, "the addresses to arrive", |cx| {
            pane.read(cx).selections.is_some()
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(
                pane.has_content(),
                "an account whose addresses have not moved is still an account"
            );
            assert_eq!(pane.status.state(cx), WidgetState::Ready);
            assert!(!pane.status.state(cx).is_failed());
        });
    }

    /// An account with no addresses is a pane with nothing to follow, and a feed with no
    /// transfers in it reads exactly the same as one whose addresses were never added. Which
    /// of the two it is is a question only the address list answers, so the pane asks it —
    /// without a dialog, and without anyone asking it to.
    ///
    /// The answer takes the body away as well: with nothing to follow there is no list to draw,
    /// so the slot goes to the framework's placeholder and the sentence it carries.
    #[gpui_kit::test]
    fn an_account_with_no_addresses_should_say_it_has_none(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_selections(selections(Vec::new(), 3));
        let pane = pane(cx, &source);

        settle_until(cx, "the addresses to arrive", |cx| {
            pane.read(cx).selections.is_some()
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(
                pane.status.state(cx).notice(),
                Some(&Notice::message("widget.solana_wallet.empty_addresses")),
                "an account that monitors nothing says so rather than drawing an empty feed"
            );
            assert_eq!(
                pane.status.state(cx),
                WidgetState::waiting(Notice::message("widget.solana_wallet.empty_addresses"))
            );
            assert!(!pane.status.state(cx).is_failed());
            assert!(
                !pane.has_content(),
                "with nothing to follow the pane has nothing to draw, so the slot is the \
                 framework's to fill"
            );
        });

        // And the body is gone: the sentence belongs to the framework's placeholder, which draws
        // over the same slot. `EMPTY_STATE` is not asserted here because this pane has no body to
        // draw an empty state in — the placeholder is what reads, and it is not the body's own.
        let (_, window) = cx.add_window_view({
            let pane = pane.clone();
            move |_, _| PaneHarness { pane }
        });
        window.run_until_parked();
        // The list is drawn inside the body, which is the one cached layer: an absence read off a
        // frame that hit the cache would pass even after the list came back (AGENTS.md §10).
        repaint(window);
        assert!(
            window
                .debug_bounds("TEST_SOLANA_WALLET_TRANSFER_LIST")
                .is_none(),
            "an account with no addresses has no feed to draw"
        );
    }

    /// While the pane is reading the addresses it is loading, even though the live stream has
    /// already handed it a feed. That feed is empty — the service publishes one before anything
    /// else — and an empty feed is exactly what an account with no addresses looks like, so the
    /// pane does not report on it until it knows which of the two it is looking at.
    #[gpui_kit::test]
    fn a_pane_reading_its_addresses_should_be_loading(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        // The read is held open, so the pane stays in the window this test is about rather
        // than leaving it as soon as the fake answers.
        let _held = session.block_selections();
        session.answer_selections(selections(vec![selection(FIRST)], 3));
        let pane = pane(cx, &source);

        settle_until(cx, "the read to be sent", |_| session.state().fetches == 1);

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(
                pane.has_content(),
                "the stream has already published the empty feed this window is about"
            );
            assert!(
                pane.selections.is_none(),
                "and the list has not answered yet"
            );
            assert_eq!(
                pane.status.state(cx),
                WidgetState::Loading,
                "a pane that does not yet know what it follows is loading"
            );
        });
    }

    /// A read that failed is a failure, and the pane reports it as one rather than sitting on
    /// it. It is *not* a list with nothing in it: saying "no addresses" here would tell a reader
    /// they have none when they do.
    #[gpui_kit::test]
    fn a_read_that_failed_should_be_reported_rather_than_swallowed(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.refuse_selections(SolanaWalletApiError::Unavailable);
        let pane = pane(cx, &source);

        // Waiting on the read itself rather than on `pending`: a pane that has not got round
        // to asking yet also has nothing pending, which is not what this test is about.
        settle_until(cx, "the read to be refused", |_| {
            session.state().fetches == 1
        });
        settle_until(cx, "the failure to be reported", |cx| {
            pane.read(cx).status.state(cx).is_failed()
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(pane.selections.is_none());
            let status = pane.status.state(cx);
            assert!(
                status.is_failed(),
                "a pane that could not read what it follows is a failed pane"
            );
            assert!(
                status.notice().is_none(),
                "and it does not say the account has no addresses"
            );
            assert_eq!(
                status
                    .failure()
                    .map(|failure| failure.action().label(cx).to_string()),
                Some("Retry".to_owned()),
                "the failure offers the framework's own recovery"
            );
        });
    }

    /// The pane reads the addresses whether or not the dialog is open: they are its own
    /// scope, not the dialog's list, and nothing else can tell it that the account it follows
    /// has nothing to follow.
    #[gpui_kit::test]
    fn a_pane_without_a_dialog_should_read_the_addresses(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_selections(selections(vec![selection(FIRST)], 3));
        let pane = pane(cx, &source);

        settle_until(cx, "the addresses to arrive", |cx| {
            addresses_of(pane.read(cx)) == [FIRST.to_owned()]
        });
        assert_eq!(
            session.state().fetches,
            1,
            "the pane asks for the addresses on its own behalf"
        );
    }

    #[gpui_kit::test]
    fn a_failure_should_be_reported_with_the_panes_own_recovery(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let pane = quiet_pane(cx, source);
        let mut feed = FakeFeed::new();
        let live = feed.push("one", 1);

        cx.update(|cx| {
            report_feed(&pane, live, cx);
            report_status(
                &pane,
                SolanaWalletStatus::Failed(SolanaWalletStreamError::Transport(
                    TransportError::Closed,
                )),
                cx,
            );
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            let status = pane.status.state(cx);
            assert!(status.is_failed());
            assert_eq!(
                status.failure().map(|failure| failure.text(cx).to_string()),
                Some(
                    SolanaWalletStreamError::Transport(TransportError::Closed)
                        .localized_message(cx)
                        .to_string()
                )
            );
            assert_eq!(
                status
                    .failure()
                    .map(|failure| failure.action().label(cx).to_string()),
                Some("Retry".to_owned()),
                "every failure this pane reports offers the framework's recovery"
            );
            assert!(
                pane.has_content(),
                "transfers that were read before a failure are not dropped by it"
            );
        });
    }

    #[gpui_kit::test]
    fn a_message_after_a_failure_should_be_refused(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let pane = quiet_pane(cx, source);
        let mut feed = FakeFeed::new();
        let live = feed.push("one", 1);

        cx.update(|cx| {
            report_status(
                &pane,
                SolanaWalletStatus::Failed(SolanaWalletStreamError::Transport(
                    TransportError::Closed,
                )),
                cx,
            );
        });

        assert!(
            !cx.update(|cx| report_feed(&pane, live, cx)),
            "a failure is terminal until the user recovers it"
        );
        cx.update(|cx| {
            assert_eq!(event_ids_of(pane.read(cx)), Vec::<String>::new());
        });
    }

    #[gpui_kit::test]
    fn a_message_for_another_account_should_be_refused(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let pane = quiet_pane(cx, source);
        let mut feed = FakeFeed::new();
        let live = feed.push("one", 1);
        let other = SolanaWalletScope::new(Some(&SmolStr::new_static("somebody-else")))
            .expect("an account is a scope");

        let changed = cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.apply_latest(
                    other,
                    Arc::new(SolanaWalletStreamMessage::Snapshot(live)),
                    cx,
                )
            })
        });

        assert!(!changed, "another account's transfers are not this pane's");
        cx.update(|cx| assert!(!pane.read(cx).has_content()));
    }

    #[gpui_kit::test]
    fn a_recovery_should_keep_the_transfers_and_an_account_change_should_not(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        // An account with addresses, so what this test follows is the feed and not the list: what
        // an account with none of them shows is a rule of its own.
        let pane = live_pane(cx, &source, &session, vec![selection(FIRST)], 3);
        let mut feed = FakeFeed::new();
        let live = feed.push("one", 1);
        cx.update(|cx| report_feed(&pane, live, cx));

        // A recovery restarts the stream over what the reader is looking at.
        cx.update(|cx| {
            pane.update(cx, |pane, cx| pane.start(true, cx));
        });
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(pane.has_content());
            assert_eq!(pane.status.state(cx), WidgetState::Connecting);
        });

        // Another account is another feed, so what this one showed is dropped.
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.reconcile_account(Some(SmolStr::new_static("somebody-else")), cx);
            });
        });
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(!pane.has_content());
            assert!(pane.selections.is_none());
        });
    }

    /// Another account is another feed, and the pane hears about it from the settings
    /// rather than from a reload: it opens that account's session and reads for it.
    #[gpui_kit::test]
    fn an_account_change_should_read_the_new_account(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let first = source.session(ACCOUNT);
        let _first_control = first.subscribe();
        let second = source.session("someone-else");
        let _second_control = second.subscribe();
        let pane = pane(cx, &source);
        settle_until(cx, "the stream to come up", |cx| pane.read(cx).ready);

        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.account = Some(AccountConfig {
                            user_id: "someone-else".to_owned(),
                            wallet_address: FIRST.to_owned(),
                            wallet_kind: AccountWalletKind::Solana,
                        });
                    },
                    cx,
                );
            });
        });

        settle_until(cx, "the replacement session", |_| {
            second.state().subscribes == 1
        });
        cx.update(|cx| {
            assert_eq!(pane.read(cx).account_id.as_deref(), Some("someone-else"));
        });
        assert_eq!(
            source.requested(),
            [
                SmolStr::new_static(ACCOUNT),
                SmolStr::new_static("someone-else"),
            ],
            "the session the pane opened is the new account's"
        );
    }

    #[gpui_kit::test]
    fn a_read_should_wait_for_the_stream_and_be_sent_once_it_is_live(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_selections(selections(vec![selection(FIRST)], 3));
        let pane = pane(cx, &source);

        // Nothing is holding the account yet, so the read waits rather than being sent.
        let started = cx.update(|cx| pane.update(cx, |pane, cx| pane.load_selections(cx)));
        assert!(started);
        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(
                pane.pending,
                Some(SelectionPending {
                    operation: SelectionOperation::Loading,
                    address: None,
                    sent: false,
                })
            );
        });
        assert_eq!(
            session.state().fetches,
            0,
            "a read before the stream is live has no session to answer it"
        );

        settle_until(cx, "the addresses to arrive", |cx| {
            pane.read(cx).selections.is_some()
        });
        assert_eq!(session.state().fetches, 1);
    }

    #[gpui_kit::test]
    fn a_stream_that_came_back_should_read_the_addresses_again(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(cx, &source, &session, vec![selection(FIRST)], 3);
        let _dialog = attach_dialog(&pane, cx);
        assert_eq!(session.state().fetches, 1);
        session.answer_selections(selections(vec![selection(FIRST), selection(SECOND)], 3));

        cx.update(|cx| {
            report_status(&pane, SolanaWalletStatus::Connecting, cx);
            report_status(&pane, SolanaWalletStatus::Ready, cx);
        });

        settle_until(cx, "the addresses to be read again", |cx| {
            addresses_of(pane.read(cx)).len() == 2
        });
        assert_eq!(
            session.state().fetches,
            2,
            "the list a pane shows is only as true as the last read over the live stream"
        );
    }

    #[gpui_kit::test]
    fn a_read_that_failed_should_say_why_and_leave_nothing_behind(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.refuse_selections(SolanaWalletApiError::Unavailable);
        let pane = pane(cx, &source);

        // The read is the pane's own, sent by the readiness that arrives: asking for a second one
        // here would be a read the session has no answer queued for, and which of the two the
        // refusal belongs to would depend on which got to the session first.
        settle_until(cx, "the read to be refused", |cx| {
            matches!(pane.read(cx).selection_read, SelectionRead::Failed(_))
        });
        settle_until(cx, "the refusal to arrive", |cx| {
            pane.read(cx).pending.is_none()
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(pane.selections.is_none());
            assert!(matches!(
                pane.notice,
                Some(SelectionNotice::Problem(SelectionFailure::Rpc(
                    RpcCallError::Service {
                        source: SolanaWalletApiError::Unavailable,
                        ..
                    }
                )))
            ));
        });
    }

    #[gpui_kit::test]
    fn a_dialog_should_be_open_exactly_when_the_pane_holds_one(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let pane = quiet_pane(cx, source);
        cx.update(|cx| assert!(!pane.read(cx).dialog_open()));

        let dialog = attach_dialog(&pane, cx);

        cx.update(|cx| assert!(pane.read(cx).dialog_open()));
        drop(dialog);
        cx.update(|cx| {
            assert!(
                !pane.read(cx).dialog_open(),
                "a dialog the window layer let go is not one the pane can still reach"
            );
        });
    }

    /// The window a dialog is really opened over: the layer holds it, draws it, and is what
    /// drops it once its exit transition is over.
    fn dialog_window<'a>(
        cx: &'a mut TestAppContext,
        pane: &Entity<SolanaWalletWidget>,
    ) -> &'a mut VisualTestContext {
        let pane = pane.clone();
        let (_, window) =
            cx.add_window_view(move |_, cx| Layer::new(cx.new(move |_| PaneHarness { pane })));
        window.run_until_parked();

        window
    }

    /// Lets the dialog's own transition finish and repaints, so that one just opened is past
    /// its first frames and one just dismissed is gone rather than still fading out.
    fn settle_dialog(window: &mut VisualTestContext) {
        window
            .executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        window.update(|window, _| window.refresh());
        window.run_until_parked();
    }

    /// The control at the foot of the dialog closes it by emitting the event the layer listens
    /// for, rather than by asking the window to dismiss whatever happens to be on top — and the
    /// pane is left without it once the layer has let it go.
    ///
    /// Both halves are the point: the dialog leaving the tree is the emit having reached the
    /// layer that hosts it, and the pane losing its handle is the same thing seen from the side
    /// that has to open the next one. A pane that went on believing a dialog was open would
    /// refuse the next one, which the end of this test asks for.
    #[gpui_kit::test]
    fn a_control_at_the_foot_of_the_dialog_should_close_it_and_leave_the_pane_without_one(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(cx, &source, &session, vec![selection(FIRST)], 3);
        // The read the dialog asks for when it opens, answered the way the pane's own was.
        session.answer_selections(selections(vec![selection(FIRST)], 3));

        let window = dialog_window(cx, &pane);
        window.update(|window, cx| SolanaWalletDialog::show(pane.downgrade(), window, cx));
        settle_dialog(window);

        assert!(
            window
                .debug_bounds("TEST_SOLANA_WALLET_ADDRESS_FIELD")
                .is_some(),
            "the dialog should be drawn over the layer once it is opened"
        );
        assert!(
            window.cx.update(|cx| pane.read(cx).dialog_open()),
            "the pane should hold the dialog it opened"
        );

        let dialog = window.cx.update(|cx| {
            pane.read(cx)
                .dialog
                .as_ref()
                .and_then(WeakEntity::upgrade)
                .expect("the pane should hold the dialog it opened")
        });
        let close = window
            .debug_bounds("TEST_SOLANA_WALLET_DIALOG_CLOSE")
            .expect("the dialog should draw the control at its foot")
            .center();
        // The handle the test holds goes first: the layer is what holds the last strong
        // reference to the dialog, and one kept here would keep it — and everything it owns —
        // alive after it has left, which is the very thing asserted below.
        drop(dialog);

        window.simulate_click(close, Modifiers::none());
        for _ in 0..4 {
            settle_dialog(window);
            if !window.cx.update(|cx| pane.read(cx).dialog_open()) {
                break;
            }
        }

        assert!(
            !window.cx.update(|cx| pane.read(cx).dialog_open()),
            "the pane should be left without the dialog its own control closed"
        );
        assert!(
            window
                .debug_bounds("TEST_SOLANA_WALLET_ADDRESS_FIELD")
                .is_none(),
            "the dialog should have left the tree once its exit transition was over"
        );

        // And the pane opens another one: what it was holding was the dialog that left rather
        // than one kept alive by the pane itself.
        window.update(|window, cx| SolanaWalletDialog::show(pane.downgrade(), window, cx));
        settle_dialog(window);
        assert!(
            window
                .debug_bounds("TEST_SOLANA_WALLET_ADDRESS_FIELD")
                .is_some(),
            "a pane whose dialog has left should open another one"
        );
    }

    #[gpui_kit::test]
    fn an_address_the_service_confirmed_should_join_the_list(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(cx, &source, &session, vec![selection_at(FIRST, 1)], 3);
        session
            .state()
            .adds
            .push_back(SolanaWalletMutationOutcome::Confirmed(selection_at(
                SECOND, 2,
            )));

        assert!(cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.add_selection(SmolStr::new_static(SECOND), cx)
            })
        }));
        settle_until(cx, "the address to be added", |cx| {
            addresses_of(pane.read(cx)).len() == 2
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(
                addresses_of(pane),
                [SECOND.to_owned(), FIRST.to_owned()],
                "the confirmed address reads in the order a fresh answer would"
            );
            assert_eq!(pane.notice, Some(SelectionNotice::Added));
            assert_eq!(pane.pending, None);
        });
    }

    #[gpui_kit::test]
    fn an_address_the_service_refused_should_say_why(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(cx, &source, &session, vec![selection(FIRST)], 3);
        session
            .state()
            .adds
            .push_back(SolanaWalletMutationOutcome::Rejected(
                SolanaWalletApiError::Conflict,
            ));

        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.add_selection(SmolStr::new_static(SECOND), cx);
            });
        });
        settle_until(cx, "the refusal to arrive", |cx| {
            pane.read(cx).pending.is_none()
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(
                pane.notice,
                Some(SelectionNotice::Problem(SelectionFailure::Rejected(
                    SolanaWalletApiError::Conflict
                )))
            );
            assert_eq!(
                addresses_of(pane),
                [FIRST.to_owned()],
                "a refused change leaves the list as it was"
            );
        });
    }

    /// A change the service could not tell the outcome of is not one the pane can apply: it
    /// says so and leaves the list alone rather than guessing either way.
    #[gpui_kit::test]
    fn a_change_of_an_unknown_outcome_should_say_so_and_leave_the_list_alone(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(cx, &source, &session, vec![selection(FIRST)], 3);
        session
            .state()
            .adds
            .push_back(SolanaWalletMutationOutcome::Unknown(
                SolanaWalletMutationUnknown::Timeout,
            ));

        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.add_selection(SmolStr::new_static(SECOND), cx);
            });
        });
        settle_until(cx, "the outcome to arrive", |cx| {
            pane.read(cx).pending.is_none()
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(
                pane.notice,
                Some(SelectionNotice::Problem(SelectionFailure::Unknown(
                    SolanaWalletMutationUnknown::Timeout
                )))
            );
            assert_eq!(addresses_of(pane), [FIRST.to_owned()]);
        });
    }

    #[gpui_kit::test]
    fn an_address_the_service_confirmed_removed_should_leave_the_list(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(
            cx,
            &source,
            &session,
            vec![selection(FIRST), selection(SECOND)],
            3,
        );
        session
            .state()
            .deletes
            .push_back(SolanaWalletMutationOutcome::Confirmed(()));

        assert!(cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.delete_selection(SmolStr::new_static(FIRST), cx)
            })
        }));
        settle_until(cx, "the address to be left", |cx| {
            addresses_of(pane.read(cx)) == [SECOND.to_owned()]
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.notice, None, "a removal is visible in the list itself");
            assert_eq!(pane.pending, None);
        });
    }

    #[gpui_kit::test]
    fn a_change_should_be_refused_while_the_stream_is_not_live(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(cx, &source, &session, vec![selection(FIRST)], 3);
        cx.update(|cx| {
            report_status(&pane, SolanaWalletStatus::Connecting, cx);
        });

        assert!(
            !cx.update(|cx| {
                pane.update(cx, |pane, cx| {
                    pane.add_selection(SmolStr::new_static(SECOND), cx)
                })
            }),
            "a change goes over the session the stream was opened with"
        );
        assert_eq!(
            session.state().add_calls,
            0,
            "a refused change never reaches the account"
        );
    }

    /// A change the pane stopped waiting for — because the user recovered the stream — is
    /// dropped rather than decided: the pane does not know whether it was made, and it does
    /// not ask again either. The read that follows the stream coming back is what settles it.
    #[gpui_kit::test]
    fn a_change_a_recovery_replaced_should_be_forgotten(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(cx, &source, &session, vec![selection(FIRST)], 3);
        let block = session.block_mutation();
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.add_selection(SmolStr::new_static(SECOND), cx);
            });
        });
        settle_until(cx, "the change to reach the account", |_| {
            session.state().add_calls == 1
        });

        let recovered = source.session(ACCOUNT);
        let _recovered_control = recovered.subscribe();
        cx.update(|cx| {
            pane.update(cx, |pane, cx| pane.start(true, cx));
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert_eq!(
                pane.pending, None,
                "a question the pane stopped waiting for is not one it is still asking"
            );
            assert!(pane.notice.is_none());
        });
        assert_eq!(
            session.state().add_calls,
            1,
            "a change of an unknown outcome is not asked again"
        );
        settle_until(cx, "the account to stop being asked", |_| {
            block.is_dropped()
        });
    }

    /// The pane draws the feed it holds. What a dialog over it draws is the dialog's own: it is
    /// asserted where the dialog's header, toolbar and list are.
    #[gpui_kit::test]
    fn a_pane_should_draw_its_feed(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(cx, &source, &session, vec![selection(FIRST)], 3);
        let mut feed = FakeFeed::new();
        let live = feed.push("one", 1);
        cx.update(|cx| report_feed(&pane, live, cx));

        let (_, window) = cx.add_window_view(move |_, _| PaneHarness { pane });
        window.run_until_parked();
        repaint(window);
        assert!(
            window
                .debug_bounds("TEST_SOLANA_WALLET_TRANSFER_LIST")
                .is_some(),
            "the body should draw the list it holds"
        );
    }

    /// A feed nothing the list held survives in is one the list is built again for: a list holds
    /// the transfers it was given, so the feed a pane draws is the one it has now rather than the
    /// rows of the one before it.
    #[gpui_kit::test]
    fn a_feed_nothing_of_which_survives_should_be_held_by_a_list_built_again(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let pane = quiet_pane(cx, source);
        let mut feed = FakeFeed::new();

        let held = cx.update(|cx| {
            report_feed(&pane, feed.push("one", 1), cx);
            report_feed(&pane, feed.push("two", 2), cx);
            pane.read(cx).list.entity_id()
        });

        let mut replacement = FakeFeed::new();
        let snapshot = replacement.push("three", 3);
        let built = cx.update(|cx| {
            report_feed(&pane, snapshot, cx);

            assert_eq!(
                event_ids_of(pane.read(cx)),
                ["three"],
                "the pane holds the feed it was given"
            );

            pane.read(cx).list.entity_id()
        });

        assert_ne!(
            held, built,
            "the list is built again for a feed none of what it held survives in"
        );
    }

    /// An account that does follow addresses, none of which has moved, is the other emptiness:
    /// the pane is ready and its body says it holds nothing, rather than drawing a list with no
    /// rows in it — which reads the same as a list that failed to load.
    #[gpui_kit::test]
    fn a_feed_with_no_transfers_should_say_the_body_holds_nothing(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(cx, &source, &session, vec![selection(FIRST)], 3);
        cx.update(|cx| report_feed(&pane, FakeFeed::new().snapshot(), cx));

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(
                pane.has_content(),
                "an account that follows an address is content whatever has moved"
            );
            assert_eq!(pane.status.state(cx), WidgetState::Ready);
            assert!(pane.status.state(cx).notice().is_none());
        });

        let (_, window) = cx.add_window_view(move |_, _| PaneHarness { pane });
        window.run_until_parked();
        // Both readings below are of the body, which is the one cached layer, so the frame they are
        // read off has to be one that could not have hit the cache (AGENTS.md §10).
        repaint(window);

        assert!(
            window.debug_bounds("EMPTY_STATE").is_some(),
            "an empty feed should say the pane holds nothing"
        );
        assert!(
            window
                .debug_bounds("TEST_SOLANA_WALLET_TRANSFER_LIST")
                .is_none(),
            "a list with no rows in it is not drawn at all"
        );
    }

    /// Draws one more frame with every cache missed, so what a debug selector reports is what this
    /// frame painted rather than what the frame before it left behind. A paint action is what fills
    /// a selector, and a frame that hits the body cache reuses the last paint range without drawing
    /// (AGENTS.md §10); `window.refresh()` is the one frame that cannot hit it.
    fn repaint(window: &mut VisualTestContext) {
        window.update(|window, _| window.refresh());
        window.run_until_parked();
    }

    /// A frame of its own, so a pane can be drawn without the pane the framework builds.
    struct PaneHarness {
        pane: Entity<SolanaWalletWidget>,
    }

    impl Render for PaneHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.pane.clone())
        }
    }
}

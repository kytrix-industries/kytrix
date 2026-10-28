//! The dialog a pane opens to manage the addresses its transfers are watched for.
//!
//! # What the dialog owns, and what it does not
//!
//! It owns what a pane has no use for — what the user is typing, the list of addresses with
//! the place a reader is in it, the sentence the body stands in with, and the entities those
//! are drawn through — and draws everything else from the pane, which is what holds the
//! addresses and the service that answers a read or a change. The state that outlives any one
//! dialog belongs to the pane: a dialog that held the addresses would lose them on the next
//! open, and one that held the change in flight would hold it for a session the pane is running.
//!
//! # Why the two sides only hold each other weakly
//!
//! The dialog reaches the pane through a [`WeakEntity`], and the pane holds the dialog the
//! same way. A dialog holding the pane strongly would keep a closed pane's scope alive, and a
//! pane holding the dialog strongly would never let it close: what the layer holds is the only
//! strong reference to a dialog, which is what makes closing one release the state below.

use std::sync::Arc;

use base_ui::{
    badge::Badge,
    button::Button,
    icon::{Icon, IconKind},
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    interactive::{Interactive, InteractiveVariantExt},
    layer::LayerExt,
    markdown::view::{Markdown, MarkdownState},
    separator::Separator,
    size::UnifiedSizable,
    spinner::Spinner,
    static_sized_list::StaticSizedList,
    style::theme::ThemeExt,
    tooltip::TooltipExt,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, DismissEvent, ElementId, Entity, EventEmitter,
    FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Render, Role, SharedString,
    StatefulInteractiveElement, Styled, WeakEntity, Window, base::Disableable, div, px,
};
use i18n::t;
use kytrix_service_provider::service::ethereum_wallet::{
    EthereumWalletSelection, EthereumWalletSelectionStatus, ethereum_wallet_address,
};
use smol_str::{SmolStr, format_smolstr};
use widget::{error::LocalizedError, status::WidgetAction};

use super::{
    model::SelectionData,
    view::{EthereumWalletWidget, SelectionNotice, SelectionOperation},
};

/// The size of the address dialog's panel.
const DIALOG_WIDTH: f32 = 620.0;
const DIALOG_HEIGHT: f32 = 480.0;

/// The address field of the dialog.
///
/// Shared with the tests that build a dialog by hand, so that what they build the field under is the
/// id production uses rather than a copy of it.
pub(super) const ADDRESS_INPUT_ID: &str = "ethereum_wallet-monitor-address";

/// The list the dialog draws the addresses with.
const ADDRESS_LIST_ID: &str = "ethereum_wallet-monitor-address-list";

/// The height one address of that list is drawn at.
///
/// A list of places measures the first row it draws and lays every other one out at that height,
/// so what a row is worth has to be the row's own height: this one is an address with what the
/// account is doing with it on the line below, and the medium surface it is drawn as is a control
/// of one line.
const ADDRESS_ROW_HEIGHT: f32 = 60.0;

/// The dialog a pane opens to manage the addresses its transfers are watched for.
///
/// The dialog says it is done by emitting [`DismissEvent`], which is the signal the layer that
/// hosts it subscribes to: it needs no window to be handed and no way of being the dialog on
/// top, and a dismissal taken from the window instead would close whatever happens to be on top.
pub(super) struct EthereumWalletDialog {
    widget: WeakEntity<EthereumWalletWidget>,
    address: Entity<InputState>,
    /// What the field holds.
    ///
    /// The input element is rebuilt every frame and its state exposes no value, so what is
    /// typed is followed through the events the input emits rather than read back off it.
    typed: Option<SmolStr>,
    list: Entity<StaticSizedList<EthereumWalletSelection>>,
    /// The addresses the list was last told to draw, which is how a frame knows whether what
    /// the pane holds is something the list has not been shown.
    shown: Option<Arc<[EthereumWalletSelection]>>,
    /// The document the body's own message is drawn from when there is nothing to list.
    ///
    /// The sentence is catalogue copy — the same copy the pane's placeholder draws — and it is
    /// written as markdown, so it is drawn by the markdown component here too. The document
    /// lives with the dialog rather than being built per frame, and a frame that resolves a
    /// different sentence is the one that re-parses it: a language the interface moved to since
    /// is a different sentence, so the same frame catches that as well.
    message: Entity<MarkdownState>,
}

impl EventEmitter<DismissEvent> for EthereumWalletDialog {}

impl EthereumWalletDialog {
    /// Opens the dialog over the window, for the pane the control belongs to.
    ///
    /// Nothing happens when it is already open: the control is the dialog's only entrance,
    /// and replacing the open one would throw away what the user has typed.
    pub(super) fn show(
        widget: WeakEntity<EthereumWalletWidget>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(pane) = widget.upgrade() else {
            return;
        };
        if pane.read(cx).dialog_open() {
            return;
        }
        // The field belongs to the dialog, and so does what it holds: a field that emptied
        // itself under the user would be one they could not correct, so it keeps the address
        // it was given until the user replaces or clears it.
        let address = cx.new(|cx| {
            InputStateBuilder::new(ADDRESS_INPUT_ID)
                .placeholder(t!(cx, "widget.ethereum_wallet.address_placeholder"))
                .build(window, cx)
        });
        let dialog = cx.new(|cx| Self::new(widget.clone(), address, cx));
        // The pane's handle is taken before the entity is handed to the layer, which is what
        // holds the only strong reference to it from here: a handle kept past this call would
        // be a second owner, and the dialog would outlive its own closing.
        let handle = dialog.downgrade();
        window.dialog(cx, dialog);
        pane.update(cx, |pane, cx| pane.attach_dialog(handle, cx));
    }

    /// The dialog a pane holds.
    ///
    /// The tests on either side of it build one this way too: `show` needs the window layer the
    /// application installs, and what a test is about is what the dialog draws and asks for.
    pub(super) fn new(
        widget: WeakEntity<EthereumWalletWidget>,
        address: Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(
            &address,
            |dialog, _, event: &InputStateEvent, cx| match event {
                InputStateEvent::Change(value) => {
                    let typed = (!value.is_empty()).then(|| SmolStr::new(value.as_str()));
                    if dialog.typed != typed {
                        dialog.typed = typed;
                        cx.notify();
                    }
                }
                InputStateEvent::PressEnter(_) => dialog.add(cx),
                // Nothing here commits on a blur: what these fields hold is submitted by
                // the button or by Enter.
                InputStateEvent::Blur(_) => {}
            },
        )
        .detach();

        // The list is built once and told what to draw, so the row it draws is given the
        // dialog rather than the state of the frame it is drawn in: what a row reads is the
        // pane the dialog reads, which is what holds the addresses and the change in flight.
        let dialog = cx.entity().downgrade();
        let list = cx.new(|_| {
            StaticSizedList::new(
                ElementId::Name(SharedString::new_static(ADDRESS_LIST_ID)),
                move |_index, selection: &EthereumWalletSelection, _window, cx| {
                    address_row(selection, &dialog, cx)
                },
            )
        });

        Self {
            widget,
            address,
            typed: None,
            list,
            shown: None,
            // The document starts empty: the sentence a body with nothing to list says depends
            // on the pane, which the first frame reads and resolves like every frame after it.
            message: cx.new(|cx| MarkdownState::new("", cx)),
        }
    }

    /// Whether a change may be asked for right now.
    ///
    /// A row disables its control instead of leaving it to fail, and a row is drawn on every
    /// frame of the dialog, so what disables it is read from the pane as the row is drawn
    /// rather than handed to the list once.
    fn may_change(&self, cx: &App) -> bool {
        self.widget
            .upgrade()
            .is_some_and(|pane| pane.read(cx).dialog_state().may_change())
    }

    /// Reads the addresses again.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.widget.upgrade() else {
            return;
        };
        pane.update(cx, |pane, cx| {
            pane.load_selections(cx);
        });
    }

    /// Monitors the address that was typed, when it is one that could be monitored.
    ///
    /// The control that sends it is disabled for anything else, but the field also answers
    /// Enter, which reaches here without a control to have been disabled.
    fn add(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.widget.upgrade() else {
            return;
        };
        let typed = self.typed.clone();
        let address = pane.read(cx).dialog_state().addable_address(typed.as_ref());
        let Some(address) = address else {
            return;
        };
        pane.update(cx, |pane, cx| {
            pane.add_selection(address, cx);
        });
    }

    /// Stops monitoring one address.
    fn delete(&mut self, address: SmolStr, cx: &mut Context<Self>) {
        let Some(pane) = self.widget.upgrade() else {
            return;
        };
        pane.update(cx, |pane, cx| {
            pane.delete_selection(address, cx);
        });
    }

    /// What the account monitors, and the way to read it again.
    fn header(&self, state: &DialogState, dialog: &Entity<Self>, cx: &Context<Self>) -> AnyElement {
        let count = state.selections.as_ref().map_or_else(
            || t!(cx, "widget.ethereum_wallet.dialog_title"),
            |data| {
                t!(
                    cx,
                    "widget.ethereum_wallet.monitored_addresses",
                    current = data.selections().len(),
                    limit = data.limit()
                )
            },
        );
        let refresh = Button::new("ethereum_wallet-monitor-refresh")
            .ghost()
            .medium()
            .disabled(state.operation.is_some() || !state.ready)
            .aria_label(t!(cx, "widget.ethereum_wallet.refresh"))
            .with_tooltip(t!(cx, "widget.ethereum_wallet.refresh"))
            .on_click({
                let dialog = dialog.clone();
                move |_, _, cx| {
                    dialog.update(cx, |dialog, cx| dialog.reload(cx));
                }
            })
            .child(Icon::new(IconKind::ArrowsClockwise).size_3());

        div()
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(count),
            )
            .child(div().flex_none().child(refresh))
            .into_any_element()
    }

    /// The address field and the control that monitors what it holds.
    ///
    /// The field itself is never disabled: typing is free, and what would be done with the
    /// address is what the control beside it decides. An account that may not monitor
    /// another one — because the stream is down, the list is full, or the address is already
    /// on it — is refused there rather than by a field that will not take input.
    ///
    /// The control carries no label of its own, so its tooltip is what names it and its icon
    /// is sized as the text it stands in for: a plus that is smaller than the address beside
    /// it reads as a decoration of the field rather than as the act of adding to it.
    fn toolbar(
        &self,
        state: &DialogState,
        dialog: &Entity<Self>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let add = Button::new("ethereum_wallet-monitor-add")
            .filled()
            .medium()
            .disabled(state.addable_address(self.typed.as_ref()).is_none())
            .aria_label(t!(cx, "widget.ethereum_wallet.add_address"))
            .with_tooltip(t!(cx, "widget.ethereum_wallet.add_address"))
            .on_click({
                let dialog = dialog.clone();
                move |_, _, cx| {
                    dialog.update(cx, |dialog, cx| dialog.add(cx));
                }
            })
            .child(Icon::new(IconKind::Plus).size_4());

        div()
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .debug_selector(|| "TEST_ETHEREUM_WALLET_ADDRESS_FIELD".into())
                    .min_w_0()
                    .flex_1()
                    .child(Input::new(&self.address).medium().clearable(true)),
            )
            .child(
                div()
                    .debug_selector(|| "TEST_ETHEREUM_WALLET_ADD_ADDRESS".into())
                    .flex_none()
                    .child(add),
            )
            .into_any_element()
    }

    /// What the last change says, if it said anything.
    fn notice(&self, state: &DialogState, cx: &Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let (text, color) = match state.notice.as_ref()? {
            SelectionNotice::Added => (
                t!(cx, "widget.ethereum_wallet.add_success"),
                theme.roles.status.success.base.hsla,
            ),
            SelectionNotice::Problem(failure) => (
                failure.localized_message(cx),
                theme.roles.status.danger.base.hsla,
            ),
        };

        Some(
            div()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(color)
                .child(text)
                .into_any_element(),
        )
    }

    /// The addresses, or why there are none to show.
    ///
    /// A sentence stands in for the list by drawing the document the frame read into
    /// [`Self::resolve_message`], so nothing here parses anything: the two situations that have
    /// nothing to list are one document, read before the body is drawn.
    fn body(
        &mut self,
        state: &DialogState,
        dialog: &Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        if !state.signed_in {
            return centered_message(&self.message, theme.roles.fg.muted.hsla);
        }
        let Some(data) = state.selections.as_ref() else {
            return match state.operation {
                // A read is on its way, whether it is out or waiting for the stream.
                Some(operation) if operation.is_read() => loading_addresses(cx),
                // Nothing is holding the account, so a read has nothing to answer it: the way
                // out is the recovery the pane itself offers, which is the same control the
                // pane is showing behind this dialog.
                _ if !state.ready => message_with_action(
                    t!(cx, "widget.ethereum_wallet.stream_unavailable"),
                    recovery_button(&state.recovery, cx),
                ),
                // The read was answered with a failure, which the notice above names.
                _ => message_with_action(
                    t!(cx, "widget.ethereum_wallet.load_failed"),
                    self.retry_button(dialog, cx),
                ),
            };
        };
        if data.selections().is_empty() {
            return centered_message(&self.message, theme.roles.fg.muted.hsla);
        }

        self.show_addresses(&data.shared_selections(), cx);

        self.address_list()
    }

    /// The sentence the body stands in with when it has nothing to list.
    ///
    /// It is the same copy the pane's placeholder shows for the same two situations, so the
    /// pane behind the dialog and the dialog itself say one thing rather than two.
    fn message_text(state: &DialogState, cx: &App) -> SharedString {
        if !state.signed_in {
            return t!(cx, "widget.ethereum_wallet.logged_out");
        }

        t!(cx, "widget.ethereum_wallet.empty_addresses")
    }

    /// Reads the body's sentence into this dialog's document, if it is not the one already there.
    ///
    /// `set_source` is what decides that: a sentence that has not moved is left alone, so a frame
    /// spends no parse on it — and a sentence that has moved, because the pane changed or the
    /// language did, is parsed exactly once.
    fn resolve_message(&mut self, state: &DialogState, cx: &mut Context<Self>) {
        let message = Self::message_text(state, cx);
        self.message
            .update(cx, |document, cx| document.set_source(message, cx));
    }

    /// Shows the addresses the pane is holding, if they are not the ones the list holds.
    ///
    /// A list draws what it was given and remembers where the reader is, so it is told about a
    /// new set of addresses rather than rebuilt every frame: being told would put the reader
    /// back at the top of a list that had not changed.
    fn show_addresses(
        &mut self,
        selections: &Arc<[EthereumWalletSelection]>,
        cx: &mut Context<Self>,
    ) {
        if self
            .shown
            .as_ref()
            .is_some_and(|shown| Arc::ptr_eq(shown, selections))
        {
            return;
        }
        self.shown = Some(selections.clone());
        self.list.update(cx, |list, cx| {
            list.replace(selections.iter().cloned());
            cx.notify();
        });
    }

    /// The addresses of the account, each a row that can be left.
    fn address_list(&self) -> AnyElement {
        div()
            .debug_selector(|| "TEST_ETHEREUM_WALLET_ADDRESS_LIST".into())
            .size_full()
            .min_h_0()
            .child(self.list.clone())
            .into_any_element()
    }

    /// The way to read the addresses again, for a read that was answered with a failure.
    fn retry_button(&self, dialog: &Entity<Self>, cx: &Context<Self>) -> AnyElement {
        Button::new("ethereum_wallet-monitor-retry")
            .outline()
            .small()
            .aria_label(t!(cx, "common.retry"))
            .on_click({
                let dialog = dialog.clone();
                move |_, _, cx| {
                    dialog.update(cx, |dialog, cx| dialog.reload(cx));
                }
            })
            .child(t!(cx, "common.retry"))
            .into_any_element()
    }

    /// The way out of the dialog.
    ///
    /// The control closes the dialog by emitting, which is the dialog saying it is done to the
    /// layer that hosts it: it needs no window to hand and no way of being the dialog on top.
    fn footer(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .child(Separator::horizontal())
            .child(
                div().w_full().flex().items_center().justify_end().child(
                    Button::new("ethereum_wallet-monitor-close")
                        .tonal()
                        .medium()
                        .debug_selector(|| "TEST_ETHEREUM_WALLET_DIALOG_CLOSE".into())
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
                        .child(t!(cx, "common.close")),
                ),
            )
            .into_any_element()
    }
}

impl Render for EthereumWalletDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(pane) = self.widget.upgrade() else {
            return div().into_any_element();
        };
        // Everything the dialog draws is the pane's own, read once for the frame.
        let state = pane.read(cx).dialog_state();
        // The body's sentence is read into this dialog's document before anything is drawn, so
        // the frame that shows a different one is the frame that parsed it.
        self.resolve_message(&state, cx);
        let dialog = cx.entity();
        let theme = cx.theme();

        div()
            .w(px(DIALOG_WIDTH))
            .h(px(DIALOG_HEIGHT))
            .flex()
            .flex_col()
            .gap_2()
            .child(self.header(&state, &dialog, cx))
            .child(self.toolbar(&state, &dialog, cx))
            .children(self.notice(&state, cx))
            .child(
                div()
                    .min_w_0()
                    .min_h_0()
                    .flex_1()
                    .overflow_hidden()
                    .rounded_sm()
                    .border_1()
                    .border_color(theme.roles.line.divider.hsla)
                    .bg(theme.roles.surface.raised.hsla)
                    .child(self.body(&state, &dialog, cx)),
            )
            .child(self.footer(cx))
            .into_any_element()
    }
}

/// The pane's own selection state, read once for the frame that draws it.
///
/// It is built by the pane, which is what holds the state: the dialog draws what it is handed
/// rather than reading the pane for each of its fields, and what a row reads again as it is
/// drawn goes back to the pane through `dialog_state`.
pub(super) struct DialogState {
    signed_in: bool,
    ready: bool,
    selections: Option<SelectionData>,
    operation: Option<SelectionOperation>,
    notice: Option<SelectionNotice>,
    recovery: WidgetAction,
}

impl DialogState {
    /// The state one pane is in, as the pane hands it over.
    pub(super) fn new(
        signed_in: bool,
        ready: bool,
        selections: Option<SelectionData>,
        operation: Option<SelectionOperation>,
        notice: Option<SelectionNotice>,
        recovery: WidgetAction,
    ) -> Self {
        Self {
            signed_in,
            ready,
            selections,
            operation,
            notice,
            recovery,
        }
    }

    /// Whether a change may be asked for right now.
    fn may_change(&self) -> bool {
        self.ready && self.selections.is_some() && self.operation.is_none()
    }

    /// The address a change would carry, when there is one that could be made.
    ///
    /// An address counts only when the account may monitor another: one that is already
    /// monitored, or that would be the address past the quota, is not a change the service
    /// would accept, so the control that sends it is disabled rather than left to fail.
    fn addable_address(&self, typed: Option<&SmolStr>) -> Option<SmolStr> {
        if !self.may_change() {
            return None;
        }
        let typed = canonical_address(typed?)?;
        ethereum_wallet_address(&typed)?;
        let data = self.selections.as_ref()?;
        if data.quota_full()
            || data
                .selections()
                .iter()
                .any(|current| current.address == typed)
        {
            return None;
        }
        Some(typed)
    }
}

/// The address a reader typed, as the address it names.
///
/// An Ethereum address is the same address in either case: a wallet writes one in mixed case
/// to carry a checksum over its digits, and the service holds the addresses it is asked about
/// in one case, so what is typed is folded to that case before anything reads it — a reader
/// pasting the address their wallet shows is pasting the address they meant. An address that
/// is not written in hex is left as it was typed rather than folded into something else.
fn canonical_address(typed: &str) -> Option<SmolStr> {
    let typed = typed.trim();
    if typed.is_empty() {
        return None;
    }
    Some(match typed.strip_prefix("0x") {
        Some(hex) => format_smolstr!("0x{}", hex.to_ascii_lowercase()),
        None => SmolStr::new(typed),
    })
}

/// One monitored address: which address it is, and what the account is doing with it.
///
/// The row is the surface the list draws an address on rather than a control of its own: nothing
/// about an address is asked for by clicking it, so the only thing the row carries that answers
/// the mouse is the trash that stops monitoring it.
///
/// The dialog is read for whether a change may be asked for rather than handed the answer, so
/// that a row drawn by the list reads what the pane holds now: a list draws its rows again on
/// every frame of the dialog, and what a row draws is the frame it is drawn in.
fn address_row(
    selection: &EthereumWalletSelection,
    dialog: &WeakEntity<EthereumWalletDialog>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let address = selection.address.clone();
    let enabled = dialog
        .upgrade()
        .is_some_and(|dialog| dialog.read_with(cx, |dialog, cx| dialog.may_change(cx)));
    let label = t!(cx, "widget.ethereum_wallet.delete_address");
    let delete = Button::new((
        ElementId::Name(SharedString::new_static("EthereumWalletAddressDelete")),
        selection.address.as_str(),
    ))
    .ghost()
    .small()
    .disabled(!enabled)
    .aria_label(label.clone())
    .with_tooltip(label)
    .on_click({
        let dialog = dialog.clone();
        move |_, _, cx| {
            let address = address.clone();
            let Some(dialog) = dialog.upgrade() else {
                return;
            };
            dialog.update(cx, |dialog, cx| dialog.delete(address, cx));
        }
    })
    .child(Icon::new(IconKind::Trash).size_3());

    Interactive::new((
        ElementId::Name(SharedString::new_static("EthereumWalletAddressRow")),
        selection.address.as_str(),
    ))
    .rounded_sm()
    .ghost()
    .medium()
    // The row is given the height every place of its list is laid out at rather than the height
    // of a control: a control is one line tall, and this row is an address with what the account
    // is doing with it below it, so a row sized as a control is a row its own content overflows.
    .h(px(ADDRESS_ROW_HEIGHT))
    .debug_selector(|| "TEST_ETHEREUM_WALLET_ADDRESS_ROW".into())
    .child(
        div()
            .debug_selector(|| "TEST_ETHEREUM_WALLET_ADDRESS_ROW_CONTENT".into())
            .min_w_0()
            .flex_1()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.roles.fg.default.hsla)
                    .child(SharedString::from(selection.address.as_str())),
            )
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        Badge::new()
                            .solid()
                            .small()
                            .child(status_label(selection.status, cx)),
                    ),
            ),
    )
    .child(delete)
    .into_any_element()
}

fn status_label(status: EthereumWalletSelectionStatus, cx: &App) -> SharedString {
    match status {
        EthereumWalletSelectionStatus::Active => {
            t!(cx, "widget.ethereum_wallet.active")
        }
        EthereumWalletSelectionStatus::Paused => {
            t!(cx, "widget.ethereum_wallet.paused")
        }
    }
}

/// The recovery the pane itself offers, as the dialog's way out of having no stream.
fn recovery_button(action: &WidgetAction, cx: &App) -> AnyElement {
    Button::new("ethereum_wallet-monitor-recover")
        .outline()
        .small()
        .disabled(!action.is_enabled())
        .on_click({
            let action = action.clone();
            move |_, _, cx| action.run(cx)
        })
        .child(action.label(cx))
        .into_any_element()
}

/// The addresses are being read.
fn loading_addresses(cx: &Context<EthereumWalletDialog>) -> AnyElement {
    div()
        .id("ethereum_wallet-monitor-address-loading")
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .role(Role::ProgressIndicator)
        .aria_label(t!(cx, "widget.ethereum_wallet.loading_addresses"))
        .child(
            div()
                .size_5()
                .child(Spinner::new("ethereum_wallet-monitor-address-spinner")),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().roles.fg.muted.hsla)
                .child(t!(cx, "widget.ethereum_wallet.loading_addresses")),
        )
        .into_any_element()
}

/// A message and the control that offers a way out of it.
fn message_with_action(message: SharedString, action: AnyElement) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .px_4()
        .text_center()
        .text_xs()
        .child(message)
        .child(div().flex_none().child(action))
        .into_any_element()
}

/// A sentence standing in the middle of the body, drawn from the document it was read into.
///
/// The copy is catalogue copy, and the sentences of the catalogue are written as markdown — the
/// ones this dialog shares with the pane's own placeholder carry a `<p align="center">` tag pair
/// — so it is drawn by the markdown component rather than as the string it is written in. The
/// document is the dialog's own, which is what keeps the parse out of the frame that draws it.
fn centered_message(message: &Entity<MarkdownState>, color: Hsla) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .px_4()
        .text_center()
        .text_xs()
        .text_color(color)
        .child(Markdown::new(message))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use base_ui::{animation::ANIMATION_TICK, layer::Layer, size::UnifiedSize, style::theme};
    use gpui_kit::{AppContext, EmptyView, TestAppContext, VisualTestContext, WeakEntity};
    use kytrix_service_provider::service::ethereum_wallet::EthereumWalletSelection;

    use super::*;
    use crate::{
        test_support::{ACCOUNT, FakeSource, selection, selections, settle_until},
        view::tests::{asked_for, forget_last_change, live_pane},
    };

    const FIRST: &str = "0x1111111111111111111111111111111111111111";
    const SECOND: &str = "0x2222222222222222222222222222222222222222";

    /// One address, in the two cases a wallet may write it in: the mixed case is the checksum
    /// the address is displayed with, and the lowercase one is the address itself.
    const CHECKSUMMED: &str = "0x52908400098527886E0F7030069857D2E4169EE7";
    const CANONICAL: &str = "0x52908400098527886e0f7030069857d2e4169ee7";

    /// `SECOND` with the whitespace a paste can bring along.
    const SPACED_SECOND: &str = "  0x2222222222222222222222222222222222222222  ";

    /// A Solana address, which is base58 and therefore not one this pane accepts.
    const SOLANA: &str = "So11111111111111111111111111111111111111112";

    /// The state of a pane that is live, holds `addresses`, and is waiting for nothing.
    fn state(addresses: Vec<EthereumWalletSelection>, limit: usize) -> DialogState {
        DialogState::new(
            true,
            true,
            Some(SelectionData::new(selections(addresses, limit))),
            None,
            None,
            WidgetAction::new("common.retry", |_: &mut App| {}),
        )
    }

    /// A window whose root is the layer an application mounts, which is what a dialog is
    /// opened on and dismissed from.
    fn harness(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(|cx| {
            theme::install(cx);
            i18n::global::embedded::install(cx).expect("the embedded locales should load");
        });
        let (_, cx) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| EmptyView)));
        cx.run_until_parked();

        cx
    }

    /// Lets the dialog's own transition finish and repaints, so that one just opened is past
    /// its first frames and one just dismissed is gone rather than still fading out.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    #[test]
    fn an_address_should_be_read_as_the_address_it_names() {
        assert_eq!(
            ethereum_wallet_address(CHECKSUMMED),
            None,
            "the checksum case is not the case the service holds an address in"
        );
        assert_eq!(
            ethereum_wallet_address(SOLANA),
            None,
            "a Solana address is not an Ethereum one"
        );

        assert_eq!(
            canonical_address(CHECKSUMMED),
            Some(SmolStr::new_static(CANONICAL)),
            "an Ethereum address is the address it names in either case"
        );
        assert_eq!(
            canonical_address(CANONICAL),
            Some(SmolStr::new_static(CANONICAL)),
            "an address already written the way the service holds it is left alone"
        );
        assert_eq!(
            canonical_address(SPACED_SECOND),
            Some(SmolStr::new_static(SECOND)),
            "whitespace around an address is not part of one"
        );
        assert_eq!(
            canonical_address(SOLANA),
            Some(SmolStr::new_static(SOLANA)),
            "an address that is not written in hex is left as it was typed"
        );
        assert_eq!(canonical_address(""), None);
        assert_eq!(canonical_address("   "), None);
    }

    #[test]
    fn an_address_should_only_be_offered_when_it_could_be_monitored() {
        let state = state(vec![selection(FIRST)], 3);

        assert!(state.may_change());
        assert_eq!(
            state.addable_address(Some(&SmolStr::new_static(SECOND))),
            Some(SmolStr::new_static(SECOND)),
            "an address the account does not monitor is one it may"
        );
        assert_eq!(
            state.addable_address(Some(&SmolStr::new_static(SPACED_SECOND))),
            Some(SmolStr::new_static(SECOND)),
            "an address pasted with its whitespace is the address in it"
        );
        assert_eq!(
            state.addable_address(Some(&SmolStr::new_static(FIRST))),
            None,
            "an address already on the list is not a change"
        );
        assert_eq!(
            state.addable_address(Some(&SmolStr::new_static("0x1234"))),
            None,
            "a partial address is not one"
        );
        assert_eq!(state.addable_address(Some(&SmolStr::new_static(""))), None);
        assert_eq!(state.addable_address(None), None);
    }

    #[test]
    fn a_full_quota_should_refuse_another_address() {
        let state = state(vec![selection(FIRST)], 1);

        assert_eq!(
            state.addable_address(Some(&SmolStr::new_static(SECOND))),
            None,
            "the address past the quota is not one the account may monitor"
        );
        assert!(
            state.may_change(),
            "a full quota is not a reason to refuse leaving one"
        );
    }

    /// The dialog a pane holds, attached the way the pane attaches the one it opens.
    ///
    /// `show` needs the window layer the application installs, and what these tests are about is
    /// what the dialog draws and asks the pane for rather than how it was opened.
    fn attach(
        pane: &Entity<EthereumWalletWidget>,
        cx: &mut TestAppContext,
    ) -> Entity<EthereumWalletDialog> {
        let window = cx.add_empty_window();
        let dialog = window.update(|window, cx| {
            let address = cx.new(|cx| InputStateBuilder::new(ADDRESS_INPUT_ID).build(window, cx));
            cx.new(|cx| EthereumWalletDialog::new(pane.downgrade(), address, cx))
        });
        pane.update(cx, |pane, cx| pane.attach_dialog(dialog.downgrade(), cx));

        dialog
    }

    /// Waits for the read the pane sends when it takes a dialog over, which is the one thing
    /// that stands between opening a dialog and being able to ask it for a change.
    fn settle_attached(pane: &Entity<EthereumWalletWidget>, cx: &mut TestAppContext) {
        settle_until(cx, "the read the dialog asked for to be answered", |cx| {
            pane.read(cx).dialog_state().operation.is_none()
        });
    }

    /// A change is asked for from the dialog and made by the pane, which then has to repaint
    /// the dialog — from inside a frame the dialog is already being updated for, which is the
    /// one time an entity may not be asked to update itself.
    ///
    /// Both ways of asking are here: the control beside the field, and the field's own Enter.
    #[gpui_kit::test]
    fn adding_an_address_should_not_re_enter_the_dialog_that_asked_for_it(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(cx, &source, &session, vec![selection(FIRST)], 3);
        // The read the pane sends when it takes the dialog over, answered like the one before it.
        session.answer_selections(selections(vec![selection(FIRST)], 3));
        let dialog = attach(&pane, cx);
        settle_attached(&pane, cx);
        let field = cx.update(|cx| dialog.read(cx).address.clone());

        // What the control beside the field does when it is clicked.
        cx.update(|cx| {
            dialog.update(cx, |dialog, _| {
                dialog.typed = Some(SmolStr::new_static(CHECKSUMMED));
            });
            dialog.update(cx, |dialog, cx| dialog.add(cx));
        });
        cx.update(|cx| {
            assert_eq!(
                asked_for(&pane, cx).as_deref(),
                Some(CANONICAL),
                "the address the ethereum_wallet wrote in mixed case is the one that was asked for"
            );
        });

        // What the field does when the same address is confirmed with Enter, which reaches
        // the pane without a control to have been disabled.
        cx.update(|cx| {
            forget_last_change(&pane, cx);
            field.update(cx, |_, cx| {
                cx.emit(InputStateEvent::Change(SharedString::new_static(
                    CHECKSUMMED,
                )));
                cx.emit(InputStateEvent::PressEnter(SharedString::new_static(
                    CHECKSUMMED,
                )));
            });
        });
        cx.update(|cx| {
            assert_eq!(
                asked_for(&pane, cx).as_deref(),
                Some(CANONICAL),
                "the field answers Enter with the address it holds"
            );
        });
    }

    /// Leaving an address is asked for the same way, so it repaints the dialog the same way.
    #[gpui_kit::test]
    fn removing_an_address_should_not_re_enter_the_dialog_that_asked_for_it(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(cx, &source, &session, vec![selection(FIRST)], 3);
        session.answer_selections(selections(vec![selection(FIRST)], 3));
        let dialog = attach(&pane, cx);
        settle_attached(&pane, cx);

        // What the control beside an address on the list does when it is clicked.
        cx.update(|cx| {
            dialog.update(cx, |dialog, cx| {
                dialog.delete(SmolStr::new_static(FIRST), cx);
            });
        });
        cx.update(|cx| {
            assert_eq!(
                asked_for(&pane, cx).as_deref(),
                Some(FIRST),
                "the address of the row that was clicked is the one that was asked to leave"
            );
        });
    }

    /// The dialog draws the addresses the pane holds — each one a row of the height its own lines
    /// are laid out at — and the toolbar that adds one is drawn at the size of the field beside it.
    #[gpui_kit::test]
    fn a_dialog_should_draw_the_addresses_the_pane_holds(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let pane = live_pane(cx, &source, &session, vec![selection(FIRST)], 3);
        session.answer_selections(selections(vec![selection(FIRST)], 3));
        let dialog = attach(&pane, cx);
        settle_attached(&pane, cx);

        let (_, window) = cx.add_window_view({
            let dialog = dialog.clone();
            move |_, _| DialogHarness { dialog }
        });
        window.run_until_parked();
        assert!(
            window
                .debug_bounds("TEST_ETHEREUM_WALLET_ADDRESS_LIST")
                .is_some(),
            "the dialog should draw the addresses the pane holds"
        );
        let row = window
            .debug_bounds("TEST_ETHEREUM_WALLET_ADDRESS_ROW")
            .expect("a row is drawn from the pane while the list draws it");
        let content = window
            .debug_bounds("TEST_ETHEREUM_WALLET_ADDRESS_ROW_CONTENT")
            .expect("the row draws the address and what the account is doing with it");
        assert_eq!(
            row.size.height,
            px(ADDRESS_ROW_HEIGHT),
            "a row is drawn at the height its own lines are laid out at"
        );
        assert!(
            row.size.height > UnifiedSize::Medium.h(),
            "a row carries more than the one line a control is tall"
        );
        assert!(
            content.top() >= row.top() && content.bottom() <= row.bottom(),
            "what a row holds is drawn inside it (row {:?}, content {:?})",
            row,
            content
        );
        let shown = window.update(|_, cx| {
            dialog.read_with(cx, |dialog, _| {
                dialog.shown.as_ref().map(|shown| {
                    shown
                        .iter()
                        .map(|selection| selection.address.clone())
                        .collect::<Vec<_>>()
                })
            })
        });
        assert_eq!(
            shown,
            Some(vec![SmolStr::new_static(FIRST)]),
            "the addresses the pane holds should be the ones the list is given"
        );

        let field = window
            .debug_bounds("TEST_ETHEREUM_WALLET_ADDRESS_FIELD")
            .expect("the toolbar should draw the field an address is typed into");
        let add = window
            .debug_bounds("TEST_ETHEREUM_WALLET_ADD_ADDRESS")
            .expect("the toolbar should draw the control that adds what the field holds");
        assert_eq!(
            add.size.height,
            UnifiedSize::Medium.h(),
            "the control that adds an address is drawn at the size the field beside it is"
        );
        assert!(
            field.size.height >= UnifiedSize::Medium.h(),
            "the field is drawn at that size too, inside its own border (field {:?}, control {:?})",
            field.size,
            add.size
        );
        assert_eq!(
            add.size.width, add.size.height,
            "the control is its icon and nothing else, so it is as wide as it is tall"
        );
    }

    /// A frame of its own for a dialog, which the window layer would otherwise hold.
    struct DialogHarness {
        dialog: Entity<EthereumWalletDialog>,
    }

    impl Render for DialogHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.dialog.clone())
        }
    }

    /// The dialog owns its own state, so the layer releasing the dialog is the whole of the
    /// cleanup: nothing outside it has to take its field, its list or what it was showing down.
    ///
    /// The dialog closes itself through the event the layer subscribes to, which is the path a
    /// control of its own takes. The pane is not part of this: what a dialog owns is its own
    /// whether or not a pane is still there, and the dialog draws nothing without one.
    #[gpui_kit::test]
    fn a_dialog_should_release_its_own_state_when_it_closes_itself(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let (dialog, field, list) = cx.update(|window, cx| {
            let field = cx.new(|cx| InputStateBuilder::new(ADDRESS_INPUT_ID).build(window, cx));
            let dialog = cx
                .new(|cx| EthereumWalletDialog::new(WeakEntity::new_invalid(), field.clone(), cx));
            let list = dialog.read(cx).list.clone();

            window.dialog(cx, dialog.clone());

            (dialog, field, list)
        });
        let (content, field_weak, list_weak) =
            (dialog.downgrade(), field.downgrade(), list.downgrade());
        settle(cx);

        assert!(
            content.upgrade().is_some()
                && field_weak.upgrade().is_some()
                && list_weak.upgrade().is_some(),
            "the dialog and what it owns should be alive while it is open"
        );

        // The dialog saying it is done, which is what its own control does by emitting. The
        // handles the test holds go with it: the layer is what owns the dialog, and a handle
        // kept here would be the very reference the assertions below are about.
        cx.update(|_, cx| dialog.update(cx, |_, cx| cx.emit(DismissEvent)));
        drop((dialog, field, list));
        // The exit transition is drawn over several frames, and the layer drops the dialog on
        // the first one past it: this waits for that rather than guessing which frame it is.
        for _ in 0..4 {
            if content.upgrade().is_none() {
                break;
            }
            settle(cx);
        }

        assert!(
            content.upgrade().is_none(),
            "the dialog should have been released once it left the layer"
        );
        assert!(
            field_weak.upgrade().is_none(),
            "the field the dialog owns should be released with the dialog that holds it"
        );
        assert!(
            list_weak.upgrade().is_none(),
            "the list the dialog owns should be released with the dialog that holds it"
        );
    }
}

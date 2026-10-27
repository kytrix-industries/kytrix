//! The dialog one pane opens to manage the wallets it monitors.
//!
//! # What the dialog owns, and what it does not
//!
//! It owns what the user is typing, the error the last submission produced, and the place a
//! reader is in the list. Everything else — which wallets are followed, and whether one more may
//! be added — belongs to the pane, so the dialog draws the configuration rather than a copy of it
//! and asks the pane to change it.
//!
//! # Why there is nothing to choose but an address
//!
//! Which of the venue's partitions a wallet is read from is the venue's answer rather than the
//! user's choice, so the dialog offers one decision — add this address, or
//! stop following that one — and the pane's list of addresses is its whole content.
//!
//! # Why adding does not clear the field
//!
//! The address field is the user's, and the component that draws it has no way to write a value
//! into it. Nothing is lost by leaving it: a new address appears in the list under the field,
//! which disables the control that would add it again.

use base_ui::{
    button::Button,
    icon::{Icon, IconKind},
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    interactive::{Interactive, InteractiveVariantExt},
    layer::LayerExt,
    separator::Separator,
    size::UnifiedSizable,
    static_sized_list::StaticSizedList,
    style::theme::ThemeExt,
    tooltip::TooltipExt,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, DismissEvent, ElementId, Entity, EventEmitter,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, WeakEntity, Window, base::Disableable, div, px,
};
use i18n::t;
use market_participants_model::identifier::WalletAddress;
use smol_str::SmolStr;
use widget::error::LocalizedError;

use super::{view::HyperliquidWalletMonitorWidget, watch::MAX_WALLET_ADDRESSES};

/// The size of the dialog's panel.
pub(super) const DIALOG_WIDTH: f32 = 620.0;
pub(super) const DIALOG_HEIGHT: f32 = 480.0;

/// The address field of the dialog.
const ADDRESS_INPUT_ID: &str = "hyperliquid-wallet-address";
/// The list the dialog draws the monitored addresses with.
const ADDRESS_LIST_ID: &str = "hyperliquid-wallet-address-list";

/// Why an address was not monitored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AddressDialogError {
    /// What was typed is not a wallet address.
    Invalid,
    /// The address is on the list already.
    Duplicate,
    /// The list holds as many addresses as it may.
    Limit,
}

impl LocalizedError for AddressDialogError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Invalid => t!(cx, "widget.hyperliquid_wallet_monitor.invalid_address"),
            Self::Duplicate => t!(cx, "widget.hyperliquid_wallet_monitor.already_monitored"),
            Self::Limit => t!(cx, "widget.hyperliquid_wallet_monitor.limit_reached"),
        }
    }
}

/// The dialog a pane opens to manage the wallets it monitors.
pub(super) struct WalletAddressDialog {
    pane: WeakEntity<HyperliquidWalletMonitorWidget>,
    address: Entity<InputState>,
    /// What the field holds.
    ///
    /// The input element is rebuilt every frame and its state exposes no value, so what is
    /// typed is followed through the events the input emits rather than read back off it.
    typed: Option<SmolStr>,
    error: Option<AddressDialogError>,
    list: Entity<StaticSizedList<WalletAddress>>,
    /// The addresses the list was last told to draw, which is how a frame knows whether what the
    /// pane holds is something the list has not been shown.
    shown: Option<Vec<WalletAddress>>,
}

/// The dialog says it is done by emitting [`DismissEvent`], which is the signal the layer that
/// hosts it subscribes to: it needs no window to be handed, and a dismissal taken from the
/// window would close whatever happens to be the dialog on top rather than this one.
impl EventEmitter<DismissEvent> for WalletAddressDialog {}

impl WalletAddressDialog {
    /// Opens the dialog over the window, for the pane the control belongs to.
    ///
    /// Nothing happens when it is already open: the control is the dialog's only entrance, and
    /// replacing the open one would throw away what the user has typed.
    pub(super) fn show(
        pane: WeakEntity<HyperliquidWalletMonitorWidget>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(entity) = pane.upgrade() else {
            return;
        };
        if entity.read(cx).dialog_open() {
            return;
        }
        let address = cx.new(|cx| {
            InputStateBuilder::new(ADDRESS_INPUT_ID)
                .placeholder(t!(cx, "widget.hyperliquid_wallet_monitor.placeholder"))
                .build(window, cx)
        });
        let dialog = cx.new(|cx| Self::new(pane.clone(), address, cx));
        // The pane's handle is taken before the entity is handed to the layer, which holds the
        // only strong reference to it from here: a handle kept past this call would be a second
        // owner, and the dialog — with the state it owns — would outlive its own closing.
        let handle = dialog.downgrade();
        window.dialog(cx, dialog);
        entity.update(cx, |pane, cx| {
            pane.attach_dialog(handle, cx);
        });
    }

    /// The dialog a pane holds.
    ///
    /// The pane's own tests build one this way too, to answer it the way a submission does.
    pub(super) fn new(
        pane: WeakEntity<HyperliquidWalletMonitorWidget>,
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
                        // Typing is not a submission, so a failure of the last one is no longer
                        // about anything on screen.
                        dialog.error = None;
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

        // The list is built once and told what to draw, so a row is given the dialog rather than
        // the state of the frame it is drawn in: what a row does is ask the dialog to change the
        // configuration it draws.
        let dialog = cx.entity().downgrade();
        let list = cx.new(|_| {
            StaticSizedList::new(
                ElementId::Name(SharedString::new_static(ADDRESS_LIST_ID)),
                move |_index, address: &WalletAddress, _window, cx| {
                    address_row(address, &dialog, cx)
                },
            )
        });

        Self {
            pane,
            address,
            typed: None,
            error: None,
            list,
            shown: None,
        }
    }

    /// The addresses the pane follows, as the dialog draws them.
    fn followed(&self, cx: &App) -> Vec<WalletAddress> {
        self.pane
            .upgrade()
            .map(|pane| pane.read(cx).addresses().to_vec())
            .unwrap_or_default()
    }

    /// Monitors what the field holds.
    ///
    /// The control that sends it is disabled for anything unusable, but the field also answers
    /// Enter, which reaches here without a control to have been disabled.
    fn add(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.pane.upgrade() else {
            return;
        };
        let Some(address) = parsed_address(self.typed.as_deref()) else {
            if self.typed.is_some() {
                self.error = Some(AddressDialogError::Invalid);
            }
            cx.notify();
            return;
        };
        let result = pane.update(cx, |pane, cx| pane.add_address(address, cx));
        match result {
            Ok(()) => self.error = None,
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    /// Stops monitoring one address.
    fn remove(&mut self, address: WalletAddress, cx: &mut Context<Self>) {
        let Some(pane) = self.pane.upgrade() else {
            return;
        };
        pane.update(cx, |pane, cx| pane.remove_address(&address, cx));
        self.error = None;
        cx.notify();
    }

    /// Whether what is typed could be monitored.
    ///
    /// An address counts only when the pane may follow another: one that is already on the list,
    /// or that would be the address past the limit, is not a change the pane would make, so the
    /// control that sends it is disabled rather than left to fail.
    fn addable(&self, followed: &[WalletAddress]) -> Option<WalletAddress> {
        let typed = self.typed.clone()?;
        let address = parsed_address(Some(typed.as_str()))?;
        if followed.len() >= MAX_WALLET_ADDRESSES || followed.contains(&address) {
            return None;
        }
        Some(address)
    }

    fn header(&self, count: usize, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex_none()
            .min_w_0()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .child(t!(cx, "widget.hyperliquid_wallet_monitor.manage_addresses")),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.roles.fg.muted.hsla)
                    .child(t!(
                        cx,
                        "widget.hyperliquid_wallet_monitor.monitored_count",
                        current = count,
                        limit = MAX_WALLET_ADDRESSES
                    )),
            )
    }

    fn toolbar(
        &self,
        followed: &[WalletAddress],
        dialog: &Entity<Self>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let add = Button::new("hyperliquid-wallet-address-add")
            .filled()
            .medium()
            .disabled(self.addable(followed).is_none())
            .aria_label(t!(cx, "widget.hyperliquid_wallet_monitor.add"))
            .with_tooltip(t!(cx, "widget.hyperliquid_wallet_monitor.add"))
            .on_click({
                let dialog = dialog.clone();
                move |_, _, cx| dialog.update(cx, |dialog, cx| dialog.add(cx))
            })
            .child(Icon::new(IconKind::Plus).size_4());

        div()
            .flex_none()
            .min_w_0()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .child(Input::new(&self.address).medium().clearable(true)),
            )
            .child(div().flex_none().child(add))
    }

    fn notice(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let error = self.error.as_ref()?;
        let theme = cx.theme();
        Some(
            div()
                .flex_none()
                .min_w_0()
                .w_full()
                .truncate()
                .text_xs()
                .text_color(theme.roles.status.danger.base.hsla)
                .child(error.localized_message(cx)),
        )
    }

    fn address_list(&self) -> impl IntoElement {
        div()
            .debug_selector(|| "TEST_HYPERLIQUID_WALLET_ADDRESS_LIST".into())
            .size_full()
            .min_h_0()
            .child(self.list.clone())
    }

    /// Shows the addresses the pane follows, if they are not the ones the list holds.
    ///
    /// A list draws what it was given and remembers where the reader is, so it is told about
    /// another list rather than rebuilt every frame: being told would put the reader back at the
    /// top of a list that had not changed.
    fn show_addresses(&mut self, addresses: Vec<WalletAddress>, cx: &mut Context<Self>) {
        if self.shown.as_ref() == Some(&addresses) {
            return;
        }
        self.shown = Some(addresses.clone());
        self.list.update(cx, |list, cx| {
            list.replace(addresses);
            cx.notify();
        });
    }

    /// The dialog's own empty list, which says nothing about where to add a wallet.
    ///
    /// The pane's placeholder uses `empty_addresses`, and that copy points at the settings control
    /// in the top-left corner — which is the control that opened *this* dialog. Reusing it here
    /// would tell a reader who is already inside that dialog to go and open it.
    fn body(&self, addresses: &[WalletAddress], cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if addresses.is_empty() {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .px_4()
                .text_center()
                .text_xs()
                .text_color(theme.roles.fg.muted.hsla)
                .child(t!(cx, "widget.hyperliquid_wallet_monitor.empty_monitored"))
                .into_any_element();
        }
        self.address_list().into_any_element()
    }

    /// The way out of the dialog.
    ///
    /// The control closes the dialog by emitting, which is the dialog saying it is done to the
    /// layer that hosts it: it needs no window to hand and no way of being the dialog on top.
    fn footer(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .flex_none()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(Separator::horizontal())
            .child(
                div().w_full().flex().items_center().justify_end().child(
                    Button::new("hyperliquid-wallet-dialog-close")
                        .tonal()
                        .medium()
                        .debug_selector(|| "TEST_HYPERLIQUID_WALLET_DIALOG_CLOSE".into())
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
                        .child(t!(cx, "common.close")),
                ),
            )
    }
}

impl Render for WalletAddressDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The pane owns the list, so the dialog asks the pane it was opened from rather than
        // reading a setting every other pane also writes.
        let addresses = self.followed(cx);
        self.show_addresses(addresses.clone(), cx);
        let count = addresses.len();
        let dialog = cx.entity();
        let theme = cx.theme();

        div()
            .w(px(DIALOG_WIDTH))
            .h(px(DIALOG_HEIGHT))
            .flex()
            .flex_col()
            .gap_2()
            .child(self.header(count, cx))
            .child(self.toolbar(&addresses, &dialog, cx))
            .children(self.notice(cx))
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
                    .child(self.body(&addresses, cx)),
            )
            .child(self.footer(cx))
    }
}

/// One monitored address, and the control that stops monitoring it.
fn address_row(
    address: &WalletAddress,
    dialog: &WeakEntity<WalletAddressDialog>,
    cx: &App,
) -> AnyElement {
    let label = t!(
        cx,
        "widget.hyperliquid_wallet_monitor.remove_address",
        address = SharedString::new(address.as_str())
    );
    let remove = Button::new((
        ElementId::Name(SharedString::new_static("HyperliquidWalletAddressRemove")),
        address.as_str(),
    ))
    .ghost()
    .small()
    .aria_label(label.clone())
    .with_tooltip(label)
    .on_click({
        let dialog = dialog.clone();
        let address = address.clone();
        move |_, _, cx| {
            let Some(dialog) = dialog.upgrade() else {
                return;
            };
            dialog.update(cx, |dialog, cx| dialog.remove(address.clone(), cx));
        }
    })
    .child(Icon::new(IconKind::Trash).size_3());

    Interactive::new((
        ElementId::Name(SharedString::new_static("HyperliquidWalletAddressRow")),
        address.as_str(),
    ))
    .rounded_sm()
    .ghost()
    .medium()
    .child(
        div()
            .debug_selector(|| "TEST_HYPERLIQUID_WALLET_ADDRESS_ROW".into())
            .min_w_0()
            .flex_1()
            .truncate()
            .child(SharedString::new(address.as_str())),
    )
    .child(remove)
    .into_any_element()
}

/// A wallet address as a person types it: forty hexadecimal digits, with or without the prefix.
///
/// The venue writes an address with `0x`, and a person pasting one may or may not: what is
/// refused is anything that is not forty hexadecimal digits, which is a shape rather than a
/// checksum.
fn parsed_address(typed: Option<&str>) -> Option<WalletAddress> {
    let typed = typed?.trim();
    let digits = typed
        .strip_prefix("0x")
        .or_else(|| typed.strip_prefix("0X"))
        .unwrap_or(typed);
    if digits.len() != 40 || !digits.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    format!("0x{digits}").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_read_with_or_without_its_prefix() {
        let digits = "1111111111111111111111111111111111111111";
        let expected = format!("0x{digits}")
            .parse::<WalletAddress>()
            .expect("the test address should be valid");

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
}

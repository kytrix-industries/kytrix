//! What a sign-in says while it happens, and the one question a sign-out asks.
//!
//! A sign-in is finished in the browser, so all three things the desktop half has to say
//! about it are reports: the callback is being waited for, the account came back, it did
//! not. Each is one alert — a title, a message and the button that says the message was
//! read — because none of them asks anything: what ends the wait is the callback the
//! browser delivers, or the attempt's own expiry. The one about the wait carries the way
//! out of it as well, because an alert is the only thing the user has to answer while the
//! attempt runs: its button is the cancellation, and reading it ends what it reports.
//!
//! Signing out is the one question here, and it is the same alert with a cancel entry: it
//! is the only thing on this menu with two answers, and its ok and cancel buttons are how
//! both of them are given.

use base_ui::layer::LayerExt;
use gpui_kit::*;
use i18n::t;
use tokio::task::AbortHandle;

/// Shows the alert that says the sign-in has to be finished in the browser.
///
/// The alert is the whole of what the user can do about the wait, so it is also what ends
/// it: its one button says the report was read, and reading it aborts `sign_in` — the
/// attempt this alert is the report of. That handle is the only thing it knows about the
/// attempt, which is why the way to end one is handed to it rather than reached for.
///
/// Ending the attempt there is what keeps the menu honest: an alert that could be
/// dismissed while the attempt behind it stayed open would be a menu offering a sign-in
/// that does nothing, for as long as the abandoned attempt lived.
pub(crate) fn show_awaiting_authorization(window: &mut Window, cx: &mut App, sign_in: AbortHandle) {
    let title = t!(cx, "window.account.sign_in");
    let message = t!(cx, "window.account.awaiting_authorization");
    let cancel = t!(cx, "window.account.cancel_sign_in");

    window.alert(cx, move |alert, _, _| {
        alert
            .ok(title, message)
            .ok_text(cancel)
            .ok_action(move |_, _, _| sign_in.abort())
    });
}

/// Shows why the sign-in the user was just watching did not finish.
///
/// It replaces the alert that was waiting for the browser rather than closing it and
/// reporting somewhere else: that alert is where the user is looking, and a failure that
/// has to be found by opening a menu is a failure the user does not see. The attempt is
/// over by the time this is drawn — the loopback port is already released — so there is
/// nothing left to do but read the reason.
pub(crate) fn show_authorization_failed(window: &mut Window, cx: &mut App, reason: SharedString) {
    let title = t!(cx, "window.account.sign_in");
    let confirm = t!(cx, "common.confirm");

    window.alert(cx, move |alert, _, _| {
        alert.ok(title, reason).ok_text(confirm)
    });
}

/// Shows the alert that reports the account that was just authorized.
///
/// The message is one line of copy — what the account is, the wallet family it belongs to,
/// and its address — and it is a message of its own rather than one assembled here, so that
/// the centring every other alert message carries is on this one too.
pub(crate) fn show_authorization_complete(
    window: &mut Window,
    cx: &mut App,
    wallet_address: SharedString,
    wallet_kind_label: SharedString,
) {
    let title = t!(cx, "window.account.authorization_complete");
    let message = t!(
        cx,
        "window.account.authorization_complete_message",
        wallet = wallet_address,
        kind = wallet_kind_label
    );
    let confirm = t!(cx, "common.confirm");

    window.alert(cx, move |alert, _, _| {
        alert.ok(title, message).ok_text(confirm)
    });
}

/// Asks whether the current account should be signed out.
///
/// The answer is the caller's to act on: a confirmation that signed out on its own
/// could not be reasoned about from the menu that opened it. Answering the alert is the
/// whole decision — the ok button is the sign-out and the cancel button is the way of
/// saying no — so nothing here ends the session.
pub(crate) fn show_sign_out_confirmation(
    window: &mut Window,
    cx: &mut App,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
) {
    let title = t!(cx, "window.account.sign_out");
    let message = t!(cx, "window.account.sign_out_confirm");
    let cancel = t!(cx, "common.cancel");

    window.alert(cx, move |alert, _, _| {
        alert
            .ok_cancel(title.clone(), message)
            .ok_text(title)
            .cancel_text(cancel)
            .ok_action(move |_, window, cx| on_confirm(window, cx))
    });
}

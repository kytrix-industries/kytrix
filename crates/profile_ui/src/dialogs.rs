//! What a sign-in says while it happens, and the one question a sign-out asks.
//!
//! A sign-in is finished in the browser, so all three things the desktop half has to say about it
//! are reports: the callback is being waited for, the account came back, it did not. Each is one
//! alert — a title, a message and the button that says the message was read — because none of
//! them asks anything: what ends the wait is the callback the browser delivers, or the attempt's
//! own expiry. The one about the wait carries the way out of it as well, because an alert is the
//! only thing the user has to answer while the attempt runs: its button is the cancellation, and
//! reading it ends what it reports.
//!
//! Signing out is the one question here, and it is the same alert with a cancel entry: it is the
//! only thing on this feature with two answers, and its ok and cancel buttons are how both of
//! them are given.
//!
//! # Where the words come from
//!
//! The title, the message and the button text are resolved by the caller, in the language the
//! moment is reported in — a failure that is still on screen when the language changes keeps the
//! words it was reported in (that is what the user is looking at). A report whose message is one
//! of the `error.account.*` sentences gets it from the failure's [`TranslatedError`]
//! implementation, never from a `Display` string.
//!
//! [`TranslatedError`]: widget::error::TranslatedError

use std::rc::Rc;

use base_ui::kit::{
    alert::AlertBuilder,
    dialog::{Dialog, DialogBuilder},
};
use smol_str::SmolStr;
use ui_core::app::App;

use crate::host::ProfileHost;

/// What one of an alert's buttons does besides closing it.
pub(crate) type AlertAction = Rc<dyn Fn(&mut App)>;

/// What an alert says and what its buttons do.
///
/// The buttons close the alert before they run their action: reading a report is the end of what
/// it reports, and a sign-out that closed its own confirmation afterwards would be answering a
/// question the user can still see.
pub(crate) enum AlertContent {
    /// One button that says the report was read.
    Report {
        /// The alert's title (what it is about).
        title: SmolStr,
        /// The sentence the report is made of.
        message: SmolStr,
        /// The text of the one button.
        confirm: SmolStr,
        /// What reading the report does besides closing it.
        on_confirm: Option<AlertAction>,
    },
    /// Two answers: the ok button agrees, the cancel button declines.
    Question {
        /// The alert's title (the question's subject).
        title: SmolStr,
        /// The sentence the question is asked in.
        message: SmolStr,
        /// The text of the agreeing button.
        confirm: SmolStr,
        /// The text of the declining button.
        cancel: SmolStr,
        /// What agreeing does.
        on_confirm: AlertAction,
    },
}

impl AlertContent {
    /// The title, which the dialog's own a11y name is declared with.
    fn title(&self) -> SmolStr {
        match self {
            Self::Report { title, .. } | Self::Question { title, .. } => title.clone(),
        }
    }
}

/// Builds the alert `content` describes (closed; the caller opens it).
///
/// `key` is the control's per-instance alert key: kit keys are application-level action
/// identities, so two controls in one application must not share one (`crates/widgets/AGENTS.md`
/// §1-11).
pub(crate) fn build(
    app: &mut App,
    host: &ProfileHost,
    key: SmolStr,
    content: AlertContent,
) -> Dialog {
    let content = Rc::new(content);
    let title = content.title();
    DialogBuilder::alert(title, move |dialog, _theme| {
        alert(dialog.clone(), Rc::clone(&content))
    })
    .build(
        app,
        host.theme().clone(),
        host.layer().clone(),
        host.animation(),
        key,
    )
}

/// The content of one alert: its sentence, its button texts and what the buttons do.
fn alert(dialog: Dialog, content: Rc<AlertContent>) -> AlertBuilder {
    match &*content {
        AlertContent::Report {
            title,
            message,
            confirm,
            on_confirm,
        } => {
            let action = on_confirm.clone();
            AlertBuilder::new()
                .ok(title.clone(), message.clone())
                .ok_text(confirm.clone())
                .ok_action(move |_, app| {
                    dialog.close(app);
                    if let Some(action) = &action {
                        action(app);
                    }
                })
        }
        AlertContent::Question {
            title,
            message,
            confirm,
            cancel,
            on_confirm,
        } => {
            let action = Rc::clone(on_confirm);
            let closing = dialog.clone();
            AlertBuilder::new()
                .ok_cancel(title.clone(), message.clone())
                .ok_text(confirm.clone())
                .cancel_text(cancel.clone())
                .ok_action(move |_, app| {
                    dialog.close(app);
                    action(app);
                })
                .cancel_action(move |app| {
                    closing.close(app);
                })
        }
    }
}

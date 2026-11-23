use std::rc::Rc;

use smol_str::SmolStr;
use ui_core::app::App;
use ui_kit::kit::{
    alert::AlertBuilder,
    dialog::{Dialog, DialogBuilder},
};

use crate::host::ProfileHost;

pub(crate) type AlertAction = Rc<dyn Fn(&mut App)>;

pub(crate) enum AlertContent {
    Report {
        title: SmolStr,
        message: SmolStr,
        confirm: SmolStr,
        on_confirm: Option<AlertAction>,
    },
    Question {
        title: SmolStr,
        message: SmolStr,
        confirm: SmolStr,
        cancel: SmolStr,
        on_confirm: AlertAction,
    },
}

impl AlertContent {
    fn title(&self) -> SmolStr {
        match self {
            Self::Report { title, .. } | Self::Question { title, .. } => title.clone(),
        }
    }
}

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

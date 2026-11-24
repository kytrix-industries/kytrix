use std::rc::Rc;

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    event::Event,
    math::px,
    node::signal::{Signal, SignalScope},
    window::WindowId,
};
use ui_kit::{
    kit::{
        alert::AlertBuilder,
        dialog::{Dialog, DialogBuilder},
        input::{self, InputSpec, InputState},
    },
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
};

use super::ui;
use crate::composition::Composition;

pub(crate) struct Dialogs {
    window: WindowId,
    theme: ThemeHandle,
    open: Vec<OpenDialog>,
    opened: u64,
}

struct OpenDialog {
    dialog: Dialog,
    scope: Option<SignalScope>,
}

pub(crate) struct AlertCopy {
    pub title: SmolStr,
    pub message: SmolStr,
    pub confirm: SmolStr,
    pub cancel: SmolStr,
}

pub(crate) struct NameAlert {
    pub title: SmolStr,
    pub message: SmolStr,
    pub placeholder: SmolStr,
    pub initial: SmolStr,
    pub confirm: SmolStr,
    pub cancel: SmolStr,
}

impl Dialogs {
    pub(crate) fn new(window: WindowId, composition: &Composition) -> Self {
        Self {
            window,
            theme: composition.theme.clone(),
            open: Vec::new(),
            opened: 0,
        }
    }

    fn next_key(&mut self) -> SmolStr {
        let key = format_smolstr!("kytrix.dialog.{}", self.opened);
        self.opened += 1;
        key
    }

    pub(crate) fn alert(
        &mut self,
        app: &mut App,
        composition: &Composition,
        title: impl Into<SmolStr>,
        message: impl Into<SmolStr>,
        confirm: impl Into<SmolStr>,
        on_confirm: impl Fn(&mut App) + 'static,
    ) {
        let theme = composition.theme.clone();
        let message: SmolStr = message.into();
        let confirmed: Rc<dyn Fn(&mut App)> = Rc::new(on_confirm);
        let builder = DialogBuilder::new(title).content(move |app, window, parent| {
            ui::label(
                app,
                window,
                parent,
                message.clone(),
                px(12.0),
                400,
                Role::FgDefault,
                &theme,
            )
            .map(|_| ())
        });
        let builder = builder.confirm(confirm, move |app| confirmed(app));
        self.open_builder(app, composition, builder, None);
    }

    pub(crate) fn alert_with_cancel(
        &mut self,
        app: &mut App,
        composition: &Composition,
        alert: AlertCopy,
        on_confirm: impl Fn(&mut App) + 'static,
    ) {
        let AlertCopy {
            title,
            message,
            confirm,
            cancel,
        } = alert;
        let theme = composition.theme.clone();
        let confirmed: Rc<dyn Fn(&mut App)> = Rc::new(on_confirm);
        let builder = DialogBuilder::new(title).content(move |app, window, parent| {
            ui::label(
                app,
                window,
                parent,
                message.clone(),
                px(12.0),
                400,
                Role::FgDefault,
                &theme,
            )
            .map(|_| ())
        });
        let builder = builder
            .confirm(confirm, move |app| confirmed(app))
            .cancel(cancel, |_| {});
        self.open_builder(app, composition, builder, None);
    }

    pub(crate) fn name_alert(
        &mut self,
        app: &mut App,
        composition: &Composition,
        alert: NameAlert,
        validate: impl Fn(&str) -> Result<(), SmolStr> + 'static,
        on_confirm: impl Fn(Option<SmolStr>, &mut App) + 'static,
    ) {
        let scope = app.signal_scope();
        let state: Signal<InputState> =
            app.signal_scoped(scope, InputState::new(alert.initial.as_str()));
        let placeholder: Signal<SmolStr> = app.signal_scoped(scope, alert.placeholder.clone());
        let spec = std::sync::Arc::new(InputSpec {
            name: SmolStr::new_static("shell.name-alert"),
            placeholder: alert.placeholder.clone(),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            size: ControlSize::Medium,
            submit: None,
        });
        let theme = composition.theme.clone();
        let field = move |tree: &mut ui_core::node::tree::NodeTree, parent| {
            input::build(
                tree,
                parent,
                std::sync::Arc::clone(&spec),
                &theme,
                &state,
                &placeholder,
            )
        };
        let validator: Rc<dyn Fn(&str) -> Result<(), SmolStr>> = Rc::new(validate);
        let confirmed: Rc<dyn Fn(Option<SmolStr>, &mut App)> = Rc::new(on_confirm);
        let NameAlert {
            title,
            message,
            confirm,
            cancel,
            ..
        } = alert;
        let builder = DialogBuilder::alert(title.clone(), move |dialog, _theme| {
            let dialog = dialog.clone();
            let validator = Rc::clone(&validator);
            let confirmed = Rc::clone(&confirmed);
            let field = field.clone();
            AlertBuilder::new()
                .input_ok_cancel(title.clone(), message.clone(), Rc::new(field))
                .ok_text(confirm.clone())
                .cancel_text(cancel.clone())
                .validate(move |value| validator(value))
                .ok_action({
                    let dialog = dialog.clone();
                    move |value, app| {
                        confirmed(value, app);
                        dialog.close(app);
                    }
                })
                .cancel_action(move |app| {
                    dialog.close(app);
                })
        });
        self.open_builder(app, composition, builder, Some(scope));
    }

    fn open_builder(
        &mut self,
        app: &mut App,
        composition: &Composition,
        builder: DialogBuilder,
        scope: Option<SignalScope>,
    ) {
        let key = self.next_key();
        let dialog = builder.build(
            app,
            self.theme.clone(),
            (*composition.layer).clone(),
            (*composition.animation_router).clone(),
            key,
        );
        if let Err(error) = dialog.open(app, self.window) {
            log::error!("a dialog could not be opened: {error}");
            if let Some(scope) = scope {
                app.release_scope(scope);
            }
            return;
        }
        self.open.push(OpenDialog { dialog, scope });
    }

    pub(crate) fn sync(&mut self, app: &mut App) {
        let mut index = 0;
        while index < self.open.len() {
            let dialog = self.open[index].dialog.clone();
            if let Err(error) = dialog.sync(app) {
                log::error!("a dialog could not be synchronized: {error}");
            }
            if !dialog.is_open(app) {
                let mut closed = self.open.remove(index);
                closed.dialog.release(app);
                if let Some(scope) = closed.scope.take() {
                    app.release_scope(scope);
                }
                continue;
            }
            index += 1;
        }
    }

    pub(crate) fn dispatch(&mut self, app: &mut App, event: &Event) {
        for open in &mut self.open {
            if let Some(button) = open.dialog.confirm_button() {
                let _ = button.dispatch(app, event);
            }
            if let Some(button) = open.dialog.cancel_button() {
                let _ = button.dispatch(app, event);
            }
            if let Some(button) = open.dialog.close_button() {
                let _ = button.dispatch(app, event);
            }
        }
    }

    pub(crate) fn release(&mut self, app: &mut App) {
        for mut open in std::mem::take(&mut self.open) {
            open.dialog.release(app);
            if let Some(scope) = open.scope.take() {
                app.release_scope(scope);
            }
        }
    }
}

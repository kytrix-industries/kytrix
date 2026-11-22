//! The dialogs the shell opens: the startup notice, and the workspace naming alerts.
//!
//! Every dialog is a kit dialog built on the application's one layer stack and animation
//! router, opened on demand and driven by this host once per frame. The host owns the
//! handles so a dialog's fade-out is synchronized until it has finished, and releases a
//! dialog's nodes and signal scope once it is closed — opening and closing alerts therefore
//! does not accumulate components.
//!
//! # Input alerts
//!
//! The workspace list asks for a name in an alert with one field. The field's state is
//! minted in a signal scope the dialog owns and released with it, not kept process-wide.

use std::rc::Rc;

use base_ui::{
    kit::{
        alert::AlertBuilder,
        dialog::{Dialog, DialogBuilder},
        input::{self, InputSpec, InputState},
    },
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    event::Event,
    node::signal::{Signal, SignalScope},
    window::WindowId,
};

use super::ui;
use crate::composition::Composition;

/// The dialogs one window has open.
pub(crate) struct Dialogs {
    window: WindowId,
    theme: ThemeHandle,
    /// The open dialogs and the scopes their input fields were minted in.
    open: Vec<OpenDialog>,
    /// How many dialogs have been opened (their keys must not repeat).
    opened: u64,
}

struct OpenDialog {
    dialog: Dialog,
    scope: Option<SignalScope>,
}

/// What a naming alert asks for.
pub(crate) struct NameAlert {
    pub title: SmolStr,
    pub message: SmolStr,
    pub placeholder: SmolStr,
    pub initial: SmolStr,
    pub confirm: SmolStr,
    pub cancel: SmolStr,
}

impl Dialogs {
    /// Builds the host over the composition root's theme.
    pub(crate) fn new(window: WindowId, composition: &Composition) -> Self {
        Self {
            window,
            theme: composition.theme.clone(),
            open: Vec::new(),
            opened: 0,
        }
    }

    /// The next dialog's identity.
    fn next_key(&mut self) -> SmolStr {
        let key = format_smolstr!("kytrix.dialog.{}", self.opened);
        self.opened += 1;
        key
    }

    /// Opens an alert with one paragraph and a single confirmation.
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
                12.0,
                400,
                Role::FgDefault,
                &theme,
            )
            .map(|_| ())
        });
        let builder = builder.confirm(confirm, move |app| confirmed(app));
        self.open_builder(app, composition, builder, None);
    }

    /// Opens an alert with one paragraph and a confirmation and a cancellation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn alert_with_cancel(
        &mut self,
        app: &mut App,
        composition: &Composition,
        title: impl Into<SmolStr>,
        message: impl Into<SmolStr>,
        confirm: impl Into<SmolStr>,
        cancel: impl Into<SmolStr>,
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
                12.0,
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

    /// Opens an alert with one input field: `on_confirm` receives what was typed once the
    /// field passed `validate`.
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

    /// Builds and opens one dialogue builder.
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

    /// Runs one frame of every open dialog, releasing the ones that closed.
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

    /// Forwards one platform event to the dialogs' kit controls.
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

    /// Releases every dialog.
    pub(crate) fn release(&mut self, app: &mut App) {
        for mut open in std::mem::take(&mut self.open) {
            open.dialog.release(app);
            if let Some(scope) = open.scope.take() {
                app.release_scope(scope);
            }
        }
    }
}

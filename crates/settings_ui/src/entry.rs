//! Where the settings are opened from.
//!
//! Opening the settings does not need a window of its own, because the settings are a dialog on
//! the window layer: opening them pushes them onto it, over whatever dialog was already there, and
//! dismissing them takes them off it again.
//!
//! What is opened is a session rather than the settings themselves: it starts from what is
//! stored, every control of the dialog changes the session, and the settings are written once
//! the user confirms them — see [`crate::draft`].

use app_config_provider::manager::ConfigManager;
use base_ui::layer::LayerExt;
use gpui_kit::{App, AppContext as _, Window};

use crate::{dialog::SettingsDialog, draft::SettingsDraft};

/// Presents the settings as a dialog on the window layer.
///
/// Opening the settings pushes the dialog, which is why this is called from one place: nothing has
/// to bring the settings back, because dismissing the dialog opened over them — the form that
/// changes the proxy catalog — uncovers them where they are. See
/// [`crate::editor::ProxyEditor`].
pub fn show_settings(window: &mut Window, cx: &mut App) {
    let draft = cx.new(|cx| {
        let stored = cx.global::<ConfigManager>().read();

        SettingsDraft::new(stored.as_ref().clone())
    });
    let dialog = cx.new(|cx| SettingsDialog::new(draft, cx));

    window.dialog(cx, dialog);
}

use super::App;
#[cfg(feature = "hud")]
use crate::debug_hud;
#[cfg(feature = "hud")]
use crate::debug_hud::DebugHudState;
use crate::{error::EngineError, window::WindowId};

#[cfg(feature = "hud")]
pub(crate) fn set_debug_hud(
    app: &mut App,
    window: WindowId,
    enabled: bool,
) -> Result<(), EngineError> {
    if !app.windows.contains_key(&window) {
        return Err(EngineError::UnknownWindow { window });
    }
    let mut changed = false;
    if enabled {
        if let Some(root) = app.debug_hud.get(&window).map(DebugHudState::root) {
            let valid = app
                .windows
                .get(&window)
                .is_some_and(|target| target.tree().get(root).is_ok());
            if valid {
                return Ok(());
            }
            app.debug_hud.remove(&window);
        }
        let Some(target) = app.windows.get_mut(&window) else {
            return Err(EngineError::UnknownWindow { window });
        };
        let state = debug_hud::install(target, app.clock)?;
        app.debug_hud.insert(window, state);
        changed = true;
    } else if let Some(state) = app.debug_hud.remove(&window) {
        let _ = app.remove_node(window, state.root());
        changed = true;
    }
    if changed {
        app.request_frame();
    }
    Ok(())
}

#[cfg(feature = "hud")]
#[must_use]
pub(crate) fn debug_hud_enabled(app: &App, window: WindowId) -> bool {
    let Some(state) = app.debug_hud.get(&window) else {
        return false;
    };
    app.windows
        .get(&window)
        .is_some_and(|target| target.tree().get(state.root()).is_ok())
}

#[cfg(feature = "hud")]
pub(crate) fn prepare_debug_hud(app: &mut App, window: WindowId) {
    let now = app.clock;
    let Some(state) = app.debug_hud.get_mut(&window) else {
        return;
    };
    let Some(target) = app.windows.get_mut(&window) else {
        return;
    };
    debug_hud::prepare_frame(state, target, now);
}

#[cfg(feature = "hud")]
pub(crate) fn apply_debug_hud(app: &mut App, window: WindowId) {
    let Some(state) = app.debug_hud.get_mut(&window) else {
        return;
    };
    let Some(target) = app.windows.get_mut(&window) else {
        return;
    };
    debug_hud::apply_frame(state, target);
}

#[cfg(feature = "hud")]
pub(crate) fn raise_debug_hud(app: &mut App, window: WindowId) {
    let Some(state) = app.debug_hud.get(&window) else {
        return;
    };
    let hud = state.root();
    let Some(target) = app.windows.get_mut(&window) else {
        return;
    };
    let tree = target.tree_mut();
    let root = tree.root();
    let _ = tree.append_child(root, hud);
}

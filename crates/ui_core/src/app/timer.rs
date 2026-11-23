use std::time::{Duration, Instant};

use super::{App, TimerId};
use crate::{error::EngineError, node::id::NodeId, window::WindowId};

pub(crate) struct TimerEntry {
    id: u32,
    window: WindowId,
    scope: Option<NodeId>,
    deadline: Instant,
    callback: Box<dyn FnOnce(&mut App)>,
}

pub(crate) fn after(
    app: &mut App,
    window: WindowId,
    scope: Option<NodeId>,
    delay: Duration,
    callback: impl FnOnce(&mut App) + 'static,
) -> Result<TimerId, EngineError> {
    let Some(target) = app.windows.get(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    if let Some(scope) = scope
        && (scope.window() != Some(window) || target.tree().get(scope).is_err())
    {
        return Err(EngineError::StaleNode { node: scope });
    }
    let id = app.next_timer;
    app.next_timer = app.next_timer.wrapping_add(1);
    let reference = if app.clock_live {
        Instant::now()
    } else {
        app.clock
    };
    let deadline = reference.checked_add(delay).unwrap_or(reference);
    app.timers.push(TimerEntry {
        id,
        window,
        scope,
        deadline,
        callback: Box::new(callback),
    });
    Ok(TimerId::new(window, id))
}

pub(crate) fn cancel(app: &mut App, id: &TimerId) -> bool {
    let before = app.timers.len();
    app.timers
        .retain(|timer| !(timer.window == id.window() && timer.id == id.generation()));
    app.timers.len() != before
}

pub(crate) fn next_deadline(app: &App) -> Option<Instant> {
    app.timers.iter().map(|timer| timer.deadline).min()
}

pub(crate) fn run_due(app: &mut App) {
    let now = app.clock;
    let mut due: Vec<(Instant, u32)> = app
        .timers
        .iter()
        .filter(|timer| timer.deadline <= now)
        .map(|timer| (timer.deadline, timer.id))
        .collect();
    if due.is_empty() {
        return;
    }
    due.sort_unstable();
    for (_, id) in due {
        let Some(index) = app.timers.iter().position(|timer| timer.id == id) else {
            continue;
        };
        let timer = app.timers.swap_remove(index);
        let Some(target) = app.windows.get(&timer.window) else {
            continue;
        };
        if let Some(scope) = timer.scope
            && target.tree().get(scope).is_err()
        {
            continue;
        }
        (timer.callback)(app);
    }
}

pub(crate) fn cancel_window(app: &mut App, window: WindowId) {
    app.timers.retain(|timer| timer.window != window);
}

pub(crate) fn retain_live_scopes(app: &mut App, window: WindowId) -> usize {
    let Some(target) = app.windows.get(&window) else {
        return 0;
    };
    let tree = target.tree();
    let before = app.timers.len();
    app.timers.retain(|timer| {
        timer.window != window || timer.scope.is_none_or(|scope| tree.get(scope).is_ok())
    });
    before - app.timers.len()
}

#[cfg(all(test, feature = "test-support"))]
pub(crate) fn count(app: &App) -> usize {
    app.timers.len()
}

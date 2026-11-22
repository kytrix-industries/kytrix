//! How a widget tells the framework that its persisted config changed.
//!
//! The framework cannot work this out on its own. A widget's config may end up
//! different from the one it was created with for reasons only the widget knows — a
//! user picking an instrument, or a stored value the venue no longer offers being
//! reset to its default — so the widget reports it.
//!
//! Reporting is what gets the change written out. The pane persists through the
//! widget config the widget itself hands back, so a widget that changes a persisted
//! field without reporting it keeps that change in memory only, and the next start
//! reads the old value back.
//!
//! The report runs against the engine's application context. It is fire-and-forget:
//! the work it triggers runs after the current frame's synchronization, so a widget
//! may report from its own `sync` or from a control's handler without re-entering the
//! widget that is already running, and reporting more than once for one change is
//! harmless.

use std::rc::Rc;

use ui_core::app::App;

/// Reports that the widget's persisted config is no longer the one its pane holds.
///
/// The pane constructs this for the widget it is about to host, so what happens
/// next belongs to the framework: the pane is snapshotted and the settings are
/// written. A widget gets one of these through
/// [`WidgetView::new`](crate::view::WidgetView::new) and calls
/// [`publish`](Self::publish) at every point it changes a field
/// [`config`](crate::view::WidgetView::config) hands back.
#[derive(Clone)]
pub struct WidgetConfigChange(Rc<dyn Fn(&mut App)>);

impl WidgetConfigChange {
    /// Creates a report that runs `report` in the application context.
    pub fn new(report: impl Fn(&mut App) + 'static) -> Self {
        Self(Rc::new(report))
    }

    /// Reports a change to the widget's persisted config.
    pub fn publish(&self, app: &mut App) {
        (self.0)(app);
    }
}

impl Default for WidgetConfigChange {
    /// A report that goes nowhere, for a widget with no pane to write back to.
    fn default() -> Self {
        Self(Rc::new(|_| {}))
    }
}

impl std::fmt::Debug for WidgetConfigChange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("WidgetConfigChange")
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use ui_core::app::App;

    use super::WidgetConfigChange;

    fn app() -> App {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("the test runtime builds");
        App::new(runtime.handle().clone())
    }

    #[test]
    fn publish_should_run_the_report() {
        let mut app = app();
        let reports = Rc::new(Cell::new(0));
        let change = WidgetConfigChange::new({
            let reports = reports.clone();
            move |_: &mut App| reports.set(reports.get() + 1)
        });

        change.publish(&mut app);
        change.publish(&mut app);

        assert_eq!(reports.get(), 2);
    }

    #[test]
    fn the_default_report_should_do_nothing() {
        let mut app = app();
        let change = WidgetConfigChange::default();

        change.publish(&mut app);
    }
}

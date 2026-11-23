use std::rc::Rc;

use ui_core::app::App;

#[derive(Clone)]
pub struct WidgetConfigChange(Rc<dyn Fn(&mut App)>);

impl WidgetConfigChange {
    pub fn new(report: impl Fn(&mut App) + 'static) -> Self {
        Self(Rc::new(report))
    }

    pub fn publish(&self, app: &mut App) {
        (self.0)(app);
    }
}

impl Default for WidgetConfigChange {
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

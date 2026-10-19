//! The clock the settings say data timestamps are read in.
//!
//! A widget that draws a timestamp asks for the offset on the frame it draws it:
//! the setting can move while the widget is open, and the frame that follows the
//! change is what reads the new value. Reading it here rather than formatting once
//! when data arrives is what keeps a converted clock from being cached into a
//! snapshot.
//!
//! A process with no settings manager — an offline runtime, a widget test that
//! never installed one — reads UTC, which is the setting's default and the clock
//! the services publish.

use app_config::time_zone::DataTimeZone;
use app_config_provider::{events::DataTimeZoneChanged, manager::ConfigManager};
use gpui_kit::{App, Context, Subscription};
use time::UtcOffset;

/// The offset every data timestamp is displayed in.
pub fn data_time_zone(cx: &App) -> UtcOffset {
    cx.try_global::<ConfigManager>()
        .map_or(UtcOffset::UTC, |manager| {
            utc_offset(manager.read().data_time_zone)
        })
}

/// The offset the setting names.
///
/// Every value the setting holds is one a UTC offset can express, so this cannot
/// fail; the range is enforced where the value is decoded.
fn utc_offset(zone: DataTimeZone) -> UtcOffset {
    UtcOffset::from_hms(zone.hours(), 0, 0)
        .expect("an offset the setting offers is a valid UTC offset")
}

/// Wakes the widget behind `cx` whenever the display time zone moves.
///
/// The returned subscription is the connection: a widget keeps it for as long as it
/// wants to follow the setting, and dropping it stops following. Nothing is returned
/// when there is no settings manager to follow, which is what an offline runtime and
/// a widget test look like.
pub fn notify_on_data_time_zone_change<T: 'static>(cx: &mut Context<T>) -> Option<Subscription> {
    on_data_time_zone_change(cx, |_, _, cx| cx.notify())
}

/// Runs `changed` with the new offset whenever the display time zone moves.
///
/// A widget whose timestamps are drawn by a child entity — a table, a list, a page —
/// notifies that entity here: the parent's notification alone does not rebuild a
/// cached child, so the row that reads the setting would keep the clock it drew
/// before the change.
pub fn on_data_time_zone_change<T: 'static>(
    cx: &mut Context<T>,
    mut changed: impl FnMut(&mut T, UtcOffset, &mut Context<T>) + 'static,
) -> Option<Subscription> {
    let events = cx.try_global::<ConfigManager>()?.events().clone();

    Some(
        cx.subscribe(&events, move |this, _, event: &DataTimeZoneChanged, cx| {
            changed(this, utc_offset(event.0), cx);
        }),
    )
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use app_config::{
        settings::{Appearance, Config},
        time_zone::DataTimeZone,
    };
    use app_config_provider::manager::ConfigManager;
    use gpui_kit::{
        AppContext, BorrowAppContext as _, Context, Entity, IntoElement, ParentElement, Render,
        Subscription, TestAppContext, Window, div,
    };

    use super::*;

    /// Every offset the setting offers is the UTC offset of that many hours, and
    /// the one no manager is installed for is UTC.
    #[test]
    fn an_offered_offset_should_be_that_many_hours_from_utc() {
        for zone in DataTimeZone::ALL {
            let offset = utc_offset(zone);

            assert_eq!(offset.whole_hours(), zone.hours());
            assert_eq!(offset.whole_seconds(), i32::from(zone.hours()) * 3_600);
        }
    }

    #[gpui_kit::test]
    fn a_widget_should_read_the_zone_the_settings_hold(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert_eq!(
                data_time_zone(cx),
                UtcOffset::UTC,
                "a process without a settings manager reads UTC"
            );

            let mut config = Config::default();
            config.data_time_zone =
                DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset");
            let manager = ConfigManager::in_memory(config, cx);
            cx.set_global(manager);

            assert_eq!(data_time_zone(cx).whole_hours(), 8);
        });
    }

    /// A widget that follows the setting is woken when it moves, which is what makes
    /// the frame after the change draw the new clock rather than the old one.
    #[gpui_kit::test]
    fn a_zone_change_should_wake_the_widget_that_follows_it(cx: &mut TestAppContext) {
        struct Probe {
            renders: Rc<Cell<usize>>,
            _subscription: Option<Subscription>,
        }

        impl Render for Probe {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.renders.set(self.renders.get() + 1);
                div()
            }
        }

        cx.update(|cx| {
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);
        });

        let renders = Rc::new(Cell::new(0));
        let counted = Rc::clone(&renders);
        let (_, cx) = cx.add_window_view(move |_, cx| Probe {
            renders: counted,
            _subscription: notify_on_data_time_zone_change(cx),
        });
        let initial = renders.get();
        assert!(initial > 0, "the probe draws once the window exists");

        cx.update(|_, cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.data_time_zone =
                            DataTimeZone::from_hours(-3).expect("UTC-3 is an offered offset");
                    },
                    cx,
                );
            });
        });
        cx.run_until_parked();

        assert!(
            renders.get() > initial,
            "moving the display zone redraws the widget that follows it"
        );
    }

    /// The widget follows the display zone rather than the settings file: an edit to
    /// another setting is not a reason to rebuild a frame.
    #[gpui_kit::test]
    fn another_setting_should_not_wake_the_widget(cx: &mut TestAppContext) {
        struct Probe {
            renders: Rc<Cell<usize>>,
            _subscription: Option<Subscription>,
        }

        impl Render for Probe {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.renders.set(self.renders.get() + 1);
                div()
            }
        }

        cx.update(|cx| {
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);
        });

        let renders = Rc::new(Cell::new(0));
        let counted = Rc::clone(&renders);
        let (_, cx) = cx.add_window_view(move |_, cx| Probe {
            renders: counted,
            _subscription: notify_on_data_time_zone_change(cx),
        });
        cx.run_until_parked();
        let initial = renders.get();
        assert!(initial > 0, "the probe draws once the window exists");

        cx.update(|_, cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.appearance = Appearance::Dark;
                    },
                    cx,
                );
            });
        });
        cx.run_until_parked();

        assert_eq!(
            renders.get(),
            initial,
            "a setting the clock does not read leaves the frame alone"
        );
    }

    /// A widget's rows are often a cached child entity, and a notification of the
    /// widget does not rebuild it. Following the setting therefore means waking the
    /// entity that draws the rows, which is what this holds the callback to.
    #[gpui_kit::test]
    fn a_zone_change_should_reach_the_child_entity_that_draws_the_rows(cx: &mut TestAppContext) {
        struct Rows {
            renders: Rc<Cell<usize>>,
        }

        impl Render for Rows {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.renders.set(self.renders.get() + 1);
                div()
            }
        }

        struct Pane {
            rows: Entity<Rows>,
            _subscription: Option<Subscription>,
        }

        impl Render for Pane {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().child(self.rows.clone())
            }
        }

        cx.update(|cx| {
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);
        });

        let renders = Rc::new(Cell::new(0));
        let counted = Rc::clone(&renders);
        let (_, cx) = cx.add_window_view(move |_, cx| {
            let rows = cx.new(|_| Rows {
                renders: Rc::clone(&counted),
            });
            Pane {
                // Only the rows are told: the pane is not notified, so a child that
                // still drew the clock from before the change would stay that way.
                _subscription: on_data_time_zone_change::<Pane>(cx, |this, _, cx| {
                    this.rows.update(cx, |_, cx| cx.notify());
                }),
                rows,
            }
        });
        let initial = renders.get();
        assert!(initial > 0, "the rows draw once the window exists");

        cx.update(|_, cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.data_time_zone =
                            DataTimeZone::from_hours(9).expect("UTC+9 is an offered offset");
                    },
                    cx,
                );
            });
        });
        cx.run_until_parked();

        assert!(
            renders.get() > initial,
            "moving the display zone redraws the rows the widget keeps in a child entity"
        );
    }
}

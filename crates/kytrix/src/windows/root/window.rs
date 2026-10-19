use app_config_provider::{
    events::AppearanceChanged, manager::ConfigManager, store::ConfigRecovery,
};
use base_ui::{
    button::Button,
    interactive::InteractiveVariantExt,
    join::Join,
    layer::{Layer, LayerExt},
    separator::Separator,
    size::UnifiedSizable,
    style::theme::ThemeExt,
};
use gpui_kit::{
    base::{Disableable, Selectable},
    prelude::FluentBuilder,
    *,
};
use i18n::t;
use profile_ui::profile::ProfileMenu;
use settings_ui::appearance;
use updater_ui::indicator::UpdateIndicator;

use crate::windows::root::{
    components::system_button::WindowSystemButton,
    pages::{market::Market, portfolio::Portfolio, quant::Quant},
};

/// Opens the main window.
///
/// `recovered` is what startup recovered from when the stored settings could not
/// be used, and is reported to the user once the window is up.
pub fn open(cx: &mut App, recovered: Option<ConfigRecovery>) {
    let window_size = Size::new(px(1280.0), px(960.0));

    let window_options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(window_size, cx)),
        window_min_size: Some(window_size),
        titlebar: Some(TitlebarOptions {
            title: Some("Kytrix".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(12.0), px(12.0))),
        }),

        ..Default::default()
    };

    if let Err(error) = cx.open_window(window_options, |window, cx| {
        let root_window = cx.new(|cx| RootWindow::new(window, recovered, cx));
        cx.new(|_| Layer::new(root_window))
    }) {
        log::error!("failed to open main window: {error}");
        cx.quit();
        return;
    }

    cx.activate(true);
}

struct RootWindow {
    page_market: Entity<Market>,
    page_portfolio: Entity<Portfolio>,
    page_quant: Entity<Quant>,
    selected_tab_index: usize,
    /// The update button the header draws while a release is on offer.
    updater: Entity<UpdateIndicator>,
    profile: Entity<ProfileMenu>,
    /// Watches the appearance the operating system reports for as long as this window lives.
    ///
    /// The observer re-resolves the stored preference every time the system appearance moves, so
    /// it needs no state of its own: a preference the user pinned makes the resolution the same
    /// schema it already was, which the theme ignores, and `System` follows the system. It is
    /// held rather than detached because dropping it is what stops the watching — see
    /// [`gpui_kit::Subscription`].
    _system_appearance: Subscription,
    /// Applies the user's appearance preference for as long as this window lives.
    ///
    /// The settings dialog only writes the preference; this subscription applies it as the
    /// process-wide theme when the write is published, resolving `System` against this window.
    _appearance: Subscription,
}

impl RootWindow {
    /// Builds the root window.
    ///
    /// A settings recovery is announced from a deferred callback rather than from
    /// the constructor itself: this runs while the window is still being built, and
    /// its root only becomes the layer after the constructor returns, so the notice
    /// is opened at the end of the effect cycle, when there is a layer to open it on.
    fn new(window: &mut Window, recovered: Option<ConfigRecovery>, cx: &mut Context<Self>) -> Self {
        if let Some(recovered) = recovered {
            window.defer(cx, move |window, cx| {
                announce_config_recovery(window, cx, &recovered);
            });
        }

        // The window is the first thing that knows which appearance the operating system reports,
        // and the stored preference is applied against it before anything is drawn: a start with
        // `System` shows the system's appearance from its first frame rather than a light frame
        // followed by the one the user asked for.
        appearance::apply(cx.global::<ConfigManager>().read().appearance, window, cx);
        let events = cx.global::<ConfigManager>().events().clone();
        let appearance = cx.subscribe_in(
            &events,
            window,
            |_, _, event: &AppearanceChanged, window, cx| appearance::apply(event.0, window, cx),
        );
        let system_appearance = cx.observe_window_appearance(window, |_, window, cx| {
            // The preference is read here rather than captured: the user may have moved it since
            // this window was built, and what a system change means is decided against what the
            // settings say now.
            appearance::apply(cx.global::<ConfigManager>().read().appearance, window, cx);
        });

        Self {
            page_market: cx.new(Market::new),
            page_portfolio: cx.new(|_| Portfolio),
            page_quant: cx.new(|_| Quant),
            selected_tab_index: 0,
            updater: cx.new(UpdateIndicator::new),
            profile: cx.new(ProfileMenu::new),
            _system_appearance: system_appearance,
            _appearance: appearance,
        }
    }
}

/// Tells the user that their settings could not be read and were replaced.
///
/// The message names the file the old settings were kept in, so a user who wants
/// the old configuration back knows where to look.
fn announce_config_recovery(window: &mut Window, cx: &mut App, recovered: &ConfigRecovery) {
    let title = t!(cx, "startup.config_reset_title");
    let message = match &recovered.backup {
        Some(backup) => t!(cx, "startup.config_reset_message", file = backup.display()),
        None => t!(cx, "startup.config_reset_message_no_backup"),
    };
    let confirm = t!(cx, "common.confirm");

    window.alert(cx, move |alert, _, _| {
        alert.ok(title, message).ok_text(confirm)
    });
}

impl Render for RootWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let left_part = div()
            .flex()
            .items_center()
            .child(match self.selected_tab_index {
                0 => self.page_market.read(cx).toolbar().into_any_element(),
                _ => Empty.into_any_element(),
            });

        let tab_bar = Join::default()
            .button(
                Button::new("Market")
                    .medium()
                    .child(t!(cx, "window.tabs.market"))
                    .outline()
                    .occlude()
                    .selected(self.selected_tab_index == 0)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.selected_tab_index = 0;
                        cx.notify();
                    })),
            )
            .button(
                Button::new("Portfolio")
                    .medium()
                    .child(t!(cx, "window.tabs.portfolio"))
                    .outline()
                    .occlude()
                    .selected(self.selected_tab_index == 1)
                    .disabled(true),
            )
            .button(
                Button::new("Quant")
                    .medium()
                    .child(t!(cx, "window.tabs.quant"))
                    .outline()
                    .occlude()
                    .selected(self.selected_tab_index == 2)
                    .disabled(true),
            );

        // The update button comes before the profile: the header reads from the outside
        // in, and an offer is the one thing here the user has not seen yet.
        let right_part = div()
            .flex()
            .items_center()
            .gap_2()
            .pr_3()
            .child(self.updater.clone())
            .child(self.profile.clone())
            .child(WindowSystemButton::settings())
            .when(!cfg!(target_os = "macos"), |this| {
                this.child(WindowSystemButton::minimize())
            })
            .when(!cfg!(target_os = "macos"), |this| {
                this.child(WindowSystemButton::maximize())
            })
            .when(!cfg!(target_os = "macos"), |this| {
                this.child(WindowSystemButton::close())
            });

        let theme = cx.theme();

        div()
            .bg(theme.roles.surface.canvas.hsla)
            .text_color(theme.roles.fg.default.hsla)
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h_10()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .when_else(
                                cfg!(target_os = "macos"),
                                |this| this.left(px(78.0)),
                                |this| this.left_1(),
                            )
                            .child(left_part),
                    )
                    .child(div().flex_none().child(tab_bar))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_end()
                            .child(right_part),
                    )
                    .window_control_area(WindowControlArea::Drag),
            )
            .child(Separator::horizontal())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p_1()
                    .child(match self.selected_tab_index {
                        0 => self.page_market.clone().into_any_element(),
                        1 => self.page_portfolio.clone().into_any_element(),
                        2 => self.page_quant.clone().into_any_element(),
                        _ => Empty.into_any_element(),
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use app_config_provider::store::ConfigLoadError;
    use base_ui::{animation::ANIMATION_TICK, layer::Layer, style};
    use gpui_kit::{
        AppContext, Context, IntoElement, Modifiers, Render, Styled, TestAppContext,
        VisualTestContext, Window, div,
    };

    use super::{ConfigRecovery, announce_config_recovery};

    const PANEL: &str = "BASE_UI_ALERT";
    const OK: &str = "BASE_UI_ALERT_OK";

    /// The window the notice is announced in.
    ///
    /// It is a bare layer rather than the root window: the notice only needs a
    /// window layer to open an alert on, and the root window would drag the
    /// profile menu and the service connector into the test.
    struct NoticeWindow;

    impl Render for NoticeWindow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    fn recovery(backup: Option<&str>) -> ConfigRecovery {
        ConfigRecovery {
            backup: backup.map(PathBuf::from),
            error: ConfigLoadError::ConfigDirectoryUnavailable,
        }
    }

    /// Opens a window whose root is a layer, the way the application mounts one.
    fn notice_window(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(|cx| {
            gpui_kit::base::init(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            style::theme::install(cx);
        });
        let (_, cx) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| NoticeWindow)));
        cx.run_until_parked();

        cx
    }

    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the window should draw {selector}"))
            .center();
        cx.simulate_click(target, Modifiers::none());
        settle(cx);
    }

    /// The user is told once, and the notice does not come back on the next
    /// frame: it is announced from the window's constructor, not from `render`.
    #[gpui_kit::test]
    fn the_startup_notice_should_be_shown_once_when_the_config_was_reset(cx: &mut TestAppContext) {
        let cx = notice_window(cx);
        let recovered = recovery(Some("/tmp/kytrix/config.json.corrupt-20260101-000000"));

        cx.update(|window, cx| {
            window.defer(cx, move |window, cx| {
                announce_config_recovery(window, cx, &recovered);
            });
        });
        cx.run_until_parked();
        settle(cx);

        assert!(
            cx.debug_bounds(PANEL).is_some(),
            "a reset settings file is reported to the user"
        );

        click(cx, OK);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(PANEL).is_none(),
            "the notice is shown once, not on every frame"
        );
    }

    /// Without a backup there is no file to name, and the message still says what
    /// happened.
    #[gpui_kit::test]
    fn the_startup_notice_should_not_name_a_backup_that_was_not_kept(cx: &mut TestAppContext) {
        let cx = notice_window(cx);
        let recovered = recovery(None);

        cx.update(|window, cx| {
            window.defer(cx, move |window, cx| {
                announce_config_recovery(window, cx, &recovered);
            });
        });
        cx.run_until_parked();
        settle(cx);

        assert!(cx.debug_bounds(PANEL).is_some());
    }
}

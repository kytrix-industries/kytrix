//! The workspace switcher: the dropdown the top-left corner shows, the three
//! buttons beside it, and the alerts that confirm a deletion and ask for a name.
//!
//! The switcher owns nothing. It reads the workspace list to draw itself and
//! calls back into [`Workspaces`] to change it, which keeps that entity the only
//! place that knows what a workspace is.

use app_config::workspace::validate_workspace_name;
use base_ui::{
    button::Button,
    icon::{Icon, IconKind},
    input::InputStateBuilder,
    interactive::InteractiveVariantExt,
    join::Join,
    layer::LayerExt,
    menu::{MenuItem, MenuItemAction},
    select::Select,
    size::UnifiedSize,
    tooltip::TooltipExt,
};
use gpui_kit::{
    App, AppContext, Entity, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder,
};
use i18n::t;

use super::Workspaces;

/// The workspace controls in the window's top-left corner.
#[derive(IntoElement)]
pub(crate) struct WorkspaceSwitcher {
    workspaces: Entity<Workspaces>,
}

impl WorkspaceSwitcher {
    pub(crate) fn new(workspaces: Entity<Workspaces>) -> Self {
        Self { workspaces }
    }
}

impl RenderOnce for WorkspaceSwitcher {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self { workspaces } = self;
        let (names, active, can_delete) = {
            let workspaces = workspaces.read(cx);
            (
                workspaces
                    .names()
                    .map(|name| SharedString::from(name.as_str()))
                    .collect::<Vec<_>>(),
                SharedString::from(workspaces.active_name().as_str()),
                workspaces.can_delete(),
            )
        };

        let mut select = Select::new("workspace-select")
            .outline()
            .placeholder(t!(cx, "window.workspace.empty"))
            .debug_selector(|| "WORKSPACE_SELECT".into())
            .w_40()
            .selected_item(Some(active.clone()))
            .with_tooltip(t!(cx, "window.workspace.select"));
        // A workspace is named by its name: it has no other identity, so the item
        // id and the label are the same string.
        for name in &names {
            select = select.menu_item(MenuItem::Action(MenuItemAction::new(
                name.clone(),
                name.clone(),
            )));
        }
        let select = select.on_change({
            let workspaces = workspaces.clone();
            move |selected: &Option<SharedString>, _, cx| {
                let Some(name) = selected.clone() else {
                    return;
                };
                workspaces.update(cx, |workspaces, cx| workspaces.select(&name, cx));
            }
        });

        let delete = Button::new("workspace-delete")
            .outline()
            .debug_selector(|| "WORKSPACE_DELETE".into())
            .child(Icon::new(IconKind::Trash).size(UnifiedSize::Medium.icon_size()))
            .with_tooltip(t!(cx, "window.workspace.delete"))
            .when(!can_delete, |this| this.cursor_not_allowed())
            .when(can_delete, |this| {
                this.on_click({
                    let workspaces = workspaces.clone();
                    let active = active.clone();
                    move |_, window, cx| {
                        // An alert's builders and actions are `'static`, so every
                        // message is resolved while the application is still here.
                        let title = t!(cx, "window.workspace.delete_title");
                        let message = t!(cx, "window.workspace.delete_message", name = active);
                        let confirm = t!(cx, "common.confirm");
                        let cancel = t!(cx, "common.cancel");

                        window.alert(cx, {
                            let workspaces = workspaces.clone();
                            move |alert, _, _| {
                                alert
                                    .ok_cancel(title, message)
                                    .ok_text(confirm)
                                    .cancel_text(cancel)
                                    .ok_action(move |_, _, cx| {
                                        workspaces.update(cx, |workspaces, cx| {
                                            workspaces.delete_active(cx)
                                        });
                                    })
                            }
                        });
                    }
                })
            });

        let rename = Button::new("workspace-rename")
            .outline()
            .debug_selector(|| "WORKSPACE_RENAME".into())
            .child(Icon::new(IconKind::NotePencil).size(UnifiedSize::Medium.icon_size()))
            .with_tooltip(t!(cx, "window.workspace.rename"))
            .on_click({
                let workspaces = workspaces.clone();
                let names = names.clone();
                move |_, window, cx| {
                    let title = t!(cx, "window.workspace.rename_title");
                    let message = t!(cx, "window.workspace.rename_message", name = active);
                    let placeholder = t!(cx, "window.workspace.name_placeholder");
                    let confirm = t!(cx, "common.confirm");
                    let cancel = t!(cx, "common.cancel");
                    // A workspace may keep the name it has, so it is not one of
                    // the names its own new name must differ from.
                    let taken = names
                        .iter()
                        .filter(|name| **name != active)
                        .cloned()
                        .collect::<Vec<_>>();
                    let validate = name_validator(taken, cx);
                    let initial = active.clone();

                    window.alert(cx, {
                        let workspaces = workspaces.clone();
                        move |alert, window, cx| {
                            let input = cx.new(|cx| {
                                let mut state = InputStateBuilder::new("alert-input")
                                    .placeholder(placeholder)
                                    .build(window, cx);
                                state.update_value(&initial, window, cx);
                                state
                            });

                            alert
                                .input_ok_cancel(title, message, input)
                                .ok_text(confirm)
                                .cancel_text(cancel)
                                .validate(validate)
                                .ok_action(move |value, _, cx| {
                                    let value = value.unwrap_or_default();
                                    if let Err(error) =
                                        workspaces.update(cx, |this, cx| this.rename(&value, cx))
                                    {
                                        log::error!(
                                            "failed to rename the active workspace \
                                             to `{value}`: {error}"
                                        );
                                    }
                                })
                        }
                    });
                }
            });

        let add = Button::new("workspace-add")
            .outline()
            .debug_selector(|| "WORKSPACE_ADD".into())
            .child(Icon::new(IconKind::Plus).size(UnifiedSize::Medium.icon_size()))
            .with_tooltip(t!(cx, "window.workspace.add"))
            .on_click({
                // The last control to be built takes the list and the entity: it
                // is the only remaining user of either.
                move |_, window, cx| {
                    let title = t!(cx, "window.workspace.add_title");
                    let message = t!(cx, "window.workspace.add_message");
                    let placeholder = t!(cx, "window.workspace.name_placeholder");
                    let confirm = t!(cx, "common.confirm");
                    let cancel = t!(cx, "common.cancel");
                    // The field starts at the next free default name, so adding a
                    // workspace without typing anything does the obvious thing.
                    let suggested = workspaces.read(cx).suggested_name(cx);
                    let validate = name_validator(names.clone(), cx);

                    window.alert(cx, {
                        let workspaces = workspaces.clone();
                        move |alert, window, cx| {
                            let input = cx.new(|cx| {
                                let mut state = InputStateBuilder::new("alert-input")
                                    .placeholder(placeholder)
                                    .build(window, cx);
                                state.update_value(&suggested, window, cx);
                                state
                            });

                            alert
                                .input_ok_cancel(title, message, input)
                                .ok_text(confirm)
                                .cancel_text(cancel)
                                .validate(validate)
                                .ok_action(move |value, _, cx| {
                                    let value = value.unwrap_or_default();
                                    if let Err(error) =
                                        workspaces.update(cx, |this, cx| this.add(&value, cx))
                                    {
                                        log::error!(
                                            "failed to add the workspace `{value}`: {error}"
                                        );
                                    }
                                })
                        }
                    });
                }
            });

        // The controls have to keep the title bar's drag area from claiming the
        // space they occupy, and blocking mouse events behind them is what does
        // that. The blocking belongs to this wrapper rather than to the controls
        // themselves: GPUI stops its hover hit test at the first element that
        // blocks, and a control is painted *after* the hover wrapper of the
        // tooltip it was given — so a control that blocks can never open its own
        // tooltip. Wrapping the row keeps the block and lets the hover reach the
        // tooltips inside it, which is why none of them blocks on its own.
        div().occlude().child(
            Join::default()
                .select(select)
                .button(delete)
                .button(rename)
                .button(add),
        )
    }
}

/// Builds the validator both name alerts use.
///
/// An alert's validator runs without an application context, so the names a new
/// name must differ from are the ones that were taken when the alert opened.
/// Nothing can add a workspace while it is up: the alert is modal.
fn name_validator(
    taken: Vec<SharedString>,
    cx: &App,
) -> impl Fn(&SharedString) -> Result<(), SharedString> + 'static {
    let required = t!(cx, "window.workspace.name_required");
    let exists = t!(cx, "window.workspace.name_exists");

    move |value| {
        let name = validate_workspace_name(value.as_str()).map_err(|_| required.clone())?;
        if taken.iter().any(|taken| taken.as_ref() == name.as_str()) {
            return Err(exists.clone());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use app_config::{
        settings::{Config, Language},
        workspace::{WorkspaceConfig, WorkspaceLayout, WorkspacesConfig},
    };
    use app_config_provider::manager::ConfigManager;
    use base_ui::{animation::ANIMATION_TICK, layer::Layer, style};
    use gpui_kit::{
        AppContext, Context, Entity, IntoElement, Modifiers, Render, TestAppContext,
        VisualTestContext, Window, div, px, size,
    };
    use i18n::language::codes::LanguageCode;
    use smol_str::SmolStr;

    use super::*;

    const PANEL: &str = "BASE_UI_ALERT";
    const OK: &str = "BASE_UI_ALERT_OK";
    const CANCEL: &str = "BASE_UI_ALERT_CANCEL";
    const INPUT: &str = "BASE_UI_INPUT";
    const TOOLTIP: &str = "BASE_UI_TOOLTIP";
    const SELECT: &str = "WORKSPACE_SELECT";
    const DELETE: &str = "WORKSPACE_DELETE";
    const RENAME: &str = "WORKSPACE_RENAME";
    const ADD: &str = "WORKSPACE_ADD";

    /// The window the switcher is mounted in.
    ///
    /// It is the switcher alone rather than the root window: the root window
    /// needs the profile menu and the service connector, none of which the
    /// workspace controls depend on.
    struct SwitcherWindow {
        workspaces: Entity<Workspaces>,
    }

    impl Render for SwitcherWindow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(WorkspaceSwitcher::new(self.workspaces.clone()))
        }
    }

    /// Settings that already hold `names`, with the first one active, in English
    /// so a test can type a name the same way a user would.
    fn config_with(names: &[&str]) -> Config {
        let mut config = Config::default();
        config.language = Language::En;
        config.workspaces = WorkspacesConfig {
            entries: names
                .iter()
                .map(|name| WorkspaceConfig {
                    name: SmolStr::new(*name),
                    layout: WorkspaceLayout::single_pane(),
                })
                .collect(),
            active: names.first().map(|name| SmolStr::new(*name)),
        };

        config
    }

    /// Opens a window whose root is a layer, the way the application mounts one,
    /// so the switcher has a window layer to open its alerts on.
    fn switcher_window(
        cx: &mut TestAppContext,
        config: Config,
    ) -> (Entity<Workspaces>, &mut VisualTestContext) {
        let language = match config.language {
            Language::ZhCn => LanguageCode::ZhCn,
            Language::En => LanguageCode::En,
        };
        cx.update(|cx| {
            gpui_kit::base::init(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            widget_registry::catalogue::install(cx).expect("the catalogue should install");
            style::theme::install(cx);
            cx.set_global(language);
            let manager = ConfigManager::in_memory(config, cx);
            cx.set_global(manager);
        });
        let workspaces = cx.update(|cx| cx.new(Workspaces::load));
        let (_, cx) = cx.add_window_view({
            let workspaces = workspaces.clone();
            move |_, cx| Layer::new(cx.new(|_| SwitcherWindow { workspaces }))
        });
        cx.simulate_resize(size(px(640.0), px(240.0)));
        cx.run_until_parked();

        (workspaces, cx)
    }

    /// Lets the alert's own fade finish and repaints, so an alert in its first
    /// frames — painted at zero opacity — is not mistaken for one that stays.
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

    /// Types into the alert's field, which is not focused until it is clicked.
    fn type_into_alert(cx: &mut VisualTestContext, text: &str) {
        click(cx, INPUT);
        cx.simulate_input(text);
        cx.run_until_parked();
    }

    /// Hovers one control and lets the tooltip's own delay and fade pass.
    fn hover(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the window should draw {selector}"))
            .center();
        cx.simulate_mouse_move(target, None, Modifiers::none());
        cx.run_until_parked();
        for _ in 0..10 {
            settle(cx);
        }
    }

    fn active_name(cx: &VisualTestContext, workspaces: &Entity<Workspaces>) -> SmolStr {
        workspaces.read_with(cx, |workspaces, _| workspaces.active_name().clone())
    }

    fn names(cx: &VisualTestContext, workspaces: &Entity<Workspaces>) -> Vec<SmolStr> {
        workspaces.read_with(cx, |workspaces, _| workspaces.names().cloned().collect())
    }

    #[test]
    fn a_name_validator_should_reject_empty_and_taken_names() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            let required = t!(cx, "window.workspace.name_required");
            let exists = t!(cx, "window.workspace.name_exists");
            let validate = name_validator(vec![SharedString::new_static("Taken")], cx);

            assert_eq!(validate(&SharedString::new_static("   ")), Err(required));
            assert_eq!(
                validate(&SharedString::new_static("  Taken  ")),
                Err(exists),
                "a padded name collides with the same name trimmed"
            );
            assert_eq!(validate(&SharedString::new_static("Taken 2")), Ok(()));
            assert_eq!(validate(&SharedString::new_static("taken")), Ok(()));
        });
    }

    #[gpui_kit::test]
    fn every_workspace_control_should_show_its_tooltip(cx: &mut TestAppContext) {
        let (_, cx) = switcher_window(cx, Config::default());

        for control in [SELECT, DELETE, RENAME, ADD] {
            hover(cx, control);

            assert!(
                cx.debug_bounds(TOOLTIP).is_some(),
                "hovering {control} should explain what it does"
            );
        }
    }

    #[gpui_kit::test]
    fn the_delete_button_should_not_open_the_alert_when_only_one_workspace_exists(
        cx: &mut TestAppContext,
    ) {
        let (_, cx) = switcher_window(cx, Config::default());

        assert!(
            cx.debug_bounds(SELECT).is_some(),
            "the switcher draws its dropdown and its buttons"
        );

        click(cx, DELETE);

        assert!(
            cx.debug_bounds(PANEL).is_none(),
            "the only workspace cannot be deleted, so nothing asks to confirm it"
        );
    }

    #[gpui_kit::test]
    fn deleting_should_ask_first_and_remove_only_the_confirmed_workspace(cx: &mut TestAppContext) {
        let (workspaces, cx) = switcher_window(cx, config_with(&["one", "two"]));

        click(cx, DELETE);
        assert!(
            cx.debug_bounds(PANEL).is_some(),
            "deleting a workspace asks first"
        );

        click(cx, CANCEL);
        assert_eq!(
            names(cx, &workspaces),
            ["one", "two"],
            "cancel deletes nothing"
        );

        click(cx, DELETE);
        click(cx, OK);
        assert_eq!(
            names(cx, &workspaces),
            ["two"],
            "the workspace the user confirmed is the one that goes"
        );
    }

    #[gpui_kit::test]
    fn adding_should_offer_the_next_default_name_and_switch_to_it(cx: &mut TestAppContext) {
        let (workspaces, cx) = switcher_window(cx, Config::default());

        click(cx, ADD);
        assert!(cx.debug_bounds(PANEL).is_some(), "adding asks for a name");
        click(cx, OK);

        assert_eq!(
            active_name(cx, &workspaces),
            SmolStr::new_static("工作区 2"),
            "the field starts at the next free default name, so confirming it adds \
             that workspace and switches to it"
        );
    }

    #[gpui_kit::test]
    fn renaming_should_reject_a_name_another_workspace_uses(cx: &mut TestAppContext) {
        let (workspaces, cx) = switcher_window(cx, config_with(&["a", "ab"]));

        click(cx, RENAME);
        type_into_alert(cx, "b");
        click(cx, OK);

        assert!(
            cx.debug_bounds(PANEL).is_some(),
            "a name another workspace uses leaves the alert open"
        );
        assert_eq!(
            active_name(cx, &workspaces),
            SmolStr::new_static("a"),
            "a refused name changes nothing"
        );

        type_into_alert(cx, "c");
        click(cx, OK);

        assert!(
            cx.debug_bounds(PANEL).is_none(),
            "an accepted name dismisses the alert"
        );
        assert_eq!(active_name(cx, &workspaces), SmolStr::new_static("abc"));
    }
}

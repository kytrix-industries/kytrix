//! The sidebar panel itself: the workspace section, and the settings entry at its foot.
//!
//! The panel is drawn twice over the window's life: docked beside the page, and floating
//! over it while the sidebar is collapsed and the pointer asks for the full one — on the
//! title bar's toggle while hidden, or on the icon-only rail.
//!
//! Both are this one view, so there is nothing a floating panel can do that the docked one
//! cannot.

use std::rc::Rc;

use app_config::ui::SidebarMode;
use base_ui::{separator::Separator, style::theme::ThemeExt};
use gpui_kit::{
    App, ElementId, Empty, Entity, InteractiveElement, IntoElement, ParentElement, Pixels,
    RenderOnce, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};

use super::{footer::SidebarFooter, layout::TITLE_BAR_HEIGHT, workspaces::SidebarWorkspaces};
use crate::windows::root::components::pages::MainPages;

/// The panel, whichever of the two places it is drawn in.
pub(crate) const SIDEBAR: &str = "SIDEBAR";

/// The wrapper the panel takes when it floats over the page.
pub(crate) const SIDEBAR_FLOATING: &str = "SIDEBAR_FLOATING";

/// The divider drawn on the panel's right edge.
const SIDEBAR_DIVIDER: &str = "SIDEBAR_DIVIDER";

/// The window's left sidebar.
///
/// The window decides how wide the panel is and animates that width between the sidebar's
/// three states; the panel draws whatever width it is handed, and nothing at all once that
/// width reaches the hidden state.
#[derive(IntoElement)]
pub(crate) struct Sidebar {
    /// The sidebar state the panel is drawn for.
    ///
    /// The hidden state draws the full panel's content, not the rail's: the frame the panel
    /// disappears on is the full panel collapsing to no width, and it keeps the labels it
    /// was showing rather than switching to icons on its way out.
    mode: SidebarMode,
    width: Pixels,
    pages: Entity<MainPages>,
    /// Called as the pointer enters or leaves the panel's content, below the title bar.
    content_hover: Option<Rc<dyn Fn(&bool, &mut Window, &mut App)>>,
    /// Whether the panel is moving away rather than coming in or standing still.
    ///
    /// The rows are still drawn while the panel moves out, but the controls that belong to
    /// the settled panel — the workspace close buttons — are not offered on the way out.
    retracting: bool,
    /// Identifies one opening of the panel, so its rows and menus do not outlive it.
    popup_scope: u64,
}

impl Sidebar {
    pub(crate) fn new(mode: SidebarMode, width: Pixels, pages: Entity<MainPages>) -> Self {
        Self {
            mode,
            width,
            pages,
            content_hover: None,
            retracting: false,
            popup_scope: 0,
        }
    }

    /// Reports the pointer entering and leaving the panel's content, below the title bar.
    ///
    /// The window hands this to the icon-only rail: a pointer on the rail asks for the full
    /// panel, and the rail's content is what the pointer reaches.
    pub(crate) fn on_content_hover(
        mut self,
        callback: impl Fn(&bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.content_hover = Some(Rc::new(callback));
        self
    }

    /// Whether the panel is on its way out.
    ///
    /// A panel that is retracting still draws its rows, but it does not offer the close
    /// buttons that belong to the settled panel.
    pub(crate) fn retracting(mut self, retracting: bool) -> Self {
        self.retracting = retracting;
        self
    }

    /// Names this opening of the panel.
    ///
    /// Every row keys its own state by this number, so the state of the docked rail's rows
    /// is not shared with the floating panel drawn over it, and a menu that was left behind
    /// is not brought back by the next opening.
    pub(crate) fn popup_scope(mut self, scope: u64) -> Self {
        self.popup_scope = scope;
        self
    }
}

impl RenderOnce for Sidebar {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        // A panel with no width is the hidden state's resting place: there is nothing to
        // draw, and the divider would be a line at the window's edge.
        if self.width <= px(0.0) {
            return Empty.into_any_element();
        }

        // The panel draws labels unless it is the icon-only rail; the hidden state keeps the
        // full panel it was collapsing from.
        let full = !matches!(self.mode, SidebarMode::IconOnly);
        let mut content = div()
            // The docked panel and the floating one can be drawn in the same frame (the
            // panel collapsing to nothing under a preview the pointer asks for), so the
            // content wrapper names the opening as well as the content it draws.
            .id(ElementId::NamedInteger(
                SharedString::new_static(if full {
                    "sidebar-content-full"
                } else {
                    "sidebar-content-icon-only"
                }),
                self.popup_scope,
            ))
            // The title bar is drawn over the window rather than beside the panel, so the
            // panel's background reaches the top of the window while its content starts
            // below the bar. The margin, rather than the panel's padding, is what keeps the
            // content's hover region out of the title bar.
            .mt(TITLE_BAR_HEIGHT)
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col();
        if let Some(on_hover) = self.content_hover {
            content = content.on_hover(move |hovered, window, cx| on_hover(hovered, window, cx));
        }

        let workspaces = self.pages.read(cx).workspaces().clone();
        let workspaces = SidebarWorkspaces::new(full, workspaces)
            .popup_scope(self.popup_scope)
            .show_close(!self.retracting);
        let footer = SidebarFooter::new(full, self.pages);

        content = content
            .child(workspaces)
            .child(Separator::horizontal())
            .child(footer);

        div()
            .debug_selector(|| SIDEBAR.into())
            .relative()
            .w(self.width)
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            // The panel narrows while it collapses, and its rows keep the width of the
            // state they belong to: clipping is what makes the panel's edge the edge of
            // what it shows.
            .overflow_hidden()
            .bg(theme.roles.surface.sunken.hsla)
            .child(content)
            // The divider is drawn over the panel's right edge rather than as a border: a
            // border would take one of the width's pixels, and the rows inside are sized
            // against the whole width.
            .child(
                div()
                    .debug_selector(|| SIDEBAR_DIVIDER.into())
                    .absolute()
                    .right_0()
                    .top_0()
                    .bottom_0()
                    .w(px(1.0))
                    .bg(theme.roles.line.divider.hsla),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use app_config::{
        settings::{Config, Language},
        ui::SidebarMode,
        workspace::{WorkspaceConfig, WorkspaceLayout, WorkspacesConfig},
    };
    use app_config_provider::manager::ConfigManager;
    use base_ui::{animation::ANIMATION_TICK, layer::Layer, style};
    use gpui_kit::{
        AppContext, Context, Entity, IntoElement, ParentElement, Pixels, Render, Styled,
        Subscription, TestAppContext, VisualTestContext, Window, div, px, size,
    };
    use i18n::language::codes::LanguageCode;
    use smol_str::SmolStr;

    use super::{SIDEBAR, SIDEBAR_DIVIDER, Sidebar};
    use crate::windows::root::components::{
        pages::MainPages,
        sidebar::{
            footer::{SIDEBAR_FOOTER_SETTINGS, SIDEBAR_FOOTER_SETTINGS_LABEL},
            layout::{
                SIDEBAR_FULL_WIDTH, SIDEBAR_ICON_SIZE, SIDEBAR_ICON_WIDTH, SIDEBAR_ITEM_SIZE,
                SIDEBAR_PADDING, width,
            },
            workspaces::{
                SIDEBAR_NEW_WORKSPACE, SIDEBAR_NEW_WORKSPACE_LABEL, SIDEBAR_WORKSPACE_MARK,
                SIDEBAR_WORKSPACE_NAME,
            },
        },
        workspace::Workspaces,
    };

    /// The window the panel is mounted in: the real sidebar, with the entities the window
    /// hands it.
    struct Harness {
        mode: SidebarMode,
        pages: Entity<MainPages>,
        /// The sidebar reads the list while this view draws, so the view follows the store
        /// the way the window around the panel does.
        _workspaces: Subscription,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().child(Sidebar::new(
                self.mode,
                width(self.mode),
                self.pages.clone(),
            ))
        }
    }

    /// Settings that already hold `names`, with the first one active, in English so a test
    /// can type a name the same way a user would.
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

    /// Opens a window whose root is a layer, the way the application mounts one, with the
    /// real panel and the entities the window gives it.
    fn harness(
        cx: &mut TestAppContext,
        config: Config,
        mode: SidebarMode,
    ) -> (&mut VisualTestContext, Entity<Workspaces>) {
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
            widget_tokio_runtime::global::install(cx).expect("the test runtime should install");
        });
        let workspaces = cx.update(|cx| cx.new(Workspaces::load));
        let pages = cx.update(|cx| cx.new(|cx| MainPages::new(workspaces.clone(), cx)));
        let (_, cx) = cx.add_window_view({
            let workspaces = workspaces.clone();
            move |_, cx| {
                let harness = cx.new(move |cx| {
                    let subscription = cx.observe(&workspaces, |_, _, cx| cx.notify());

                    Harness {
                        mode,
                        pages,
                        _workspaces: subscription,
                    }
                });

                Layer::new(harness)
            }
        });
        cx.simulate_resize(size(px(800.0), px(600.0)));
        cx.run_until_parked();

        (cx, workspaces)
    }

    /// Lets the theme's transitions finish and repaints, so a control's settled frame is the
    /// one the assertions see.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    fn drawn(cx: &mut VisualTestContext, selector: &'static str) -> gpui_kit::Bounds<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("the sidebar should draw {selector}"))
    }

    /// The full panel draws the workspace section and the footer inside a 220 px column, and
    /// the content is the whole 220: the divider is drawn over the edge rather than taking a
    /// pixel from the column.
    #[gpui_kit::test]
    fn the_full_sidebar_should_draw_the_workspace_section_and_the_footer(cx: &mut TestAppContext) {
        let (cx, _) = harness(cx, config_with(&["one", "two"]), SidebarMode::Full);

        assert_eq!(
            drawn(cx, SIDEBAR).size.width,
            SIDEBAR_FULL_WIDTH,
            "the drawn panel is the full sidebar's width"
        );
        assert_eq!(
            drawn(cx, "SIDEBAR_ITEM_one").size,
            size(
                SIDEBAR_FULL_WIDTH - SIDEBAR_PADDING * 2.0,
                SIDEBAR_ITEM_SIZE
            ),
            "a row is the 212 px the panel leaves inside its padding"
        );
        let item = drawn(cx, "SIDEBAR_ITEM_one");
        assert_eq!(
            drawn(cx, SIDEBAR_WORKSPACE_MARK).left(),
            item.left() + (SIDEBAR_ITEM_SIZE - SIDEBAR_ICON_SIZE) / 2.0,
            "the mark sits at the inset the icon-only square centres its mark at"
        );
        assert_eq!(
            drawn(cx, SIDEBAR_FOOTER_SETTINGS).size,
            size(
                SIDEBAR_FULL_WIDTH - SIDEBAR_PADDING * 2.0,
                SIDEBAR_ITEM_SIZE
            ),
            "the settings entry is the foot's full-width row"
        );
        // The divider is drawn over the right edge rather than taking a pixel from the
        // width: the content and the divider together are still the panel's 220 px.
        let divider = drawn(cx, SIDEBAR_DIVIDER);
        assert_eq!(divider.size.width, px(1.0));
        assert_eq!(
            divider.right(),
            SIDEBAR_FULL_WIDTH,
            "the divider lies on the panel's right edge, not inside its content"
        );
        for selector in [
            SIDEBAR_NEW_WORKSPACE,
            SIDEBAR_NEW_WORKSPACE_LABEL,
            "SIDEBAR_ITEM_one",
            SIDEBAR_WORKSPACE_NAME,
            SIDEBAR_FOOTER_SETTINGS,
            SIDEBAR_FOOTER_SETTINGS_LABEL,
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "the full sidebar draws {selector}"
            );
        }
    }

    /// The icon-only panel is 40 px, keeps the rows and the footer, and drops every label.
    #[gpui_kit::test]
    fn the_icon_only_sidebar_should_draw_icons_without_labels(cx: &mut TestAppContext) {
        let (cx, _) = harness(cx, config_with(&["one", "two"]), SidebarMode::IconOnly);

        assert_eq!(
            drawn(cx, SIDEBAR).size.width,
            SIDEBAR_ICON_WIDTH,
            "the drawn panel is the icon-only sidebar's width"
        );
        assert_eq!(
            drawn(cx, "SIDEBAR_ITEM_one").size,
            size(SIDEBAR_ITEM_SIZE, SIDEBAR_ITEM_SIZE),
            "a row is the title bar toggle's square"
        );
        assert_eq!(
            drawn(cx, SIDEBAR_FOOTER_SETTINGS).size,
            size(SIDEBAR_ITEM_SIZE, SIDEBAR_ITEM_SIZE),
            "the settings gear at the foot is the same square"
        );
        assert_eq!(
            drawn(cx, SIDEBAR_FOOTER_SETTINGS).left(),
            SIDEBAR_PADDING,
            "the foot's controls keep the column the items above them start"
        );
        assert_eq!(
            drawn(cx, SIDEBAR_FOOTER_SETTINGS).right(),
            SIDEBAR_ICON_WIDTH - SIDEBAR_PADDING,
            "the foot's square leaves the same padding on the panel's right edge"
        );
        assert_eq!(
            drawn(cx, SIDEBAR_DIVIDER).right(),
            SIDEBAR_ICON_WIDTH,
            "the icon-only panel's divider is on its right edge too"
        );
        for selector in [
            SIDEBAR_NEW_WORKSPACE,
            "SIDEBAR_ITEM_one",
            SIDEBAR_FOOTER_SETTINGS,
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "the icon-only sidebar draws {selector}"
            );
        }
        for selector in [
            SIDEBAR_NEW_WORKSPACE_LABEL,
            SIDEBAR_WORKSPACE_NAME,
            SIDEBAR_FOOTER_SETTINGS_LABEL,
        ] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "the icon-only sidebar draws no {selector}"
            );
        }
    }

    /// A workspace name longer than the 150 px a name may take is cut off there rather than
    /// running past the row it belongs to.
    #[gpui_kit::test]
    fn a_long_workspace_name_should_be_cut_off_at_its_maximum_width(cx: &mut TestAppContext) {
        let (cx, workspaces) = harness(cx, config_with(&["one"]), SidebarMode::Full);
        let long_name = "a".repeat(200);

        cx.update(|_, cx| {
            workspaces.update(cx, |workspaces, cx| {
                workspaces
                    .add(&long_name, cx)
                    .expect("a long name is still a valid name");
                // The short workspace goes, so the one name label in the panel is the long
                // one and the measurement below cannot be about another row.
                workspaces.delete_entry("one", cx);
            });
        });
        settle(cx);

        let name = drawn(cx, SIDEBAR_WORKSPACE_NAME);
        assert!(
            name.size.width <= px(150.0),
            "a long name is cut off at the 150 px maximum: {name:?}"
        );
    }

    /// The footer's settings label is the gear's label in the full panel, and the icon-only
    /// panel keeps the gear alone.
    #[gpui_kit::test]
    fn the_footer_settings_should_draw_its_label_only_in_the_full_sidebar(cx: &mut TestAppContext) {
        let (cx, _) = harness(cx, config_with(&["one"]), SidebarMode::Full);
        let label = drawn(cx, SIDEBAR_FOOTER_SETTINGS_LABEL);
        assert!(
            label.right() <= drawn(cx, SIDEBAR_FOOTER_SETTINGS).right(),
            "the settings label stays inside the settings button"
        );
    }

    /// A hidden sidebar draws nothing: the window gives it no width, and the floating panel
    /// is what shows the state on hover.
    #[gpui_kit::test]
    fn a_hidden_sidebar_should_draw_nothing(cx: &mut TestAppContext) {
        let (cx, _) = harness(cx, config_with(&["one"]), SidebarMode::Hidden);

        assert!(
            cx.debug_bounds(SIDEBAR).is_none(),
            "a panel with no width draws nothing while the sidebar state is hidden"
        );
    }
}

use std::{cell::Cell, rc::Rc, time::Duration};

use app_config::ui::SidebarMode;
use app_config_provider::{
    events::AppearanceChanged, manager::ConfigManager, store::ConfigRecovery,
};
use base_ui::{
    animation::popup_should_render,
    button::Button,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    layer::{Layer, LayerExt as _},
    style::theme::ThemeExt,
    tooltip::TooltipExt,
};
use gpui_kit::{
    base::{Easing, ElementExt as _, Presence, Transition, box_shadow, transition},
    prelude::FluentBuilder,
    *,
};
use i18n::t;
use profile_ui::profile::ProfileMenu;
use settings_ui::appearance;
use updater_ui::indicator::UpdateIndicator;

use crate::windows::root::{
    components::{
        sidebar::{
            FLOATING_CLOSE_DELAY, FloatingSidebar, SIDEBAR_FLOATING, SIDEBAR_FULL_WIDTH,
            SIDEBAR_ICON_WIDTH, SIDEBAR_ITEM_SIZE, Sidebar, TITLE_BAR_HEIGHT, TOGGLE_INSET,
            TOGGLE_INSET_MACOS, next_mode, width as sidebar_width,
        },
        system_button::WindowSystemButton,
        workspace::Workspaces,
    },
    pages::market::Market,
};

/// The title bar's sidebar toggle.
const TITLE_BAR_SIDEBAR_TOGGLE: &str = "TITLE_BAR_SIDEBAR_TOGGLE";

/// The mark inside the title bar's sidebar toggle, named so a test can centre it.
const TITLE_BAR_SIDEBAR_TOGGLE_ICON: &str = "TITLE_BAR_SIDEBAR_TOGGLE_ICON";

/// The column the page is drawn in, named so a test can say where the sidebar leaves it.
const PAGE: &str = "ROOT_PAGE";

/// How long the sidebar takes to move between its states, and for a floating preview to come
/// in or go out.
///
/// The movement is slower than the controls' own transitions because it is a whole panel
/// travelling across the window rather than a colour settling.
const SIDEBAR_ANIMATION: Duration = Duration::from_millis(250);

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
        // On macOS the system titlebar is shorter than the title bar drawn here, so the
        // window could not be dragged by its lower part. The title bar takes over the
        // drag itself via `Window::start_window_move`, and AppKit must stop treating
        // the native titlebar as a window-move region for that to be the only drag.
        // Other platforms ignore this and keep using `WindowControlArea::Drag`.
        app_owns_titlebar_drag: true,
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
    /// The workspaces the page shows and the sidebar lists.
    workspaces: Entity<Workspaces>,
    /// How the sidebar is drawn, mirrored from the settings this window read at startup and
    /// wrote back on every press of the toggle.
    sidebar: SidebarMode,
    /// The collapsed sidebar's floating panel: whether it is up, and whether the pointer
    /// rests on the toggle or on the panel itself. It is not a fourth state: the panel is
    /// transient, and the state the settings hold stays collapsed.
    floating: FloatingSidebar,
    /// Names the current opening of the floating panel. Its rows key their own state by
    /// it, so the docked rail's rows and the floating panel's rows do not share it, and a
    /// row's context menu ids start fresh with each opening.
    popup_scope: u64,
    /// The sidebar state the floating panel being drawn belongs to.
    ///
    /// The two collapsed states ask for the same panel from different places: the hidden
    /// state's toggle has nothing to grow out of, so that preview slides in from the
    /// window's edge, while the icon-only rail already is the panel at its narrow width, so
    /// that preview grows out of the rail. Where the preview started is what decides, not
    /// the state the window is in by the time the preview is leaving.
    preview_from_rail: bool,
    /// Whether the panel is waiting for the next pointer move to decide whether to close.
    ///
    /// A context menu keeps the panel up while it is open, and the menu is a layer of its
    /// own: a pointer that leaves the menu may already be outside the panel, and there is
    /// no hover event left to hear. The flag carries that decision to the next mouse move,
    /// which is the first moment the pointer's new position is known.
    close_on_next_move: bool,
    /// The floating panel's bounds, recorded as it is drawn.
    ///
    /// A context menu covers the panel while it is up, so the panel's own hover reports it
    /// as left. The bounds are what says whether the pointer that moved after the menu
    /// closed is back on the panel.
    floating_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The window's layer, for the questions the floating panel asks of it: whether a
    /// context menu is up, and how to take one down.
    layer: Option<WeakEntity<Layer>>,
    /// The pending close of the floating panel. Dropping it cancels the close, which keeps
    /// the panel up across the pointer's move between the trigger and the panel.
    floating_close: Option<Task<()>>,
    /// Whether a left press on the title bar should move the window on the next mouse move.
    ///
    /// Only macOS reads this: there the title bar owns the drag instead of AppKit's
    /// titlebar, which is shorter than the bar. Elsewhere the platform drags the
    /// window from `WindowControlArea::Drag` itself.
    should_move: bool,
    /// The update button the title bar draws while a release is on offer.
    updater: Entity<UpdateIndicator>,
    profile: Entity<ProfileMenu>,
    /// Watches the workspace list for as long as this window lives.
    ///
    /// The sidebar reads the list while this window draws, so an added, renamed, deleted or
    /// switched workspace is a change to what the window shows. The stage is a view of its
    /// own and redraws itself; the list around it does not.
    _workspaces: Subscription,
    /// Watches the account the footer draws for as long as this window lives, so a sign-in
    /// or a sign-out repaints the entry at the foot of the sidebar.
    _profile: Subscription,
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

/// One of the places that can keep the floating panel up.
#[derive(Clone, Copy)]
enum FloatingRegion {
    /// The title bar's toggle, which floats the hidden sidebar's preview.
    Toggle,
    /// The panel itself.
    Panel,
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

        // The sidebar state is a preference the window draws, not a setting anything
        // subscribes to: it is read here, once, and written back by the toggle.
        let sidebar = cx.global::<ConfigManager>().read().ui.sidebar;
        let workspaces = cx.new(Workspaces::load);
        // The sidebar reads the workspace list while this window draws, so an added,
        // renamed, deleted or switched workspace is a change to what the window shows. The
        // stage is a view of its own and redraws itself; the list around it does not. Both
        // subscriptions are held: an observer that is dropped stops following.
        let workspaces_changes = cx.observe(&workspaces, |_, _, cx| cx.notify());
        let page_market = cx.new(|cx| Market::new(cx, workspaces.clone()));
        let profile = cx.new(ProfileMenu::new);
        let profile_changes = cx.observe(&profile, |_, _, cx| cx.notify());

        Self {
            page_market,
            workspaces,
            sidebar,
            floating: FloatingSidebar::default(),
            popup_scope: 0,
            preview_from_rail: false,
            close_on_next_move: false,
            floating_bounds: Rc::new(Cell::new(Bounds::default())),
            layer: None,
            floating_close: None,
            should_move: false,
            updater: cx.new(UpdateIndicator::new),
            profile,
            _workspaces: workspaces_changes,
            _profile: profile_changes,
            _system_appearance: system_appearance,
            _appearance: appearance,
        }
    }

    /// Advances the sidebar to its next state, and stores what it advanced to.
    ///
    /// The setting is written without publishing an event — nothing subscribes to the
    /// sidebar — so this window is also what carries the new state to the frame that draws
    /// it.
    fn cycle_sidebar(&mut self, cx: &mut Context<Self>) {
        let mode = next_mode(self.sidebar);
        store_sidebar_mode(mode, cx);

        self.sidebar = mode;
        // Whether the panel was floating belonged to the state being left: the state now
        // drawn is either docked or absent, and a panel floating over it is gone.
        self.dismiss_context_menu(cx);
        self.floating.reset();
        self.floating_close = None;
        self.close_on_next_move = false;
        cx.notify();
    }

    /// Opens the floating full panel when the pointer reaches the icon-only rail.
    ///
    /// The rail lies inside the rectangle the floating panel takes, so the pointer that
    /// asked for the panel is already on it the moment the frame draws it. Recording the
    /// panel's hover here keeps the panel up until the pointer actually leaves, rather than
    /// waiting for a move that may never come.
    fn rail_hover(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if !hovered || self.sidebar != SidebarMode::IconOnly {
            return;
        }

        self.hover_floating(FloatingRegion::Panel, true, cx);
    }

    /// Feeds one hover region to the floating sidebar and schedules or calls off its close.
    ///
    /// Every region comes here because they all answer the same question: the panel stays
    /// up while any of them holds the pointer. Which listener GPUI dispatches first is
    /// not assumed — the state machine decides from the flags the listeners leave behind.
    fn hover_floating(&mut self, region: FloatingRegion, hovered: bool, cx: &mut Context<Self>) {
        // The toggle floats the preview only while the sidebar is hidden: in the drawn
        // states the toggle changes the mode instead. The panel floats over the icon-only
        // rail and over the hidden preview.
        if matches!(region, FloatingRegion::Toggle) && self.sidebar != SidebarMode::Hidden {
            return;
        }
        if matches!(region, FloatingRegion::Panel) && self.sidebar == SidebarMode::Full {
            return;
        }

        let was_open = self.floating.is_open();
        let close = match region {
            FloatingRegion::Toggle => self.floating.hover_toggle(hovered),
            FloatingRegion::Panel => self.floating.hover_panel(hovered),
        };
        if close {
            self.schedule_floating_close(cx);
        } else {
            self.floating_close = None;
            self.close_on_next_move = false;
        }
        if !was_open && self.floating.is_open() {
            // The rows of this opening get a namespace of their own. The docked icon-only
            // rail is drawn from the same workspace rows while the preview is up, so without
            // one its keyed state would be shared with the floating panel's rows.
            self.popup_scope = self.popup_scope.wrapping_add(1);
            // Where the preview comes from decides how it comes in, and that does not change
            // while it is on its way out.
            self.preview_from_rail = self.sidebar == SidebarMode::IconOnly;
        }
        if self.floating.is_open() != was_open {
            cx.notify();
        }
    }

    /// Whether the window's layer has an open context menu.
    ///
    /// The panel stays up while the answer is yes, whichever menu it is: a context menu is
    /// a window-level surface, and the menu that holds the panel is the one the user is
    /// reading, not necessarily a menu the panel itself opened.
    fn has_context_menu(&self, cx: &App) -> bool {
        self.layer
            .as_ref()
            .and_then(|layer| layer.upgrade())
            .is_some_and(|layer| layer.read(cx).has_context_menu(cx))
    }

    /// Takes the window's context menu down, if there is one.
    fn dismiss_context_menu(&mut self, cx: &mut Context<Self>) {
        if let Some(layer) = self.layer.as_ref().and_then(|layer| layer.upgrade()) {
            layer.update(cx, |layer, cx| layer.dismiss_context_menu(cx));
        }
    }

    /// Reconsiders a close that was waiting for the pointer to move.
    ///
    /// A context menu keeps the panel up while it is open, and the menu covers the panel:
    /// when the menu closes there may be no hover event left to hear. The first mouse move
    /// afterwards is when the pointer's position is known again. If it is still inside the
    /// panel's rectangle the panel stays, and the panel's own hover listener is what says
    /// whether the pointer is on the panel or on something covering it; a move outside the
    /// rectangle closes the panel.
    fn pointer_moved(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.close_on_next_move {
            return;
        }
        if self.has_context_menu(cx) {
            return;
        }
        if !self.floating.is_open() {
            self.close_on_next_move = false;
            return;
        }

        if self.floating_bounds.get().contains(&position) {
            // The rectangle is the sidebar's, not the panel's: the title-bar toggle and a
            // dialog over the panel are inside it without the panel being hovered. The
            // panel's hover listener is the only thing that knows the difference, so the
            // wait is left standing until it reports, or until the pointer leaves.
            self.floating_close = None;
            return;
        }

        self.close_on_next_move = false;
        self.floating.hover_panel(false);
        self.floating_close = None;
        if self.floating.should_close() {
            self.floating.reset();
            cx.notify();
        }
    }

    /// Closes the floating sidebar once the pointer has had time to reach it.
    fn schedule_floating_close(&mut self, cx: &mut Context<Self>) {
        if !self.floating.should_close() {
            return;
        }

        // A close that is already waiting for the next move is not reconsidered by a hover
        // report: the menu covered the panel, and a leave reported while the pointer is
        // still inside the panel's rectangle must not start the ordinary close delay.
        if self.close_on_next_move {
            self.floating_close = None;
            return;
        }

        // A context menu holds the panel for as long as it is open, and its close is not a
        // hover: there is nothing to close now. The next move decides, once the menu is
        // gone and the pointer's position is known again.
        if self.has_context_menu(cx) {
            self.floating_close = None;
            self.close_on_next_move = true;
            return;
        }

        self.close_on_next_move = false;
        // Replacing the task is what cancels the previous close: a dropped task never runs,
        // and the timer it was waiting on is released with it. The timer checks the state
        // again rather than trusting the schedule: the pointer may have come back by the
        // time it fires, and a menu may have opened since.
        self.floating_close = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FLOATING_CLOSE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if this.close_on_next_move {
                    return;
                }

                if this.has_context_menu(cx) {
                    this.close_on_next_move = true;
                    return;
                }

                if this.floating.should_close() {
                    this.close_on_next_move = false;
                    this.floating.reset();
                    cx.notify();
                }
            });
        }));
    }

    /// Dismisses the floating panel on a press outside it.
    fn dismiss_floating(&mut self, cx: &mut Context<Self>) {
        self.dismiss_context_menu(cx);
        self.floating_close = None;
        self.close_on_next_move = false;
        if self.floating.dismiss() {
            cx.notify();
        }
    }

    /// The toggle that walks the sidebar through its three states.
    fn sidebar_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = t!(cx, "window.sidebar.toggle");

        // The toggle takes the pointer explicitly, because the title bar under it is the
        // window's drag area. The occlusion is on this wrapper rather than on the button: GPUI
        // stops its hover hit test at the first occluding element, so a button that blocked the
        // pointer could never open the tooltip it was given.
        div()
            .id("title-bar-sidebar-toggle")
            .occlude()
            .on_hover(cx.listener(|this, hovered, _, cx| {
                this.hover_floating(FloatingRegion::Toggle, *hovered, cx)
            }))
            .child(
                Button::new(TITLE_BAR_SIDEBAR_TOGGLE)
                    .ghost()
                    .h(SIDEBAR_ITEM_SIZE)
                    .w(SIDEBAR_ITEM_SIZE)
                    .justify_center()
                    .debug_selector(|| TITLE_BAR_SIDEBAR_TOGGLE.into())
                    .aria_label(label.clone())
                    .with_tooltip(label)
                    .child(
                        div()
                            .debug_selector(|| TITLE_BAR_SIDEBAR_TOGGLE_ICON.into())
                            .child(Icon::new(IconKind::Sidebar).size(px(16.0))),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_sidebar(cx))),
            )
    }

    /// The window's title bar: a transparent strip drawn over everything the window shows.
    ///
    /// It has no background of its own in the design, so the sidebar's reaches the top of the
    /// window under it and the page's shows through beside it. It is temporarily tinted while
    /// its bounds are being debugged; that tint is not part of the design. Being drawn last,
    /// it is also what the pointer finds first across the top of the window, which is who
    /// drags the window.
    fn title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title_bar = div()
            .id("title-bar")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(TITLE_BAR_HEIGHT)
            .flex()
            .flex_row()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    // The traffic lights the system draws in the window's top-left corner are
                    // what the macOS inset clears; elsewhere the toggle is all that is there.
                    .pl(if cfg!(target_os = "macos") {
                        TOGGLE_INSET_MACOS
                    } else {
                        TOGGLE_INSET
                    })
                    .child(self.sidebar_toggle(cx)),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .pr_3()
                    .child(self.updater.clone())
                    .when(!cfg!(target_os = "macos"), |this| {
                        this.child(WindowSystemButton::minimize())
                    })
                    .when(!cfg!(target_os = "macos"), |this| {
                        this.child(WindowSystemButton::maximize())
                    })
                    .when(!cfg!(target_os = "macos"), |this| {
                        this.child(WindowSystemButton::close())
                    }),
            )
            // A press on the title bar is a press outside the floating panel — the toggle is
            // the one exception, and its press changes the mode, which resets the panel.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.dismiss_floating(cx)),
            )
            .window_control_area(WindowControlArea::Drag);

        // AppKit no longer drags this window from its native titlebar (see
        // `WindowOptions::app_owns_titlebar_drag`), so the title bar moves the window
        // itself. The move starts on the first mouse move instead of the press, which
        // leaves clicks on the toggle working.
        title_bar.when(cfg!(target_os = "macos"), |this| {
            this.on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.should_move = true;
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.should_move = false;
                }),
            )
            .on_mouse_down_out(cx.listener(|this, _, _, _| {
                this.should_move = false;
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, _| {
                if this.should_move && event.dragging() {
                    this.should_move = false;
                    window.start_window_move();
                }
            }))
            .on_click(|event, window, _| {
                if event.standard_click() && event.click_count() == 2 {
                    window.titlebar_double_click();
                }
            })
        })
    }

    /// The sidebar floating over the page, drawn while the hidden sidebar's toggle or the
    /// icon-only rail asks for it, and while it moves away once it was let go.
    ///
    /// Where it comes from decides how it moves. The toggle of the hidden state has nothing
    /// to grow out of, so its preview slides in from the window's left edge. The icon-only
    /// rail is the panel at its narrow width already, so its preview grows out of the rail:
    /// it starts as the rail, widens into the labelled panel, and narrows back onto the rail
    /// when it closes. `progress` is how far it has come — 0 at the rail or outside the
    /// window, 1 as the full panel — and `open` says whether it is still the panel the
    /// pointer can use.
    ///
    /// It is the full panel, not the state's own: a floating panel has room for its labels,
    /// which is the reason the pointer asks for it. It sits under the title bar, so the toggle
    /// that floated it stays clickable. The menus its items open belong to the window layer,
    /// not to this panel; the window asks the layer whether one is up and holds the panel
    /// there while the answer is yes.
    fn floating_sidebar(
        &self,
        open: bool,
        progress: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let shadow = cx.theme().components.menu.shadow.hsla;
        let floating_bounds = self.floating_bounds.clone();

        // The docked panel is taking this preview's place: it has grown to the full width
        // behind the preview, so the preview keeps the width it was showing. What the user
        // is looking at then stays where it is and the page beside it is what makes room.
        let handoff = !open && self.preview_from_rail && self.sidebar == SidebarMode::Full;
        let (left, width) = if self.preview_from_rail {
            let width = if handoff {
                SIDEBAR_FULL_WIDTH
            } else {
                SIDEBAR_ICON_WIDTH + (SIDEBAR_FULL_WIDTH - SIDEBAR_ICON_WIDTH) * progress
            };
            (px(0.0), width)
        } else {
            (SIDEBAR_FULL_WIDTH * (progress - 1.0), SIDEBAR_FULL_WIDTH)
        };
        // The preview is the panel the pointer uses while it is up, and while the docked
        // panel takes its place it is still the same rows at the same width. Only a preview
        // that is moving away from the panel it covers takes no presses.
        let live = open || handoff;

        div()
            .id("sidebar-floating")
            .debug_selector(|| SIDEBAR_FLOATING.into())
            .absolute()
            .left(left)
            .top_0()
            .bottom_0()
            .w(width)
            // The panel's bounds are what the next pointer move after a context menu closes
            // is measured against: the menu covers the panel, so its hover cannot say
            // whether the pointer came back.
            .on_prepaint(move |bounds, _, _| floating_bounds.set(bounds))
            .shadow(vec![
                box_shadow(0., 1., 2., 0., shadow),
                box_shadow(0., 3., 6., -2., shadow),
            ])
            .when(live, |this| {
                this.occlude().on_hover(cx.listener(|this, hovered, _, cx| {
                    this.hover_floating(FloatingRegion::Panel, *hovered, cx)
                }))
            })
            // The same panel takes nothing of its own content while it moves away: what it
            // draws is on its way out, and a press that lands on it must not reach a row that
            // is already closing. The guard is a capture-phase listener, so it runs before the
            // content's own handlers.
            .when(!live, |this| {
                this.capture_any_mouse_down(|_, _, cx| cx.stop_propagation())
            })
            .child(
                Sidebar::new(
                    SidebarMode::Full,
                    width,
                    self.workspaces.clone(),
                    self.profile.clone(),
                )
                .popup_scope(self.popup_scope),
            )
    }
}

/// Writes the sidebar state to the settings.
///
/// The interface preferences take no part in the configuration events: nothing subscribes to
/// them, so the write is silent, and the window that draws the state is what carries it to
/// the next frame. It is written where it changes, which is what makes it the state the next
/// start opens with.
fn store_sidebar_mode(mode: SidebarMode, cx: &mut App) {
    cx.update_global::<ConfigManager, _>(|manager, cx| {
        manager.update(|config| config.ui.sidebar = mode, cx);
    });
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.layer.is_none() {
            self.layer = window
                .root::<Layer>()
                .flatten()
                .map(|layer| layer.downgrade());
        }

        let canvas_color = cx.theme().roles.surface.canvas.hsla;
        let text = cx.theme().roles.fg.default.hsla;

        // The sidebar's width follows its state rather than cutting to it: the panel and the
        // page beside it move together, so a press of the toggle reads as one movement. The
        // hidden state is a width like the other two rather than a panel that vanishes.
        let docked_width = transition(
            ("sidebar", "docked-width"),
            sidebar_width(self.sidebar),
            Transition::new(SIDEBAR_ANIMATION).easing(Easing::EaseOut),
            window,
            cx,
        );
        // The panel is drawn while it collapses out of the full state, so it keeps the
        // labels it was showing rather than switching to an icon rail on its way to nothing.
        let mut panel = Sidebar::new(
            self.sidebar,
            docked_width,
            self.workspaces.clone(),
            self.profile.clone(),
        );
        // The icon-only rail is the collapsed full panel: a pointer on its content asks for
        // the panel, which is drawn over it.
        if self.sidebar == SidebarMode::IconOnly {
            panel = panel.on_content_hover(
                cx.listener(|this, hovered, _, cx| this.rail_hover(*hovered, cx)),
            );
        }

        // The preview's presence is its movement: it is drawn from the frame it starts
        // entering until the frame it has finished leaving, and how far it has come decides
        // where and how wide it is drawn.
        let preview_open = self.floating.is_open();
        let preview = Presence::new(("sidebar", "preview"), preview_open)
            .transition(Transition::new(SIDEBAR_ANIMATION).easing(Easing::EaseOut))
            .sample(window, cx);

        div()
            .relative()
            .size_full()
            .flex()
            .flex_row()
            .bg(canvas_color)
            .text_color(text)
            .child(panel)
            .child(
                div()
                    .debug_selector(|| PAGE.into())
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    // A press on the page is a press outside the floating panel: the preview
                    // is dismissed at once rather than waiting out its close delay. The panel
                    // and its popups are drawn over this column, so their presses never
                    // reach it.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.dismiss_floating(cx)),
                    )
                    // The title bar is drawn over the window rather than laid out beside the
                    // page, so the page leaves its height free at the top.
                    .child(div().flex_none().h(TITLE_BAR_HEIGHT))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .p_1()
                            .child(self.page_market.clone()),
                    ),
            )
            .when(popup_should_render(preview_open, preview), |this| {
                this.child(self.floating_sidebar(preview_open, preview.progress, cx))
            })
            .child(self.title_bar(cx))
            // A window-level move listener, not a hitbox: the first move after a context
            // menu closes is what decides whether the panel that waited for it goes away,
            // and that move may belong to an alert over the page. The canvas paints nothing;
            // it exists to register the listener for the next frame.
            .child(
                canvas(|_, _, _| (), {
                    let this = cx.weak_entity();
                    move |_, _, window, _| {
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase == DispatchPhase::Bubble
                                && let Some(this) = this.upgrade()
                            {
                                this.update(cx, |this, cx| this.pointer_moved(event.position, cx));
                            }
                        });
                    }
                })
                .absolute()
                .top_0()
                .left_0()
                .w(px(0.0))
                .h(px(0.0)),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };

    use app_config::settings::{Config, Language};
    use app_config_provider::{manager::ConfigManager, store::ConfigLoadError};
    use base_ui::{layer::Layer, style};
    use gpui_kit::{
        AppContext, Context, IntoElement, Modifiers, MouseButton, Pixels, Point, Render, Styled,
        TestAppContext, VisualTestContext, Window, div, point, px, size,
    };
    use i18n::language::codes::LanguageCode;
    use market_participants_connector::connector::MarketParticipantsConnector;
    use network::transport::hub::{TransportHub, TransportPolicy};
    use updater_ui::global::UpdaterDeps;
    use widget_tokio_runtime::runtime::Runtime;

    use super::{
        ConfigRecovery, FLOATING_CLOSE_DELAY, PAGE, RootWindow, SIDEBAR_ANIMATION,
        SIDEBAR_FLOATING, SidebarMode, TITLE_BAR_SIDEBAR_TOGGLE, TITLE_BAR_SIDEBAR_TOGGLE_ICON,
        announce_config_recovery, store_sidebar_mode,
    };
    use crate::windows::root::{
        components::sidebar::{SIDEBAR_FULL_WIDTH, SIDEBAR_ICON_WIDTH, SIDEBAR_ITEM_SIZE},
        test_support::install_service_connector,
    };

    const PANEL: &str = "BASE_UI_ALERT";
    const OK: &str = "BASE_UI_ALERT_OK";
    const CONFIG_FILE_NAME: &str = "config.json";

    /// The docked panel's debug selector, which the sidebar module keeps to itself: the
    /// window's tests name the same string the panel is drawn with.
    const SIDEBAR: &str = "SIDEBAR";

    /// The menu item a workspace row's right click opens, named by the entry's id.
    const RENAME_ENTRY: &str = "TEST_MENU_ITEM_rename";

    /// The workspace the default configuration starts with, in English.
    const WORKSPACE_ROW: &str = "SIDEBAR_ITEM_Workspace 1";

    /// The account entry at the foot of the panel.
    const PROFILE_ENTRY: &str = "SIDEBAR_FOOTER_PROFILE";

    /// The account menu the entry opens, and its signed-out entry.
    const PROFILE_MENU: &str = "TEST_PROFILE_MENU";
    const SIGN_IN_ENTRY: &str = "TEST_MENU_ITEM_sign-in";

    /// The name the full panel draws inside a workspace row.
    const WORKSPACE_NAME: &str = "SIDEBAR_WORKSPACE_NAME";

    /// The labels the full panel draws beside the marks that stay in both states.
    const ADD_WORKSPACE_LABEL: &str = "SIDEBAR_NEW_WORKSPACE_LABEL";
    const SETTINGS_LABEL: &str = "SIDEBAR_FOOTER_SETTINGS_LABEL";
    const PROFILE_LABEL: &str = "SIDEBAR_FOOTER_PROFILE_LABEL";

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

    /// A temporary directory of the test's own, under the workspace's build output.
    ///
    /// The system temporary directory is not usable here: a test that runs behind a
    /// restricted integrity token cannot create directories in `%TEMP%`, which leaves every
    /// test about a file on disk failing for a reason that has nothing to do with the code
    /// under test. The build output is the workspace's own writable scratch space.
    fn test_temp(name: &str) -> tempfile::TempDir {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("this crate lives two directories below the workspace root");
        let root = workspace.join("target").join("test-temp").join("kytrix");
        std::fs::create_dir_all(&root).expect("the test temporary directory should be created");
        tempfile::Builder::new()
            .prefix(name)
            .tempdir_in(&root)
            .expect("a temporary directory should be created")
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

    /// Advances the animation clock by `by`, and draws the frame it asks for.
    fn animate(cx: &mut VisualTestContext, by: Duration) {
        cx.executor().advance_clock(by);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// Lets the window's colour transitions and the sidebar's movements finish, so a control
    /// or a panel the test has just acted on is drawn in its settled state.
    ///
    /// It advances past [`FLOATING_CLOSE_DELAY`] too, so a preview the pointer has left and
    /// nothing holds has started sliding away by the frame it draws.
    fn settle(cx: &mut VisualTestContext) {
        animate(cx, SIDEBAR_ANIMATION + Duration::from_millis(80));
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the window should draw {selector}"))
            .center();
        cx.simulate_click(target, Modifiers::none());
        settle(cx);
    }

    /// Presses the right button on what `selector` names, and draws the menu it opens.
    fn right_click(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the window should draw {selector}"))
            .center();
        cx.simulate_mouse_down(target, MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(target, MouseButton::Right, Modifiers::none());
        settle(cx);
    }

    /// Opens the window the application opens, with the real root view and the settings
    /// already on `sidebar`.
    ///
    /// Every global the root's own views read is installed the way the composition root
    /// installs it: the sidebar's account menu is built over the connector's signed-out
    /// in-memory test session, the market page's participants menu is built over its
    /// connector, and the title bar draws the updater.
    fn root_window(cx: &mut TestAppContext, sidebar: SidebarMode) -> &mut VisualTestContext {
        // The account menu's session query is answered by the test connector's auth actor on
        // its own "kytrix-auth-session" thread. Parking lets the deterministic scheduler wait
        // for that answer instead of treating activity from another thread as a test that made
        // its own scheduling; it is an accommodation for that thread, not a sleep.
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_kit::base::init(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            widget_registry::catalogue::install(cx).expect("the catalogue should install");
            style::theme::install(cx);
            cx.set_global(LanguageCode::En);
            let mut config = Config::default();
            config.language = Language::En;
            config.ui.sidebar = sidebar;
            let manager = ConfigManager::in_memory(config, cx);
            cx.set_global(manager);
            widget_tokio_runtime::global::install(cx).expect("the test runtime should install");
            install_service_connector(cx);
            MarketParticipantsConnector::install(cx)
                .expect("the market participants connector should install");
            updater_ui::global::install(
                cx,
                UpdaterDeps {
                    transport: Arc::new(TransportHub::new(TransportPolicy::default())),
                    runtime: Runtime::for_test(),
                },
            );
        });
        let (_, cx) = cx.add_window_view(|window, cx| {
            Layer::new(cx.new(|cx| RootWindow::new(window, None, cx)))
        });
        cx.simulate_resize(size(px(1280.0), px(960.0)));
        cx.run_until_parked();

        cx
    }

    /// The sidebar state the settings hold now, which is what the window drew from.
    fn sidebar_mode(cx: &mut VisualTestContext) -> SidebarMode {
        cx.update(|_, cx| cx.global::<ConfigManager>().read().ui.sidebar)
    }

    fn drawn(cx: &mut VisualTestContext, selector: &'static str) -> gpui_kit::Bounds<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("the window should draw {selector}"))
    }

    /// Moves the pointer to `at`, and draws the frame the move asks for.
    fn hover_at(cx: &mut VisualTestContext, at: Point<Pixels>) {
        cx.simulate_mouse_move(at, None, Modifiers::none());
        settle(cx);
    }

    /// Moves the pointer to the middle of what `selector` names, and draws the frame the
    /// move asks for.
    fn hover(cx: &mut VisualTestContext, selector: &'static str) {
        let at = drawn(cx, selector).center();
        hover_at(cx, at);
    }

    /// Presses the left button at `at`, and draws the frame the press asks for.
    fn click_at(cx: &mut VisualTestContext, at: Point<Pixels>) {
        cx.simulate_click(at, Modifiers::none());
        settle(cx);
    }

    /// Lets the floating panel's close delay pass and the movement it plays on its way out
    /// finish, and draws the frames those ask for.
    ///
    /// The delay is drawn first because it is what starts the movement: the frame it ends on
    /// is the movement's first one, and only after that does it have time to run.
    fn settle_floating(cx: &mut VisualTestContext) {
        animate(cx, FLOATING_CLOSE_DELAY + Duration::from_millis(80));
        animate(cx, SIDEBAR_ANIMATION + Duration::from_millis(80));
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

    /// The sidebar state is a preference the settings hold: it is written where the toggle
    /// changes it, which is what the next start reads back.
    #[gpui_kit::test]
    fn the_sidebar_state_should_be_written_to_the_settings(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);
        });

        cx.update(|cx| {
            assert_eq!(
                cx.global::<ConfigManager>().read().ui.sidebar,
                SidebarMode::Full,
                "a fresh installation opens with the full sidebar"
            );

            store_sidebar_mode(SidebarMode::IconOnly, cx);

            assert_eq!(
                cx.global::<ConfigManager>().read().ui.sidebar,
                SidebarMode::IconOnly,
                "what the toggle advanced to is what the settings keep"
            );
        });
    }

    /// The sidebar state is the interface preference a restart reads back: the write the
    /// toggle makes is in the settings file, with the one wire value the setting stores.
    #[gpui_kit::test]
    fn the_sidebar_state_should_survive_a_restart_from_the_settings_file(cx: &mut TestAppContext) {
        let directory = test_temp("sidebar-state");
        let path = directory.path().join(CONFIG_FILE_NAME);

        cx.update(|cx| {
            let manager = ConfigManager::from_path(&path, cx)
                .expect("a missing settings file starts from defaults");
            cx.set_global(manager);
            store_sidebar_mode(SidebarMode::Hidden, cx);
        });

        // A second load of the same file is what the next start of the application does.
        cx.update(|cx| {
            let restarted = ConfigManager::from_path(&path, cx)
                .expect("the settings file the window wrote should decode");

            assert_eq!(
                restarted.read().ui.sidebar,
                SidebarMode::Hidden,
                "the next start reads the state the toggle left behind"
            );
            cx.set_global(restarted);
        });

        let stored = std::fs::read_to_string(&path).expect("the settings file should be written");
        let stored: serde_json::Value =
            serde_json::from_str(&stored).expect("the settings file should be JSON");
        assert_eq!(
            stored["ui"]["sidebar"],
            serde_json::json!("hidden"),
            "the state is stored under the wire value the setting defines"
        );
    }

    /// The title bar's toggle walks the sidebar through its three states — hidden, icon
    /// only, full, and round again — and every press stores the state it arrived at: the
    /// next start opens on whatever the last press left.
    #[gpui_kit::test]
    fn the_toggle_should_walk_hidden_icon_only_and_full_and_persist_each(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::Full);

        assert_eq!(
            drawn(cx, SIDEBAR).size.width,
            SIDEBAR_FULL_WIDTH,
            "the window starts on the state the settings hold"
        );
        assert_eq!(sidebar_mode(cx), SidebarMode::Full);

        // The cycle is hidden, icon only, full: from the full panel the next state is the
        // hidden one, which draws no docked panel at all.
        click(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        assert!(
            cx.debug_bounds(SIDEBAR).is_none(),
            "the press after the full panel hides the sidebar"
        );
        assert_eq!(
            sidebar_mode(cx),
            SidebarMode::Hidden,
            "the state the press arrived at is what the settings keep"
        );

        click(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        assert_eq!(
            drawn(cx, SIDEBAR).size.width,
            px(40.0),
            "the next press draws the item-and-padding icon-only panel"
        );
        assert_eq!(sidebar_mode(cx), SidebarMode::IconOnly);

        click(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        assert_eq!(
            drawn(cx, SIDEBAR).size.width,
            SIDEBAR_FULL_WIDTH,
            "the cycle comes back round to the full panel"
        );
        assert_eq!(sidebar_mode(cx), SidebarMode::Full);
    }

    /// A press of the toggle does not cut to the next state's width: the panel travels to it,
    /// and the page beside it has to make room for that movement rather than for its end.
    #[gpui_kit::test]
    fn the_panel_should_move_between_the_states_instead_of_cutting(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::Full);
        let toggle = drawn(cx, TITLE_BAR_SIDEBAR_TOGGLE).center();

        // The frame the press is dispatched on still draws the width the panel had.
        cx.simulate_click(toggle, Modifiers::none());
        assert_eq!(
            drawn(cx, SIDEBAR).size.width,
            SIDEBAR_FULL_WIDTH,
            "the movement starts from the width the panel had"
        );
        assert!(
            cx.debug_bounds(WORKSPACE_NAME).is_some(),
            "the panel keeps the labels it was showing while it collapses"
        );

        animate(cx, SIDEBAR_ANIMATION / 2);
        let halfway = drawn(cx, SIDEBAR).size.width;
        assert!(
            halfway > px(0.0) && halfway < SIDEBAR_FULL_WIDTH,
            "the panel is between the full and hidden widths halfway through: {halfway}"
        );

        animate(cx, SIDEBAR_ANIMATION);
        assert!(
            cx.debug_bounds(SIDEBAR).is_none(),
            "the movement ends at the hidden state's no width"
        );

        // The next press grows the icon-only rail out of that nothing.
        cx.simulate_click(toggle, Modifiers::none());
        assert!(
            cx.debug_bounds(SIDEBAR).is_none(),
            "the growth starts from no width"
        );

        animate(cx, SIDEBAR_ANIMATION / 2);
        let halfway = drawn(cx, SIDEBAR).size.width;
        assert!(
            halfway > px(0.0) && halfway < SIDEBAR_ICON_WIDTH,
            "the rail is between the hidden and icon-only widths halfway through: {halfway}"
        );

        animate(cx, SIDEBAR_ANIMATION);
        assert_eq!(
            drawn(cx, SIDEBAR).size.width,
            SIDEBAR_ICON_WIDTH,
            "the movement ends at the icon-only state's width"
        );
    }

    /// The title bar's toggle and the sidebar's items are the same square: the button is a
    /// row's height, and the icon-only panel's rows are the button's width under it.
    #[gpui_kit::test]
    fn the_toggle_and_the_icon_only_items_should_be_the_same_square(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::IconOnly);

        let toggle = drawn(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        let item = drawn(cx, WORKSPACE_ROW);
        let icon = drawn(cx, TITLE_BAR_SIDEBAR_TOGGLE_ICON);

        assert_eq!(
            toggle.size,
            size(SIDEBAR_ITEM_SIZE, SIDEBAR_ITEM_SIZE),
            "the toggle is one item's square"
        );
        assert_eq!(
            item.size,
            size(SIDEBAR_ITEM_SIZE, SIDEBAR_ITEM_SIZE),
            "an icon-only item is the toggle's square"
        );
        assert_eq!(
            item.left(),
            toggle.left(),
            "the item's column starts under the toggle"
        );
        assert!(
            (icon.center().x - toggle.center().x).abs() <= px(0.5)
                && (icon.center().y - toggle.center().y).abs() <= px(0.5),
            "the toggle's mark is centred in its square: toggle {:?}, icon {:?}",
            toggle,
            icon
        );
    }

    /// The workspace section adds no top padding: the 32 px toggle leaves one item gap at
    /// the bottom of the 40 px title bar, and the button above the list adds none of its
    /// own, so the head and the list start on the same rhythm.
    #[gpui_kit::test]
    fn the_add_button_should_sit_one_item_gap_below_the_toggle(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::Full);

        let toggle = drawn(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        let add = drawn(cx, "SIDEBAR_NEW_WORKSPACE");
        let row = drawn(cx, WORKSPACE_ROW);

        assert_eq!(
            add.top() - toggle.bottom(),
            px(4.0),
            "the title bar leaves one item gap below the toggle, and the button above the list adds none"
        );
        assert_eq!(
            row.top() - add.bottom(),
            px(4.0) + px(1.0) + px(4.0),
            "the divider sits in its own gaps between the button and the list"
        );
    }

    /// The title bar's toggle is the hidden state's trigger: hovering it over a drawn
    /// sidebar floats nothing, because the panel it asks for is already docked. The
    /// icon-only state's trigger is its rail's content, not the toggle.
    #[gpui_kit::test]
    fn hovering_the_toggle_in_a_drawn_state_should_not_float_a_panel(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::Full);

        hover(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        assert_eq!(
            drawn(cx, SIDEBAR).size.width,
            SIDEBAR_FULL_WIDTH,
            "the full sidebar stays docked while the pointer is on the toggle"
        );
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the full sidebar has no preview to float"
        );

        // The full panel's presses walk it through hidden to the icon-only panel, the
        // other drawn state: its toggle hover must not float a preview either.
        click(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        click(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        hover_at(cx, point(px(600.0), px(300.0)));
        hover(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        assert_eq!(
            drawn(cx, SIDEBAR).size.width,
            px(40.0),
            "the icon-only sidebar stays docked while the pointer is on the toggle"
        );
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the icon-only sidebar has no preview to float"
        );
    }

    /// A press on the toggle while the hidden sidebar's preview is open changes the mode,
    /// and the mode change resets the preview: the next frame is the docked icon-only
    /// panel, with nothing floating over it.
    #[gpui_kit::test]
    fn a_toggle_press_while_the_preview_is_open_should_draw_the_next_state(
        cx: &mut TestAppContext,
    ) {
        let cx = root_window(cx, SidebarMode::Hidden);

        hover(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the hover floats the preview the press will leave"
        );

        click(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the mode change resets the preview the press arrived over"
        );
        assert_eq!(
            drawn(cx, SIDEBAR).size.width,
            px(40.0),
            "the next state is the docked icon-only panel"
        );
        assert_eq!(
            sidebar_mode(cx),
            SidebarMode::IconOnly,
            "the press stored the state it arrived at"
        );
    }

    /// The hidden sidebar is a preview the pointer asks for: it floats while the pointer is
    /// on the toggle or the panel, and closes a moment after it leaves both.
    #[gpui_kit::test]
    fn a_hidden_sidebar_should_float_on_hover_and_close_after_the_pointer_leaves(
        cx: &mut TestAppContext,
    ) {
        let cx = root_window(cx, SidebarMode::Hidden);

        assert!(
            cx.debug_bounds(SIDEBAR).is_none(),
            "the hidden sidebar draws no docked panel"
        );
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "nothing floats before the pointer asks for it"
        );

        hover(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the toggle under the pointer floats the preview"
        );

        hover_at(cx, point(px(110.0), px(300.0)));
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the panel stays while the pointer is on it"
        );

        hover_at(cx, point(px(600.0), px(300.0)));
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the preview closes once the pointer has left it, because nothing holds it"
        );
    }

    /// The hidden sidebar's preview slides in from the window's left edge rather than appearing
    /// over the page: the panel belongs to that edge, and the movement says where it came from.
    #[gpui_kit::test]
    fn the_hidden_preview_should_slide_in_from_the_window_edge(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::Hidden);
        let toggle = drawn(cx, TITLE_BAR_SIDEBAR_TOGGLE).center();

        cx.simulate_mouse_move(toggle, None, Modifiers::none());
        let entering = drawn(cx, SIDEBAR_FLOATING);
        assert!(
            entering.left() < px(0.0),
            "the preview starts outside the window's left edge: {entering:?}"
        );
        assert_eq!(
            entering.size.width, SIDEBAR_FULL_WIDTH,
            "the panel is the full width while it slides"
        );

        animate(cx, SIDEBAR_ANIMATION / 2);
        let halfway = drawn(cx, SIDEBAR_FLOATING);
        assert!(
            halfway.left() > entering.left() && halfway.left() < px(0.0),
            "the preview is between the edge and the panel's place halfway through: {halfway:?}"
        );

        animate(cx, SIDEBAR_ANIMATION);
        assert_eq!(
            drawn(cx, SIDEBAR_FLOATING).left(),
            px(0.0),
            "the movement ends against the window's left edge"
        );
    }

    /// The preview slides back out from where it rested, and only then leaves the tree: a
    /// preview that vanished on its first closing frame would have no way out.
    #[gpui_kit::test]
    fn the_hidden_preview_should_slide_back_out_when_it_closes(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::Hidden);
        let toggle = drawn(cx, TITLE_BAR_SIDEBAR_TOGGLE).center();

        cx.simulate_mouse_move(toggle, None, Modifiers::none());
        settle(cx);
        // The pointer leaves both regions; the close delay runs out inside the settle, which
        // is what starts the slide the frame after it draws is on.
        cx.simulate_mouse_move(point(px(600.0), px(300.0)), None, Modifiers::none());
        settle(cx);

        let leaving = drawn(cx, SIDEBAR_FLOATING);
        assert!(
            leaving.left() < px(0.0),
            "the panel is still drawn while it slides away: {leaving:?}"
        );

        animate(cx, SIDEBAR_ANIMATION);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the slide ends with no preview left to draw"
        );
    }

    /// A preview on its way out is still drawn, but nothing it draws is still a control: a
    /// press that lands on its add button must not open the alert behind it.
    #[gpui_kit::test]
    fn a_press_on_the_leaving_preview_should_not_reach_its_controls(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::Hidden);
        let toggle = drawn(cx, TITLE_BAR_SIDEBAR_TOGGLE).center();

        cx.simulate_mouse_move(toggle, None, Modifiers::none());
        settle(cx);

        // The pointer leaves both regions and the close delay runs out: the preview is halfway
        // out of the window, with its add button still drawn.
        cx.simulate_mouse_move(point(px(600.0), px(300.0)), None, Modifiers::none());
        animate(cx, FLOATING_CLOSE_DELAY + Duration::from_millis(80));
        let leaving = drawn(cx, SIDEBAR_FLOATING);
        assert!(
            leaving.left() < px(0.0),
            "the preview is on its way out: {leaving:?}"
        );

        // The add button is the first row of the panel's content, 200 px into its own width.
        let at = point(leaving.left() + px(200.0), px(60.0));
        cx.simulate_click(at, Modifiers::none());
        settle(cx);

        assert!(
            cx.debug_bounds(PANEL).is_none(),
            "the button of a preview that is leaving must not open its alert"
        );
    }

    /// The icon-only rail is the other collapsed state: a pointer on one of its items floats
    /// the full panel over it, the way the hidden sidebar's toggle does, and the panel closes
    /// a moment after the pointer leaves it.
    #[gpui_kit::test]
    fn an_icon_only_item_should_float_the_full_panel_over_the_rail(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::IconOnly);

        assert_eq!(
            drawn(cx, SIDEBAR).size.width,
            px(40.0),
            "the window starts on the icon-only rail"
        );
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "nothing floats before the pointer reaches an item"
        );

        hover(cx, WORKSPACE_ROW);
        assert_eq!(
            drawn(cx, SIDEBAR_FLOATING).size.width,
            SIDEBAR_FULL_WIDTH,
            "the item under the pointer floats the full panel over the rail"
        );
        assert!(
            cx.debug_bounds(WORKSPACE_NAME).is_some(),
            "the floating panel is the labelled one"
        );

        hover_at(cx, point(px(110.0), px(300.0)));
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the panel stays while the pointer is on it"
        );

        hover_at(cx, point(px(600.0), px(300.0)));
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the panel closes once the pointer has left it, because nothing holds it"
        );
    }

    /// The icon-only rail is the panel at its narrow width, so its preview grows out of the
    /// rail rather than sliding in from the window's edge: at the first frame it is the rail,
    /// and leaving narrows it back onto the rail.
    #[gpui_kit::test]
    fn the_icon_only_preview_should_grow_out_of_the_rail(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::IconOnly);
        let item = drawn(cx, WORKSPACE_ROW).center();

        cx.simulate_mouse_move(item, None, Modifiers::none());
        let entering = drawn(cx, SIDEBAR_FLOATING);
        assert_eq!(
            entering.left(),
            px(0.0),
            "the preview grows where the rail is, not from the window's edge"
        );
        assert_eq!(
            entering.size.width, SIDEBAR_ICON_WIDTH,
            "the preview starts as the rail it grows out of"
        );
        assert_eq!(
            drawn(cx, WORKSPACE_NAME).size.width,
            px(0.0),
            "the labels the full panel draws have no room at the rail's width"
        );

        animate(cx, SIDEBAR_ANIMATION / 2);
        let halfway = drawn(cx, SIDEBAR_FLOATING).size.width;
        assert!(
            halfway > SIDEBAR_ICON_WIDTH && halfway < SIDEBAR_FULL_WIDTH,
            "the preview is between the rail and the full panel halfway through: {halfway}"
        );

        animate(cx, SIDEBAR_ANIMATION);
        assert_eq!(
            drawn(cx, SIDEBAR_FLOATING).size.width,
            SIDEBAR_FULL_WIDTH,
            "the movement ends at the full panel's width"
        );

        // Leaving the panel narrows the preview back onto the rail it grew out of.
        cx.simulate_mouse_move(point(px(600.0), px(300.0)), None, Modifiers::none());
        animate(cx, FLOATING_CLOSE_DELAY + Duration::from_millis(80));
        let leaving = drawn(cx, SIDEBAR_FLOATING);
        assert!(
            leaving.size.width < SIDEBAR_FULL_WIDTH && leaving.size.width > SIDEBAR_ICON_WIDTH,
            "the preview is on its way back to the rail: {leaving:?}"
        );
        assert_eq!(
            leaving.left(),
            px(0.0),
            "the preview narrows where it is rather than sliding away"
        );

        animate(cx, SIDEBAR_ANIMATION);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the movement ends with the rail alone"
        );
    }

    /// Expanding the sidebar while the rail's preview is up hands the panel over instead of
    /// drawing a second one: the preview keeps the width it was showing while the docked
    /// panel grows to it, so the page is what moves.
    #[gpui_kit::test]
    fn expanding_from_the_icon_only_preview_should_keep_the_panel_in_place(
        cx: &mut TestAppContext,
    ) {
        let cx = root_window(cx, SidebarMode::IconOnly);

        hover(cx, WORKSPACE_ROW);
        assert_eq!(
            drawn(cx, SIDEBAR_FLOATING).size.width,
            SIDEBAR_FULL_WIDTH,
            "the rail's preview is the full panel before the press"
        );

        let toggle = drawn(cx, TITLE_BAR_SIDEBAR_TOGGLE).center();
        cx.simulate_click(toggle, Modifiers::none());
        animate(cx, SIDEBAR_ANIMATION / 2);
        assert_eq!(
            drawn(cx, SIDEBAR_FLOATING).size.width,
            SIDEBAR_FULL_WIDTH,
            "the preview holds the width it was showing while the docked panel takes its place"
        );
        let page = drawn(cx, PAGE).left();
        assert!(
            page > SIDEBAR_ICON_WIDTH && page < SIDEBAR_FULL_WIDTH,
            "the page beside the panel is what makes room under it: {page}"
        );

        animate(cx, SIDEBAR_ANIMATION);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the preview leaves once the docked panel is the full width"
        );
        assert_eq!(
            drawn(cx, PAGE).left(),
            SIDEBAR_FULL_WIDTH,
            "the page made room for the docked panel"
        );
    }

    /// A press inside the floating panel does not hold it: the panel is a hover preview, and
    /// clicking one of its controls is not a reason to keep it after the pointer leaves. A
    /// context menu the press opens is held by the window layer for as long as it is up.
    #[gpui_kit::test]
    fn a_press_inside_should_not_hold_the_floating_panel(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::Hidden);

        hover(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        click(cx, WORKSPACE_ROW);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the panel is still up under the pointer that pressed it"
        );

        hover_at(cx, point(px(600.0), px(300.0)));
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the preview closes when the pointer leaves, press or no press"
        );
    }

    /// A press on the title bar over the open panel is dispatched to the title bar, which
    /// dismisses the panel. The next hover floats a fresh preview, and the pointer leaving
    /// that preview closes it.
    #[gpui_kit::test]
    fn a_press_on_the_title_bar_over_the_floating_panel_should_dismiss_it(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::Hidden);
        let toggle = drawn(cx, TITLE_BAR_SIDEBAR_TOGGLE).center();

        hover_at(cx, toggle);
        assert!(cx.debug_bounds(SIDEBAR_FLOATING).is_some());

        click_at(cx, point(px(150.0), px(20.0)));
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the press on the title bar dismisses the panel"
        );

        hover_at(cx, toggle);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the next hover floats a fresh preview"
        );
        hover_at(cx, point(px(600.0), px(300.0)));
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the next preview closes when the pointer leaves it"
        );
    }

    /// A press inside the icon-only preview does not hold it either: clicking a workspace is
    /// using the rail's preview, not opening a state, so the preview retracts as soon as the
    /// pointer leaves it.
    #[gpui_kit::test]
    fn a_press_inside_the_icon_only_preview_should_not_hold_it(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::IconOnly);

        hover(cx, WORKSPACE_ROW);
        assert!(cx.debug_bounds(SIDEBAR_FLOATING).is_some());

        // The row is a control of the preview: selecting it must not keep the preview up.
        click(cx, WORKSPACE_ROW);
        hover_at(cx, point(px(600.0), px(300.0)));
        settle_floating(cx);

        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the icon-only preview must close when the pointer leaves it, even after a press"
        );
    }

    /// The row's menu opens at the pointer, so it covers the row. A press inside the panel
    /// but outside the menu closes the menu and does not reach the panel under it. The
    /// pointer is still inside the panel's rectangle, so the preview stays; the next move
    /// is measured against the panel, and a move off it closes the preview.
    #[gpui_kit::test]
    fn closing_the_row_menu_should_keep_the_icon_only_preview(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::IconOnly);

        hover(cx, WORKSPACE_ROW);
        right_click(cx, WORKSPACE_ROW);
        assert!(
            cx.debug_bounds(RENAME_ENTRY).is_some(),
            "the right click opens the row's menu"
        );

        // The menu opens at the pointer, so the row is under the menu panel. A press inside
        // the panel but outside the menu lands on the menu's sheet: it closes the menu and
        // does not reach the panel under it.
        click_at(cx, point(px(20.0), px(400.0)));
        assert!(
            cx.debug_bounds(RENAME_ENTRY).is_none(),
            "the press closes the menu"
        );
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the preview stays: the press landed on the menu's sheet, inside the panel rectangle"
        );

        hover(cx, WORKSPACE_ROW);
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the first move after the menu closed is back on the panel, so the preview stays"
        );

        hover_at(cx, point(px(600.0), px(300.0)));
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "a move off the panel after the menu is gone closes the preview"
        );
    }

    /// A move inside the panel's rectangle but on the title bar's occluding toggle is not a
    /// move onto the panel: the old code forged a hover there and the preview stuck when the
    /// pointer later left. The wait must hold the preview while the pointer is in the
    /// rectangle, and the next move outside must close it.
    #[gpui_kit::test]
    fn an_occluded_move_inside_the_preview_rectangle_should_not_stick_it(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::IconOnly);

        hover(cx, WORKSPACE_ROW);
        right_click(cx, WORKSPACE_ROW);
        assert!(
            cx.debug_bounds(RENAME_ENTRY).is_some(),
            "the right click opens the row's menu"
        );

        // Leave the panel for the menu, which sets the wait, then press outside to close the
        // menu. The pointer is outside the panel and the wait is still armed.
        hover(cx, RENAME_ENTRY);
        click_at(cx, point(px(600.0), px(300.0)));
        assert!(
            cx.debug_bounds(RENAME_ENTRY).is_none(),
            "the press outside closes the menu"
        );
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the preview stays until the pointer moves after the menu closes"
        );

        // The toggle lies inside the panel's rectangle but occludes the panel: this is the
        // move that used to forge a hover. The wait must keep the preview here.
        hover(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the pointer is inside the panel's rectangle, and the wait holds it there"
        );

        hover_at(cx, point(px(600.0), px(300.0)));
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "an occluded move inside the rectangle must not leave the preview stuck after the \
             pointer leaves it"
        );
    }

    /// A row's menu is a context menu owned by the window layer, drawn outside the panel.
    /// Moving onto it leaves the panel but not the sidebar: the menu holds the preview up.
    /// Moving away does not close the menu — a context menu closes on a press — so the panel
    /// stays until the menu is dismissed, and then until the pointer moves again.
    #[gpui_kit::test]
    fn the_icon_only_previews_menu_should_keep_it_up_until_the_pointer_leaves(
        cx: &mut TestAppContext,
    ) {
        let cx = root_window(cx, SidebarMode::IconOnly);

        hover(cx, WORKSPACE_ROW);
        right_click(cx, WORKSPACE_ROW);
        assert!(
            cx.debug_bounds(RENAME_ENTRY).is_some(),
            "the right click opens the row's menu"
        );

        hover(cx, RENAME_ENTRY);
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the menu the pointer is on holds the preview past the close delay"
        );

        // Move off the menu without pressing: the menu is still open, so the preview stays.
        hover_at(cx, point(px(600.0), px(300.0)));
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "a menu that is still open holds the preview, wherever the pointer is"
        );
        assert!(
            cx.debug_bounds(RENAME_ENTRY).is_some(),
            "moving off a context menu does not close it"
        );

        // The press closes the menu. The preview waits for the next move, which is the first
        // moment the pointer's position is known after the menu covered the panel.
        click_at(cx, point(px(600.0), px(300.0)));
        assert!(
            cx.debug_bounds(RENAME_ENTRY).is_none(),
            "a press outside the menu closes it"
        );
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the preview stays until the pointer moves after the menu closes"
        );

        hover_at(cx, point(px(610.0), px(310.0)));
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the preview closes on the first move after the menu is gone"
        );

        hover(cx, WORKSPACE_ROW);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the rail floats the preview again"
        );
        assert!(
            cx.debug_bounds(WORKSPACE_ROW).is_some(),
            "the row under the pointer is drawn in the new opening"
        );
        assert!(
            cx.debug_bounds(RENAME_ENTRY).is_none(),
            "the menu that was closed is not part of the next opening"
        );
    }

    /// A menu that closes by choosing an entry is gone without the pointer moving. The panel
    /// has to release on the next move, not wait for a hover report the unmounted menu can no
    /// longer make.
    #[gpui_kit::test]
    fn choosing_a_row_menu_entry_should_release_the_icon_only_preview(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::IconOnly);

        hover(cx, WORKSPACE_ROW);
        right_click(cx, WORKSPACE_ROW);
        click(cx, RENAME_ENTRY);
        assert!(
            cx.debug_bounds(PANEL).is_some(),
            "renaming asks for the new name"
        );

        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the preview waits for the first move after the menu entry closes the menu"
        );

        // The alert's backdrop covers the panel. This move is inside the panel's rectangle
        // but not on the panel; the wait holds the preview, so the ordinary close delay must
        // not take it down from a hover the panel could not have.
        hover_at(cx, point(px(100.0), px(300.0)));
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the rectangle holds the preview while the pointer is over the alert"
        );

        hover_at(cx, point(px(1000.0), px(800.0)));
        settle(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the preview retracts on the first move after the menu that held it is gone"
        );
        assert!(
            cx.debug_bounds(PANEL).is_some(),
            "the alert the menu entry opened is not taken down with the preview"
        );
    }

    /// The account menu is the other popup the preview can leave the panel for: it opens
    /// beside the foot's entry, and the pointer on it holds the preview up just as a row's
    /// menu does. It closes on a press, and the preview waits for the next move after that.
    #[gpui_kit::test]
    fn the_icon_only_previews_account_menu_should_keep_it_up_until_the_pointer_leaves(
        cx: &mut TestAppContext,
    ) {
        let cx = root_window(cx, SidebarMode::IconOnly);

        hover(cx, WORKSPACE_ROW);
        hover(cx, PROFILE_ENTRY);
        click(cx, PROFILE_ENTRY);
        assert!(
            cx.debug_bounds(PROFILE_MENU).is_some(),
            "the foot's account entry opens the account menu"
        );

        hover(cx, SIGN_IN_ENTRY);
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the account menu the pointer is on holds the preview past the close delay"
        );

        hover_at(cx, point(px(600.0), px(300.0)));
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "a menu that is still open holds the preview, wherever the pointer is"
        );

        click_at(cx, point(px(600.0), px(300.0)));
        assert!(
            cx.debug_bounds(PROFILE_MENU).is_none(),
            "a press outside the account menu closes it"
        );
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the preview stays until the pointer moves after the menu closes"
        );

        hover_at(cx, point(px(610.0), px(310.0)));
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the preview closes on the first move after the account menu is gone"
        );
    }

    /// The row menu is a context menu owned by the window layer, so it is drawn outside the
    /// panel. The menu, not the press, is what keeps the panel up while the pointer is on it;
    /// a press outside closes the menu, and the next move closes the preview.
    #[gpui_kit::test]
    fn the_row_menu_should_hold_the_floating_panel_while_it_is_open(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::Hidden);

        hover(cx, TITLE_BAR_SIDEBAR_TOGGLE);
        assert!(cx.debug_bounds(SIDEBAR_FLOATING).is_some());

        // The default configuration starts with one workspace, named in English.
        hover(cx, WORKSPACE_ROW);
        right_click(cx, WORKSPACE_ROW);
        assert!(
            cx.debug_bounds(RENAME_ENTRY).is_some(),
            "the right click opens the row's menu"
        );

        hover(cx, RENAME_ENTRY);
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the panel the menu was opened from stays up past the close delay"
        );
        assert!(
            cx.debug_bounds(RENAME_ENTRY).is_some(),
            "the menu stays mounted while the pointer is on it"
        );
        assert!(
            cx.debug_bounds(WORKSPACE_ROW).is_some(),
            "the row stays mounted while the menu it opened is up"
        );

        click_at(cx, point(px(600.0), px(300.0)));
        assert!(
            cx.debug_bounds(RENAME_ENTRY).is_none(),
            "the press outside closes the menu"
        );
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_some(),
            "the press that closed the menu did not reach the panel under it"
        );

        hover_at(cx, point(px(610.0), px(310.0)));
        settle_floating(cx);
        assert!(
            cx.debug_bounds(SIDEBAR_FLOATING).is_none(),
            "the preview closes on the first move after the menu is gone"
        );
    }

    /// Every label of the full panel starts on the same column, and keeps it while the panel
    /// narrows: a mark that shrinks under the flex pressure would drag its label left on
    /// every frame of the movement, which reads as the row flickering.
    #[gpui_kit::test]
    fn the_full_panels_labels_should_hold_their_column_as_it_grows(cx: &mut TestAppContext) {
        let cx = root_window(cx, SidebarMode::IconOnly);
        let item = drawn(cx, WORKSPACE_ROW).center();
        cx.simulate_mouse_move(item, None, Modifiers::none());

        // The workspace row's mark sits in a box of its own, so its name is the column the
        // other labels have to keep.
        let column = drawn(cx, WORKSPACE_NAME).left();
        for selector in [ADD_WORKSPACE_LABEL, SETTINGS_LABEL, PROFILE_LABEL] {
            assert_eq!(
                drawn(cx, selector).left(),
                column,
                "{selector} starts on the name's column at the rail's width"
            );
        }

        animate(cx, SIDEBAR_ANIMATION / 2);
        for selector in [ADD_WORKSPACE_LABEL, SETTINGS_LABEL, PROFILE_LABEL] {
            assert_eq!(
                drawn(cx, selector).left(),
                column,
                "{selector} holds the column halfway through the growth"
            );
        }

        animate(cx, SIDEBAR_ANIMATION);
        for selector in [ADD_WORKSPACE_LABEL, SETTINGS_LABEL, PROFILE_LABEL] {
            assert_eq!(
                drawn(cx, selector).left(),
                column,
                "{selector} holds the column at the full width"
            );
        }
    }
}

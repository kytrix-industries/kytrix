//! The window's title bar: the shell's own chrome, drawn over everything the window shows.
//!
//! The window is frameless on Windows (E6): the platform draws no title bar, and this strip
//! is what the user sees instead. It carries the sidebar toggle, the market participants
//! menu, and — everywhere but macOS, where the system draws its traffic lights — the
//! window's minimize / maximize / close buttons.
//!
//! # Dragging the window
//!
//! The bar is not a drag handle by itself: the shell publishes the strip between its
//! controls as the window's drag region every frame ([`TitleBar::drag_regions`]), and the
//! platform answers `WM_NCHITTEST` over it with `HTCAPTION`, which is what keeps Aero Snap,
//! the system menu and double-click-to-maximize working. The controls sit above the region
//! and keep their own presses.
//!
//! # Rebuilding
//!
//! The bar is built once. Its maximize button is rebuilt when the window's maximized state
//! changes (the icon is the state), and its toggle is rebuilt when the language changes
//! (the label is catalogue copy). The market participants menu synchronizes itself and
//! follows the language on its own.

use base_ui::{
    kit::{button::Button, icon::IconKind, size::UnifiedSizable},
    style_builder::StyleBuilder,
    theme::handle::ThemeHandle,
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_ui::{host::MarketParticipantsHost, menu::MarketParticipants};
use smol_str::SmolStr;
use ui_core::{
    app::App,
    error::EngineError,
    event::Event,
    math::Rect,
    node::id::NodeId,
    window::{WindowId, WindowState},
};

use super::{
    intent::Intent,
    metrics,
    ui::{self, Intents},
};
use crate::composition::Composition;

/// The toggle's wrapper debug id, which the window's tests also find it by.
pub(crate) const TITLE_BAR_SIDEBAR_TOGGLE: &str = "TITLE_BAR_SIDEBAR_TOGGLE";

/// The debug id of the strip itself.
pub(crate) const TITLE_BAR: &str = "TITLE_BAR";

/// The window's system buttons.
pub(crate) const WINDOW_MINIMIZE: &str = "WINDOW_MINIMIZE";
pub(crate) const WINDOW_MAXIMIZE: &str = "WINDOW_MAXIMIZE";
pub(crate) const WINDOW_CLOSE: &str = "WINDOW_CLOSE";

/// Which window button a control is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SystemButton {
    Minimize,
    Maximize,
    Close,
}

/// The title bar and the controls it keeps.
pub(crate) struct TitleBar {
    window: WindowId,
    node: NodeId,
    left: NodeId,
    right: NodeId,
    intents: Intents,
    theme: ThemeHandle,
    translate: Translate,
    language: LanguageCode,
    /// The sidebar toggle (rebuilt on a language change).
    toggle: Option<Button>,
    /// The window's system buttons, kept for event dispatch and release.
    system_buttons: Vec<(SystemButton, Button)>,
    /// The market participants menu the bar draws, when the build has a connector.
    market: Option<MarketParticipants>,
    market_host: Option<MarketParticipantsHost>,
    /// Whether the window was maximized when the bar last synchronized.
    maximized: bool,
    /// The strip between the controls, published as the window's drag region.
    drag_regions: Vec<Rect>,
}

impl TitleBar {
    /// Builds the bar into `root` (the window's root node), drawn above the window's views.
    pub(crate) fn build(
        app: &mut App,
        window: WindowId,
        root: NodeId,
        composition: &Composition,
        intents: Intents,
    ) -> Result<Self, EngineError> {
        let node = ui::named(
            app,
            window,
            root,
            TITLE_BAR,
            StyleBuilder::new()
                .absolute()
                .top(0.0)
                .left(0.0)
                .right(0.0)
                .height(metrics::TITLE_BAR_HEIGHT)
                .flex_row()
                .items_center()
                .build(),
        )?;
        let left = ui::container(
            app,
            window,
            node,
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .flex_none()
                .height_full()
                .gap(metrics::CONTROL_GAP)
                .padding_x(metrics::bar_inset())
                .build(),
        )?;
        // The flexible strip between the controls is the window's drag region (E6). Its own
        // node is what the drag rectangles are computed from, so a control added to either
        // cluster narrows the region without the region's code changing.
        let drag_strip = ui::container(
            app,
            window,
            node,
            StyleBuilder::new()
                .flex_1()
                .min_width_0()
                .height_full()
                .build(),
        )?;
        let _ = drag_strip;
        let right = ui::container(
            app,
            window,
            node,
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .justify_end()
                .flex_none()
                .height_full()
                .gap(metrics::CONTROL_GAP)
                .padding_x(metrics::bar_right_inset())
                .build(),
        )?;

        let language = composition
            .hub
            .read(|config| crate::language::code(config.language));
        let maximized = app
            .window(window)
            .is_some_and(|target| target.state().maximized);

        let mut bar = Self {
            window,
            node,
            left,
            right,
            intents,
            theme: composition.theme.clone(),
            translate: composition.translate.clone(),
            language,
            toggle: None,
            system_buttons: Vec::new(),
            market: None,
            market_host: None,
            maximized,
            drag_regions: Vec::new(),
        };
        bar.build_toggle(app)?;
        bar.build_market_menu(app, composition)?;
        if metrics::draws_system_buttons() {
            for which in [
                SystemButton::Minimize,
                SystemButton::Maximize,
                SystemButton::Close,
            ] {
                let button = bar.build_system_button(app, which)?;
                bar.system_buttons.push((which, button));
            }
        }

        Ok(bar)
    }

    /// Builds the sidebar toggle into the left cluster.
    fn build_toggle(&mut self, app: &mut App) -> Result<(), EngineError> {
        let label = t_smol!(self.translate, "window.sidebar.toggle");
        let intents = self.intents.clone();
        let button = Button::new(
            app,
            self.theme.clone(),
            SmolStr::new_static("kytrix.titlebar.sidebar-toggle"),
        )
        .ghost()
        .small()
        .icon(IconKind::Sidebar)
        .tooltip_text(label.clone())
        .a11y_label(label)
        .on_click(move |ctx| ui::report(ctx, &intents, Intent::ToggleSidebar));
        // The wrapper is what the tests measure: it is the control's square, and it is what
        // a window drag region excludes from the drag strip.
        let wrapper = ui::named(
            app,
            self.window,
            self.left,
            TITLE_BAR_SIDEBAR_TOGGLE,
            StyleBuilder::new().build(),
        )?;
        button.build(app, self.window, wrapper)?;
        self.toggle = Some(button);

        Ok(())
    }

    /// Releases the toggle's component and removes its nodes before a rebuild.
    fn release_toggle(&mut self, app: &mut App) {
        if let Some(toggle) = self.toggle.take()
            && let Some(node) = toggle.node()
        {
            toggle.release(app);
            let _ = app.remove_node(self.window, node);
        }
    }

    /// Builds the market participants menu the title bar hosts.
    fn build_market_menu(
        &mut self,
        app: &mut App,
        composition: &Composition,
    ) -> Result<(), EngineError> {
        let Ok(host) =
            MarketParticipantsHost::from_services(std::rc::Rc::clone(composition.services()))
        else {
            log::warn!("the market participants menu is unavailable: no directory connector");
            return Ok(());
        };
        let mut menu = MarketParticipants::new(host.clone(), app);
        if let Err(error) = menu.build(app, self.window, self.left) {
            log::error!("the market participants menu could not be built: {error}");
            return Ok(());
        }
        self.market = Some(menu);
        self.market_host = Some(host);

        Ok(())
    }

    /// Builds one system button into the right cluster.
    fn build_system_button(
        &mut self,
        app: &mut App,
        which: SystemButton,
    ) -> Result<Button, EngineError> {
        let (label, icon, selector) = match which {
            SystemButton::Minimize => (
                t_smol!(self.translate, "window.system.minimize"),
                IconKind::Minus,
                WINDOW_MINIMIZE,
            ),
            SystemButton::Maximize if self.maximized => (
                t_smol!(self.translate, "window.system.restore"),
                IconKind::CopySimple,
                WINDOW_MAXIMIZE,
            ),
            SystemButton::Maximize => (
                t_smol!(self.translate, "window.system.maximize"),
                IconKind::Square,
                WINDOW_MAXIMIZE,
            ),
            SystemButton::Close => (
                t_smol!(self.translate, "window.system.close"),
                IconKind::X,
                WINDOW_CLOSE,
            ),
        };
        let intent = match which {
            SystemButton::Minimize => Intent::Minimize,
            SystemButton::Maximize => Intent::ToggleMaximize,
            SystemButton::Close => Intent::CloseWindow,
        };
        let key = match which {
            SystemButton::Minimize => "kytrix.titlebar.minimize",
            SystemButton::Maximize => "kytrix.titlebar.maximize",
            SystemButton::Close => "kytrix.titlebar.close",
        };
        let intents = self.intents.clone();
        let button = Button::new(app, self.theme.clone(), SmolStr::new_static(key))
            .ghost()
            .small()
            .icon(icon)
            .tooltip_text(label.clone())
            .a11y_label(label)
            .on_click(move |ctx| ui::report(ctx, &intents, intent.clone()));
        let wrapper = ui::named(
            app,
            self.window,
            self.right,
            selector,
            StyleBuilder::new().build(),
        )?;
        button.build(app, self.window, wrapper)?;

        Ok(button)
    }

    /// Runs one frame of the bar: the market menu, the window's state and the drag region.
    pub(crate) fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        if app.window(self.window).is_none() {
            return Ok(());
        }

        if let Some(market) = &mut self.market
            && let Err(error) = market.sync(app)
        {
            log::error!("the market participants menu could not be synchronized: {error}");
        }

        self.sync_window_state(app)?;
        self.sync_language(app)?;
        self.publish_drag_regions(app);

        Ok(())
    }

    /// Rebuilds the maximize button when the window's state changed.
    fn sync_window_state(&mut self, app: &mut App) -> Result<(), EngineError> {
        let Some(state): Option<WindowState> = app.window(self.window).map(|target| target.state())
        else {
            return Ok(());
        };
        if state.maximized == self.maximized {
            return Ok(());
        }
        self.maximized = state.maximized;

        if let Some(index) = self
            .system_buttons
            .iter()
            .position(|(which, _)| *which == SystemButton::Maximize)
        {
            let (_, old) = self.system_buttons.remove(index);
            if let Some(node) = old.node() {
                old.release(app);
                let _ = app.remove_node(self.window, node);
            }
        }
        let button = self.build_system_button(app, SystemButton::Maximize)?;
        self.system_buttons.push((SystemButton::Maximize, button));

        Ok(())
    }

    /// Rebuilds the toggle when the language changed.
    fn sync_language(&mut self, app: &mut App) -> Result<(), EngineError> {
        let language = self
            .market_host
            .as_ref()
            .map(|host| host.language(app))
            .unwrap_or(self.language);
        if language == self.language {
            return Ok(());
        }
        self.language = language;
        self.release_toggle(app);
        self.build_toggle(app)?;

        Ok(())
    }

    /// Publishes the strip between the controls as the window's drag region (E6).
    fn publish_drag_regions(&mut self, app: &mut App) {
        let Some(target) = app.window(self.window) else {
            return;
        };
        let tree = target.tree();
        let bounds = |node: NodeId| {
            tree.get(node)
                .map(|node| node.layout().bounds)
                .unwrap_or_default()
        };
        let bar = bounds(self.node);
        let left = bounds(self.left);
        let right = bounds(self.right);
        let gap = metrics::CONTROL_GAP;
        let start = left.origin.x + left.size.width + gap;
        let end = right.origin.x - gap;
        let regions = if end > start && bar.size.height > 0.0 {
            vec![Rect::new(start, bar.origin.y, end - start, bar.size.height)]
        } else {
            Vec::new()
        };
        if regions != self.drag_regions {
            self.drag_regions = regions.clone();
            if let Some(target) = app.window_mut(self.window) {
                target.set_drag_regions(&regions);
            }
        }
    }

    /// The drag region the bar published on the last frame.
    #[cfg(test)]
    pub(crate) fn drag_regions(&self) -> &[Rect] {
        &self.drag_regions
    }

    /// Forwards one platform event to the bar's kit controls.
    pub(crate) fn dispatch(&mut self, app: &mut App, event: &Event) {
        if let Some(toggle) = &self.toggle {
            let _ = toggle.dispatch(app, event);
        }
        for (_, button) in &self.system_buttons {
            let _ = button.dispatch(app, event);
        }
    }

    /// Releases every component and node the bar owns.
    pub(crate) fn release(&mut self, app: &mut App) {
        if let Some(mut market) = self.market.take() {
            market.release(app);
        }
        self.market_host = None;
        self.release_toggle(app);
        for (_, button) in std::mem::take(&mut self.system_buttons) {
            if let Some(node) = button.node() {
                button.release(app);
                let _ = app.remove_node(self.window, node);
            }
        }
        let _ = app.remove_node(self.window, self.node);
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    /// Every window button maps to the control the shell requests, and the shell's own
    /// buttons (toggle, sidebar, gallery) do not.
    #[test]
    fn the_window_buttons_should_map_to_window_controls() {
        assert_eq!(
            Intent::Minimize.window_control(),
            Some(ui_core::window::WindowControl::Minimize)
        );
        assert_eq!(
            Intent::ToggleMaximize.window_control(),
            Some(ui_core::window::WindowControl::ToggleMaximize)
        );
        assert!(Intent::CloseWindow.window_control().is_none());
        assert!(Intent::ToggleSidebar.window_control().is_none());
    }
}

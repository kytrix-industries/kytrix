use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_ui::{host::MarketParticipantsHost, menu::MarketParticipants};
use profile_ui::profile::ProfileMenu;
use smol_str::SmolStr;
use ui_core::{
    app::App,
    error::EngineError,
    event::Event,
    math::Rect,
    node::id::NodeId,
    paint::style_builder::StyleBuilder,
    window::{WindowId, WindowState},
};
use ui_kit::{
    kit::{button::Button, icon::IconKind, size::UnifiedSizable},
    theme::handle::ThemeHandle,
};
use updater_ui::{indicator::UpdateIndicator, updater::Updater};

use super::{
    intent::Intent,
    metrics,
    ui::{self, Intents},
};
use crate::composition::Composition;

pub(crate) const TITLE_BAR_SIDEBAR_TOGGLE: &str = "TITLE_BAR_SIDEBAR_TOGGLE";

pub(crate) const TITLE_BAR: &str = "TITLE_BAR";

pub(crate) const WINDOW_MINIMIZE: &str = "WINDOW_MINIMIZE";
pub(crate) const WINDOW_MAXIMIZE: &str = "WINDOW_MAXIMIZE";
pub(crate) const WINDOW_CLOSE: &str = "WINDOW_CLOSE";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SystemButton {
    Minimize,
    Maximize,
    Close,
}

pub(crate) struct TitleBar {
    window: WindowId,
    node: NodeId,
    left: NodeId,
    right: NodeId,
    intents: Intents,
    theme: ThemeHandle,
    translate: Translate,
    language: LanguageCode,
    toggle: Option<Button>,
    system_buttons: Vec<(SystemButton, Button)>,
    market: Option<MarketParticipants>,
    market_host: Option<MarketParticipantsHost>,
    profile: Option<ProfileMenu>,
    updater: Option<Updater>,
    indicator: Option<UpdateIndicator>,
    maximized: bool,
    drag_regions: Vec<Rect>,
}

impl TitleBar {
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
            profile: None,
            updater: None,
            indicator: None,
            maximized,
            drag_regions: Vec::new(),
        };
        bar.build_toggle(app)?;
        bar.build_market_menu(app, composition)?;
        bar.build_updater(app, composition)?;
        bar.build_profile(app, composition)?;
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

    fn release_toggle(&mut self, app: &mut App) {
        if let Some(toggle) = self.toggle.take()
            && let Some(node) = toggle.node()
        {
            toggle.release(app);
            let _ = app.remove_node(self.window, node);
        }
    }

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

    fn build_updater(
        &mut self,
        app: &mut App,
        composition: &Composition,
    ) -> Result<(), EngineError> {
        let updater = Updater::install(
            app,
            self.window,
            std::rc::Rc::clone(composition.services()),
            composition.updater_deps(),
        );
        let indicator = UpdateIndicator::mount(
            app,
            self.window,
            self.right,
            std::rc::Rc::clone(composition.services()),
            updater.clone(),
            "kytrix.updater",
        );
        self.updater = Some(updater);
        self.indicator = Some(indicator);

        Ok(())
    }

    fn build_profile(
        &mut self,
        app: &mut App,
        composition: &Composition,
    ) -> Result<(), EngineError> {
        let mut menu = ProfileMenu::new(composition.profile_host());
        menu.build(app, self.window, self.right)?;
        self.profile = Some(menu);

        Ok(())
    }

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

    pub(crate) fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        if app.window(self.window).is_none() {
            return Ok(());
        }

        if let Some(market) = &mut self.market
            && let Err(error) = market.sync(app)
        {
            log::error!("the market participants menu could not be synchronized: {error}");
        }
        if let Some(profile) = &mut self.profile
            && let Err(error) = profile.sync(app)
        {
            log::error!("the account control could not be synchronized: {error}");
        }

        self.sync_window_state(app)?;
        self.sync_language(app)?;
        self.publish_drag_regions(app);

        Ok(())
    }

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

    #[cfg(test)]
    pub(crate) fn drag_regions(&self) -> &[Rect] {
        &self.drag_regions
    }

    pub(crate) fn dispatch(&mut self, app: &mut App, event: &Event) {
        if let Some(toggle) = &self.toggle {
            let _ = toggle.dispatch(app, event);
        }
        if let Some(profile) = &mut self.profile {
            profile.dispatch(app, event);
        }
        for (_, button) in &self.system_buttons {
            let _ = button.dispatch(app, event);
        }
    }

    pub(crate) fn release(&mut self, app: &mut App) {
        if let Some(profile) = &mut self.profile {
            if let Some(node) = profile.trigger_node() {
                let _ = app.remove_node(self.window, node);
            }
            profile.release(app);
        }
        if let Some(indicator) = self.indicator.take() {
            indicator.release(app);
        }
        if let Some(updater) = self.updater.take() {
            updater.release(app);
        }
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

    #[cfg(test)]
    pub(crate) fn indicator(&self) -> Option<&UpdateIndicator> {
        self.indicator.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn updater(&self) -> Option<&Updater> {
        self.updater.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn profile(&self) -> Option<&ProfileMenu> {
        self.profile.as_ref()
    }
}

#[cfg(test)]
mod tests {

    use super::*;

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

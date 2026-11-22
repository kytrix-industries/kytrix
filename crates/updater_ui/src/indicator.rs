//! The top bar's update button.
//!
//! A kit ghost button with the update icon, built into the header row the composition
//! root hands it and drawn while an update is on offer — and at no other time, which
//! is the whole of what this feature says until the user asks for more: there is no
//! "nothing available" state to open and no failed check to report, because an update
//! nobody asked for is not something to interrupt anyone with.
//!
//! The button is not rebuilt every frame: it is built when the updater first offers a
//! release, kept while the offer stands, and removed (with its tooltip) when the
//! offer goes away. Its frame route syncs the control's hover/press state and its
//! tooltip; steady frames do nothing (I3). A language switch rebuilds it, because the
//! a11y name and the tooltip text are baked in at build time.
//!
//! # Lifecycle
//!
//! `UpdateIndicator::mount` builds the control and registers the route; the host
//! keeps the handle and calls [`UpdateIndicator::release`] when the header goes away.
//! **Release the indicator before the updater** it was mounted for: the indicator
//! reads the updater's view signal, and that signal is minted in the updater's scope.

use std::{cell::RefCell, rc::Rc};

use base_ui::{
    kit::{
        button::Button,
        icon::IconKind,
        size::UnifiedSizable as _,
        tooltip::{Tooltip, TooltipContent, TooltipOptions},
    },
    theme::handle::ThemeHandle,
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    node::{
        id::NodeId,
        signal::{Signal, SignalScope},
    },
    window::WindowId,
};
use widget::{frame::RouteId, services::WidgetServices};

use crate::updater::{Updater, UpdaterView};

/// The top bar's update button.
#[derive(Clone)]
pub struct UpdateIndicator {
    core: Rc<RefCell<IndicatorCore>>,
}

impl UpdateIndicator {
    /// Builds the indicator into `parent` (the header row's slot) over the updater.
    ///
    /// `key` prefixes the control's kit identities: it must be unique among the
    /// application's kit keys, exactly like a widget's `cx.unique_key` suffix (one
    /// indicator per application: `kytrix.updater`).
    pub fn mount(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        services: Rc<WidgetServices>,
        updater: Updater,
        key: impl Into<SmolStr>,
    ) -> Self {
        let key = key.into();
        let theme = services.theme.clone();
        let translate = services.translate.clone();
        let language = services.language;
        let view = updater.view_signal();
        let scope = app.signal_scope();
        let core = Rc::new(RefCell::new(IndicatorCore {
            window,
            services: Rc::clone(&services),
            theme,
            translate,
            language,
            view,
            updater,
            key,
            scope,
            parent,
            built: None,
            route: None,
            released: false,
        }));
        {
            let weak = Rc::downgrade(&core);
            let mut core = core.borrow_mut();
            core.route = Some(services.frame_router.register(move |app| {
                let Some(core) = weak.upgrade() else {
                    return Ok(());
                };
                core.borrow_mut().sync(app)
            }));
            if let Err(error) = core.sync(app) {
                log::warn!("building the update indicator failed: {error}");
            }
        }
        Self { core }
    }

    /// The node the header row placed (present only while an update is offered).
    #[must_use]
    pub fn node(&self) -> Option<NodeId> {
        self.core.borrow().built.as_ref().map(|built| built.node)
    }

    /// Whether the control is built (an update is on offer).
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.core.borrow().built.is_some()
    }

    /// Releases the control, its tooltip, its route and its value scope. Idempotent.
    pub fn release(&self, app: &mut App) -> usize {
        let mut core = self.core.borrow_mut();
        if core.released {
            return 0;
        }
        core.released = true;
        if let Some(route) = core.route.take() {
            core.services.frame_router.unregister(route);
        }
        let released = core.release_built(app);
        released + app.release_scope(core.scope)
    }

    /// Whether this indicator has been released.
    #[must_use]
    pub fn is_released(&self) -> bool {
        self.core.borrow().released
    }
}

impl std::fmt::Debug for UpdateIndicator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let core = self.core.borrow();
        formatter
            .debug_struct("UpdateIndicator")
            .field("visible", &core.built.is_some())
            .field("released", &core.released)
            .finish_non_exhaustive()
    }
}

/// What one built indicator owns.
struct IndicatorBuilt {
    node: NodeId,
    button: Button,
    tooltip: Tooltip,
    language: LanguageCode,
}

/// The state behind the indicator.
struct IndicatorCore {
    window: WindowId,
    services: Rc<WidgetServices>,
    theme: ThemeHandle,
    translate: Translate,
    language: Signal<LanguageCode>,
    view: Signal<Option<UpdaterView>>,
    updater: Updater,
    key: SmolStr,
    scope: SignalScope,
    parent: NodeId,
    built: Option<IndicatorBuilt>,
    route: Option<RouteId>,
    released: bool,
}

impl IndicatorCore {
    /// One frame: the control exists exactly while the updater offers a release, and
    /// its hover / press / tooltip state follows the pointer.
    fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        if self.released {
            return Ok(());
        }
        let offered = app.read(&self.view).is_some_and(UpdaterView::offered);
        let language = *app.read(&self.language);

        // A language switch rebuilds: the a11y name and the tooltip text are baked in
        // at build time, and the kit has no binding face for either.
        if self
            .built
            .as_ref()
            .is_some_and(|built| built.language != language)
        {
            self.release_built(app);
        }
        if offered && self.built.is_none() {
            self.build(app, language)?;
        } else if !offered && self.built.is_some() {
            self.release_built(app);
        }

        if let Some(built) = self.built.as_ref() {
            let _ = built.button.sync(app)?;
            let _ = built.tooltip.sync(app);
        }
        Ok(())
    }

    /// Builds the button and its tooltip into the header row.
    fn build(&mut self, app: &mut App, language: LanguageCode) -> Result<(), EngineError> {
        let label = t_smol!(&self.translate, "window.updater.available");
        let updater = self.updater.clone();
        let button = Button::new(
            app,
            self.theme.clone(),
            format_smolstr!("{}.indicator", self.key),
        )
        .ghost()
        .medium()
        .icon(IconKind::ArrowCircleUpFill)
        .a11y_label(label.clone())
        .on_click(move |ctx| updater.open_dialog(ctx.app_mut()));
        button.build(app, self.window, self.parent)?;
        let node = button
            .node()
            .expect("a built button is a node in the window");
        let tooltip = Tooltip::new(
            app,
            self.theme.clone(),
            (*self.services.layer).clone(),
            (*self.services.animation_router).clone(),
            format_smolstr!("{}.indicator-tooltip", self.key),
            TooltipOptions::default(),
        );
        tooltip.set_content(Some(TooltipContent::new(label)));
        tooltip.attach(app, self.window, node);
        self.built = Some(IndicatorBuilt {
            node,
            button,
            tooltip,
            language,
        });
        Ok(())
    }

    /// Removes the control and gives its components their scopes back.
    fn release_built(&mut self, app: &mut App) -> usize {
        let Some(built) = self.built.take() else {
            return 0;
        };
        built.tooltip.release(app);
        let released = built.button.release(app);
        let _ = app.remove_node(self.window, built.node);
        released
    }
}
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use download_provider::task::DownloadId;
    use smol_str::format_smolstr;
    use ui_core::event::{Action, ActionSource};

    use super::*;
    use crate::{
        state::UpdaterState,
        test_support::{Harness, KEY, release},
        updater::{UpdaterTestExt, UpdaterView},
    };

    /// A harness with the updater installed, its first check disarmed, and the
    /// indicator mounted into the window's root.
    fn mounted() -> (Harness, UpdateIndicator) {
        let mut harness = Harness::new();
        let updater = harness.install_updater(None);
        updater.test_cancel_check(harness.app.app_mut());
        let indicator = harness.mount_indicator();
        harness.settle(2);
        (harness, indicator)
    }

    #[test]
    fn the_indicator_should_be_drawn_only_while_an_update_is_offered() {
        let (mut harness, indicator) = mounted();
        assert!(
            !indicator.is_visible(),
            "nothing is offered, so the top bar has no button"
        );
        assert!(harness.find("test.updater.indicator").is_none());

        let updater = harness.updater().clone();
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::Available(release("1.2.3")),
        );
        harness.settle(2);
        assert!(
            indicator.is_visible(),
            "an offered update is what the button is for"
        );
        assert!(harness.find("test.updater.indicator").is_some());

        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::downloading_for_test(release("1.2.3"), DownloadId::new(), 0, None),
        );
        harness.settle(2);
        assert!(
            !indicator.is_visible(),
            "a download in flight is the dialog's business, not the button's"
        );
        assert!(harness.find("test.updater.indicator").is_none());
    }

    #[test]
    fn the_button_should_stay_while_the_last_attempt_failed() {
        let (mut harness, indicator) = mounted();
        let updater = harness.updater().clone();
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::failed_for_test(
                release("1.2.3"),
                crate::error::UpdaterError::UnsupportedPlatform,
            ),
        );
        harness.settle(2);

        assert!(
            indicator.is_visible(),
            "a failure to download is not a release that stopped existing"
        );
        assert_eq!(
            updater.view(harness.app.app()),
            Some(UpdaterView::Failure("error.updater.unsupported_platform"))
        );
    }

    /// The click opens the dialog for the update on offer: it is the indicator's own
    /// action, and the updater is what opens the dialog.
    #[test]
    fn pressing_the_indicator_should_open_the_dialog_for_the_offered_update() {
        let (mut harness, _indicator) = mounted();
        let updater = harness.updater().clone();
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::Available(release("1.2.3")),
        );
        harness.settle(2);

        let node = harness
            .find("test.updater.indicator")
            .expect("an offered update is drawn as the button that opens it");
        let window = harness.window();
        let action = Action::Custom(format_smolstr!("{KEY}.indicator.click"));
        assert!(
            harness
                .app
                .app_mut()
                .dispatch_action(window, node, action, ActionSource::Programmatic),
            "the control's action is registered"
        );
        harness.settle(3);

        assert!(
            updater.test_dialog_open(harness.app.app()),
            "pressing the button opens the dialog the update is read about from"
        );
    }

    /// The control is the icon alone, and what it says is in its tooltip. Focusing it
    /// is what opens the tip (the kit tooltip triggers on hover or focus inside the
    /// trigger).
    #[test]
    fn focusing_the_indicator_should_open_its_tooltip() {
        let (mut harness, _indicator) = mounted();
        let updater = harness.updater().clone();
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::Available(release("1.2.3")),
        );
        harness.settle(2);
        let node = harness
            .find("test.updater.indicator")
            .expect("the button is drawn");
        let window = harness.window();
        assert!(
            harness.app.app_mut().set_focus(window, Some(node)),
            "the button takes the keyboard"
        );
        // One frame for the tooltip to observe the focus and arm its delay, then the
        // clock for the delay to elapse (a timer fires at the next frame's entry, and
        // the tooltip opens on the sync after that).
        harness.drain();
        harness.advance(Duration::from_millis(600));
        harness.drain();

        assert!(
            !harness.layer.is_empty(window),
            "the tooltip the control is labelled with is open"
        );
    }

    #[test]
    fn releasing_the_indicator_should_reclaim_its_control_and_route() {
        let mut harness = Harness::new();
        let updater = harness.install_updater(None);
        updater.test_cancel_check(harness.app.app_mut());
        let baseline = harness.signal_slot_stats();
        let routes = harness.router.len();
        let indicator = harness.mount_indicator();
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::Available(release("1.2.3")),
        );
        harness.settle(2);
        assert!(harness.find("test.updater.indicator").is_some());

        indicator.release(harness.app.app_mut());
        indicator.release(harness.app.app_mut());

        assert!(
            harness.find("test.updater.indicator").is_none(),
            "releasing removes the control's nodes"
        );
        assert!(
            harness.layer.is_empty(harness.window()),
            "releasing withdraws the tooltip"
        );
        assert_eq!(
            harness.router.len(),
            routes,
            "releasing unregisters the indicator's route"
        );
        assert_eq!(
            harness.signal_slot_stats().live,
            baseline.live,
            "releasing reclaims every value the control minted"
        );
        assert!(indicator.is_released());
    }
}

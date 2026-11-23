use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

use ahash::{AHashMap, AHashSet};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_connector::connector::VenueLoadError;
use market_participants_model::identifier::VenueId;
use smol_str::{SmolStr, format_smolstr};
use tokio::sync::mpsc;
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    error::EngineError,
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
    },
    paint::{style::StylePatch, style_builder::StyleBuilder},
    text::{TextOverflow, TextStyle},
    window::WindowId,
};
use ui_kit::{
    kit::{
        alert::AlertBuilder,
        brand::{Brand, BrandKind},
        button::Button,
        dialog::{Dialog, DialogBuilder},
        icon::{IconKind, IconSpec},
        interactive::Interactive,
        layer::Band,
        overlay::{
            Anchor, DismissPolicy, Overlay, OverlayA11y, OverlayOptions, OverlaySurface, Placement,
        },
        size::{UnifiedSizable, UnifiedSize},
        spinner::Spinner,
        tooltip::{Tooltip, TooltipContent, TooltipOptions},
    },
    theme::{components::InteractiveVariant, handle::ThemeHandle, role::Role},
};
use widget::{error::TranslatedError, venue::VenueDisplay};

use crate::{
    error::{MenuError, VenueLoadFailure},
    host::{MarketParticipantsHost, VenueDirectory},
};

const KEY_BASE: &str = "market-participants";

const PANEL_WIDTH: f32 = 280.0;

const SURFACE_PADDING: f32 = 4.0;

const PANEL_CONTENT_WIDTH: f32 = PANEL_WIDTH - 2.0 * SURFACE_PADDING;

const TRIGGER_ICON_SIZE: f32 = 16.0;

const MARK_SIZE: f32 = 14.0;

const ACTION_WIDTH: f32 = 64.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Activity {
    Loading,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VenueRow {
    venue: VenueId,
    loaded: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PanelSignature {
    language: LanguageCode,
    rows: Vec<(VenueId, bool, Option<Activity>)>,
}

struct LoadOutcome {
    venue: VenueId,
    result: Result<(), VenueLoadError>,
}

#[derive(Default)]
struct RowComponents {
    button: Option<Button>,
    spinner: Option<Spinner>,
}

#[derive(Clone)]
pub struct MarketParticipants {
    inner: Rc<RefCell<MenuInner>>,
}

impl MarketParticipants {
    pub fn new(host: MarketParticipantsHost, app: &mut App) -> Self {
        let scope = app.signal_scope();
        let key = format_smolstr!("{KEY_BASE}@{}", scope.index());
        let language = host.language(app);
        let title = t_smol!(host.translate(), "window.market_participants.title");
        let a11y_label = app.signal_scoped(scope, title);
        let rows = read_rows(host.directory());
        let (outcome_tx, outcome_rx) = mpsc::unbounded_channel();
        let inner = Rc::new(RefCell::new(MenuInner {
            host,
            key,
            scope,
            a11y_label,
            language,
            rows,
            activity: AHashMap::new(),
            pending: AHashSet::new(),
            outcome_tx,
            outcome_rx,
            window: None,
            trigger: None,
            trigger_node: None,
            tooltip: None,
            overlay: None,
            rows_root: None,
            buttons: Vec::new(),
            spinners: Vec::new(),
            panel_signature: None,
            alert: None,
            released: false,
            self_weak: Weak::new(),
        }));
        let menu = Self { inner };
        menu.inner.borrow_mut().self_weak = Rc::downgrade(&menu.inner);
        menu
    }

    #[must_use]
    pub fn key(&self) -> SmolStr {
        self.inner.borrow().key.clone()
    }

    #[must_use]
    pub fn trigger_node(&self) -> Option<NodeId> {
        self.inner.borrow().trigger_node
    }

    #[must_use]
    pub fn is_open(&self, app: &App) -> bool {
        self.inner
            .borrow()
            .overlay
            .as_ref()
            .is_some_and(|overlay| overlay.is_open(app))
    }

    pub fn build(
        &mut self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<(), MenuError> {
        self.inner.borrow_mut().build(app, window, parent)
    }

    pub fn sync(&mut self, app: &mut App) -> Result<(), MenuError> {
        self.inner.borrow_mut().sync(app)
    }

    pub fn release(&mut self, app: &mut App) {
        self.inner.borrow_mut().release(app);
    }

    #[cfg(test)]
    pub(crate) fn cancel_load(&mut self, venue: VenueId) {
        self.inner.borrow().host.directory().cancel_load(&venue);
    }

    #[cfg(test)]
    fn rows(&self) -> Vec<VenueRow> {
        self.inner.borrow().rows.clone()
    }

    #[cfg(test)]
    pub(crate) fn pending_loads(&self) -> usize {
        self.inner.borrow().pending.len()
    }

    #[cfg(test)]
    pub(crate) fn start_load(&mut self, app: &mut App, venue: VenueId) -> Result<(), MenuError> {
        self.inner.borrow_mut().start_load(app, venue)
    }
}

struct MenuInner {
    host: MarketParticipantsHost,
    key: SmolStr,
    scope: SignalScope,
    a11y_label: Signal<SmolStr>,
    language: LanguageCode,
    rows: Vec<VenueRow>,
    activity: AHashMap<VenueId, Activity>,
    pending: AHashSet<VenueId>,
    outcome_tx: mpsc::UnboundedSender<LoadOutcome>,
    outcome_rx: mpsc::UnboundedReceiver<LoadOutcome>,
    window: Option<WindowId>,
    trigger: Option<Interactive>,
    trigger_node: Option<NodeId>,
    tooltip: Option<Tooltip>,
    overlay: Option<Overlay>,
    rows_root: Option<NodeId>,
    buttons: Vec<Button>,
    spinners: Vec<Spinner>,
    panel_signature: Option<PanelSignature>,
    alert: Option<Dialog>,
    released: bool,
    self_weak: Weak<RefCell<MenuInner>>,
}

impl MenuInner {
    fn build(&mut self, app: &mut App, window: WindowId, parent: NodeId) -> Result<(), MenuError> {
        if self.released || self.trigger.is_some() {
            return Ok(());
        }
        self.window = Some(window);

        let theme = self.host.theme().clone();
        let trigger_key = format_smolstr!("{}:trigger", self.key);
        let weak = self.self_weak.clone();
        let trigger = Interactive::new(app, theme.clone(), trigger_key)
            .variant(InteractiveVariant::Ghost)
            .with_unified_size(UnifiedSize::Medium)
            .a11y_role(A11yRole::Button)
            .a11y_label_signal(self.a11y_label)
            .on_click(move |ctx| {
                let Some(shared) = weak.upgrade() else {
                    return;
                };
                let window = ctx.window();
                let app = ctx.app_mut();
                if let Err(error) = shared.borrow_mut().toggle_panel(app, window) {
                    log::warn!("toggling the market participants panel failed: {error}");
                }
            });
        let trigger_node = trigger.build_bare(app, window, parent)?;
        IconSpec::new(IconKind::Bank)
            .with_size(TRIGGER_ICON_SIZE)
            .build_themed(app, window, trigger_node, &theme, Role::FgDefault)?;
        self.trigger = Some(trigger);
        self.trigger_node = Some(trigger_node);

        let tooltip_key = format_smolstr!("{}:tooltip", self.key);
        let tooltip = Tooltip::new(
            app,
            theme.clone(),
            self.host.layer().clone(),
            self.host.animation(),
            tooltip_key,
            TooltipOptions {
                placement: Placement::BOTTOM,
                ..TooltipOptions::default()
            },
        );
        tooltip.attach(app, window, trigger_node);
        tooltip.set_content(Some(TooltipContent::new(t_smol!(
            self.host.translate(),
            "window.market_participants.title"
        ))));
        self.tooltip = Some(tooltip);

        let overlay_key = format_smolstr!("{}:panel", self.key);
        let overlay = Overlay::new(
            app,
            theme,
            self.host.layer().clone(),
            self.host.animation(),
            overlay_key,
            OverlayOptions {
                placement: Placement::BOTTOM_START,
                gap: 4.0,
                dismiss: DismissPolicy::OutsideClick,
                modal: true,
                band: Band::Dialog,
                surface: OverlaySurface::menu(),
                a11y: OverlayA11y::menu(t_smol!(
                    self.host.translate(),
                    "window.market_participants.title"
                )),
                scrim: None,
                ..OverlayOptions::default()
            },
        );
        self.overlay = Some(overlay);
        Ok(())
    }

    fn toggle_panel(&mut self, app: &mut App, window: WindowId) -> Result<(), MenuError> {
        if self.released {
            return Ok(());
        }
        let Some(overlay) = self.overlay.clone() else {
            return Ok(());
        };
        if overlay.is_open(app) {
            overlay.close(app);
            return Ok(());
        }
        let Some(trigger) = self.trigger_node else {
            return Ok(());
        };
        if self.window != Some(window) {
            self.window = Some(window);
        }
        self.rows = read_rows(self.host.directory());
        self.panel_signature = None;
        overlay.open(app, window, Anchor::Node(trigger), |_, _| Ok(()))?;
        Ok(())
    }

    fn sync(&mut self, app: &mut App) -> Result<(), MenuError> {
        if self.released || self.window.is_none() {
            return Ok(());
        }

        while let Ok(outcome) = self.outcome_rx.try_recv() {
            self.finish_load(outcome, app)?;
        }

        let language = self.host.language(app);
        if language != self.language {
            self.language = language;
            let title = t_smol!(self.host.translate(), "window.market_participants.title");
            let _ = app.write(&self.a11y_label, title);
            self.panel_signature = None;
        }

        if let Some(trigger) = self.trigger.clone() {
            trigger.sync(app)?;
        }

        let Some(overlay) = self.overlay.clone() else {
            return Ok(());
        };
        let was_open = overlay.is_open(app);
        overlay.sync(app);
        let open = overlay.is_open(app);
        if was_open && !open {
            self.clear_panel(app);
        }
        if let Some(trigger) = self.trigger.clone() {
            trigger.set_selected(app, open);
        }
        self.sync_tooltip(app, open);
        self.sync_alert(app)?;
        if open {
            let signature = self.current_signature();
            if self.panel_signature.as_ref() != Some(&signature) {
                self.rebuild_panel(app, signature)?;
            }
            for spinner in &self.spinners {
                spinner.sync(app)?;
            }
        }
        Ok(())
    }

    fn sync_tooltip(&mut self, app: &mut App, panel_open: bool) {
        let Some(tooltip) = self.tooltip.clone() else {
            return;
        };
        let content = if panel_open {
            None
        } else {
            Some(TooltipContent::new(t_smol!(
                self.host.translate(),
                "window.market_participants.title"
            )))
        };
        tooltip.set_content(content);
        let _ = tooltip.sync(app);
    }

    fn current_signature(&self) -> PanelSignature {
        PanelSignature {
            language: self.language,
            rows: self
                .rows
                .iter()
                .map(|row| {
                    (
                        row.venue,
                        row.loaded,
                        self.activity.get(&row.venue).copied(),
                    )
                })
                .collect(),
        }
    }

    fn rebuild_panel(&mut self, app: &mut App, signature: PanelSignature) -> Result<(), MenuError> {
        let Some(window) = self.window else {
            return Ok(());
        };
        let Some(overlay) = self.overlay.clone() else {
            return Ok(());
        };
        let Some(panel) = overlay.panel_node() else {
            return Ok(());
        };

        if let Some(root) = self.rows_root.take() {
            let _ = app.remove_node(window, root);
        }
        self.release_row_components(app);

        let translate = self.host.translate().clone();
        let theme = self.host.theme().clone();
        let language = self.host.language_signal();
        let key = self.key.clone();
        let rows = self.rows.clone();
        let weak = self.self_weak.clone();
        let rows_root = {
            let tree = app
                .window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut();
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:rows"))
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .width(PANEL_CONTENT_WIDTH)
                        .min_width_0()
                        .build(),
                )
                .build(tree, panel)?
        };
        for row in rows {
            let activity = self.activity.get(&row.venue).copied();
            let components = build_row(
                app, window, rows_root, &key, &weak, row, activity, language, &translate, &theme,
            )?;
            if let Some(button) = components.button {
                self.buttons.push(button);
            }
            if let Some(spinner) = components.spinner {
                self.spinners.push(spinner);
            }
        }
        self.rows_root = Some(rows_root);
        self.panel_signature = Some(signature);
        Ok(())
    }

    fn clear_panel(&mut self, app: &mut App) {
        self.rows_root = None;
        self.panel_signature = None;
        self.release_row_components(app);
    }

    fn release_row_components(&mut self, app: &mut App) {
        for button in self.buttons.drain(..) {
            button.release(app);
        }
        for spinner in self.spinners.drain(..) {
            spinner.release(app);
        }
    }

    fn start_load(&mut self, app: &mut App, venue: VenueId) -> Result<(), MenuError> {
        if self.released || self.pending.contains(&venue) {
            return Ok(());
        }
        let Some(window) = self.window else {
            return Ok(());
        };
        if self.host.directory().needs_credentials(venue) {
            let market = venue.display_string(self.host.translate());
            let title = t_smol!(
                self.host.translate(),
                "window.market_participants.credentials_required_title",
                market = market
            );
            let message = t_smol!(
                self.host.translate(),
                "window.market_participants.credentials_required_message",
                market = market
            );
            self.raise_alert(app, title, message)?;
            return Ok(());
        }

        self.activity.insert(venue, Activity::Loading);
        self.pending.insert(venue);
        self.panel_signature = None;

        let directory = self.host.directory_arc();
        let outcome_tx = self.outcome_tx.clone();
        let executor = self.host.executor();
        let wake = app.handle();
        executor.spawn(Box::pin(async move {
            let result = directory.load(venue).await;
            if outcome_tx.send(LoadOutcome { venue, result }).is_ok() {
                wake.send(move |app| {
                    let _ = app.request_repaint(window);
                });
            }
        }));
        Ok(())
    }

    fn finish_load(&mut self, outcome: LoadOutcome, app: &mut App) -> Result<(), MenuError> {
        let LoadOutcome { venue, result } = outcome;
        if self.released {
            return Ok(());
        }
        self.pending.remove(&venue);
        match result {
            Ok(()) => {
                self.activity.remove(&venue);
            }
            Err(error) => {
                log::warn!("loading {venue} failed: {error}");
                self.activity.insert(venue, Activity::Failed);
                let market = venue.display_string(self.host.translate());
                let title = t_smol!(
                    self.host.translate(),
                    "window.market_participants.failed",
                    market = market
                );
                let message =
                    VenueLoadFailure::new(error).translated_message(self.host.translate());
                self.raise_alert(app, title, message)?;
            }
        }
        self.rows = read_rows(self.host.directory());
        self.panel_signature = None;
        Ok(())
    }

    fn raise_alert(
        &mut self,
        app: &mut App,
        title: SmolStr,
        message: SmolStr,
    ) -> Result<(), MenuError> {
        let Some(window) = self.window else {
            return Ok(());
        };
        if let Some(previous) = self.alert.take() {
            previous.release(app);
        }
        let theme = self.host.theme().clone();
        let layer = self.host.layer().clone();
        let animation = self.host.animation();
        let key = format_smolstr!("{}:alert", self.key);
        let confirm = t_smol!(self.host.translate(), "common.confirm");
        let dialog_title = title.clone();
        let dialog = DialogBuilder::alert(dialog_title, move |dialog, _theme| {
            let dialog = dialog.clone();
            AlertBuilder::new()
                .ok(title.clone(), message.clone())
                .ok_text(confirm.clone())
                .ok_action(move |_, app| {
                    dialog.close(app);
                })
        })
        .build(app, theme, layer, animation, key);
        dialog.open(app, window)?;
        self.alert = Some(dialog);
        Ok(())
    }

    fn sync_alert(&mut self, app: &mut App) -> Result<(), MenuError> {
        let Some(dialog) = self.alert.clone() else {
            return Ok(());
        };
        dialog.sync(app)?;
        if !dialog.is_open(app) {
            self.alert = None;
            dialog.release(app);
        }
        Ok(())
    }

    fn release(&mut self, app: &mut App) {
        if self.released {
            return;
        }
        self.released = true;

        let pending: Vec<VenueId> = self.pending.drain().collect();
        for venue in pending {
            self.host.directory().cancel_load(&venue);
        }
        if let Some(overlay) = self.overlay.take() {
            overlay.release(app);
        }
        self.release_row_components(app);
        if let Some(alert) = self.alert.take() {
            alert.release(app);
        }
        if let Some(tooltip) = self.tooltip.take() {
            tooltip.release(app);
        }
        if let Some(trigger) = self.trigger.take() {
            trigger.release(app);
        }
        self.trigger_node = None;
        let _ = app.release_scope(self.scope);
    }
}

fn read_rows(directory: &dyn VenueDirectory) -> Vec<VenueRow> {
    directory
        .venues()
        .into_iter()
        .map(|venue| VenueRow {
            loaded: directory.is_loaded(&venue),
            venue,
        })
        .collect()
}

const fn brand_of(venue: VenueId) -> BrandKind {
    match venue {
        VenueId::Binance => BrandKind::Binance,
        VenueId::Okx => BrandKind::Okx,
        VenueId::Hyperliquid => BrandKind::Hyperliquid,
        VenueId::Longbridge => BrandKind::Longbridge,
        VenueId::Polymarket => BrandKind::Polymarket,
    }
}

fn build_row(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    key: &str,
    weak: &Weak<RefCell<MenuInner>>,
    row: VenueRow,
    activity: Option<Activity>,
    language: Signal<LanguageCode>,
    translate: &Translate,
    theme: &ThemeHandle,
) -> Result<RowComponents, MenuError> {
    let venue = row.venue;
    let market = venue.display_string(translate);
    let mut components = RowComponents::default();

    let row_node = {
        let tree = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut();
        NodeBuilder::boxed()
            .id(format_smolstr!("TEST_MARKET_PARTICIPANTS_ROW_{venue:?}"))
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .gap(8.0)
                    .width_full()
                    .padding_y(2.0)
                    .build(),
            )
            .build(tree, parent)?
    };

    Brand::new(brand_of(venue))
        .with_size(16.0)
        .with_alt(market.clone())
        .build(app, window, row_node)?;

    let line = {
        let tree = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut();
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .flex_1()
                    .min_width_0()
                    .build(),
            )
            .build(tree, row_node)?
    };
    let mut name_style = TextStyle::new(UnifiedSize::Small.text_size());
    name_style.max_lines = 1;
    name_style.overflow = TextOverflow::Ellipsis;
    {
        let tree = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut();
        NodeBuilder::text(market.clone(), name_style)
            .style(StyleBuilder::new().min_width_0().build())
            .build(tree, line)?;
    }
    if matches!(activity, Some(Activity::Failed)) {
        let failed_theme = theme.clone();
        let failed_translate = translate.clone();
        let mut failed_style = TextStyle::new(UnifiedSize::Small.text_size());
        failed_style.max_lines = 1;
        failed_style.overflow = TextOverflow::Ellipsis;
        let tree = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut();
        NodeBuilder::text(SmolStr::default(), failed_style)
            .id(format_smolstr!("TEST_MARKET_PARTICIPANTS_FAILED_{venue:?}"))
            .style(StyleBuilder::new().min_width_0().build())
            .bind_text(move |cx| {
                let _language = cx.read(&language);
                let market = venue.display_string(&failed_translate);
                t_smol!(
                    failed_translate,
                    "window.market_participants.failed",
                    market = market
                )
            })
            .bind_style(move |cx| StylePatch {
                foreground: Some(failed_theme.role(cx, Role::StatusDangerBase)),
                ..StylePatch::default()
            })
            .build(tree, line)?;
    }

    let mark = {
        let label = match (activity, row.loaded) {
            (Some(Activity::Loading), _) => {
                t_smol!(
                    translate,
                    "window.market_participants.loading",
                    market = market
                )
            }
            (_, true) => t_smol!(
                translate,
                "window.market_participants.loaded",
                market = market
            ),
            (_, false) => {
                t_smol!(
                    translate,
                    "window.market_participants.unloaded",
                    market = market
                )
            }
        };
        let (selector, icon) = match (activity, row.loaded) {
            (Some(Activity::Loading), _) => (
                format_smolstr!("TEST_MARKET_PARTICIPANTS_LOADING_{venue:?}"),
                None,
            ),
            (_, true) => (
                format_smolstr!("TEST_MARKET_PARTICIPANTS_LOADED_{venue:?}"),
                Some(IconKind::PlugsConnected),
            ),
            (_, false) => (
                format_smolstr!("TEST_MARKET_PARTICIPANTS_UNLOADED_{venue:?}"),
                Some(IconKind::Plugs),
            ),
        };
        let tree = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut();
        let mark = NodeBuilder::boxed()
            .id(selector)
            .style(
                StyleBuilder::new()
                    .size(MARK_SIZE, MARK_SIZE)
                    .flex_none()
                    .build(),
            )
            .a11y(A11yOverride {
                role: Some(A11yRole::GenericContainer),
                label: Some(label),
                ..A11yOverride::default()
            })
            .build(tree, row_node)?;
        if let Some(icon) = icon {
            IconSpec::new(icon).with_size(MARK_SIZE).build_themed(
                app,
                window,
                mark,
                theme,
                Role::FgMuted,
            )?;
        }
        mark
    };
    if matches!(activity, Some(Activity::Loading)) {
        let spinner = Spinner::new(
            app,
            theme.clone(),
            format_smolstr!("{key}:spinner:{venue:?}"),
        )
        .with_size(MARK_SIZE)
        .with_label(t_smol!(
            translate,
            "window.market_participants.loading",
            market = market
        ));
        spinner.build(app, window, mark)?;
        components.spinner = Some(spinner);
    }

    let (label, state, loading) = match (row.loaded, activity) {
        (true, _) => (
            t_smol!(translate, "window.market_participants.refresh"),
            "REFRESH",
            matches!(activity, Some(Activity::Loading)),
        ),
        (false, _) => (
            t_smol!(translate, "window.market_participants.load"),
            "LOAD",
            matches!(activity, Some(Activity::Loading)),
        ),
    };
    let action_cell = {
        let tree = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut();
        NodeBuilder::boxed()
            .id(format_smolstr!(
                "TEST_MARKET_PARTICIPANTS_ACTION_{state}_{venue:?}"
            ))
            .style(StyleBuilder::new().flex_none().build())
            .build(tree, row_node)?
    };
    let action_key = format_smolstr!("{key}:action:{venue:?}");
    let mut button = Button::new(app, theme.clone(), action_key)
        .primary()
        .small()
        .label(label.clone())
        .a11y_label(label)
        .disabled(loading);
    if !loading {
        let weak = weak.clone();
        button = button.on_click(move |ctx| {
            let Some(shared) = weak.upgrade() else {
                return;
            };
            let app = ctx.app_mut();
            if let Err(error) = shared.borrow_mut().start_load(app, venue) {
                log::warn!("starting a load of {venue} failed: {error}");
            }
        });
    }
    let action_node = button.build(app, window, action_cell)?;
    fix_width(app, window, action_node, ACTION_WIDTH)?;
    components.button = Some(button);
    Ok(components)
}

fn fix_width(app: &mut App, window: WindowId, node: NodeId, width: f32) -> Result<(), EngineError> {
    let tree = app
        .window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree_mut();
    let mut style = tree.get(node)?.style().clone();
    style
        .width_length(width)
        .flex_grow(0.0)
        .flex_shrink(0.0)
        .justify_center();
    tree.set_style(node, style)
}

#[cfg(test)]
mod tests {
    use i18n::language::codes::LanguageCode;
    use market_participants_connector::{
        catalog::{error::CatalogError, source::RouteInputRevision},
        error::MarketParticipantError,
        routing::{DescriptorError, RegistryError},
    };
    use market_participants_model::{
        display::lexical::scope_display_code, identifier::VenueScopeId, listing::route::VenueRoute,
    };
    use smol_str::SmolStr;
    use ui_core::{math::Point, node::builder::NodeBuilder};

    use super::*;
    use crate::{
        host::{MarketParticipantsHost, VenueDirectory},
        test_support::{Harness, supported_venues, translate, translate_in, venue},
    };

    fn okx() -> VenueId {
        venue("okx")
    }

    fn binance() -> VenueId {
        venue("binance")
    }

    fn longbridge() -> VenueId {
        venue("longbridge")
    }

    fn okx_spot() -> VenueRoute {
        VenueRoute::new(
            okx(),
            VenueScopeId::new("spot").expect("the built-in scope is canonical"),
        )
    }

    fn open_panel(harness: &mut Harness) {
        let trigger = harness.menu().trigger_node().expect("the trigger is built");
        harness.click(trigger);
        harness.settle(2);
        assert!(
            harness.menu().is_open(harness.app()),
            "the trigger opens the panel"
        );
        assert!(harness.shows_panel(), "the open panel draws venue rows");
    }

    fn click_action(harness: &mut Harness, venue: VenueId, loaded: bool) {
        let node = harness
            .action(venue, loaded)
            .unwrap_or_else(|| panic!("the panel offers {venue}'s action"));
        harness.click(node);
        harness.settle(1);
    }

    fn alert_dialog(harness: &Harness) -> Option<Dialog> {
        harness.menu().inner.borrow().alert.clone()
    }

    fn alert_open(harness: &Harness) -> bool {
        alert_dialog(harness).is_some_and(|dialog| dialog.is_open(harness.app()))
    }

    fn alert_title(harness: &Harness) -> Option<SmolStr> {
        alert_dialog(harness)
            .and_then(|dialog| dialog.with_alert(|alert| SmolStr::new(alert.title())))
    }

    fn click_alert_ok(harness: &mut Harness) {
        let dialog = alert_dialog(harness).expect("an alert is open");
        let button = dialog
            .with_alert(|alert| alert.ok_button().clone())
            .expect("the alert has its ok button");
        harness.click_button(&button);
        harness.settle_fade();
        assert!(!alert_open(harness), "reading the alert closes it");
    }

    fn activity(harness: &Harness, venue: VenueId) -> Option<Activity> {
        harness.menu().inner.borrow().activity.get(&venue).copied()
    }

    #[test]
    fn the_menu_should_start_with_every_venue_and_no_activity() {
        let harness = Harness::new();

        assert_eq!(harness.menu().rows().len(), supported_venues().len());
        assert!(harness.menu().rows().iter().all(|row| !row.loaded));
        assert_eq!(harness.menu().pending_loads(), 0);
        assert!(harness.menu().trigger_node().is_some());
        assert!(!harness.menu().is_open(harness.app()));
        assert!(alert_dialog(&harness).is_none());
        assert_eq!(harness.overlay_count(), 0);
    }

    #[test]
    fn the_panel_should_list_every_venue_this_build_speaks() {
        let mut harness = Harness::new();
        assert!(
            !harness.shows_panel(),
            "a panel nobody opened draws nothing"
        );

        open_panel(&mut harness);

        let translate = translate();
        let venues = supported_venues();
        assert_eq!(venues.len(), 5, "this build speaks five venues");
        for venue in venues {
            assert!(harness.shows_row(venue), "the panel draws {venue}");
            assert!(
                harness.shows_mark(venue, "UNLOADED"),
                "a venue nothing loaded shows the unloaded mark"
            );
            assert!(
                harness.action(venue, false).is_some(),
                "a venue nothing loaded offers to load"
            );
            let name = venue.display_string(&translate);
            assert!(
                harness.has_text(harness.row(venue).expect("the row is drawn"), &name),
                "the row names {venue}"
            );
        }
    }

    #[test]
    fn a_load_without_credentials_should_be_refused_with_an_alert() {
        let gated = longbridge();
        let mut harness = Harness::new();
        harness.directory().require_credentials(gated);
        open_panel(&mut harness);

        click_action(&mut harness, gated, false);

        assert!(
            harness.directory().loads().is_empty(),
            "the refused load was never requested"
        );
        assert_eq!(harness.menu().pending_loads(), 0);
        assert!(activity(&harness, gated).is_none(), "nothing was started");
        assert!(
            harness.action(gated, false).is_some(),
            "the row still offers the load it offered before the press"
        );
        assert!(!harness.shows_mark(gated, "LOADING"));
        assert!(
            alert_open(&harness),
            "the refusal is stated where the user is"
        );

        let translate = translate();
        let expected_title = t_smol!(
            translate,
            "window.market_participants.credentials_required_title",
            market = gated.display_string(&translate)
        );
        let expected_message = t_smol!(
            translate,
            "window.market_participants.credentials_required_message",
            market = gated.display_string(&translate)
        );
        assert_eq!(
            alert_title(&harness).as_deref(),
            Some(expected_title.as_str())
        );
        assert!(
            harness.has_text_anywhere(&expected_message),
            "the alert says which value to configure"
        );
    }

    #[test]
    fn a_configured_market_should_load_through_the_same_path() {
        let gated = longbridge();
        let mut harness = Harness::new();
        open_panel(&mut harness);
        assert!(
            !harness.directory().needs_credentials(gated),
            "a fresh installation holds no Longbridge credentials, so the venue is configured"
        );

        click_action(&mut harness, gated, false);

        assert_eq!(harness.directory().loads(), vec![gated]);
        assert_eq!(harness.menu().pending_loads(), 1);
        assert!(
            harness.shows_mark(gated, "LOADING"),
            "a configured venue starts the load its row offers"
        );
        assert!(
            !alert_open(&harness),
            "a load that started is not a refusal"
        );

        harness.directory().complete(
            gated,
            Err(VenueLoadError::Discovery {
                venue: gated,
                source: Box::new(MarketParticipantError::RuntimeUnavailable),
            }),
        );
        harness.settle(2);

        assert_eq!(harness.menu().pending_loads(), 0);
        assert!(activity(&harness, gated).is_some(), "the load reported");
        assert!(alert_open(&harness), "the failure reported itself");
    }

    #[test]
    fn venues_whose_reads_are_public_should_never_meet_the_credentials_gate() {
        let mut harness = Harness::new();
        open_panel(&mut harness);

        for venue in [binance(), okx(), venue("hyperliquid")] {
            assert!(!harness.directory().needs_credentials(venue));
            let before = harness.directory().loads().len();

            click_action(&mut harness, venue, false);

            assert_eq!(
                harness.directory().loads().len(),
                before + 1,
                "{venue} started the load its row offers"
            );
            assert!(harness.shows_mark(venue, "LOADING"));
            assert!(!alert_open(&harness));

            harness.menu_mut().cancel_load(venue);
            harness.settle(2);
            assert!(alert_open(&harness), "{venue}'s reported load is reported");
            click_alert_ok(&mut harness);
            assert!(
                harness.action(venue, false).is_some(),
                "the row offers the load again once the alert is read"
            );
        }
    }

    #[test]
    fn the_credentials_alert_should_read_in_every_language() {
        let gated = longbridge();
        let mut seen = Vec::new();

        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let translate = translate_in(language);
            let market = gated.display_string(&translate);
            let title = t_smol!(
                translate,
                "window.market_participants.credentials_required_title",
                market = market
            );
            let message = t_smol!(
                translate,
                "window.market_participants.credentials_required_message",
                market = market
            );

            assert!(
                !title.is_empty() && !message.is_empty(),
                "the refusal reads as a sentence in {language}"
            );
            assert!(
                title.contains(market.as_str()) && message.contains(market.as_str()),
                "the refusal names the market in {language}"
            );
            assert!(
                message.contains("API Key"),
                "the refusal says which value to configure in {language}: {message}"
            );
            seen.push((title, message));
        }

        assert_ne!(
            seen[0], seen[1],
            "the two languages really state the refusal differently"
        );
    }

    #[test]
    fn pressing_a_row_action_should_leave_the_panel_open() {
        let mut harness = Harness::new();
        open_panel(&mut harness);

        click_action(&mut harness, okx(), false);
        harness
            .directory()
            .complete(okx(), Err(VenueLoadError::Interrupted { venue: okx() }));
        harness.settle(2);

        assert!(
            harness.menu().is_open(harness.app()),
            "the panel stays open"
        );
        assert!(
            harness.shows_row(okx()),
            "the row the press was made in is still drawn"
        );
        assert!(
            alert_open(&harness),
            "what the press started reported itself"
        );
    }

    #[test]
    fn a_clicked_load_that_succeeds_should_offer_a_refresh() {
        let mut harness = Harness::new();
        open_panel(&mut harness);
        assert!(harness.action(okx(), false).is_some());

        click_action(&mut harness, okx(), false);
        assert!(harness.shows_mark(okx(), "LOADING"));

        harness.directory().complete(okx(), Ok(()));
        harness.settle(2);

        assert!(
            harness.directory().is_loaded(&okx()),
            "the load registered the venue"
        );
        assert!(
            harness.action(okx(), true).is_some(),
            "a loaded venue offers to refresh"
        );
        assert!(harness.action(okx(), false).is_none());
        assert!(harness.shows_mark(okx(), "LOADED"));
        assert!(
            !alert_open(&harness),
            "a load that succeeded reports nothing"
        );
        assert!(harness.menu().is_open(harness.app()));
    }

    #[test]
    fn a_first_load_that_fails_should_be_reported_once() {
        let mut harness = Harness::new();
        open_panel(&mut harness);

        click_action(&mut harness, okx(), false);
        harness
            .directory()
            .complete(okx(), Err(VenueLoadError::Interrupted { venue: okx() }));
        harness.settle(2);

        assert!(
            alert_open(&harness),
            "the failure is reported where the user is"
        );
        assert!(
            harness
                .find("TEST_MARKET_PARTICIPANTS_FAILED_Okx")
                .is_some(),
            "the row keeps saying the venue failed to load"
        );
        assert!(
            harness.action(okx(), false).is_some(),
            "the row still offers to load"
        );
        assert!(!harness.directory().is_loaded(&okx()));

        click_alert_ok(&mut harness);

        assert!(
            !alert_open(&harness),
            "the failure is reported once per action"
        );
        assert!(
            harness
                .find("TEST_MARKET_PARTICIPANTS_FAILED_Okx")
                .is_some(),
            "reading the alert does not take the row's own report away"
        );
        assert!(harness.action(okx(), false).is_some());
    }

    #[test]
    fn a_refresh_that_fails_should_keep_offering_a_refresh() {
        let mut harness = Harness::new();
        harness.directory().mark_loaded(okx());
        open_panel(&mut harness);
        assert!(harness.action(okx(), true).is_some());

        click_action(&mut harness, okx(), true);
        harness
            .directory()
            .complete(okx(), Err(VenueLoadError::Interrupted { venue: okx() }));
        harness.settle(2);

        assert!(alert_open(&harness), "the failed refresh is reported");
        assert!(
            harness
                .find("TEST_MARKET_PARTICIPANTS_FAILED_Okx")
                .is_some()
        );
        assert!(
            harness.action(okx(), true).is_some(),
            "a refresh that failed leaves the venue loaded, so the row still offers one"
        );
        assert!(harness.action(okx(), false).is_none());
        assert!(harness.directory().is_loaded(&okx()));
    }

    #[test]
    fn a_row_that_is_loading_should_ignore_a_second_press() {
        let mut harness = Harness::new();
        open_panel(&mut harness);

        click_action(&mut harness, okx(), false);
        assert!(harness.shows_mark(okx(), "LOADING"));
        assert_eq!(harness.menu().pending_loads(), 1);
        assert_eq!(
            harness.directory().pending(),
            1,
            "the load future is waiting for its answer"
        );

        let again = harness
            .action(okx(), false)
            .expect("the row keeps its action cell");
        harness.click(again);
        harness.settle(1);
        assert_eq!(
            harness.directory().loads().len(),
            1,
            "one load per venue: the press on a row in flight starts nothing"
        );
        assert!(!alert_open(&harness));

        harness
            .start_load(okx())
            .expect("the re-entrancy guard is not an error");
        harness.drain();
        assert_eq!(
            harness.directory().loads().len(),
            1,
            "the second start is refused"
        );
        assert_eq!(harness.menu().pending_loads(), 1);
    }

    #[test]
    fn loads_of_two_venues_should_run_beside_each_other() {
        let mut harness = Harness::new();
        open_panel(&mut harness);

        click_action(&mut harness, okx(), false);
        click_action(&mut harness, binance(), false);

        assert_eq!(harness.directory().loads(), vec![okx(), binance()]);
        assert_eq!(harness.menu().pending_loads(), 2);
        assert!(harness.shows_mark(okx(), "LOADING") && harness.shows_mark(binance(), "LOADING"));
        assert!(!alert_open(&harness));

        harness
            .directory()
            .complete(okx(), Err(VenueLoadError::Interrupted { venue: okx() }));
        harness.directory().complete(
            binance(),
            Err(VenueLoadError::Interrupted { venue: binance() }),
        );
        harness.settle(2);

        assert!(activity(&harness, okx()).is_some());
        assert!(activity(&harness, binance()).is_some());
        assert!(
            harness
                .find("TEST_MARKET_PARTICIPANTS_FAILED_Okx")
                .is_some()
        );
        assert!(
            harness
                .find("TEST_MARKET_PARTICIPANTS_FAILED_Binance")
                .is_some()
        );
        assert!(activity(&harness, venue("hyperliquid")).is_none());
        assert_eq!(
            harness.overlay_count(),
            2,
            "the panel and one alert: each failure is reported once"
        );
    }

    #[test]
    fn a_cancelled_load_should_not_wedge_its_row() {
        let mut harness = Harness::new();
        open_panel(&mut harness);

        click_action(&mut harness, okx(), false);
        assert!(harness.shows_mark(okx(), "LOADING"));

        harness.menu_mut().cancel_load(okx());
        harness.settle(2);

        assert_eq!(harness.menu().pending_loads(), 0);
        assert!(!harness.shows_mark(okx(), "LOADING"));
        assert!(
            harness.action(okx(), false).is_some(),
            "the row offers the action again instead of staying in flight"
        );
        assert_eq!(harness.directory().cancelled(), vec![okx()]);
        assert!(
            alert_open(&harness),
            "the directory's cancellation is reported"
        );
    }

    #[test]
    fn switching_the_language_should_redraw_the_panel() {
        let mut harness = Harness::new();
        open_panel(&mut harness);
        let row = harness.row(okx()).expect("the row is drawn");
        let action = harness.action(okx(), false).expect("the action is drawn");
        let english_name = harness.text_of(row);
        let english_action = harness.text_of(action);
        assert!(!english_name.is_empty() && !english_action.is_empty());

        harness.switch_language(LanguageCode::ZhCn);
        harness.settle(2);

        let row = harness.row(okx()).expect("the row is still drawn");
        let action = harness
            .action(okx(), false)
            .expect("the action is still drawn");
        let chinese_name = harness.text_of(row);
        let chinese_action = harness.text_of(action);
        assert_ne!(
            english_name, chinese_name,
            "the row names the venue in the new language"
        );
        assert_ne!(
            english_action, chinese_action,
            "the action labels itself in the new language"
        );
        assert!(!chinese_name.is_empty() && !chinese_action.is_empty());
    }

    #[test]
    fn an_open_panel_should_be_steady_when_nothing_moves() {
        let mut harness = Harness::new();
        open_panel(&mut harness);
        harness.settle(2);
        let requests = harness.frame_requests();

        harness.settle(2);

        assert_eq!(
            harness.frame_requests(),
            requests,
            "a steady panel requests no frame (I3)"
        );
    }

    #[test]
    fn an_open_panel_should_take_the_hint_away() {
        let mut harness = Harness::new();
        let translate = translate();
        let title = t_smol!(translate, "window.market_participants.title");
        let hint = |harness: &Harness| {
            harness
                .menu()
                .inner
                .borrow()
                .tooltip
                .as_ref()
                .and_then(|tooltip| tooltip.content())
                .map(|content| content.label())
        };
        assert_eq!(hint(&harness), Some(title));

        open_panel(&mut harness);

        assert_eq!(hint(&harness), None, "the open panel takes the hint away");
    }

    #[test]
    fn a_press_outside_the_panel_should_close_it() {
        let mut harness = Harness::new();
        open_panel(&mut harness);

        harness.click_point(Point {
            x: 1200.0,
            y: 850.0,
        });
        harness.settle_fade();

        assert!(!harness.menu().is_open(harness.app()));
        assert!(
            !harness.shows_panel(),
            "the closed panel leaves no rows behind"
        );
        assert_eq!(harness.overlay_count(), 0);
    }

    #[test]
    fn a_second_failure_should_replace_the_alert() {
        let mut harness = Harness::new();
        open_panel(&mut harness);

        click_action(&mut harness, okx(), false);
        click_action(&mut harness, binance(), false);
        harness
            .directory()
            .complete(okx(), Err(VenueLoadError::Interrupted { venue: okx() }));
        harness.directory().complete(
            binance(),
            Err(VenueLoadError::Interrupted { venue: binance() }),
        );
        harness.settle(2);

        let translate = translate();
        let expected = t_smol!(
            translate,
            "window.market_participants.failed",
            market = binance().display_string(&translate)
        );
        assert_eq!(alert_title(&harness).as_deref(), Some(expected.as_str()));
        assert_eq!(
            harness.overlay_count(),
            2,
            "the panel and exactly one alert"
        );
    }

    #[test]
    fn two_menus_should_live_side_by_side() {
        let mut harness = Harness::new();
        let services = harness.services_rc();
        let directory = harness.directory();
        let executor = harness.executor();
        let window = harness.window();
        let toolbar = {
            let tree = harness
                .app_mut()
                .window_mut(window)
                .expect("the window exists")
                .tree_mut();
            let root = tree.root();
            NodeBuilder::boxed()
                .id("TEST_MARKET_PARTICIPANTS_TOOLBAR_2")
                .build(tree, root)
                .expect("the second toolbar builds")
        };
        let host = MarketParticipantsHost::new(services, directory).with_executor(executor);
        let mut second = MarketParticipants::new(host, harness.app_mut());
        second
            .build(harness.app_mut(), window, toolbar)
            .expect("the second trigger builds");

        assert_ne!(
            harness.menu().key(),
            second.key(),
            "two instances, two identities"
        );
        assert_ne!(
            harness.menu().trigger_node(),
            second.trigger_node(),
            "each menu owns its own trigger"
        );

        harness.settle(1);
        let trigger = second.trigger_node().expect("the second trigger is built");
        harness.click(trigger);
        second
            .sync(harness.app_mut())
            .expect("the second menu's frame");
        harness.settle(1);
        assert!(
            second.is_open(harness.app()),
            "the second menu opens its own panel"
        );
        assert!(
            !harness.menu().is_open(harness.app()),
            "the first menu is untouched"
        );
        assert_eq!(
            harness.find_all("TEST_MARKET_PARTICIPANTS_ROW_").len(),
            supported_venues().len(),
            "the second menu draws every venue once"
        );

        let first_trigger = harness.menu().trigger_node().expect("the first trigger");
        harness.remove_node(first_trigger);
        harness.release_menu();

        let action = harness
            .find_all(&format!("{}:action:Okx", second.key()))
            .into_iter()
            .next()
            .expect("the second menu's action is drawn");
        harness.click(action);
        second
            .sync(harness.app_mut())
            .expect("the second menu's frame");
        harness.settle(1);
        assert_eq!(harness.directory().loads(), vec![okx()]);
        assert_eq!(second.pending_loads(), 1);

        second.release(harness.app_mut());
        harness.app_mut().remove_node(window, toolbar).ok();
    }

    #[test]
    fn releasing_the_menu_should_cancel_its_loads_and_give_everything_back() {
        let mut harness = Harness::new();
        open_panel(&mut harness);
        click_action(&mut harness, okx(), false);
        assert_eq!(harness.menu().pending_loads(), 1);

        let trigger = harness.menu().trigger_node().expect("the trigger is built");
        harness.remove_node(trigger);
        harness.release_menu();
        harness.drain();

        assert_eq!(
            harness.directory().cancelled(),
            vec![okx()],
            "the load in flight was cancelled before it was dropped"
        );
        assert_eq!(harness.overlay_count(), 0, "the panel overlay is gone");
        assert!(alert_dialog(&harness).is_none());
        assert_eq!(
            harness.menu().trigger_node(),
            None,
            "nothing of the menu's is left but the caller's node"
        );

        harness.release_menu();
    }

    #[test]
    fn building_and_releasing_menus_should_not_grow_the_window() {
        let mut harness = Harness::new();
        let mut baseline = 0;

        for index in 0..8 {
            let window = harness.window();
            let toolbar = {
                let tree = harness
                    .app_mut()
                    .window_mut(window)
                    .expect("the window exists")
                    .tree_mut();
                let root = tree.root();
                NodeBuilder::boxed()
                    .id(format!("TEST_MARKET_PARTICIPANTS_LOOP_{index}"))
                    .build(tree, root)
                    .expect("the loop toolbar builds")
            };
            let host = MarketParticipantsHost::new(harness.services_rc(), harness.directory())
                .with_executor(harness.executor());
            let mut menu = MarketParticipants::new(host, harness.app_mut());
            menu.build(harness.app_mut(), window, toolbar)
                .expect("the loop menu builds");
            menu.sync(harness.app_mut()).expect("the loop menu's frame");
            harness.drain();

            let mounted = harness.tree_len();
            menu.release(harness.app_mut());
            harness
                .app_mut()
                .remove_node(window, toolbar)
                .expect("the loop toolbar is removed");
            harness.drain();
            let after = harness.tree_len();

            if index == 0 {
                baseline = after;
            }
            assert_eq!(
                after, baseline,
                "a mounted and released menu leaves no node"
            );
            assert!(mounted > after);
        }
    }

    #[test]
    fn every_key_this_menu_reads_should_resolve_to_text() {
        let keys = [
            "common.confirm",
            "error.venue_load.catalog",
            "error.venue_load.catalog_validation",
            "error.venue_load.discovery",
            "error.venue_load.discovery_limit_exceeded",
            "error.venue_load.discovery_unverifiable",
            "error.venue_load.interrupted",
            "error.venue_load.metadata",
            "error.venue_load.registration",
            "error.venue_load.revision_changed",
            "error.venue_load.unsupported_venue",
            "window.market_participants.credentials_required_message",
            "window.market_participants.credentials_required_title",
            "window.market_participants.load",
            "window.market_participants.failed",
            "window.market_participants.loaded",
            "window.market_participants.loading",
            "window.market_participants.refresh",
            "window.market_participants.title",
            "window.market_participants.unloaded",
        ];

        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let translate = translate_in(language);
            for key in keys {
                assert!(
                    !i18n::t_smol!(translate, key).is_empty(),
                    "{key} has no copy in {language}"
                );
            }

            let discovery = VenueLoadFailure::new(VenueLoadError::Discovery {
                venue: okx(),
                source: Box::new(MarketParticipantError::RuntimeUnavailable),
            });
            let unverifiable = VenueLoadFailure::new(VenueLoadError::DiscoveryUnverifiable {
                venue: okx(),
                dex: SmolStr::new_static("dex-1"),
            });
            let limit = VenueLoadFailure::new(VenueLoadError::DiscoveryLimitExceeded {
                venue: okx(),
                named: 3,
                limit: 2,
            });
            let registration = VenueLoadFailure::new(VenueLoadError::Registration {
                venue: okx(),
                source: Box::new(RegistryError::Descriptor(DescriptorError::DuplicateRoute {
                    route: okx_spot(),
                })),
            });
            let metadata = VenueLoadFailure::new(VenueLoadError::Metadata {
                venue: okx(),
                scope: None,
                source: Box::new(MarketParticipantError::RuntimeUnavailable),
            });
            let route_catalog = VenueLoadFailure::new(VenueLoadError::Catalog {
                venue: okx(),
                scope: okx_spot().scope,
                source: Box::new(MarketParticipantError::Catalog(Box::new(
                    CatalogError::CatalogNotLoaded { route: okx_spot() },
                ))),
            });
            let revision = VenueLoadFailure::new(VenueLoadError::RevisionChanged {
                venue: okx(),
                scope: okx_spot().scope,
            });
            let interrupted = VenueLoadFailure::new(VenueLoadError::Interrupted { venue: okx() });
            let unsupported = VenueLoadFailure::new(VenueLoadError::UnsupportedVenue {
                venue: VenueId::Longbridge,
            });

            for failure in [
                &discovery,
                &unverifiable,
                &limit,
                &registration,
                &metadata,
                &route_catalog,
                &revision,
                &interrupted,
                &unsupported,
            ] {
                assert!(
                    !failure.translated_message(&translate).is_empty(),
                    "{failure:?} has no copy in {language}"
                );
            }

            let stale = VenueLoadFailure::new(VenueLoadError::Catalog {
                venue: okx(),
                scope: okx_spot().scope,
                source: Box::new(MarketParticipantError::Catalog(Box::new(
                    CatalogError::RouteRevisionChanged {
                        route: okx_spot(),
                        staged: Box::new(
                            RouteInputRevision::describe().catalog_descriptor(1).build(),
                        ),
                        current: Box::new(
                            RouteInputRevision::describe().catalog_descriptor(2).build(),
                        ),
                    },
                ))),
            });
            let expected = t_smol!(
                translate,
                "error.venue_load.revision_changed",
                market = okx().display_string(&translate),
                scope = scope_display_code(&okx_spot().scope)
            );
            assert_eq!(
                stale.translated_message(&translate),
                expected,
                "a catalog that moved under its fetch reads as its own sentence in {language}"
            );
            assert_ne!(
                route_catalog.translated_message(&translate),
                expected,
                "a catalog failure that carries a route is not reported as a moved catalog in \
                 {language}"
            );
        }
    }
}

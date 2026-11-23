use std::{cell::Cell, rc::Rc, sync::Arc};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    kit::{
        select::{Select, SelectItem, SelectOptions},
        size::UnifiedSize,
    },
    style_builder::StyleBuilder,
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::{
    auth::constants::ACCOUNT_ORIGIN,
    service::{
        connector::KytrixServiceConnector,
        engine::{ErrorCode, KytrixServiceError},
        global_shipping_chokepoint::{
            GlobalShippingChokepointArea, GlobalShippingChokepointUpdate,
        },
    },
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use ui_core::{
    app::{App, TimerId},
    error::EngineError,
    event::{Event, InputEvent, PointerButton},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::TranslatedError,
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use crate::{
    map::{ATTRIBUTION_URL, MapCanvas, MapPicture},
    model::{STALE_AFTER, Vessel, VesselStore},
    scene::{self, Scene, SceneError},
    service::{
        AisStreamError, AisStreamMessage, GlobalShippingChokepointProtocol,
        GlobalShippingChokepointRequest, GlobalShippingChokepointScope,
        GlobalShippingChokepointService,
    },
    ship::{ShipType, ShipTypeFilter},
};

const FILTER_KEY_SUFFIX: &str = "global_shipping_chokepoint.header.filter";

const FILTER_ROW_SELECTOR: &str = "TEST_CHOKEPOINT_SHIP_TYPE";

const BODY_SELECTOR: &str = "TEST_CHOKEPOINT_BODY";

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GlobalShippingChokepointConfig;

impl WidgetConfig for GlobalShippingChokepointConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the packaged map asset could not be loaded: {0}")]
struct MapAssetInvalid(#[from] SceneError);

impl TranslatedError for MapAssetInvalid {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        t_smol!(translate, "error.global_shipping_chokepoint.map_asset")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
enum StalePicture {
    #[error("the picture is missing a push")]
    Gap,
    #[error("no push arrived within the staleness window")]
    Silent,
}

impl TranslatedError for StalePicture {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Gap => t_smol!(translate, "error.global_shipping_chokepoint.stale"),
            Self::Silent => t_smol!(translate, "error.global_shipping_chokepoint.stale_data"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    Load,
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderRequest {
    Filter(ShipTypeFilter),
}

struct FilterControl {
    parent: NodeId,
    root: NodeId,
    select: Select,
    language: LanguageCode,
    types: Vec<ShipType>,
    chosen: ShipTypeFilter,
}

pub struct GlobalShippingChokepointWidget {
    status: WidgetStatus,
    recovery: WidgetAction,
    binding: Option<WidgetBinding<GlobalShippingChokepointProtocol>>,
    pending: Pending<PendingCommand>,
    account_id: Option<SmolStr>,
    scene: Option<&'static Scene>,
    vessels: VesselStore,
    painted: Arc<[Vessel]>,
    present_types: Vec<ShipType>,
    filter: ShipTypeFilter,
    picture_revision: u64,
    map: MapCanvas,
    body_root: Option<NodeId>,
    requests: Signal<Option<HeaderRequest>>,
    filter_control: Option<FilterControl>,
    stale_epoch: Rc<Cell<u64>>,
    stale_live: Rc<Cell<bool>>,
    stale_timer: Option<TimerId>,
    credit_pressed: bool,
    open_url: Rc<dyn Fn(&str)>,
}

impl WidgetView for GlobalShippingChokepointWidget {
    type Config = GlobalShippingChokepointConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.global_shipping_chokepoint");
    const DISPLAY_NAME: &'static str = "widget.names.global_shipping_chokepoint";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: Option<&'static str> =
        Some("widget.descriptions.global_shipping_chokepoint");
    const ICON: Option<&'static str> = Some("widgets/global_shipping_chokepoint.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        let binding =
            cx.services()
                .kytrix_service
                .clone()
                .map(|connector: KytrixServiceConnector| {
                    WidgetBinding::spawn(cx, GlobalShippingChokepointService::new(connector))
                });
        Ok(Self::assemble(config, binding, None, status, recovery, cx))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let retry = self.status.is_failed(cx.app());
        let account = cx.account();
        let kept = retry && account == self.account_id;
        self.account_id = account.clone();
        let Some(account) = account else {
            self.clear_picture();
            self.report_sign_in(cx);
            return;
        };
        if self.scene.is_none() {
            match scene::scene() {
                Ok(scene) => self.scene = Some(scene),
                Err(error) => {
                    let recovery = self.recovery.clone();
                    self.status.report(
                        cx.app_mut(),
                        WidgetState::failed(Arc::new(MapAssetInvalid::from(error)), recovery),
                    );
                    return;
                }
            }
        }
        if !kept {
            self.clear_picture();
        }
        if self.binding.is_none() {
            let recovery = self.recovery.clone();
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(
                    Arc::new(AisStreamError::Service(KytrixServiceError::Closed)),
                    recovery,
                ),
            );
            return;
        }
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        self.arm_stale_timer(cx);
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        self.pending.clear();
        let request_id = binding.call(
            cx,
            GlobalShippingChokepointRequest::Load {
                scope: GlobalShippingChokepointScope::new(account),
            },
        );
        self.pending.track(request_id, PendingCommand::Load);
        let _ = cx.request_frame();
    }

    fn has_content(&self) -> bool {
        self.vessels.has_state()
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.body_root.is_some() {
            return Ok(());
        }
        let root = NodeBuilder::boxed()
            .id(BODY_SELECTOR)
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        self.map.build(cx, root)?;
        self.body_root = Some(root);
        if self.stale_live.get() && self.stale_timer.is_none() {
            self.arm_stale_timer(cx);
        }
        cx.request_frame()?;
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.drain_inbox(cx);

        let account = cx.account();
        if account != self.account_id {
            self.account_id = account.clone();
            if account.is_none() {
                self.sign_out(cx);
            } else {
                self.load(cx);
            }
        }

        let request = *cx.read(&self.requests);
        if let Some(request) = request {
            cx.write(&self.requests, None);
            self.apply_header_request(request, cx);
        }

        if self.has_content() && self.body_root.is_none() {
            cx.request_frame()?;
        }

        self.sync_map(cx)?;
        self.sync_filter_control(cx)?;
        Ok(())
    }

    fn header(
        &mut self,
        group: HeaderGroup,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        if group != HeaderGroup::Left {
            return Ok(());
        }
        self.release_filter_control(cx, true);
        let control = self.build_filter_control(cx, parent)?;
        self.filter_control = Some(control);
        Ok(())
    }

    fn dispatch(&mut self, event: &Event, cx: &mut WidgetCx<'_>) -> bool {
        let Event::Input(input) = event else {
            return false;
        };
        match input {
            InputEvent::PointerDown {
                position, button, ..
            } => {
                if *button != PointerButton::Primary || !self.map.contains(*position) {
                    return false;
                }
                self.credit_pressed = self.map.attribution_contains(*position);
                self.map.pointer_down(*position);
                true
            }
            InputEvent::PointerUp { position, .. } => {
                if !self.map.is_dragging() && !self.credit_pressed && !self.map.contains(*position)
                {
                    return false;
                }
                if self.credit_pressed && self.map.attribution_contains(*position) {
                    (self.open_url)(ATTRIBUTION_URL);
                }
                self.credit_pressed = false;
                self.map.pointer_up();
                true
            }
            InputEvent::PointerMove { position, .. } => {
                let moved = self.map.pointer_move(*position, &self.painted);
                if moved {
                    let _ = cx.request_frame();
                }
                true
            }
            InputEvent::PointerLeave { .. } => {
                self.credit_pressed = false;
                self.map.pointer_up();
                if self.map.clear_hover() {
                    let _ = cx.request_frame();
                }
                true
            }
            InputEvent::PointerCancel { .. } => {
                let held = self.map.is_dragging() || self.credit_pressed;
                self.credit_pressed = false;
                self.map.pointer_up();
                held
            }
            InputEvent::Scroll {
                position, delta, ..
            } => {
                if !self.map.contains(*position) {
                    return false;
                }
                if self.map.zoom_by_wheel(*position, delta.y) {
                    self.map.track_pointer(*position, &self.painted);
                    let _ = cx.request_frame();
                }
                true
            }
            InputEvent::Magnify { factor, center, .. } => {
                if !self.map.contains(*center) {
                    return false;
                }
                if self.map.zoom_by_pinch(*center, *factor) {
                    self.map.track_pointer(*center, &self.painted);
                    let _ = cx.request_frame();
                }
                true
            }
            _ => false,
        }
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        if let Some(mut binding) = self.binding.take() {
            binding.cancel();
        }
        self.release_filter_control(cx, false);
        self.map.release(cx);
        self.disarm_stale_timer(cx);
        self.filter_control = None;
        self.body_root = None;
        self.vessels.clear();
        self.painted = Arc::default();
        self.present_types.clear();
        self.pending.clear();
    }
}

impl GlobalShippingChokepointWidget {
    fn assemble(
        _config: GlobalShippingChokepointConfig,
        binding: Option<WidgetBinding<GlobalShippingChokepointProtocol>>,
        scene: Option<&'static Scene>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let requests = cx.signal(None);
        let open_url = Rc::clone(&cx.services().open_url);
        let account_id = cx.account();
        Self {
            status,
            recovery,
            binding,
            pending: Pending::new(),
            account_id,
            scene,
            vessels: VesselStore::new(),
            painted: Arc::default(),
            present_types: Vec::new(),
            filter: ShipTypeFilter::All,
            picture_revision: 0,
            map: MapCanvas::new(),
            body_root: None,
            requests,
            filter_control: None,
            stale_epoch: Rc::new(Cell::new(0)),
            stale_live: Rc::new(Cell::new(false)),
            stale_timer: None,
            credit_pressed: false,
            open_url,
        }
    }

    #[cfg(test)]
    fn with_source<S: crate::service::GlobalShippingChokepointSource>(
        config: GlobalShippingChokepointConfig,
        source: S,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let service = GlobalShippingChokepointService::new(source);
        let binding = WidgetBinding::spawn(cx, service);
        Self::assemble(
            config,
            Some(binding),
            scene::scene().ok(),
            status,
            recovery,
            cx,
        )
    }

    fn report_sign_in(&self, cx: &mut WidgetCx<'_>) {
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::message("widget.global_shipping_chokepoint.sign_in")),
        );
    }

    fn clear_picture(&mut self) {
        self.vessels.clear();
        self.painted = Arc::default();
        self.present_types.clear();
        self.map.clear_hover();
        self.picture_revision = self.picture_revision.wrapping_add(1);
    }

    fn rebuild_painted(&mut self) {
        let vessels = self.vessels.vessels();
        self.present_types = crate::model::present_ship_types(&vessels);
        let filter = self.filter;
        self.painted = vessels
            .into_iter()
            .filter(|vessel| filter.matches(vessel.ship_type))
            .collect::<Vec<_>>()
            .into();
        if self
            .map
            .hovered()
            .is_some_and(|mmsi| !self.painted.iter().any(|vessel| vessel.mmsi == mmsi))
        {
            self.map.clear_hover();
        }
        self.picture_revision = self.picture_revision.wrapping_add(1);
    }

    fn sign_out(&mut self, cx: &mut WidgetCx<'_>) {
        self.clear_picture();
        self.disarm_stale_timer(cx);
        self.pending.clear();
        self.report_sign_in(cx);
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        let request_id = binding.call(cx, GlobalShippingChokepointRequest::Stop);
        self.pending.track(request_id, PendingCommand::Stop);
    }

    fn select_filter(&mut self, filter: ShipTypeFilter, cx: &mut WidgetCx<'_>) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        self.rebuild_painted();
        let _ = cx.request_frame();
    }

    fn apply_header_request(&mut self, request: HeaderRequest, cx: &mut WidgetCx<'_>) {
        match request {
            HeaderRequest::Filter(filter) => self.select_filter(filter, cx),
        }
    }

    fn drain_inbox(&mut self, cx: &mut WidgetCx<'_>) {
        let messages = self
            .binding
            .as_ref()
            .map(WidgetBinding::take)
            .unwrap_or_default();
        for inbound in messages {
            match inbound {
                Inbound::Reliable { key, message } => self.apply_reliable(key, message, cx),
                Inbound::Latest { key, message } => self.apply_latest(key, message, cx),
                Inbound::Rpc { request_id, result } => self.apply_rpc(request_id, result, cx),
            }
        }
    }

    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<GlobalShippingChokepointProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.status.is_failed(cx.app()) {
            return;
        }
        match self.pending.take(request_id) {
            Some(PendingCommand::Load) => {
                if let Err(error) = result {
                    self.disarm_stale_timer(cx);
                    let recovery = self.recovery.clone();
                    self.status
                        .report(cx.app_mut(), WidgetState::failed(Arc::new(error), recovery));
                }
            }
            Some(PendingCommand::Stop) => {
                if let Err(error) = result {
                    log::debug!("a sign-out's stop was refused: {error:?}");
                }
            }
            None => {
                log::debug!(
                    "the Strait of Hormuz pane ignored reply {request_id}: it answers no command in flight"
                );
            }
        }
    }

    fn apply_reliable(
        &mut self,
        area: GlobalShippingChokepointArea,
        message: Arc<AisStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if !self.is_about_the_picture(area, cx.app()) {
            return;
        }
        let AisStreamMessage::Failed(error) = message.as_ref() else {
            return;
        };
        self.disarm_stale_timer(cx);
        let action = if matches!(
            error.as_ref(),
            AisStreamError::Service(KytrixServiceError::Server(ErrorCode::EntitlementRequired))
        ) {
            let open_url = Rc::clone(&self.open_url);
            WidgetAction::new("widget.global_shipping_chokepoint.view_plans", move |_| {
                open_url(ACCOUNT_ORIGIN);
            })
        } else {
            self.recovery.clone()
        };
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(Arc::clone(error) as Arc<dyn TranslatedError>, action),
        );
    }

    fn apply_latest(
        &mut self,
        area: GlobalShippingChokepointArea,
        message: Arc<AisStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if !self.is_about_the_picture(area, cx.app()) {
            return;
        }
        let AisStreamMessage::Update(update) = message.as_ref() else {
            return;
        };
        self.apply_update(update, cx);
    }

    fn apply_update(&mut self, update: &GlobalShippingChokepointUpdate, cx: &mut WidgetCx<'_>) {
        self.vessels.apply(update);
        self.rebuild_painted();
        if self.vessels.has_state() && self.vessels.has_gap() {
            self.fail_stale(StalePicture::Gap, cx);
            return;
        }
        if self.vessels.has_state() {
            self.status.report(cx.app_mut(), WidgetState::Ready);
            self.arm_stale_timer(cx);
        } else {
            self.status.report(cx.app_mut(), WidgetState::Connecting);
            self.disarm_stale_timer(cx);
        }
        let _ = cx.request_frame();
    }

    fn is_about_the_picture(&self, area: GlobalShippingChokepointArea, app: &App) -> bool {
        area == GlobalShippingChokepointArea::Hormuz
            && self.account_id.is_some()
            && !self.status.is_failed(app)
    }

    fn fail_stale(&mut self, stale: StalePicture, cx: &mut WidgetCx<'_>) {
        self.disarm_stale_timer(cx);
        let recovery = self.recovery.clone();
        self.status
            .report(cx.app_mut(), WidgetState::failed(Arc::new(stale), recovery));
    }

    fn arm_stale_timer(&mut self, cx: &mut WidgetCx<'_>) {
        let epoch = self.stale_epoch.get().wrapping_add(1);
        self.stale_epoch.set(epoch);
        self.stale_live.set(self.vessels.has_state());
        if let Some(id) = self.stale_timer.take() {
            cx.app_mut().cancel_timer(&id);
        }
        let Some(scope) = self.body_root else {
            return;
        };
        if !self.stale_live.get() {
            return;
        }
        let status = self.status;
        let recovery = self.recovery.clone();
        let live = Rc::clone(&self.stale_live);
        let shared_epoch = Rc::clone(&self.stale_epoch);
        let window = cx.window();
        match cx
            .app_mut()
            .after_scoped(window, scope, STALE_AFTER, move |app| {
                if shared_epoch.get() == epoch && live.replace(false) {
                    status.report(
                        app,
                        WidgetState::failed(Arc::new(StalePicture::Silent), recovery),
                    );
                }
            }) {
            Ok(id) => self.stale_timer = Some(id),
            Err(error) => {
                log::error!("the Strait of Hormuz pane could not arm its staleness tick: {error}");
            }
        }
    }

    fn disarm_stale_timer(&mut self, cx: &mut WidgetCx<'_>) {
        self.stale_live.set(false);
        if let Some(id) = self.stale_timer.take() {
            cx.app_mut().cancel_timer(&id);
        }
    }

    fn sync_map(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let zone = cx.data_time_zone();
        let hover = self
            .map
            .hovered()
            .and_then(|mmsi| self.vessels.display(mmsi, zone));
        let translate = cx.translate().clone();
        let picture = MapPicture {
            vessels: &self.painted,
            present_types: &self.present_types,
            filter: self.filter,
            revision: self.picture_revision,
            zone: zone.whole_seconds(),
            hover,
        };
        let scene = self.scene;
        let changed = self.map.sync(cx, &picture, scene, &translate)?;
        if changed {
            let _ = cx.request_frame();
        }
        Ok(())
    }

    fn build_filter_control(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<FilterControl, EngineError> {
        let language = *cx.read(&cx.language());
        let row = NodeBuilder::boxed()
            .id(FILTER_ROW_SELECTOR)
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let select = self.build_filter_select(cx, row)?;
        Ok(FilterControl {
            parent,
            root: row,
            select,
            language,
            types: self.present_types.clone(),
            chosen: self.filter,
        })
    }

    fn build_filter_select(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<Select, EngineError> {
        let requests = self.requests;
        let translate = cx.translate().clone();
        let key = cx.unique_key(FILTER_KEY_SUFFIX);
        let theme = cx.theme().clone();
        let layer = base_ui::kit::layer::Layer::clone(cx.layer());
        let router = base_ui::kit::overlay::AnimationRouter::clone(cx.animation());
        let options = SelectOptions::default()
            .size(UnifiedSize::Small)
            .label(t_smol!(
                translate,
                "widget.global_shipping_chokepoint.filter.title"
            ))
            .placeholder(self.filter_trigger_text(&translate));
        let window = cx.window();
        let select = Select::new(cx.app_mut(), theme, layer, router, key, options).on_change(
            move |app, chosen| {
                let Some(id) = chosen.first() else {
                    return;
                };
                let Some(filter) = ShipTypeFilter::from_key(id.as_str()) else {
                    return;
                };
                app.write(&requests, Some(HeaderRequest::Filter(filter)));
                let _ = app.request_repaint(window);
            },
        );
        select
            .set_items(cx.app_mut(), self.filter_items(&translate))
            .unwrap_or_else(|error| {
                log::warn!("the Strait of Hormuz filter could not state its choices: {error}");
            });
        select.set_chosen(cx.app_mut(), [self.filter.key()], false);
        select.build(cx.app_mut(), window, parent)?;
        Ok(select)
    }

    fn filter_items(&self, translate: &Translate) -> Vec<SelectItem> {
        let mut items = Vec::with_capacity(self.present_types.len() + 2);
        items.push(SelectItem::new(
            SmolStr::new_static(ShipTypeFilter::All.key()),
            t_smol!(translate, "widget.global_shipping_chokepoint.filter.all"),
        ));
        for ship_type in &self.present_types {
            items.push(SelectItem::new(
                SmolStr::new(ship_type.key()),
                t_smol!(translate, ship_type.label_key()),
            ));
        }
        if let ShipTypeFilter::Only(ship_type) = self.filter
            && !self.present_types.contains(&ship_type)
        {
            items.push(
                SelectItem::new(
                    SmolStr::new(ship_type.key()),
                    t_smol!(translate, ship_type.label_key()),
                )
                .disabled(true),
            );
        }
        items
    }

    fn filter_trigger_text(&self, translate: &Translate) -> SmolStr {
        match self.filter {
            ShipTypeFilter::All => {
                t_smol!(translate, "widget.global_shipping_chokepoint.filter.all")
            }
            ShipTypeFilter::Only(ship_type) => t_smol!(translate, ship_type.label_key()),
        }
    }

    fn sync_filter_control(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some((root, parent, language, types, chosen)) =
            self.filter_control.as_ref().map(|control| {
                (
                    control.root,
                    control.parent,
                    control.language,
                    control.types.clone(),
                    control.chosen,
                )
            })
        else {
            return Ok(());
        };
        if cx.tree().get(root).is_err() {
            self.release_filter_control(cx, false);
            return Ok(());
        }
        let language_now = *cx.read(&cx.language());
        if language != language_now {
            self.release_filter_control(cx, true);
            let control = self.build_filter_control(cx, parent)?;
            self.filter_control = Some(control);
        } else {
            let types_now = self.present_types.clone();
            let filter_changed = self.filter != chosen;
            if types != types_now || filter_changed {
                let translate = cx.translate().clone();
                let items = self.filter_items(&translate);
                let Some(control) = self.filter_control.as_mut() else {
                    return Ok(());
                };
                control
                    .select
                    .set_items(cx.app_mut(), items)
                    .unwrap_or_else(|error| {
                        log::warn!(
                            "the Strait of Hormuz filter could not replace its choices: {error}"
                        );
                    });
                control
                    .select
                    .set_chosen(cx.app_mut(), [self.filter.key()], false);
                control.types = types_now;
                control.chosen = self.filter;
            }
        }
        let Some(control) = self.filter_control.as_ref() else {
            return Ok(());
        };
        control.select.sync(cx.app_mut())?;
        Ok(())
    }

    fn release_filter_control(&mut self, cx: &mut WidgetCx<'_>, remove_node: bool) {
        let Some(control) = self.filter_control.take() else {
            return;
        };
        if remove_node && cx.tree().get(control.root).is_ok() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, control.root);
        }
        let _ = control.select.release(cx.app_mut());
    }
}

#[cfg(test)]
mod tests;

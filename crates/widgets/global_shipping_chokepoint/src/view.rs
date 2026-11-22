//! The Strait of Hormuz pane: its coastline and the vessels an AIS subscription reports in
//! it.
//!
//! # What the frame draws
//!
//! One canvas, and everything on it is drawn by [`crate::map`]. The base map's coastline,
//! borders and roads are layers parsed once by [`crate::scene`], the vessels are a picture
//! held between pushes, and the labels, the legend, the count, the credit and the hover
//! card are drawn by the same canvas because they all move with the camera.
//!
//! # What the pane reports, and what it holds
//!
//! The picture is one AIS subscription's. A keyframe replaces it, a delta only applies on
//! top of the picture already held, and a push that does not continue it is a gap the pane
//! reports as a failure rather than a state it guesses at. The pane's state is therefore a
//! picture held between pushes, the camera and pointer it is drawn with, and the ship-type
//! filter the reader chose.
//!
//! # The body is the ready state
//!
//! The framework draws the placeholder, the banner and the spinner, and it only embeds
//! this body when [`GlobalShippingChokepointWidget::has_content`] is true. So there is no
//! state branch here: the body is what a pane with a picture draws, and a picture holding
//! no vessel is drawn as the map it is — the count above the legend is what says the strait
//! is empty.
//!
//! # The account is the scope, and it moves under the pane
//!
//! The subscription reads the account the settings hold. Signing out has to take the
//! picture with it and stop the stream; signing in as someone else is another read. The
//! engine has no subscription seam for settings (design §11.7): the pane reads
//! [`WidgetCx::account`] on every `sync`, and the settings write is what schedules that
//! frame — the framework's surface root reads the same signal.
//!
//! # The staleness clock
//!
//! A link that stops delivering has no next push to arm another timer, so every push arms
//! one. The timer is scoped to the body root and cancelled whenever a newer push arrives:
//! only the push that has not been followed by another reports itself as silent. A gap a
//! push reports is the other half of the same clock — a missing message is not something a
//! later push can put back, so both are failures with a new subscription behind the
//! recovery.
//!
//! # Why a reply is matched by request identity
//!
//! A load is replaced by another load and by a sign-out, and every reply is attributed by
//! the framework `RequestId` it answers ([`Pending`]). A reply whose identity is not in the
//! table answers a call the pane already replaced, and applying it would report the old
//! command's failure over the one that replaced it.
//!
//! # The header control
//!
//! The ship-type filter is the pane's only header control. Its key comes from
//! [`WidgetCx::unique_key`], so two panes of this widget never share one handler. A kit
//! select's labels, accessible name and placeholder are chosen when it is built, so a
//! language switch rebuilds it (AGENTS §15.1); what it offers is `set_items`, which a
//! picture that gained or lost a type re-states without a rebuild. The choice a control
//! reports is written into a request signal and applied by the next `sync`, because a
//! control's handler owns no widget context.

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

/// The widget-local name the filter's kit key is built from.
///
/// A kit key is application-level — a select derives its actions from it and two panes that
/// build the same constant key share one handler — so the pane never spells one out:
/// [`WidgetCx::unique_key`] stamps the suffix with the pane's own instance scope.
const FILTER_KEY_SUFFIX: &str = "global_shipping_chokepoint.header.filter";

/// The selector the filter's own row carries.
const FILTER_ROW_SELECTOR: &str = "TEST_CHOKEPOINT_SHIP_TYPE";

/// The selector the body root carries.
const BODY_SELECTOR: &str = "TEST_CHOKEPOINT_BODY";

/// The pane persists nothing about this widget; the type exists because every widget
/// carries its schema version.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GlobalShippingChokepointConfig;

impl WidgetConfig for GlobalShippingChokepointConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// Why the packaged coastline could not be drawn.
///
/// It carries [`SceneError`] whole, so the reason a vertex or a ring was rejected is not
/// lost between the parser and the pane: the user reads one sentence either way, and the
/// `Display` of this type is what a log line has to say.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the packaged map asset could not be loaded: {0}")]
struct MapAssetInvalid(#[from] SceneError);

impl TranslatedError for MapAssetInvalid {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        t_smol!(translate, "error.global_shipping_chokepoint.map_asset")
    }
}

/// Why the picture on the map is no longer being continued.
///
/// Both cases are the picture stopping being live rather than the map being wrong, so the
/// vessels stay on screen under the failure. Neither can be repaired from what the pane
/// holds — a missing message is not something a later push can put back — which is why they
/// are failures with a new subscription behind the control rather than a badge the pane
/// clears by itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
enum StalePicture {
    /// A push did not continue the picture, so a message is missing from it.
    #[error("the picture is missing a push")]
    Gap,
    /// The link delivered nothing for the staleness window.
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

/// One command this pane sent and has not seen the answer to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    /// The load that starts the picture's stream.
    Load,
    /// The stop a sign-out sends.
    Stop,
}

/// What a header control asks the pane to do.
///
/// A control's handler runs with an application context and no widget context, so it
/// cannot change the pane: it writes the request and asks for a frame, and the next `sync`
/// applies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderRequest {
    Filter(ShipTypeFilter),
}

/// The filter control this pane built, and the copy it was built with.
///
/// A kit select's accessible name, placeholder and item labels are chosen when it is built
/// (a language switch rebuilds it, AGENTS §15.1); its *selection* is not — what it states
/// is its `chosen` signal, so a picture that gained or lost a type only re-states the
/// items.
struct FilterControl {
    /// The header group the pane built this control into, which a rebuild replaces it in.
    parent: NodeId,
    /// The widget's own row inside the group the pane made, which the widget removes
    /// itself.
    root: NodeId,
    select: Select,
    /// The language the items were stated in.
    language: LanguageCode,
    /// The types the items were built from, so a picture that changed them is compared
    /// rather than re-stated every frame.
    types: Vec<ShipType>,
    /// The choice the control was last told to state.
    chosen: ShipTypeFilter,
}

/// One pane: the picture an AIS subscription publishes, the camera it is drawn with, and
/// the vessel the pointer is on.
pub struct GlobalShippingChokepointWidget {
    status: WidgetStatus,
    recovery: WidgetAction,
    binding: Option<WidgetBinding<GlobalShippingChokepointProtocol>>,
    /// The command in flight, keyed by the identity the binding returned.
    pending: Pending<PendingCommand>,
    /// The account the picture belongs to, which is what an account change is compared
    /// against.
    account_id: Option<SmolStr>,
    /// The coastline, parsed once for the process; a failure to parse it is a failure the
    /// pane reports rather than one it retries.
    scene: Option<&'static Scene>,
    /// The picture the subscription published last.
    vessels: VesselStore,
    /// The vessels the filter draws, rebuilt when a push or the filter moved.
    painted: Arc<[Vessel]>,
    /// The ship types the picture holds, in `SHIP_TYPES` order.
    present_types: Vec<ShipType>,
    /// What the map is restricted to.
    filter: ShipTypeFilter,
    /// Moves whenever the picture, the filter or the present types moved.
    picture_revision: u64,
    /// The canvas the body built, and the map it draws.
    map: MapCanvas,
    /// The body root, once the body exists.
    body_root: Option<NodeId>,
    /// Where a header control hands over what the user did.
    requests: Signal<Option<HeaderRequest>>,
    /// The filter this pane built, if the pane has built its header row.
    filter_control: Option<FilterControl>,
    /// The generation of the last push, which is what makes the one-shot stale timer that a
    /// push arms stop reporting once a newer push has arrived.
    stale_epoch: Rc<Cell<u64>>,
    /// Whether the picture on screen is still being continued; the timer reports only over
    /// a picture that is.
    stale_live: Rc<Cell<bool>>,
    /// The staleness tick this pane is waiting for, if any.
    stale_timer: Option<TimerId>,
    /// Whether the pointer went down on the ODbL credit, which is what makes the release
    /// on it a click on the credit rather than the end of a pan.
    credit_pressed: bool,
    /// The application's command for opening an external link (the ODbL credit and the
    /// entitlement's recovery control).
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
        // A host without the Kytrix service has nothing to subscribe to and nothing to
        // read over; `load` reports that as the failure it is rather than leaving the pane
        // waiting on a connection nobody is going to install. Every application supplies
        // one.
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
        // The picture on screen was served to the account that left, so a load for another
        // account starts over rather than keeping it; a retry for the same account keeps
        // what the user is still reading.
        let kept = retry && account == self.account_id;
        self.account_id = account.clone();
        let Some(account) = account else {
            self.clear_picture();
            self.report_sign_in(cx);
            return;
        };
        // The coastline is the pane's floor: without it there is no map to draw ships on,
        // so a pane that cannot parse it fails before it asks for a subscription. A retry
        // asks again; the parser caches its failure, so the answer is the same one.
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
        // A host with no Kytrix connection has nothing to subscribe over. That is a
        // failure between the application and the service, not a link that is still being
        // established: reporting `Connecting` here would leave the spinner waiting for a
        // connection nobody is going to install, with no way to say so. The retry is the
        // recovery, because the missing connection is not something this pane can repair.
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
        // The picture the retry keeps on screen is not being continued either: a
        // subscription that takes over and then delivers nothing has to fail the pane the
        // same way a link that went silent does, and this is the push it counts from.
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

    /// A pane is content once an AIS picture has arrived.
    ///
    /// It is the picture that decides, not the number of vessels in it: an empty strait is
    /// a picture the pane draws the map for, while a pane that has not received one yet has
    /// nothing to draw at all — not even an empty sea, which would claim the service
    /// answered.
    fn has_content(&self) -> bool {
        self.vessels.has_state()
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.body_root.is_some() {
            // The framework builds a body once; a second call is a caller that did not read
            // the contract, and building the canvas node twice would double it.
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
        // A push that arrived before the body exists armed nothing: the timer is scoped to
        // the body root, and this is where that scope is handed over.
        if self.stale_live.get() && self.stale_timer.is_none() {
            self.arm_stale_timer(cx);
        }
        // The canvas node has no box until the frame that lays this body out, so the map
        // cannot be painted on the frame that builds it: ask for the one that can.
        cx.request_frame()?;
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        // What the service delivered is drained before anything in this frame can replace a
        // command: an answer that arrived before the frame belongs to the command that was
        // in flight when it arrived.
        self.drain_inbox(cx);

        // The settings are the pane's scope: signing in as someone else is another read and
        // signing out is no read at all. A settings write reaches this hook on the frame it
        // schedules (the surface root reads the same signal), and the comparison is what
        // makes it a reload only when the account really moved.
        let account = cx.account();
        if account != self.account_id {
            self.account_id = account.clone();
            if account.is_none() {
                self.sign_out(cx);
            } else {
                self.load(cx);
            }
        }

        // A header control owns no `&mut self`; it asks for the transition it was used for,
        // and this is where the request becomes one. The read is taken out of the signal
        // before the write: an engine signal is not copied on read, so a write while a read
        // guard is alive would alias the value it is reading.
        let request = *cx.read(&self.requests);
        if let Some(request) = request {
            cx.write(&self.requests, None);
            self.apply_header_request(request, cx);
        }

        // The body is built by the framework from the answer `has_content` gives, and it
        // asks that question before this hook runs: content that first appeared here is one
        // frame away, so ask for that frame.
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
        // The pane is building its row; the control this widget built into the previous one
        // went with it. Only the components are the widget's to take down.
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
                // The pane routes every event it sees to this hook, not only the ones over
                // the map, so a press beside the canvas is not a press that starts a pan.
                if *button != PointerButton::Primary || !self.map.contains(*position) {
                    return false;
                }
                self.credit_pressed = self.map.attribution_contains(*position);
                self.map.pointer_down(*position);
                true
            }
            InputEvent::PointerUp { position, .. } => {
                // A release this pane did not begin is not its event: the map is handed
                // only the ones a press started on it. A release that arrives off the box
                // still ends the drag, because the press that began it is still held.
                if !self.map.is_dragging() && !self.credit_pressed && !self.map.contains(*position)
                {
                    return false;
                }
                // The credit is a link: it opens only when the press began on it and ended
                // on it, so dragging the map across the corner does not open a browser.
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
                // A pointer that leaves the pane is on no vessel any more; without this the
                // card of the last vessel it was over would stay on a map nothing is
                // pointing at.
                self.credit_pressed = false;
                self.map.pointer_up();
                if self.map.clear_hover() {
                    let _ = cx.request_frame();
                }
                true
            }
            InputEvent::PointerCancel { .. } => {
                // A cancelled session is neither a click nor a release: the engine reports
                // it when the system takes the pointer away (capture lost, window lost
                // focus, a modal opened), and there is no `Up` to follow it. The credit is
                // not opened and the drag the press began ends here — without this the map
                // keeps panning on the next move for a button nothing is holding.
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
                    // The camera moved under a stationary pointer, so which vessel it is on
                    // moved with it.
                    self.map.track_pointer(*position, &self.painted);
                    let _ = cx.request_frame();
                }
                // The map consumes the wheel: a pane next to a scrolling parent would
                // otherwise zoom and scroll at once.
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
        // Dropping the bridge aborts its forwarding tasks and cancels the service.
        if let Some(mut binding) = self.binding.take() {
            binding.cancel();
        }
        // The header's group nodes belong to the pane, which removes its row: only the
        // component's signal scope and action handlers are this widget's to release.
        self.release_filter_control(cx, false);
        // The canvas node lives in the body, which the framework removes after this hook;
        // the node, the geometry it registered and its cache are this canvas' own, so they
        // go here.
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
    /// Builds a widget over the collaborators a caller hands it.
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

    /// Builds a pane over a test's own AIS source.
    ///
    /// The framework's headless host composes the services without a Kytrix connection (the
    /// connector still needs the application's own runtime to build), so an interface test
    /// constructs the widget directly with the fixture it wants, and drives the widget hooks
    /// itself.
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

    /// Reports the neutral notice for a pane nobody is signed in for.
    fn report_sign_in(&self, cx: &mut WidgetCx<'_>) {
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::message("widget.global_shipping_chokepoint.sign_in")),
        );
    }

    /// Drops the picture and the pointer's vessel together.
    ///
    /// The store resets its baseline as well, which is what keeps a delta of the session
    /// being abandoned from being applied to the picture that replaced it. The filter is
    /// kept: it is the reader's choice, not part of the picture.
    fn clear_picture(&mut self) {
        self.vessels.clear();
        self.painted = Arc::default();
        self.present_types.clear();
        self.map.clear_hover();
        self.picture_revision = self.picture_revision.wrapping_add(1);
    }

    /// Rebuilds what the filter draws from the picture the store holds.
    ///
    /// The present types are derived from every vessel, before the filter is applied, so
    /// the legend and the header keep offering a type even while another one is selected.
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

    /// The account left: the picture goes, the stream stops, and the pane waits for a
    /// sign-in.
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

    /// Restricts the map to one ship type, or widens it back to all of them.
    ///
    /// The type is kept even when the picture no longer holds it: the filter is the
    /// reader's decision, and a type that leaves the picture leaves an empty sea rather
    /// than silently widening the selection back to everything.
    fn select_filter(&mut self, filter: ShipTypeFilter, cx: &mut WidgetCx<'_>) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        self.rebuild_painted();
        let _ = cx.request_frame();
    }

    /// Applies what a header control handed over.
    fn apply_header_request(&mut self, request: HeaderRequest, cx: &mut WidgetCx<'_>) {
        match request {
            HeaderRequest::Filter(filter) => self.select_filter(filter, cx),
        }
    }

    /// Consumes what the binding delivered, in arrival order.
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

    /// Applies one RPC reply, once its request identity says which command it answers.
    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<GlobalShippingChokepointProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        // A failure is terminal, and every entry point has to say so: the reply to a
        // command the recovery replaced would otherwise report the same failure again.
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

    /// Applies the failure the reliable stream carried.
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
        // An account without the AIS entitlement is missing the subscription rather than the
        // connection, so the control it is offered is where one is bought. Every other
        // failure offers the framework's retry.
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

    /// Applies one push of the latest stream.
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

    /// Folds one push into the picture and arms the staleness clock.
    ///
    /// A push that did not continue the picture is the one case where what arrived is not
    /// what the pane reports: the picture it holds is now missing a message, and no later
    /// push can repair that, so the pane reports the failure the banner is drawn for and
    /// leaves the vessels on the map under it.
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
            // A keyframe the server stamped with no time says it holds no state: the picture
            // is cleared and the pane waits for one instead of drawing an empty sea.
            self.status.report(cx.app_mut(), WidgetState::Connecting);
            self.disarm_stale_timer(cx);
        }
        let _ = cx.request_frame();
    }

    /// Whether a stream message belongs to the picture the pane is showing.
    ///
    /// Three things have to hold, and none is implied by the others: the message has to be
    /// about the area the map draws, somebody has to be signed in (a stop that follows a
    /// sign-out is asynchronous, and a push already in flight must not put the old account's
    /// vessels back on screen), and the pane must not have failed.
    fn is_about_the_picture(&self, area: GlobalShippingChokepointArea, app: &App) -> bool {
        area == GlobalShippingChokepointArea::Hormuz
            && self.account_id.is_some()
            && !self.status.is_failed(app)
    }

    /// Reports the failure a picture that stopped being continued is shown under.
    ///
    /// The recovery is the framework's own retry: the picture cannot be reconstructed from
    /// what the pane holds, so the way out is a new subscription rather than anything the
    /// pane could do to the one it has.
    fn fail_stale(&mut self, stale: StalePicture, cx: &mut WidgetCx<'_>) {
        self.disarm_stale_timer(cx);
        let recovery = self.recovery.clone();
        self.status
            .report(cx.app_mut(), WidgetState::failed(Arc::new(stale), recovery));
    }

    /// Arms the one-shot timer that fails a link which goes silent.
    ///
    /// The epoch is what makes a newer push cancel the older timer's report: a timer that
    /// fires after another push armed its own is not a silence, because the picture has
    /// been continued in between.
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

    /// Stops the staleness clock, because a failure or a new subscription replaced it.
    fn disarm_stale_timer(&mut self, cx: &mut WidgetCx<'_>) {
        self.stale_live.set(false);
        if let Some(id) = self.stale_timer.take() {
            cx.app_mut().cancel_timer(&id);
        }
    }

    /// Paints the map from the picture, the camera and the pointer the pane holds.
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
            // The canvas is a node the frame pipeline paints; asking for the frame here is
            // harmless when the hook already runs inside one and necessary when a hover
            // arrived between two.
            let _ = cx.request_frame();
        }
        Ok(())
    }

    /// Builds the filter row into the pane's left header group.
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

    /// Builds the select itself, with the choices the picture offers.
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
        // The items are stated in the language showing now; a language switch re-states
        // them, which is the kit's static-copy rebuild rule (§15.1).
        select
            .set_items(cx.app_mut(), self.filter_items(&translate))
            .unwrap_or_else(|error| {
                log::warn!("the Strait of Hormuz filter could not state its choices: {error}");
            });
        select.set_chosen(cx.app_mut(), [self.filter.key()], false);
        select.build(cx.app_mut(), window, parent)?;
        Ok(select)
    }

    /// The choices the filter offers: "all", then the types the picture holds.
    ///
    /// A type the picture does not hold is not offered — an entry that draws nothing would
    /// be a choice about a vessel nobody reports. The chosen type is kept in the filter even
    /// when it leaves the picture; the select then states its placeholder instead, which is
    /// what says the selection is still narrowed to something that is not there.
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
        // A type the filter is narrowed to that the picture no longer holds is still the
        // selection: it is stated as a disabled entry rather than dropped, because a select
        // that lost its chosen item would fall back to the placeholder and stop saying what
        // the map is restricted to (W6 difference: the old menu simply omitted it while the
        // trigger kept the sentence).
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

    /// The text the filter states as its placeholder: the choice it is narrowed to.
    fn filter_trigger_text(&self, translate: &Translate) -> SmolStr {
        match self.filter {
            ShipTypeFilter::All => {
                t_smol!(translate, "widget.global_shipping_chokepoint.filter.all")
            }
            ShipTypeFilter::Only(ship_type) => t_smol!(translate, ship_type.label_key()),
        }
    }

    /// Keeps the filter's copy, choices and choice current.
    ///
    /// The select's accessible name, placeholder and item labels are resolved text chosen
    /// when it is built (AGENTS §15.1), so a language switch rebuilds it in the group the
    /// pane gave it; a picture that changed the types only re-states the items.
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
        // The pane rebuilds the row it calls `header` for, so the row this widget built into
        // may already be gone — and it took the control with it. Touching a stale node would
        // fail the whole synchronization, so a row that is missing is a release.
        if cx.tree().get(root).is_err() {
            self.release_filter_control(cx, false);
            return Ok(());
        }
        let language_now = *cx.read(&cx.language());
        if language != language_now {
            // The copy is build-time: the row is replaced in the group it occupies rather
            // than re-bound, the same rule the pane's own header follows.
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

    /// Releases the filter's component, and the widget's own row when asked to.
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

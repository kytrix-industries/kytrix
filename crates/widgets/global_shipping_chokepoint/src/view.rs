//! The Strait of Hormuz pane: its coastline, its lanes, and the vessels an AIS
//! subscription reports in it.
//!
//! # What the frame draws
//!
//! One canvas, under elements that carry the words. The base map's coastline, borders and
//! roads are layers parsed once by [`crate::scene`]; [`crate::paint`] turns them, the
//! lanes and the vessel markers into the paths the canvas paints for the frame's camera,
//! and the labels, the legend, the hover card, the stale badge and the empty state are
//! absolutely positioned over it. Nothing tessellates per frame: a frame only applies the
//! camera's affine transform to geometry that was prepared once.
//!
//! # What the pane reports, and what it holds
//!
//! The picture is one AIS subscription's. A keyframe replaces it, a delta only applies on
//! top of the picture already held, and a push that does not continue it is a gap the
//! stale badge names rather than a state the pane guesses at. The frame is therefore a
//! picture held between pushes, and the two things the pane owns beyond it are the camera
//! and the vessel the pointer is on.
//!
//! # The body is the ready state
//!
//! The framework draws the placeholder, the banner and the spinner, and it only embeds
//! this body when [`GlobalShippingChokepointWidget::has_content`] is true. So there is no state
//! branch here: the body is what a pane with a picture draws, and an empty sea is drawn
//! as a map with the catalogue's own "no data" on it.

use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use app_config_provider::{events::AccountChanged, manager::ConfigManager};
use base_ui::{
    empty_state::EmptyState,
    size::UnifiedSize,
    style::theme::{Theme, ThemeExt},
};
use gpui_kit::{
    AnyElement, App, Bounds, Context, ElementId, Entity, FontWeight, Hsla, InteractiveElement,
    IntoElement, MouseButton, MouseMoveEvent, ParentElement, PinchEvent, Pixels, Point, Render,
    ScrollDelta, ScrollWheelEvent, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Window, canvas, div, point, prelude::FluentBuilder, px,
};
use i18n::t;
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
use widget::{
    account::account_of,
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::LocalizedError,
    identity::{WidgetId, WidgetInstanceId},
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    time_zone::{data_time_zone, notify_on_data_time_zone_change},
    view::WidgetView,
};
use widget_tokio_runtime::global;

use crate::{
    model::{
        self, CITY_LABELS, COUNTRY_LABELS, Camera, MAX_VESSELS, MAX_ZOOM, MIN_ZOOM, STALE_AFTER,
        ShipClass, Vessel, VesselDetails, VesselStore, WATER_LABELS, colour_for,
    },
    paint::paint_map,
    scene::{self, Scene, SceneError},
    service::{
        AisStreamError, AisStreamMessage, GlobalShippingChokepointProtocol,
        GlobalShippingChokepointRequest, GlobalShippingChokepointScope,
        GlobalShippingChokepointService, GlobalShippingChokepointSource,
    },
};

/// The zoom at which the cities are named.
///
/// At the fitted zoom the whole strait is on screen and twelve city names would be a
/// haze over it; the countries and the two seas are what a reader needs then.
const CITY_LABEL_ZOOM: f32 = 1.6;

/// How much one wheel notch scales the camera.
const WHEEL_ZOOM_STEP: f32 = 1.15;

/// How late the one-shot staleness frame is asked for, past the window it reports.
///
/// The predicate is "more than the window has gone by", and a wake exactly on the window's
/// edge races it. A link that went silent has no next push to arm another frame, so the
/// slack is the difference between a badge that appears and one that never does.
const STALE_TIMER_SLACK: Duration = Duration::from_millis(100);

/// The marker length at the fitted zoom, and at the closest one, in pixels.
const MARKER_LENGTH_NEAR: f32 = 10.0;
/// The marker length at [`MAX_ZOOM`].
const MARKER_LENGTH_FAR: f32 = 12.0;

/// How opaque a lane is drawn over the sea.
const DASH_ALPHA: f32 = 0.55;

/// How wide the hover card is.
const CARD_WIDTH: f32 = 208.0;

/// How far the hover card is kept off the hull it describes, and off the pane's edges.
const CARD_MARGIN: f32 = 12.0;

/// The height the hover card is kept inside the pane by, before it has been measured.
///
/// Fifteen rows of text at the small size, the fourteen gaps between them, and the card's
/// padding and border. A row is a 0.75 rem line box at the default 1.618 line height, so
/// about 20 px; the 10 px of slack keeps the estimate an upper bound, which is the safe
/// direction: an underestimate would let the last rows fall outside the pane, while an
/// overestimate only lifts the card a little.
const CARD_HEIGHT: f32 = 15.0 * 20.0 + 14.0 * 4.0 + 2.0 * 4.0 + 10.0;

/// The order the four vessel classes are drawn and listed in.
pub(super) const CLASSES: [ShipClass; 4] = [
    ShipClass::Tanker,
    ShipClass::Container,
    ShipClass::Bulk,
    ShipClass::Other,
];

/// The catalogue keys of the four class names, in [`CLASSES`] order.
const SHIP_TYPE_KEYS: [&str; 4] = [
    "widget.global_shipping_chokepoint.ship_type.tanker",
    "widget.global_shipping_chokepoint.ship_type.container",
    "widget.global_shipping_chokepoint.ship_type.bulk",
    "widget.global_shipping_chokepoint.ship_type.other",
];

/// The catalogue keys of the four legend entries, in [`CLASSES`] order.
const LEGEND_KEYS: [&str; 4] = [
    "widget.global_shipping_chokepoint.legend.tanker",
    "widget.global_shipping_chokepoint.legend.container",
    "widget.global_shipping_chokepoint.legend.bulk",
    "widget.global_shipping_chokepoint.legend.other",
];

/// The fifteen labels of the hover card, in the order the design lists the items in.
const HOVER_KEYS: [&str; 15] = [
    "widget.global_shipping_chokepoint.hover.ship_name",
    "widget.global_shipping_chokepoint.hover.mmsi",
    "widget.global_shipping_chokepoint.hover.imo",
    "widget.global_shipping_chokepoint.hover.call_sign",
    "widget.global_shipping_chokepoint.hover.ship_type",
    "widget.global_shipping_chokepoint.hover.flag",
    "widget.global_shipping_chokepoint.hover.destination",
    "widget.global_shipping_chokepoint.hover.eta",
    "widget.global_shipping_chokepoint.hover.draught",
    "widget.global_shipping_chokepoint.hover.dimensions",
    "widget.global_shipping_chokepoint.hover.speed",
    "widget.global_shipping_chokepoint.hover.course",
    "widget.global_shipping_chokepoint.hover.heading",
    "widget.global_shipping_chokepoint.hover.nav_status",
    "widget.global_shipping_chokepoint.hover.updated",
];

/// The catalogue keys naming the fifteen navigation statuses, by their AIS index.
const NAV_STATUS_KEYS: [&str; 15] = [
    "widget.global_shipping_chokepoint.nav_status.0",
    "widget.global_shipping_chokepoint.nav_status.1",
    "widget.global_shipping_chokepoint.nav_status.2",
    "widget.global_shipping_chokepoint.nav_status.3",
    "widget.global_shipping_chokepoint.nav_status.4",
    "widget.global_shipping_chokepoint.nav_status.5",
    "widget.global_shipping_chokepoint.nav_status.6",
    "widget.global_shipping_chokepoint.nav_status.7",
    "widget.global_shipping_chokepoint.nav_status.8",
    "widget.global_shipping_chokepoint.nav_status.9",
    "widget.global_shipping_chokepoint.nav_status.10",
    "widget.global_shipping_chokepoint.nav_status.11",
    "widget.global_shipping_chokepoint.nav_status.12",
    "widget.global_shipping_chokepoint.nav_status.13",
    "widget.global_shipping_chokepoint.nav_status.14",
];

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

impl LocalizedError for MapAssetInvalid {
    fn localized_message(&self, cx: &App) -> SharedString {
        t!(cx, "error.global_shipping_chokepoint.map_asset")
    }
}

/// The box the canvas was last laid out in, and the camera clamped into it.
///
/// A canvas learns its box during prepaint and the pointer handlers, the labels and the
/// hit test all need it, so the two values are kept in one place the entity and the canvas
/// callbacks share instead of being recomputed or dropped between frames.
#[derive(Default)]
struct MapFrame {
    bounds: Cell<Option<Bounds<Pixels>>>,
    camera: Cell<Camera>,
}

/// The colours one frame is drawn with, resolved from the theme it is drawn in.
///
/// Nothing is kept between frames: the appearance can change while the pane is open, and
/// a colour cached onto the widget would be a map that kept drawing the old theme.
///
/// [`crate::paint`] reads the map fields; the struct's own `pub(super)` keeps them inside
/// the crate even though the fields themselves are `pub`.
#[derive(Clone, Copy)]
pub(super) struct MapColors {
    /// The sea, which is also the canvas' own fill.
    ocean: Hsla,
    /// The land the coastline is filled with.
    pub land: Hsla,
    /// The land borders drawn over the land.
    pub map_border: Hsla,
    /// Motorways and trunks, drawn over the land and the sea.
    pub road: Hsla,
    /// Primary roads, the finer of the two road strokes.
    pub road_minor: Hsla,
    /// The lanes, already at their opacity.
    pub lane: Hsla,
    /// One colour per class in [`CLASSES`] order.
    pub ships: [Hsla; 4],
    /// The dot a vessel without a ship record is drawn as.
    pub unknown: Hsla,
    /// The ring around the vessel under the pointer.
    pub hover: Hsla,
    /// The fill of the labels' chips and the card.
    overlay: Hsla,
    /// The edge of those chips and of the card.
    border: Hsla,
    /// Text and the city dots.
    text: Hsla,
    /// Secondary text: the legend's title, the card's labels.
    muted: Hsla,
    /// The stale badge, which is a warning rather than an error.
    warning: Hsla,
}

impl MapColors {
    pub(super) fn resolve(theme: &Theme) -> Self {
        let roles = &theme.roles;
        Self {
            ocean: roles.chart.map_ocean.hsla,
            land: roles.chart.map_land.hsla,
            // The theme has no map-border or map-road roles, so these three borrow opaque
            // ones: the strong line for the borders, the sixth chart series for motorways
            // and the line border for primary roads. Each has to read on both fills — a
            // road survives the bridge or causeway that carries it over the sea — which is
            // what `the_map_border_and_roads_read_on_both_fills` pins.
            map_border: roles.line.strong.hsla,
            road: roles.chart.series[5].hsla,
            road_minor: roles.line.border.hsla,
            lane: roles.chart.series[3].hsla.opacity(DASH_ALPHA),
            ships: CLASSES.map(|class| colour_for(theme, class)),
            unknown: roles.fg.muted.hsla,
            hover: roles.line.focus.hsla,
            overlay: roles.surface.overlay.hsla,
            border: roles.line.border.hsla,
            text: roles.fg.default.hsla,
            muted: roles.fg.muted.hsla,
            warning: roles.status.warning.base.hsla,
        }
    }
}

/// Where the map's Mercator points land in the canvas.
#[derive(Clone, Copy, Debug)]
pub(super) struct Projection {
    /// The Mercator point drawn at the centre of the canvas.
    center: [f32; 2],
    /// The pixels one Mercator unit is drawn at.
    scale: f32,
    /// The canvas' size.
    pub viewport: [f32; 2],
    /// The camera's zoom, which is what the marker size is read from.
    pub zoom: f32,
}

impl Projection {
    pub(super) fn new(camera: Camera, viewport: [f32; 2]) -> Self {
        Self {
            center: camera.center,
            scale: model::fit_px_per_merc(viewport) * camera.zoom,
            viewport,
            zoom: camera.zoom,
        }
    }

    /// Whether the projection can draw anything at all.
    ///
    /// A pane laid out at zero size has no fit, and a marker drawn through a zero scale is
    /// a degenerate triangle rather than a vessel.
    pub(super) fn drawable(self) -> bool {
        self.scale.is_finite()
            && self.scale > 0.0
            && self.viewport[0] > 0.0
            && self.viewport[1] > 0.0
    }

    /// The canvas point one Mercator point is drawn at.
    ///
    /// A Mercator `y` grows north and a canvas `y` grows down, so the two run in opposite
    /// directions: the centre is subtracted the other way round for `y`, which is what puts
    /// the Arabian side under the Iranian one rather than over it.
    pub(super) fn screen(self, mercator: [f32; 2]) -> [f32; 2] {
        [
            (mercator[0] - self.center[0]) * self.scale + self.viewport[0] / 2.0,
            (self.center[1] - mercator[1]) * self.scale + self.viewport[1] / 2.0,
        ]
    }

    /// The Mercator point drawn at one canvas point, which is what a zoom keeps fixed.
    pub(super) fn mercator(self, canvas: [f32; 2]) -> [f32; 2] {
        if !self.drawable() {
            return self.center;
        }
        [
            self.center[0] + (canvas[0] - self.viewport[0] / 2.0) / self.scale,
            self.center[1] - (canvas[1] - self.viewport[1] / 2.0) / self.scale,
        ]
    }

    /// The Mercator rectangle the canvas shows, as `[west, south, east, north]`.
    ///
    /// This is what the culling grid is read from: a layer indexes only the cells this
    /// rectangle touches. Only meaningful for a drawable projection — a pane with no
    /// size has no scale to divide by, and every painter checks [`Self::drawable`]
    /// before it asks.
    pub(super) fn visible_mercator(self) -> [f32; 4] {
        let half = [
            self.viewport[0] / (2.0 * self.scale),
            self.viewport[1] / (2.0 * self.scale),
        ];
        [
            self.center[0] - half[0],
            self.center[1] - half[1],
            self.center[0] + half[0],
            self.center[1] + half[1],
        ]
    }

    /// Whether a canvas point is inside the canvas.
    fn on_screen(self, canvas: [f32; 2]) -> bool {
        canvas[0] >= 0.0
            && canvas[0] <= self.viewport[0]
            && canvas[1] >= 0.0
            && canvas[1] <= self.viewport[1]
    }

    /// Whether a canvas point is inside the canvas grown by `margin` on every side.
    pub(super) fn near_screen(self, canvas: [f32; 2], margin: f32) -> bool {
        canvas[0] >= -margin
            && canvas[0] <= self.viewport[0] + margin
            && canvas[1] >= -margin
            && canvas[1] <= self.viewport[1] + margin
    }

    /// Whether the box one screen-space segment covers reaches the canvas.
    ///
    /// The endpoints of a segment are not enough: a lane's segment can leave the canvas on
    /// both ends while crossing it.
    pub(super) fn segment_near(self, start: [f32; 2], end: [f32; 2], margin: f32) -> bool {
        let left = start[0].min(end[0]);
        let right = start[0].max(end[0]);
        let top = start[1].min(end[1]);
        let bottom = start[1].max(end[1]);

        right >= -margin
            && left <= self.viewport[0] + margin
            && bottom >= -margin
            && top <= self.viewport[1] + margin
    }

    /// The length one vessel marker is drawn at, which grows with the camera.
    pub(super) fn marker_length(self) -> f32 {
        let fraction = ((self.zoom - MIN_ZOOM) / (MAX_ZOOM - MIN_ZOOM)).clamp(0.0, 1.0);
        MARKER_LENGTH_NEAR + (MARKER_LENGTH_FAR - MARKER_LENGTH_NEAR) * fraction
    }
}

/// The canvas' size as the number the projection is built from.
pub(super) fn viewport_of(bounds: Bounds<Pixels>) -> [f32; 2] {
    [f32::from(bounds.size.width), f32::from(bounds.size.height)]
}

/// One pane: the picture an AIS subscription publishes, the camera it is drawn with, and
/// the vessel the pointer is on.
pub struct GlobalShippingChokepointWidget {
    binding: WidgetBinding<GlobalShippingChokepointProtocol>,
    status: Entity<WidgetStatus>,
    recovery: WidgetAction,
    /// The account the picture belongs to, which is what an account change is compared
    /// against.
    account_id: Option<SmolStr>,
    /// The coastline, parsed once for the process; a failure to parse it is a failure the
    /// pane reports rather than one it retries.
    scene: Option<&'static Scene>,
    /// The canvas' box and the camera clamped into it.
    frame: Rc<MapFrame>,
    /// The picture the subscription published last.
    vessels: VesselStore,
    /// The vessels the picture holds, joined and projected when a push arrived.
    ///
    /// Building this is the one expensive step of a push, and it is what both the painters
    /// and the hit test read: a frame does not rebuild it, because a frame changes the
    /// camera rather than the picture.
    painted: Arc<[Vessel]>,
    /// The painted vessels in canvas coordinates, filled when a pointer move is resolved.
    hit_points: Vec<(u32, [f32; 2])>,
    /// The vessel the pointer is on, if any.
    hovered: Option<u32>,
    /// The last pointer position, in window coordinates, which is what a drag's increment
    /// and the wheel's anchor are read from.
    pointer: Point<Pixels>,
    /// The generation of the last push, which is what makes the one-shot stale timer that
    /// a push arms stop notifying once a newer push has arrived.
    stale_epoch: u64,
    /// Keeps the pane following the account the settings hold.
    _account: Subscription,
    /// Keeps the pane following the display time zone the hover card's receipt time is
    /// read in.
    _time_zone: Option<Subscription>,
}

impl WidgetView for GlobalShippingChokepointWidget {
    type Config = GlobalShippingChokepointConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.global_shipping_chokepoint");
    const DISPLAY_NAME: &'static str = "widget.names.global_shipping_chokepoint";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: &'static str =
        "Live AIS vessel positions across strategic shipping chokepoints";
    const ICON: Option<&'static str> = Some("widgets/global_shipping_chokepoint.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = cx.global::<KytrixServiceConnector>().clone();
        Self::new_with_source(config, source, status, config_change, recovery, cx)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(account) = account_of(&cx.global::<ConfigManager>().read()) else {
            self.report_sign_in(cx);
            return;
        };
        self.account_id = Some(account.clone());
        // The coastline is the pane's floor: without it there is no map to draw ships on,
        // so a pane that cannot parse it fails before it asks for a subscription. A retry
        // asks again; the parser caches its failure, so the answer is the same one.
        if self.scene.is_none() && !self.load_scene(cx) {
            return;
        }
        self.status.report(WidgetState::Connecting, cx);
        self.binding.call(
            GlobalShippingChokepointRequest::Load {
                scope: GlobalShippingChokepointScope::new(account),
            },
            cx,
            Self::on_load_result,
        );
    }

    /// A pane is content once an AIS picture has arrived.
    ///
    /// It is the picture that decides, not the number of vessels in it: an empty strait is
    /// a picture the pane draws the empty state on, while a pane that has not received one
    /// yet has nothing to draw at all — not even an empty sea, which would claim the
    /// service answered.
    fn has_content(&self) -> bool {
        self.vessels.has_state()
    }
}

impl GlobalShippingChokepointWidget {
    /// The seam a test can answer through: `new` only picks the production source.
    fn new_with_source<S>(
        _config: GlobalShippingChokepointConfig,
        source: S,
        status: Entity<WidgetStatus>,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: GlobalShippingChokepointSource,
    {
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            GlobalShippingChokepointService::new(source),
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        // The account is the pane's scope, and the user can change it while the pane is
        // open: signing out has to take the picture with it, and signing in as someone
        // else has to read again.
        let events = cx.global::<ConfigManager>().events().clone();
        let account = cx.subscribe(&events, |this, _, _: &AccountChanged, cx| {
            this.reconcile_account(account_of(&cx.global::<ConfigManager>().read()), cx);
        });
        // The receipt time the card shows is read in the display zone of the frame that
        // draws it, so the pane has to be woken when that zone moves.
        let time_zone = notify_on_data_time_zone_change(cx);

        Self {
            binding,
            status,
            recovery,
            // The pane records the account it was built for, but it starts nothing: the
            // framework calls `load` once construction is done.
            account_id: account_of(&cx.global::<ConfigManager>().read()),
            scene: None,
            frame: Rc::new(MapFrame::default()),
            vessels: VesselStore::new(),
            painted: Arc::default(),
            hit_points: Vec::with_capacity(MAX_VESSELS),
            hovered: None,
            pointer: point(px(0.0), px(0.0)),
            stale_epoch: 0,
            _account: account,
            _time_zone: time_zone,
        }
    }

    /// Says what a pane nobody is signed in for waits for.
    ///
    /// The reference is into the catalogue and is resolved on the frame that draws it, in
    /// the language showing then.
    fn report_sign_in(&mut self, cx: &mut Context<Self>) {
        self.status.report(
            WidgetState::waiting(Notice::message("widget.global_shipping_chokepoint.sign_in")),
            cx,
        );
    }

    /// Parses the packaged coastline, reporting the failure it could not get past.
    ///
    /// The parser keeps both its answer and its failure for the process, so this is
    /// idempotent: a second call after a failure reaches the same error rather than
    /// re-reading the asset.
    fn load_scene(&mut self, cx: &mut Context<Self>) -> bool {
        match scene::scene() {
            Ok(scene) => {
                self.scene = Some(scene);
                true
            }
            Err(error) => {
                self.fail_map_asset(error, cx);
                false
            }
        }
    }

    /// Reports a coastline the pane could not parse, with the framework's recovery.
    fn fail_map_asset(&mut self, error: SceneError, cx: &mut Context<Self>) {
        let recovery = self.recovery.clone();
        self.status.report(
            WidgetState::failed(Arc::new(MapAssetInvalid::from(error)), recovery),
            cx,
        );
    }

    /// Follows the account the settings now hold.
    ///
    /// Signing out has to take the picture with it, and it has to do so twice: the
    /// synchronous clear does not depend on the stop request's task surviving, and the
    /// clear in the answer covers the window between this clear and the service actually
    /// cancelling the stream. A sign-in is the same start the framework's first load is.
    fn reconcile_account(&mut self, account: Option<SmolStr>, cx: &mut Context<Self>) {
        if self.account_id == account {
            return;
        }
        self.account_id = account.clone();
        match account {
            Some(_) => self.load(cx),
            None => {
                self.clear_picture();
                self.report_sign_in(cx);
                self.binding
                    .call(GlobalShippingChokepointRequest::Stop, cx, |this, _, cx| {
                        this.clear_picture();
                        cx.notify();
                    });
            }
        }
    }

    /// Drops the picture and the pointer's vessel together.
    ///
    /// The store resets its baseline as well, which is what keeps a delta of the session
    /// being abandoned from being applied to the picture that replaced it.
    fn clear_picture(&mut self) {
        self.vessels.clear();
        self.painted = Arc::default();
        self.hovered = None;
        self.hit_points.clear();
    }

    fn on_load_result(
        &mut self,
        result: WidgetRpcResult<GlobalShippingChokepointProtocol>,
        cx: &mut Context<Self>,
    ) {
        // The answer says the service took the request; what the subscription then reports
        // about itself arrives on the stream, which is where a failure to subscribe is
        // drawn from.
        let Err(error) = result else {
            return;
        };
        let recovery = self.recovery.clone();
        self.status
            .report(WidgetState::failed(Arc::new(error), recovery), cx);
    }

    /// Applies one push of the latest stream.
    ///
    /// The stream's key is the area, and the pane reads exactly one, so a message for
    /// another area is not this pane's. A pane nobody is signed in for is not receiving
    /// either: the stop that follows a sign-out is asynchronous, and the guard is what
    /// keeps a push that was already in flight from putting the old account's vessels
    /// back on screen.
    fn apply_latest(
        &mut self,
        area: GlobalShippingChokepointArea,
        message: Arc<AisStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if area != GlobalShippingChokepointArea::Hormuz
            || self.account_id.is_none()
            || self.status.is_failed(cx)
        {
            return false;
        }
        let AisStreamMessage::Update(update) = message.as_ref() else {
            return false;
        };
        self.apply_update(update, cx);
        true
    }

    /// Reports the failure the reliable stream carried.
    fn apply_reliable(
        &mut self,
        area: GlobalShippingChokepointArea,
        message: Arc<AisStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if area != GlobalShippingChokepointArea::Hormuz
            || self.account_id.is_none()
            || self.status.is_failed(cx)
        {
            return false;
        }
        let AisStreamMessage::Failed(error) = message.as_ref() else {
            return false;
        };
        // An account without the AIS entitlement is missing the subscription rather than
        // the connection, so the control it is offered is where one is bought. Every other
        // failure offers the framework's retry.
        let action = if matches!(
            error.as_ref(),
            AisStreamError::Service(KytrixServiceError::Server(ErrorCode::EntitlementRequired))
        ) {
            view_plans()
        } else {
            self.recovery.clone()
        };
        let shared: Arc<AisStreamError> = Arc::clone(error);
        let failure: Arc<dyn LocalizedError> = shared;
        self.status.report(WidgetState::failed(failure, action), cx);
        true
    }

    /// Folds one push into the picture and arms the staleness clock.
    fn apply_update(&mut self, update: &GlobalShippingChokepointUpdate, cx: &mut Context<Self>) {
        self.vessels.apply(update);
        self.painted = self.vessels.vessels().into();
        // A keyframe the server stamped with no time says it holds no state: the picture
        // is cleared and the pane waits for one instead of drawing an empty sea.
        if self.vessels.has_state() {
            self.status.report(WidgetState::Ready, cx);
        } else {
            self.status.report(WidgetState::Connecting, cx);
        }
        self.arm_stale_timer(cx);
    }

    /// Arms the one-shot timer that makes a silent link's badge appear.
    ///
    /// Staleness is derived from the store rather than written by this timer: the timer
    /// only asks for the frame that re-evaluates the predicate. Each push replaces the
    /// generation, so the timer of a push that was followed by another notifies nothing,
    /// and a timer that did reach its deadline notifies once rather than asking for every
    /// frame after it.
    fn arm_stale_timer(&mut self, cx: &mut Context<Self>) {
        self.stale_epoch += 1;
        let epoch = self.stale_epoch;
        cx.spawn(async move |this, cx| {
            // A wake exactly on the window's edge can read as "not past it" for the
            // predicate's strict comparison, and a link that went silent has no next push
            // to arm another frame: the slack is what keeps that race from leaving a dead
            // link unbadged for ever.
            cx.background_executor()
                .timer(STALE_AFTER + STALE_TIMER_SLACK)
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.stale_epoch == epoch {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The vessel nearest one window position, if one is close enough to it.
    ///
    /// The painted vessels are projected at the frame's camera, which is the geometry the
    /// pointer is actually over; a picture that arrived since then is not what the reader
    /// was pointing at, and the next move resolves against it.
    fn vessel_at(&mut self, position: Point<Pixels>) -> Option<u32> {
        let bounds = self.frame.bounds.get()?;
        if !bounds.contains(&position) {
            return None;
        }
        let projection = Projection::new(self.frame.camera.get(), viewport_of(bounds));
        if !projection.drawable() {
            return None;
        }
        let canvas = [
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
        ];
        self.hit_points.clear();
        self.hit_points.extend(
            self.painted
                .iter()
                .map(|vessel| (vessel.mmsi, projection.screen(vessel.position))),
        );
        model::hit_test(&self.hit_points, canvas)
    }

    /// Resolves the vessel under one window position, reporting whether the card moved.
    fn track_pointer(&mut self, position: Point<Pixels>) -> bool {
        let hit = self.vessel_at(position);
        if hit == self.hovered {
            return false;
        }
        self.hovered = hit;
        true
    }

    /// The pointer moving over the map: a held left button pans it, anything else resolves
    /// the vessel under the pointer.
    fn on_pointer_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self.pointer;
        self.pointer = event.position;
        if event.pressed_button == Some(MouseButton::Left) {
            // The card names a vessel the pointer has left behind as soon as the map starts
            // moving under it.
            if self.hovered.take().is_some() {
                cx.notify();
            }
            self.pan(event.position - previous, cx);
            return;
        }
        if self.track_pointer(event.position) {
            cx.notify();
        }
    }

    /// Moves the camera by one pointer increment, keeping it on the map.
    fn pan(&mut self, delta: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(bounds) = self.frame.bounds.get() else {
            return;
        };
        let camera = self.frame.camera.get();
        let scale = model::fit_px_per_merc(viewport_of(bounds)) * camera.zoom;
        if !scale.is_finite() || scale <= 0.0 {
            return;
        }
        self.set_camera(
            Camera {
                center: [
                    camera.center[0] - f32::from(delta.x) / scale,
                    camera.center[1] + f32::from(delta.y) / scale,
                ],
                zoom: camera.zoom,
            },
            cx,
        );
    }

    /// One wheel notch, as a zoom around the pointer.
    fn zoom_by_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let step = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y),
        };
        if !step.is_finite() || step == 0.0 {
            return;
        }
        // A wheel moved away from the reader is a wheel moved up — a positive delta —
        // which zooms in.
        let factor = if step > 0.0 {
            WHEEL_ZOOM_STEP
        } else {
            1.0 / WHEEL_ZOOM_STEP
        };
        self.zoom_at(event.position, factor, cx);
    }

    /// One pinch, as a zoom around the gesture's centre.
    fn zoom_by_pinch(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        if !event.delta.is_finite() || event.delta == 0.0 {
            return;
        }
        self.zoom_at(event.position, 1.0 + event.delta, cx);
    }

    /// Scales the camera around one window position, keeping the point under it fixed.
    fn zoom_at(&mut self, position: Point<Pixels>, factor: f32, cx: &mut Context<Self>) {
        let Some(bounds) = self.frame.bounds.get() else {
            return;
        };
        if !bounds.contains(&position) || !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let camera = self.frame.camera.get();
        let projection = Projection::new(camera, viewport_of(bounds));
        if !projection.drawable() {
            return;
        }
        let canvas = [
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
        ];
        let anchor = projection.mercator(canvas);
        let zoom = (camera.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if zoom == camera.zoom {
            return;
        }
        let scale = model::fit_px_per_merc(projection.viewport) * zoom;
        self.set_camera(
            Camera {
                center: [
                    anchor[0] - (canvas[0] - projection.viewport[0] / 2.0) / scale,
                    anchor[1] + (canvas[1] - projection.viewport[1] / 2.0) / scale,
                ],
                zoom,
            },
            cx,
        );
        // The camera moved under a stationary pointer, so which vessel it is on moved with
        // it.
        if self.track_pointer(position) {
            cx.notify();
        }
    }

    /// Keeps the camera on the map and asks for the frame that draws it.
    fn set_camera(&mut self, camera: Camera, cx: &mut Context<Self>) {
        let Some(bounds) = self.frame.bounds.get() else {
            return;
        };
        let clamped = camera.clamped(viewport_of(bounds));
        if clamped == self.frame.camera.get() {
            return;
        }
        self.frame.camera.set(clamped);
        cx.notify();
    }

    /// The map's labels: five countries, the cities once zoomed in, and two seas.
    ///
    /// They are elements rather than glyphs drawn on the canvas: each one goes through the
    /// catalogue, the platform's font fallback and the display's scale factor. A label
    /// whose anchor is off screen is not built at all.
    ///
    /// The countries are drawn in the default foreground rather than the design's
    /// `fg.subtle`: subtle reads about 2:1 on the land, and these are the only words drawn
    /// straight on the fill, so they take the readable colour and clear 4.5:1 in both
    /// appearances (pinned by the test below).
    fn labels(&self, projection: Projection, colors: MapColors, cx: &App) -> Vec<AnyElement> {
        let mut labels =
            Vec::with_capacity(COUNTRY_LABELS.len() + CITY_LABELS.len() + WATER_LABELS.len());
        for label in COUNTRY_LABELS {
            let [x, y] = projection.screen(model::project(label.at[0], label.at[1]));
            if !projection.on_screen([x, y]) {
                continue;
            }
            labels.push(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text)
                    .child(SharedString::from(t!(cx, label.key).to_uppercase()))
                    .into_any_element(),
            );
        }
        if projection.zoom >= CITY_LABEL_ZOOM {
            for label in CITY_LABELS {
                let [x, y] = projection.screen(model::project(label.at[0], label.at[1]));
                if !projection.on_screen([x, y]) {
                    continue;
                }
                labels.push(
                    div()
                        .absolute()
                        .left(px(x))
                        .top(px(y))
                        .flex()
                        .items_center()
                        .gap(px(3.0))
                        .child(
                            div()
                                .flex_none()
                                .size(px(3.0))
                                .rounded_full()
                                .bg(colors.text),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(colors.text)
                                .child(t!(cx, label.key)),
                        )
                        .into_any_element(),
                );
            }
        }
        for label in WATER_LABELS {
            let [x, y] = projection.screen(model::project(label.at[0], label.at[1]));
            if !projection.on_screen([x, y]) {
                continue;
            }
            labels.push(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .text_xs()
                    .italic()
                    .text_color(colors.muted)
                    .child(t!(cx, label.key))
                    .into_any_element(),
            );
        }
        labels
    }

    /// The legend's line for the size of the picture.
    ///
    /// The catalogue has no automatic plural rule, so the two keys are chosen here rather
    /// than interpolated into one: a strait holding one vessel reads "1 vessel", not "1
    /// vessels".
    fn vessel_count_text(&self, cx: &App) -> SharedString {
        let count = self.vessels.vessel_count();
        if count == 1 {
            t!(cx, "widget.global_shipping_chokepoint.vessel_count_one")
        } else {
            t!(
                cx,
                "widget.global_shipping_chokepoint.vessel_count",
                count = count
            )
        }
    }

    /// The legend in the pane's bottom-right corner: the four classes and the count.
    fn legend(&self, colors: MapColors, cx: &App) -> AnyElement {
        let size = UnifiedSize::Small;
        let count_text = self.vessel_count_text(cx);
        let entries = CLASSES
            .iter()
            .zip(LEGEND_KEYS)
            .map(|(class, key)| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex_none()
                            .size(px(8.0))
                            .rounded_sm()
                            .bg(colour_for(cx.theme(), *class)),
                    )
                    .child(div().text_color(colors.text).child(t!(cx, key)))
            })
            .collect::<Vec<_>>();

        div()
            .debug_selector(|| "TEST_CHOKEPOINT_LEGEND".into())
            .absolute()
            .right(px(8.0))
            .bottom(px(8.0))
            .flex()
            .flex_col()
            .gap(size.gap())
            .px(size.px())
            .py(size.px())
            .rounded_sm()
            .border_1()
            .border_color(colors.border)
            .bg(colors.overlay)
            .text_size(size.text_size())
            .child(
                div()
                    .text_color(colors.muted)
                    .child(t!(cx, "widget.global_shipping_chokepoint.legend.title")),
            )
            .children(entries)
            .child(div().text_color(colors.muted).child(count_text))
            .into_any_element()
    }

    /// The badge a picture that is no longer being continued is drawn under.
    ///
    /// A gap is a picture the pushes stopped continuing; a silence is a link that stopped
    /// delivering. The first names the picture, the second counts the silence, and both
    /// leave the vessels on the map: what is stale is the update, not what was received.
    fn stale_badge(&self, colors: MapColors, cx: &App) -> AnyElement {
        let text = if self.vessels.has_gap() {
            t!(cx, "widget.global_shipping_chokepoint.stale")
        } else {
            t!(
                cx,
                "widget.global_shipping_chokepoint.stale_seconds",
                seconds = self.vessels.silent_for().as_secs()
            )
        };

        div()
            .debug_selector(|| "TEST_CHOKEPOINT_STALE".into())
            .absolute()
            .top(px(8.0))
            .left(px(8.0))
            .px(px(6.0))
            .py(px(3.0))
            .rounded_sm()
            .border_1()
            .border_color(colors.border)
            .bg(colors.overlay)
            .text_xs()
            .text_color(colors.warning)
            .child(text)
            .into_any_element()
    }

    /// The map with the catalogue's "no data" over it, for a picture that holds no vessel.
    ///
    /// The map itself stays behind it: a strait with nothing in it is still the strait, and
    /// the empty state says which of "nothing there" and "nothing arrived" is being read.
    fn empty_state(&self, colors: MapColors, cx: &App) -> AnyElement {
        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(4.0))
            .child(EmptyState::new(
                "GlobalShippingChokepointEmpty",
                t!(cx, "common.no_data"),
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(colors.muted)
                    .child(t!(cx, "widget.global_shipping_chokepoint.waiting_data")),
            )
            .into_any_element()
    }

    /// The card of the vessel under the pointer, kept inside the pane and off its hull.
    fn hover_card(
        &self,
        details: &VesselDetails,
        hull: [f32; 2],
        projection: Projection,
        colors: MapColors,
        cx: &App,
    ) -> AnyElement {
        let size = UnifiedSize::Small;
        let unavailable = t!(cx, "widget.global_shipping_chokepoint.unavailable");
        let text = |value: Option<&SmolStr>| {
            value.map_or_else(
                || unavailable.clone(),
                |value| SharedString::new(value.as_str()),
            )
        };
        let values = [
            text(details.name.as_ref()),
            SharedString::new(details.mmsi.as_str()),
            text(details.imo.as_ref()),
            text(details.call_sign.as_ref()),
            details.ship_class.map_or_else(
                || unavailable.clone(),
                |class| t!(cx, SHIP_TYPE_KEYS[class_index(class)]),
            ),
            details
                .flag
                .map_or_else(|| unavailable.clone(), SharedString::new_static),
            text(details.destination.as_ref()),
            text(details.eta.as_ref()),
            text(details.draught.as_ref()),
            text(details.dimensions.as_ref()),
            text(details.speed.as_ref()),
            text(details.course.as_ref()),
            text(details.heading.as_ref()),
            details.nav_status.map_or_else(
                || unavailable.clone(),
                |status| t!(cx, NAV_STATUS_KEYS[usize::from(status.index())]),
            ),
            text(details.updated.as_ref()),
        ];
        let rows = HOVER_KEYS
            .iter()
            .zip(values)
            .map(|(key, value)| {
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_none()
                            .text_color(colors.muted)
                            .child(t!(cx, *key)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            // A destination or a call sign is as long as the report says:
                            // it is cut rather than allowed to widen the card off the map.
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(colors.text)
                            .child(value),
                    )
            })
            .collect::<Vec<_>>();

        // A card to the right of the hull would leave the pane, so it goes to its left
        // instead; the same for one that would leave through the bottom edge.
        let left = if hull[0] + CARD_MARGIN + CARD_WIDTH <= projection.viewport[0] {
            hull[0] + CARD_MARGIN
        } else {
            (hull[0] - CARD_MARGIN - CARD_WIDTH).max(0.0)
        };
        let top = (hull[1] - CARD_HEIGHT / 2.0).clamp(
            0.0,
            (projection.viewport[1] - CARD_HEIGHT - CARD_MARGIN).max(0.0),
        );

        div()
            .debug_selector(|| "TEST_CHOKEPOINT_CARD".into())
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(CARD_WIDTH))
            .flex()
            .flex_col()
            .gap(size.gap())
            .px(size.px())
            .py(size.px())
            .rounded_sm()
            .border_1()
            .border_color(colors.border)
            .bg(colors.overlay)
            .text_size(size.text_size())
            .children(rows)
            .into_any_element()
    }
}

impl Render for GlobalShippingChokepointWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = MapColors::resolve(cx.theme());
        let zone = data_time_zone(cx);

        // The card is resolved against the picture this frame draws: a vessel a delta
        // removed stops having one on the frame it leaves, rather than a frame later.
        if let Some(mmsi) = self.hovered
            && self.vessels.display(mmsi, zone).is_none()
        {
            self.hovered = None;
        }

        let prepaint_frame = Rc::clone(&self.frame);
        let paint_frame = Rc::clone(&self.frame);
        let painted = Arc::clone(&self.painted);
        let scene = self.scene;
        let hovered = self.hovered;
        // The canvas is the map: the sea is its own fill, and everything that moves is a
        // triangle list built for the camera this frame draws with.
        let map = canvas(
            move |bounds, window, _cx| {
                if prepaint_frame.bounds.replace(Some(bounds)) != Some(bounds) {
                    // The labels and the hit test are laid out from the box recorded
                    // here, so the frame that first sees a new box asks for one more.
                    // A `refresh` called in this pass is swallowed by the draw, so the
                    // request is made for the frame after it; that frame sees the same
                    // box and asks for nothing further.
                    window.on_next_frame(|window, _| window.refresh());
                }
                prepaint_frame
                    .camera
                    .set(prepaint_frame.camera.get().clamped(viewport_of(bounds)));
            },
            move |bounds, (), window, _cx| {
                paint_map(
                    bounds,
                    paint_frame.camera.get(),
                    scene,
                    &painted,
                    hovered,
                    colors,
                    window,
                );
            },
        )
        .size_full()
        .bg(colors.ocean);

        let projection = self
            .frame
            .bounds
            .get()
            .map(|bounds| Projection::new(self.frame.camera.get(), viewport_of(bounds)));
        let labels =
            projection.map_or_else(Vec::new, |projection| self.labels(projection, colors, cx));
        let legend = self.legend(colors, cx);
        let stale = self
            .vessels
            .is_stale()
            .then(|| self.stale_badge(colors, cx));
        let empty = (self.vessels.has_state() && self.vessels.vessel_count() == 0)
            .then(|| self.empty_state(colors, cx));
        let card = self.hovered.and_then(|mmsi| {
            let projection = projection?;
            let details = self.vessels.display(mmsi, zone)?;
            let hull = self
                .painted
                .iter()
                .find(|vessel| vessel.mmsi == mmsi)
                .map(|vessel| projection.screen(vessel.position))?;
            Some(self.hover_card(&details, hull, projection, colors, cx))
        });

        let wheel = cx.entity().downgrade();
        let pinch = cx.entity().downgrade();

        div()
            .id(ElementId::Name(SharedString::new_static(
                "GlobalShippingChokepoint",
            )))
            .debug_selector(|| "TEST_CHOKEPOINT_MAP".into())
            .relative()
            .size_full()
            .min_h_0()
            // The canvas is only `Styled`, so the pointer handlers belong to the box
            // around it, which is the map's own box.
            .on_scroll_wheel(move |event, _window, app| {
                let _ = wheel.update(app, |this, cx| this.zoom_by_wheel(event, cx));
                // The map consumes the wheel: a pane next to a scrolling parent would
                // otherwise zoom and scroll at once.
                app.stop_propagation();
            })
            .on_pinch(move |event, _window, app| {
                let _ = pinch.update(app, |this, cx| this.zoom_by_pinch(event, cx));
            })
            .on_mouse_move(cx.listener(Self::on_pointer_move))
            // A pointer that leaves the pane is on no vessel any more; without this the
            // card of the last vessel it was over would stay on a map nothing is pointing at.
            .on_hover(cx.listener(|this, hovering: &bool, _window, cx| {
                if !*hovering && this.hovered.take().is_some() {
                    cx.notify();
                }
            }))
            .child(map)
            .children(labels)
            .child(legend)
            .when_some(stale, |this, stale| this.child(stale))
            .when_some(card, |this, card| this.child(card))
            .when_some(empty, |this, empty| this.child(empty))
    }
}

/// The recovery an account without AIS access is offered.
///
/// The subscription is what is missing, so the control goes where one is bought.
fn view_plans() -> WidgetAction {
    WidgetAction::new("widget.global_shipping_chokepoint.view_plans", |cx| {
        cx.open_url(ACCOUNT_ORIGIN);
    })
}

/// The index one class occupies in [`CLASSES`], [`MapColors::ships`] and the legend.
pub(super) const fn class_index(class: ShipClass) -> usize {
    match class {
        ShipClass::Tanker => 0,
        ShipClass::Container => 1,
        ShipClass::Bulk => 2,
        ShipClass::Other => 3,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Mutex, PoisonError},
        time::Duration,
    };

    use app_config::settings::{AccountConfig, AccountWalletKind, Config};
    use app_config_provider::manager::ConfigManager;
    use base_ui::style::palette::Appearance;
    use gpui_kit::{
        App, AppContext, BorrowAppContext as _, Entity, IntoElement, Modifiers, ParentElement,
        Pixels, Render, Rgba, ScrollDelta, ScrollWheelEvent, Styled, TestAppContext,
        VisualTestContext, WeakEntity, Window, div, point, px, size,
    };
    use i18n::{language::codes::LanguageCode, t};
    use kytrix_service_provider::service::{
        engine::{ErrorCode, KytrixServiceError},
        global_shipping_chokepoint::{
            GlobalShippingChokepointArea, GlobalShippingChokepointPosition,
            GlobalShippingChokepointShip, GlobalShippingChokepointUpdate,
        },
    };
    use network::transport::error::TransportError;
    use smol_str::SmolStr;
    use widget::{
        config::WidgetConfigChange,
        status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    };

    use super::*;
    use crate::{
        model::HIT_RADIUS_PX,
        service::{
            AisStreamError, AisStreamMessage, GlobalShippingChokepointEventSubscription,
            GlobalShippingChokepointSession,
        },
    };

    /// The account every fixture pane is signed in as.
    const ACCOUNT: &str = "11111111-1111-4111-8111-111111111111";
    /// The account a pane is signed in as after a change.
    const OTHER_ACCOUNT: &str = "22222222-2222-4222-8222-222222222222";

    /// Settings holding `account`, or holding nobody when it is `None`.
    fn settings(account: Option<&str>) -> Config {
        let mut config = Config::default();
        config.account = account.map(|user_id| AccountConfig {
            user_id: user_id.to_owned(),
            wallet_address: "0x0000000000000000000000000000000000000001".to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        config
    }

    /// Installs everything a pane reads, with `account` signed in or nobody.
    fn install(cx: &mut App, account: Option<&str>) {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
        global::install(cx).expect("the test Tokio runtime should build");
        let manager = ConfigManager::in_memory(settings(account), cx);
        cx.set_global(manager);
    }

    /// A source that records the accounts it was asked for and never answers.
    ///
    /// The pane's messages are reported to it directly rather than through a subscription,
    /// so a test is about what the pane does with a push rather than about the service that
    /// delivers one. A stream that is opened is never answered, which is also what keeps a
    /// test from depending on its own timing.
    #[derive(Clone, Default)]
    struct RecordingSource {
        accounts: Arc<Mutex<Vec<SmolStr>>>,
    }

    impl RecordingSource {
        /// The accounts this source has been asked for a session of, in order.
        fn requested(&self) -> Vec<SmolStr> {
            self.accounts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl GlobalShippingChokepointSource for RecordingSource {
        type Session = PendingSession;

        async fn acquire_session(
            &self,
            account_id: &str,
        ) -> Result<Self::Session, KytrixServiceError> {
            self.accounts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(SmolStr::new(account_id));
            std::future::pending().await
        }
    }

    /// A session whose subscription never opens.
    #[derive(Clone)]
    struct PendingSession;

    impl GlobalShippingChokepointSession for PendingSession {
        type Subscription = PendingSubscription;

        async fn subscribe_global_shipping_chokepoint(
            &self,
            _area: GlobalShippingChokepointArea,
        ) -> Result<Self::Subscription, KytrixServiceError> {
            std::future::pending().await
        }
    }

    /// A subscription that never delivers anything.
    struct PendingSubscription;

    impl GlobalShippingChokepointEventSubscription for PendingSubscription {
        async fn recv(&mut self) -> Result<GlobalShippingChokepointUpdate, TransportError> {
            std::future::pending().await
        }
    }

    /// A pane the framework would have built: state, then one load.
    fn pane(
        cx: &mut TestAppContext,
        source: RecordingSource,
        account: Option<&str>,
    ) -> Entity<GlobalShippingChokepointWidget> {
        cx.executor().allow_parking();
        let pane = cx.update(|cx| {
            install(cx, account);
            cx.new(move |cx| {
                let status = cx.new(|_| WidgetStatus::new());
                let weak: WeakEntity<GlobalShippingChokepointWidget> = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |pane, cx| pane.load(cx));
                });
                GlobalShippingChokepointWidget::new_with_source(
                    GlobalShippingChokepointConfig,
                    source,
                    status,
                    WidgetConfigChange::default(),
                    recovery,
                    cx,
                )
            })
        });
        pane.update(cx, |pane, cx| pane.load(cx));
        cx.run_until_parked();
        pane
    }

    /// The vertices of one position in the strait, as a report carries them.
    fn position(mmsi: u32) -> GlobalShippingChokepointPosition {
        GlobalShippingChokepointPosition {
            mmsi,
            latitude: 26_500_000,
            longitude: 56_300_000,
            sog: 123,
            cog: 456,
            heading: 87,
            nav_status: 1,
            last_update: 1_700_000_000_000,
        }
    }

    /// One vessel's ship record, with every value a report can carry.
    fn ships(mmsi: u32) -> GlobalShippingChokepointShip {
        GlobalShippingChokepointShip {
            mmsi,
            imo: 9_074_729,
            call_sign: SmolStr::new_static("A8AB2"),
            name: SmolStr::new_static("SEA JEWEL"),
            destination: SmolStr::new_static("FUJAIRAH"),
            ship_type: 80,
            flag_mid: 636,
            draught: 92,
            dim_to_bow: 120,
            dim_to_stern: 30,
            dim_to_port: 15,
            dim_to_starboard: 17,
            eta_month: 3,
            eta_day: 14,
            eta_hour: 9,
            eta_minute: 5,
        }
    }

    /// A keyframe of `positions`.
    fn keyframe(
        epoch: u64,
        sequence: u64,
        positions: Vec<GlobalShippingChokepointPosition>,
    ) -> GlobalShippingChokepointUpdate {
        GlobalShippingChokepointUpdate {
            epoch,
            keyframe: true,
            sequence,
            base: 0,
            ships: Vec::new(),
            positions,
            removed: Vec::new(),
            timestamp: 1_700_000_000_000,
        }
    }

    /// A delta continuing `base` with nothing in it but the removals.
    fn removal(epoch: u64, base: u64, sequence: u64, mmsi: u32) -> GlobalShippingChokepointUpdate {
        GlobalShippingChokepointUpdate {
            epoch,
            keyframe: false,
            sequence,
            base,
            ships: Vec::new(),
            positions: Vec::new(),
            removed: vec![mmsi],
            timestamp: 1_700_000_000_000,
        }
    }

    /// Reports one push as the latest stream would.
    fn report_update(
        pane: &Entity<GlobalShippingChokepointWidget>,
        update: GlobalShippingChokepointUpdate,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, cx| {
            pane.apply_latest(
                GlobalShippingChokepointArea::Hormuz,
                Arc::new(AisStreamMessage::Update(update)),
                cx,
            )
        })
    }

    /// Reports one failure as the reliable stream would.
    fn report_failure(
        pane: &Entity<GlobalShippingChokepointWidget>,
        error: AisStreamError,
        cx: &mut App,
    ) -> bool {
        pane.update(cx, |pane, cx| {
            pane.apply_reliable(
                GlobalShippingChokepointArea::Hormuz,
                Arc::new(AisStreamMessage::Failed(Arc::new(error))),
                cx,
            )
        })
    }

    /// A window whose body is one pane, which is how the framework mounts it.
    struct MapHarness {
        pane: Entity<GlobalShippingChokepointWidget>,
    }

    impl Render for MapHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            // The framework's body sits inside a pane rather than in the window's corner,
            // so the margin keeps the canvas' box away from the window's origin — which is
            // where a coordinate mistake would otherwise be invisible.
            div()
                .size_full()
                .min_h_0()
                .p(px(16.0))
                .child(self.pane.clone())
        }
    }

    /// Mounts one pane in a window of its own, already drawn once.
    fn map_window<'a>(
        cx: &'a mut TestAppContext,
        pane: &Entity<GlobalShippingChokepointWidget>,
    ) -> &'a mut VisualTestContext {
        let pane = pane.clone();
        let (_, window) = cx.add_window_view(move |_, _| MapHarness { pane });
        window.simulate_resize(size(px(600.0), px(400.0)));
        window.run_until_parked();
        repaint(window);
        repaint(window);
        window
    }

    /// Draws one frame of the window, which is what commits the selectors and the hitboxes
    /// the pointer is tested against.
    fn repaint(window: &mut VisualTestContext) {
        window.update(|window, _| window.refresh());
        window.run_until_parked();
        window.update(|window, cx| window.draw(cx).clear(cx));
        window.run_until_parked();
    }

    /// The window position one canvas point is drawn at.
    fn window_point(
        pane: &GlobalShippingChokepointWidget,
        canvas: [f32; 2],
    ) -> Option<Point<Pixels>> {
        let bounds = pane.frame.bounds.get()?;
        Some(point(
            bounds.origin.x + px(canvas[0]),
            bounds.origin.y + px(canvas[1]),
        ))
    }

    /// The canvas position one vessel's marker is drawn at.
    fn hull_of(pane: &GlobalShippingChokepointWidget, mmsi: u32) -> Option<[f32; 2]> {
        let bounds = pane.frame.bounds.get()?;
        let projection = Projection::new(pane.frame.camera.get(), viewport_of(bounds));
        pane.painted
            .iter()
            .find(|vessel| vessel.mmsi == mmsi)
            .map(|vessel| projection.screen(vessel.position))
    }

    /// Lets the pane and the application runtime take a turn each until `ready`.
    ///
    /// A request crosses both executors — the pane sends it from GPUI's, the service
    /// answers on the application runtime — so waiting for one of them alone waits for
    /// nothing.
    fn settle_until(cx: &mut TestAppContext, what: &str, ready: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let handle = cx.update(|cx| global::handle(cx).handle().clone());
            handle.block_on(async { tokio::task::yield_now().await });
            if cx.update(|cx| ready(cx)) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what} should happen");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The WCAG contrast ratio of two colours, which is what a label has to clear to be
    /// readable over a fill.
    fn wcag_contrast(foreground: Hsla, background: Hsla) -> f32 {
        let background = Rgba::from(background);
        let foreground = Rgba::from(foreground);
        let foreground = if foreground.a < 1.0 {
            background.blend(foreground)
        } else {
            foreground
        };
        let channel = |value: f32| {
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        let luminance = |color: Rgba| {
            0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
        };
        let first = luminance(foreground);
        let second = luminance(background);
        (first.max(second) + 0.05) / (first.min(second) + 0.05)
    }

    #[gpui_kit::test]
    fn a_pane_nobody_signed_in_for_should_wait_for_a_sign_in(cx: &mut TestAppContext) {
        let source = RecordingSource::default();
        let pane = pane(cx, source.clone(), None);

        cx.update(|cx| {
            let pane = pane.read(cx);

            assert!(!pane.has_content());
            assert_eq!(
                pane.status.state(cx).notice(),
                Some(&Notice::message(
                    "widget.global_shipping_chokepoint.sign_in"
                )),
                "a pane without an account is waiting rather than failed",
            );
            assert!(!pane.status.state(cx).is_failed());
        });
        assert!(
            source.requested().is_empty(),
            "nothing is followed for a pane nobody signed in for"
        );
    }

    #[gpui_kit::test]
    fn a_signed_in_pane_should_connect_until_a_keyframe_arrives(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));

        cx.update(|cx| {
            let pane = pane.read(cx);

            assert_eq!(pane.status.state(cx), WidgetState::Connecting);
            assert!(!pane.has_content(), "no picture has arrived yet");
        });

        cx.update(|cx| {
            assert!(report_update(&pane, keyframe(7, 1, vec![position(1)]), cx));
            let pane = pane.read(cx);

            assert_eq!(pane.status.state(cx), WidgetState::Ready);
            assert!(
                pane.has_content(),
                "a picture is what the pane has content from"
            );
            assert_eq!(pane.vessels.vessel_count(), 1);
            assert_eq!(pane.painted.len(), 1, "the painted list is the picture's");
        });
    }

    /// A keyframe the server stamped with no time says it holds no state yet. It clears
    /// the picture but must not become one, or a restarted server would read as an empty
    /// strait.
    #[gpui_kit::test]
    fn a_keyframe_without_a_time_should_not_become_content(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));

        cx.update(|cx| {
            let mut empty = keyframe(7, 1, vec![position(1)]);
            empty.timestamp = 0;
            assert!(report_update(&pane, empty, cx));

            let pane = pane.read(cx);
            assert!(!pane.has_content());
            assert_eq!(
                pane.status.state(cx),
                WidgetState::Connecting,
                "the pane waits for a picture instead of drawing an empty sea",
            );
            assert_eq!(pane.vessels.vessel_count(), 0);
        });
    }

    /// An empty picture is still a picture: the pane draws the map and says it holds no
    /// vessel, rather than leaving the framework's spinner up for ever.
    #[gpui_kit::test]
    fn an_empty_keyframe_should_be_ready_with_the_empty_state(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));

        cx.update(|cx| {
            assert!(report_update(&pane, keyframe(7, 1, Vec::new()), cx));
            let pane = pane.read(cx);
            assert!(pane.has_content(), "an empty sea is a picture");
            assert_eq!(pane.status.state(cx), WidgetState::Ready);
            assert_eq!(pane.vessels.vessel_count(), 0);
        });

        let window = map_window(cx, &pane);
        assert!(
            window.debug_bounds("EMPTY_STATE").is_some(),
            "a picture with no vessel says it holds none",
        );
        assert!(
            window.debug_bounds("TEST_CHOKEPOINT_MAP").is_some(),
            "the map stays behind the empty state",
        );
        assert!(
            window.debug_bounds("TEST_CHOKEPOINT_LEGEND").is_some(),
            "the legend is part of the ready body",
        );
    }

    /// The count says one vessel in the singular and anything else in the plural, which is
    /// what the two catalogue keys are for.
    #[gpui_kit::test]
    fn the_legend_should_count_one_vessel_in_the_singular(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));

        cx.update(|cx| {
            assert!(report_update(&pane, keyframe(7, 1, vec![position(1)]), cx));
            assert_eq!(pane.read(cx).vessels.vessel_count(), 1);
            assert_eq!(
                pane.read(cx).vessel_count_text(cx).as_ref(),
                t!(cx, "widget.global_shipping_chokepoint.vessel_count_one").as_ref(),
                "one vessel is singular",
            );

            assert!(report_update(
                &pane,
                keyframe(7, 2, vec![position(1), position(2)]),
                cx,
            ));
            assert_eq!(pane.read(cx).vessels.vessel_count(), 2);
            assert_eq!(
                pane.read(cx).vessel_count_text(cx).as_ref(),
                t!(
                    cx,
                    "widget.global_shipping_chokepoint.vessel_count",
                    count = 2
                )
                .as_ref(),
                "two vessels are plural and interpolated",
            );

            assert!(report_update(&pane, keyframe(7, 3, Vec::new()), cx));
            assert_eq!(
                pane.read(cx).vessel_count_text(cx).as_ref(),
                t!(
                    cx,
                    "widget.global_shipping_chokepoint.vessel_count",
                    count = 0
                )
                .as_ref(),
                "an empty picture is plural",
            );
        });
    }

    #[gpui_kit::test]
    fn a_gap_should_draw_the_stale_badge_while_the_picture_stays(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));

        cx.update(|cx| {
            let mut frame = keyframe(7, 10, vec![position(1)]);
            frame.ships = vec![ships(1)];
            assert!(report_update(&pane, frame, cx));
            // A delta that does not continue the applied sequence is a gap rather than a
            // push to apply.
            assert!(report_update(&pane, removal(7, 5, 6, 1), cx));

            let pane = pane.read(cx);
            assert!(pane.vessels.is_stale());
            assert!(pane.vessels.has_gap());
            assert_eq!(
                pane.vessels.vessel_count(),
                1,
                "the picture stays on the map under the badge",
            );
        });

        let window = map_window(cx, &pane);
        assert!(
            window.debug_bounds("TEST_CHOKEPOINT_STALE").is_some(),
            "a picture that is not being continued is drawn under its badge",
        );
        assert!(window.debug_bounds("TEST_CHOKEPOINT_LEGEND").is_some());
    }

    #[gpui_kit::test]
    fn a_failed_stream_should_offer_the_framework_retry(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));

        cx.update(|cx| {
            assert!(report_failure(
                &pane,
                AisStreamError::Transport(TransportError::Closed),
                cx,
            ));
            let state = pane.read(cx).status.state(cx);
            let failure = state.failure().expect("the stream failed");

            assert_eq!(
                failure.text(cx).as_ref(),
                t!(cx, "error.transport.closed").as_ref(),
            );
            assert_eq!(failure.action().label_key(), "common.retry");
        });
    }

    /// An account without AIS access is missing the subscription rather than the
    /// connection, so the control it is offered is where one is bought.
    #[gpui_kit::test]
    fn a_missing_entitlement_should_offer_the_plans(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));

        let action = cx.update(|cx| {
            assert!(report_failure(
                &pane,
                AisStreamError::Service(KytrixServiceError::Server(ErrorCode::EntitlementRequired)),
                cx,
            ));
            let state = pane.read(cx).status.state(cx);
            let failure = state.failure().expect("the refusal is a failure");
            let text = failure.text(cx);

            assert_eq!(
                text.as_ref(),
                t!(cx, "widget.global_shipping_chokepoint.entitlement_required").as_ref(),
            );
            assert!(
                !text.contains('<'),
                "a failure's detail is drawn as plain text, so its copy is not markdown: {text}",
            );
            assert_eq!(
                failure.action().label(cx).as_ref(),
                t!(cx, "widget.global_shipping_chokepoint.view_plans").as_ref(),
            );
            failure.action().clone()
        });

        cx.update(|cx| action.run(cx));
        assert_eq!(cx.opened_url().as_deref(), Some(ACCOUNT_ORIGIN));
    }

    /// A failure is terminal: a push that arrives after it is not applied and does not
    /// revive the pane.
    #[gpui_kit::test]
    fn a_late_update_after_a_failure_should_not_revive_the_pane(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));

        cx.update(|cx| {
            let mut frame = keyframe(7, 1, vec![position(1)]);
            frame.ships = vec![ships(1)];
            assert!(report_update(&pane, frame, cx));
            assert!(report_failure(
                &pane,
                AisStreamError::Transport(TransportError::Closed),
                cx,
            ));

            assert!(!report_update(&pane, keyframe(7, 2, vec![position(2)]), cx));

            let pane = pane.read(cx);
            assert!(pane.status.is_failed(cx), "the failure is still the state");
            assert_eq!(
                pane.vessels.vessel_count(),
                1,
                "the late picture is refused rather than folded in",
            );
        });
    }

    #[gpui_kit::test]
    fn a_message_for_another_area_should_be_refused(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));

        cx.update(|cx| {
            let update = Arc::new(AisStreamMessage::Update(keyframe(7, 1, vec![position(1)])));
            assert!(!pane.update(cx, |pane, cx| {
                pane.apply_latest(GlobalShippingChokepointArea::Unspecified, update, cx)
            }));

            let failure = Arc::new(AisStreamMessage::Failed(Arc::new(
                AisStreamError::Transport(TransportError::Closed),
            )));
            assert!(!pane.update(cx, |pane, cx| {
                pane.apply_reliable(GlobalShippingChokepointArea::Unspecified, failure, cx)
            }));

            let pane = pane.read(cx);
            assert!(!pane.has_content());
            assert!(!pane.status.is_failed(cx));
        });
    }

    /// The pane's floor is the packaged coastline: a pane that cannot parse it reports the
    /// failure with the framework's recovery rather than waiting for data it could not draw.
    #[gpui_kit::test]
    fn a_map_asset_failure_should_be_reported_with_the_framework_retry(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));

        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.fail_map_asset(SceneError::NonFiniteCoordinate, cx);
            });
            let state = pane.read(cx).status.state(cx);
            let failure = state.failure().expect("the coastline failure is a failure");

            assert_eq!(
                failure.text(cx).as_ref(),
                t!(cx, "error.global_shipping_chokepoint.map_asset").as_ref(),
                "the sentence is the error type's own",
            );
            assert_eq!(failure.action().label_key(), "common.retry");
            assert!(!pane.read(cx).has_content());
        });
        assert!(
            MapAssetInvalid::from(SceneError::Tessellation)
                .to_string()
                .contains("tessellated"),
            "the display keeps the reason for a log line",
        );
        // The wrapper carries the whole parser error, so every reason a scene can be
        // refused keeps its own sentence rather than collapsing into one.
        for error in [
            SceneError::InvalidMagic,
            SceneError::UnsupportedVersion,
            SceneError::Truncated,
            SceneError::BboxMismatch,
            SceneError::GridMismatch,
            SceneError::MissingSection,
            SceneError::PointCountOverflow,
            SceneError::BadCellRange,
            SceneError::PieceNotInAnyCell,
            SceneError::BadPiece,
            SceneError::TrailingBytes,
            SceneError::Empty,
            SceneError::NonFiniteCoordinate,
            SceneError::Tessellation,
        ] {
            let display = MapAssetInvalid::from(error.clone()).to_string();
            assert!(
                display.contains(&error.to_string()),
                "{error:?} should survive into the log line: {display}",
            );
        }
    }

    /// Signing out takes the picture with it, and it does not wait for the stop request's
    /// answer to do it.
    #[gpui_kit::test]
    fn signing_out_should_clear_the_picture_and_wait_for_a_sign_in(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));

        cx.update(|cx| {
            let mut frame = keyframe(7, 1, vec![position(1)]);
            frame.ships = vec![ships(1)];
            assert!(report_update(&pane, frame, cx));
            assert!(pane.read(cx).has_content());

            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(|config| config.account = None, cx);
            });
        });

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(!pane.has_content(), "the old account's picture is gone");
            assert_eq!(pane.vessels.vessel_count(), 0);
            assert!(pane.painted.is_empty());
            assert_eq!(pane.hovered, None);
            assert_eq!(
                pane.status.state(cx).notice(),
                Some(&Notice::message(
                    "widget.global_shipping_chokepoint.sign_in"
                )),
            );
        });
    }

    /// A failure of the session a sign-out is abandoning is not this pane's: it is waiting
    /// for a sign-in, and a failure would offer a retry for a stream nobody holds.
    #[gpui_kit::test]
    fn a_failure_after_signing_out_should_not_replace_the_sign_in_notice(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));

        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(|config| config.account = None, cx);
            });
        });

        cx.update(|cx| {
            assert!(
                !report_failure(&pane, AisStreamError::Transport(TransportError::Closed), cx),
                "the abandoned session's failure is refused",
            );
            let state = pane.read(cx).status.state(cx);
            assert!(!state.is_failed());
            assert_eq!(
                state.notice(),
                Some(&Notice::message(
                    "widget.global_shipping_chokepoint.sign_in"
                )),
            );
        });
    }

    /// Another account is another picture: the pane asks for its stream again.
    #[gpui_kit::test]
    fn an_account_change_should_load_the_new_account(cx: &mut TestAppContext) {
        let source = RecordingSource::default();
        let pane = pane(cx, source.clone(), Some(ACCOUNT));

        settle_until(cx, "the first account to be taken", |_| {
            !source.requested().is_empty()
        });
        assert_eq!(source.requested(), [SmolStr::new_static(ACCOUNT)]);

        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.account = Some(AccountConfig {
                            user_id: OTHER_ACCOUNT.to_owned(),
                            wallet_address: "0x0000000000000000000000000000000000000002".to_owned(),
                            wallet_kind: AccountWalletKind::Ethereum,
                        });
                    },
                    cx,
                );
            });
        });

        settle_until(cx, "the new account to be taken", |_| {
            source.requested().len() == 2
        });
        assert_eq!(
            source.requested()[1],
            SmolStr::new_static(OTHER_ACCOUNT),
            "the pane follows the account the settings now hold",
        );
        cx.update(|cx| {
            assert_eq!(pane.read(cx).account_id.as_deref(), Some(OTHER_ACCOUNT));
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Connecting);
        });
    }

    /// The wheel scales the camera around the pointer, and neither end of the range can be
    /// left.
    #[gpui_kit::test]
    fn the_camera_should_zoom_around_the_pointer_within_its_limits(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        let window = map_window(cx, &pane);

        window.update(|_, cx| {
            pane.update(cx, |pane, cx| {
                let anchor = point(px(120.0), px(90.0));
                let before = pane.frame.camera.get();
                pane.zoom_at(anchor, 2.0, cx);
                let after = pane.frame.camera.get();

                assert_close(after.zoom, (before.zoom * 2.0).min(MAX_ZOOM), "the zoom");
                assert_ne!(after.center, before.center, "the anchor is kept in place");

                pane.zoom_at(anchor, 100.0, cx);
                assert_close(pane.frame.camera.get().zoom, MAX_ZOOM, "the near end");

                pane.zoom_at(anchor, 0.001, cx);
                assert_close(pane.frame.camera.get().zoom, MIN_ZOOM, "the far end");
            });
        });
    }

    /// A wheel pushed away from the reader zooms in, and one pulled back zooms out: the sign
    /// is the one the window hands the handler.
    #[gpui_kit::test]
    fn a_wheel_should_zoom_in_when_it_is_pushed_away(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        let window = map_window(cx, &pane);

        let anchor = window.update(|_, cx| {
            pane.read(cx)
                .frame
                .bounds
                .get()
                .expect("the pane has been laid out")
                .center()
        });
        let wheel = |delta: f32| ScrollWheelEvent {
            position: anchor,
            delta: ScrollDelta::Lines(point(0.0, delta)),
            ..Default::default()
        };

        window.update(|_, cx| {
            pane.update(cx, |pane, cx| {
                let before = pane.frame.camera.get().zoom;
                pane.zoom_by_wheel(&wheel(1.0), cx);
                let zoomed_in = pane.frame.camera.get().zoom;
                assert!(
                    zoomed_in > before,
                    "a wheel pushed away zooms in: {before} -> {zoomed_in}",
                );

                pane.zoom_by_wheel(&wheel(-1.0), cx);
                let zoomed_out = pane.frame.camera.get().zoom;
                assert!(
                    zoomed_out < zoomed_in,
                    "a wheel pulled back zooms out: {zoomed_in} -> {zoomed_out}",
                );
            });
        });
    }

    /// A canvas that has just been laid out in a new box asks for exactly one more frame:
    /// the labels and the hit test read the box prepaint records, so the frame that records
    /// it was built from the previous one.
    #[gpui_kit::test]
    fn a_new_canvas_box_should_ask_for_one_more_frame(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        cx.update(|cx| {
            assert!(report_update(&pane, keyframe(7, 1, vec![position(1)]), cx));
        });
        let window = map_window(cx, &pane);

        // The mounting frames can leave a callback pending; settle them before measuring.
        // The bound keeps a frame-request loop a failing assertion instead of a hang.
        for _ in 0..4 {
            if window.update(|window, cx| window.simulate_next_frame(cx)) == 0 {
                break;
            }
            window.update(|window, cx| window.draw(cx).clear(cx));
            window.run_until_parked();
        }
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            0,
            "the mounting frames settle on their own",
        );

        window.simulate_resize(size(px(640.0), px(470.0)));
        window.update(|window, cx| window.draw(cx).clear(cx));
        window.run_until_parked();
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            1,
            "the frame that first sees a new box asks for exactly one more",
        );

        window.update(|window, cx| window.draw(cx).clear(cx));
        window.run_until_parked();
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            0,
            "the extra frame sees the same box and asks for nothing further",
        );
    }

    /// The pointer over a vessel is its card; a push that removes the vessel takes the
    /// card with it.
    #[gpui_kit::test]
    fn the_pointer_over_a_vessel_should_show_its_card_and_a_removal_should_take_it_away(
        cx: &mut TestAppContext,
    ) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        cx.update(|cx| {
            let mut frame = keyframe(7, 1, vec![position(1)]);
            frame.ships = vec![ships(1)];
            assert!(report_update(&pane, frame, cx));
        });
        let window = map_window(cx, &pane);

        let at = window.update(|_, cx| {
            let pane = pane.read(cx);
            let hull = hull_of(pane, 1).expect("the vessel is painted");
            window_point(pane, hull).expect("a drawn map records its box")
        });
        window.simulate_mouse_move(at, None, Modifiers::none());
        repaint(window);

        assert_eq!(
            window.update(|_, cx| pane.read(cx).hovered),
            Some(1),
            "the pointer moved onto the vessel",
        );
        assert!(
            window.debug_bounds("TEST_CHOKEPOINT_CARD").is_some(),
            "the vessel under the pointer has a card",
        );

        window.update(|_, cx| {
            assert!(report_update(&pane, removal(7, 1, 2, 1), cx));
        });
        repaint(window);

        assert_eq!(
            window.update(|_, cx| pane.read(cx).hovered),
            None,
            "a vessel the picture dropped has no card",
        );
        assert!(window.debug_bounds("TEST_CHOKEPOINT_CARD").is_none());
    }

    /// A pointer that leaves the map is on no vessel any more: the card it was showing is
    /// taken with it rather than left hanging over a map nothing is pointing at.
    #[gpui_kit::test]
    fn a_pointer_that_leaves_the_pane_should_take_its_card(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        cx.update(|cx| {
            let mut frame = keyframe(7, 1, vec![position(1)]);
            frame.ships = vec![ships(1)];
            assert!(report_update(&pane, frame, cx));
        });
        let window = map_window(cx, &pane);

        let at = window.update(|_, cx| {
            let pane = pane.read(cx);
            let hull = hull_of(pane, 1).expect("the vessel is painted");
            window_point(pane, hull).expect("a drawn map records its box")
        });
        window.simulate_mouse_move(at, None, Modifiers::none());
        repaint(window);
        assert_eq!(
            window.update(|_, cx| pane.read(cx).hovered),
            Some(1),
            "the pointer moved onto the vessel",
        );

        // The window keeps a margin around the pane, so this is outside the map but inside
        // the window the harness draws.
        let outside = window.update(|_, cx| {
            let bounds = pane
                .read(cx)
                .frame
                .bounds
                .get()
                .expect("the pane has been laid out");
            point(bounds.origin.x - px(4.0), bounds.origin.y - px(4.0))
        });
        window.simulate_mouse_move(outside, None, Modifiers::none());
        repaint(window);

        assert_eq!(
            window.update(|_, cx| pane.read(cx).hovered),
            None,
            "a pointer outside the map is on no vessel",
        );
        assert!(window.debug_bounds("TEST_CHOKEPOINT_CARD").is_none());
    }

    /// A card over a vessel near the bottom of the pane stays inside it: the card is placed
    /// from its estimated height rather than measured, so that estimate has to bound the
    /// real one.
    #[gpui_kit::test]
    fn a_card_near_the_bottom_edge_should_stay_inside_the_pane(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        cx.update(|cx| {
            // South of the strait, which is the bottom of the map at the fitted zoom.
            let mut low = position(1);
            low.latitude = 22_500_000;
            let mut frame = keyframe(7, 1, vec![low]);
            frame.ships = vec![ships(1)];
            assert!(report_update(&pane, frame, cx));
        });
        let window = map_window(cx, &pane);

        let at = window.update(|_, cx| {
            let pane = pane.read(cx);
            let hull = hull_of(pane, 1).expect("the vessel is painted");
            window_point(pane, hull).expect("a drawn map records its box")
        });
        window.simulate_mouse_move(at, None, Modifiers::none());
        repaint(window);

        let card = window
            .debug_bounds("TEST_CHOKEPOINT_CARD")
            .expect("the vessel under the pointer has a card");
        let map = window
            .debug_bounds("TEST_CHOKEPOINT_MAP")
            .expect("the map is drawn");
        assert!(
            f32::from(card.size.height) <= CARD_HEIGHT,
            "the height the card is placed by bounds its real one: {} > {CARD_HEIGHT}",
            f32::from(card.size.height),
        );
        assert!(
            card.top() >= map.top()
                && card.bottom() <= map.bottom()
                && card.left() >= map.left()
                && card.right() <= map.right(),
            "the card is kept inside the pane: {card:?} in {map:?}",
        );
    }

    /// A key this pane hands over is one both locales define and render: a missing key
    /// translates to an empty string rather than failing, so a typo would be a blank line
    /// on screen.
    #[gpui_kit::test]
    fn every_message_this_pane_shows_should_resolve_to_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx, Some(ACCOUNT));

            let mut keys = vec![
                "common.no_data",
                "error.global_shipping_chokepoint.map_asset",
                "widget.names.global_shipping_chokepoint",
                "widget.global_shipping_chokepoint.entitlement_required",
                "widget.global_shipping_chokepoint.sign_in",
                "widget.global_shipping_chokepoint.stale",
                "widget.global_shipping_chokepoint.stale_seconds",
                "widget.global_shipping_chokepoint.unavailable",
                "widget.global_shipping_chokepoint.vessel_count",
                "widget.global_shipping_chokepoint.vessel_count_one",
                "widget.global_shipping_chokepoint.view_plans",
                "widget.global_shipping_chokepoint.waiting_data",
            ];
            keys.extend(HOVER_KEYS);
            keys.extend(SHIP_TYPE_KEYS);
            keys.extend(LEGEND_KEYS);
            keys.extend(NAV_STATUS_KEYS);
            keys.extend(COUNTRY_LABELS.iter().map(|label| label.key));
            keys.extend(CITY_LABELS.iter().map(|label| label.key));
            keys.extend(WATER_LABELS.iter().map(|label| label.key));

            for language in [LanguageCode::En, LanguageCode::ZhCn] {
                cx.set_global(language);
                for key in &keys {
                    assert!(
                        !t!(cx, *key).is_empty(),
                        "{key} has no copy in {language:?}",
                    );
                }
                assert!(
                    !t!(
                        cx,
                        "widget.global_shipping_chokepoint.vessel_count",
                        count = 3
                    )
                    .contains("%{"),
                    "the count is supplied in {language:?}",
                );
                assert!(
                    !t!(
                        cx,
                        "widget.global_shipping_chokepoint.stale_seconds",
                        seconds = 42
                    )
                    .contains("%{"),
                    "the silence is supplied in {language:?}",
                );
            }
        });
    }

    /// The country names are the one label drawn straight on the fill, so they have to
    /// clear the contrast a paragraph is held to in both appearances — `fg.subtle` does not
    /// (about 2:1 on the land), which is why the body uses the default foreground.
    #[test]
    fn the_country_labels_should_read_on_both_map_fills() {
        for appearance in [Appearance::Light, Appearance::Dark] {
            let colors = MapColors::resolve(&Theme::of(appearance));
            for (what, background) in [("the land", colors.land), ("the sea", colors.ocean)] {
                let ratio = wcag_contrast(colors.text, background);
                assert!(
                    ratio >= 4.5,
                    "{appearance:?}: the country labels read {ratio:.2}:1 on {what}",
                );
            }
        }
    }

    /// The borders and roads are drawn over both map fills, so each is opaque and each
    /// reads where it lands: a border and a motorway on the land and on the sea, and a
    /// primary road on the land it is mostly on.
    #[test]
    fn the_map_border_and_roads_read_on_both_fills() {
        for appearance in [Appearance::Light, Appearance::Dark] {
            let colors = MapColors::resolve(&Theme::of(appearance));
            for (what, color) in [
                ("the map border", colors.map_border),
                ("a motorway", colors.road),
                ("a primary road", colors.road_minor),
            ] {
                assert_eq!(color.a, 1.0, "{appearance:?}: {what} is translucent");
            }

            for (what, background) in [("the land", colors.land), ("the sea", colors.ocean)] {
                let ratio = wcag_contrast(colors.map_border, background);
                assert!(
                    ratio >= 3.0,
                    "{appearance:?}: the map border reads {ratio:.2}:1 on {what}",
                );
            }
            let on_land = wcag_contrast(colors.road, colors.land);
            assert!(
                on_land >= 3.0,
                "{appearance:?}: a motorway reads {on_land:.2}:1 on the land",
            );
            let on_sea = wcag_contrast(colors.road, colors.ocean);
            assert!(
                on_sea >= 2.0,
                "{appearance:?}: a motorway reads {on_sea:.2}:1 on the sea",
            );
            let minor = wcag_contrast(colors.road_minor, colors.land);
            assert!(
                minor >= 1.5,
                "{appearance:?}: a primary road reads {minor:.2}:1 on the land",
            );
        }
    }

    #[test]
    fn the_config_should_be_at_the_first_schema_version() {
        assert_eq!(
            GlobalShippingChokepointConfig::SCHEMA_VERSION,
            WidgetConfigSchemaVersion::V1,
        );
    }

    /// Asserts two floats are the same within the tolerance a projection needs.
    fn assert_close(left: f32, right: f32, what: &str) {
        assert!(
            (left - right).abs() < 1e-4,
            "{what}: {left} should be {right}",
        );
    }

    /// The painted list and the hit test are the same picture, which is what makes the
    /// card follow the marker a reader pointed at rather than one next to it.
    #[test]
    fn the_painted_marker_should_be_the_one_a_pointer_hits() {
        let mut hit_points = Vec::new();
        let projection = Projection::new(
            Camera {
                center: model::project(56.0, 26.0),
                zoom: 2.0,
            },
            [600.0, 400.0],
        );
        let vessel = Vessel {
            mmsi: 7,
            class: ShipClass::Tanker,
            position: model::project(56.3, 26.5),
            cog: 900,
            heading: 87,
            has_ship: true,
        };
        let at = projection.screen(vessel.position);
        hit_points.push((vessel.mmsi, at));

        assert_eq!(
            model::hit_test(&hit_points, [at[0] + HIT_RADIUS_PX / 2.0, at[1]]),
            Some(7),
        );
        assert_eq!(
            model::hit_test(&hit_points, [at[0] + HIT_RADIUS_PX * 2.0, at[1]]),
            None,
        );
        assert_close(vessel.facing(), 90.0, "the marker faces its course");
        assert!(projection.on_screen(at));
        assert!(projection.near_screen([-4.0, 200.0], 8.0));
        assert!(projection.segment_near([-40.0, 10.0], [10.0, 10.0], 0.0));
        assert!(!projection.segment_near([-40.0, 10.0], [-10.0, 10.0], 0.0));
    }

    /// A Mercator `y` grows north while a canvas `y` grows down, so the projection has to
    /// put the northern shore of the strait above the southern one — which is the one thing
    /// a mirrored map would still answer the hit test about.
    #[test]
    fn the_projection_should_put_north_above_the_centre() {
        let projection = Projection::new(
            Camera {
                center: model::project(56.0, 26.0),
                zoom: 2.0,
            },
            [600.0, 400.0],
        );

        let north = projection.screen(model::project(56.0, 27.0));
        let south = projection.screen(model::project(56.0, 25.0));
        let east = projection.screen(model::project(57.0, 26.0));
        let west = projection.screen(model::project(55.0, 26.0));

        assert!(north[1] < 200.0, "the centre is the middle of the canvas");
        assert!(north[1] < south[1], "the northern shore is drawn above");
        assert!(east[0] > 300.0 && west[0] < 300.0, "east is drawn right");

        let mercator = model::project(56.3, 26.5);
        let back = projection.mercator(projection.screen(mercator));
        assert_close(back[0], mercator[0], "the x a canvas point stands for");
        assert_close(back[1], mercator[1], "the y a canvas point stands for");
    }
}

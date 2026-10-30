//! The Strait of Hormuz pane: its coastline and the vessels an AIS subscription reports in
//! it.
//!
//! # What the frame draws
//!
//! One canvas, under elements that carry the words. The base map's coastline, borders and
//! roads are layers parsed once by [`crate::scene`]; [`crate::paint`] turns them and the
//! vessel markers into the paths the canvas paints for the frame's camera, and the labels,
//! the legend and the hover card are absolutely positioned over it.
//! Nothing tessellates per frame: a frame only applies the camera's affine transform to
//! geometry that was prepared once.
//!
//! # What the pane reports, and what it holds
//!
//! The picture is one AIS subscription's. A keyframe replaces it, a delta only applies on
//! top of the picture already held, and a push that does not continue it is a gap the pane
//! reports as a failure rather than a state it guesses at. The frame is therefore a
//! picture held between pushes, and the two things the pane owns beyond it are the camera
//! and the vessel the pointer is on.
//!
//! # The body is the ready state
//!
//! The framework draws the placeholder, the banner and the spinner, and it only embeds
//! this body when [`GlobalShippingChokepointWidget::has_content`] is true. So there is no state
//! branch here: the body is what a pane with a picture draws, and a picture holding no
//! vessel is drawn as the map it is — the legend's count is what says the strait is empty.

use std::{cell::Cell, rc::Rc, sync::Arc};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use app_config_provider::{events::AccountChanged, manager::ConfigManager};
use base_ui::{
    interactive::InteractiveVariantExt,
    menu::{MenuItem, MenuItemAction},
    select::Select,
    size::{UnifiedSizable, UnifiedSize},
    style::theme::{Theme, ThemeExt},
};
use gpui_kit::{
    AnyElement, App, Bounds, Context, ElementId, Entity, FontStyle, FontWeight, Hsla,
    InteractiveElement, IntoElement, MouseButton, MouseMoveEvent, ParentElement, PinchEvent,
    Pixels, Point, Render, ScrollDelta, ScrollWheelEvent, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Window, canvas, div, point, prelude::FluentBuilder, px,
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
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use crate::{
    model::{
        self, Camera, LabelKind, LabelSize, MAX_ZOOM, MIN_ZOOM, Projection, STALE_AFTER, Vessel,
        VesselDetails, VesselStore, place_labels,
    },
    paint::paint_map,
    scene::{self, Scene, SceneError},
    service::{
        AisStreamError, AisStreamMessage, GlobalShippingChokepointProtocol,
        GlobalShippingChokepointRequest, GlobalShippingChokepointScope,
        GlobalShippingChokepointService, GlobalShippingChokepointSource,
    },
    ship::{SHIP_TYPE_COUNT, ShipType, ShipTypeFilter, colours},
};

/// How much one wheel notch scales the camera.
const WHEEL_ZOOM_STEP: f32 = 1.15;

/// The width and height of the box each label is centred in, in pixels.
///
/// Wide enough for the longest name in either locale at the country's 21 px step, and one
/// line tall at that step, so the text sits at the centre of the box and the box at the
/// anchor.
const LABEL_BOX_WIDTH: f32 = 320.0;
const LABEL_BOX_HEIGHT: f32 = 30.0;

/// The size a country's name is drawn at, in pixels.
///
/// Half again the `text_sm` step the cities used to share with it: at the fitted zoom a
/// country is a region rather than a place, and the extra size is what says so.
const COUNTRY_LABEL_TEXT_SIZE: f32 = 21.0;

/// The size a city's name is drawn at, in pixels.
///
/// A step over the water's `text_xs` and well under the country's, so the three kinds of
/// word read in the order a paper atlas prints them in.
const CITY_LABEL_TEXT_SIZE: f32 = 13.0;

/// Where a country's or a city's halo is drawn, in pixels off the name's own box.
///
/// GPUI has no text stroke: `TextStyle` carries no stroke and no shadow, and
/// `stroke_width` exists only on paths. The outline a chart gives a name that sits on the
/// picture is therefore the name itself, drawn in the map's paper colour one pixel off in
/// each direction and painted under it. Eight offsets rather than four: a cross leaves the
/// diagonals of a thin glyph uncovered at the city's 13 px step, and the cost is a text
/// element that shapes from the same cache the name already filled.
const LABEL_HALO_OFFSETS: [[f32; 2]; 8] = [
    [-1.0, -1.0],
    [0.0, -1.0],
    [1.0, -1.0],
    [-1.0, 0.0],
    [1.0, 0.0],
    [-1.0, 1.0],
    [0.0, 1.0],
    [1.0, 1.0],
];

/// The box one label's text occupies, shaped in the style the frame draws it in.
///
/// The width is the shaped line's own, so a Chinese name is measured as narrow as it is
/// drawn and an English one as wide — which is the whole reason the placement is handed a
/// measured size rather than a count of characters. The height is one em: that is the
/// extent the glyphs themselves take, while the line box the text style resolves to is
/// 1.618 of it for the paragraph rhythm, and using that would keep two names apart that a
/// reader would see as two clear lines.
fn label_size(window: &Window, kind: LabelKind, text: &str) -> LabelSize {
    let font_size = match kind {
        LabelKind::Country => px(COUNTRY_LABEL_TEXT_SIZE),
        LabelKind::City => px(CITY_LABEL_TEXT_SIZE),
        // The water's `.text_xs()`: three quarters of the window's rem.
        LabelKind::Water => window.rem_size() * 0.75,
    };
    let mut style = window.text_style();
    style.font_size = font_size.into();
    style.font_weight = match kind {
        LabelKind::Country => FontWeight::BOLD,
        LabelKind::Water | LabelKind::City => FontWeight::NORMAL,
    };
    style.font_style = match kind {
        LabelKind::Country | LabelKind::Water => FontStyle::Italic,
        LabelKind::City => FontStyle::Normal,
    };
    let line = window.text_system().shape_line(
        SharedString::new(text),
        font_size,
        &[style.to_run(text.len())],
        None,
    );
    LabelSize {
        width: f32::from(line.width()),
        height: f32::from(font_size),
    }
}

/// The text one label reads in the language being read.
///
/// A country is upper case — an atlas prints `IRAN`, not `Iran` — and that is part of the
/// label, so the measurement above has to be given the same string the frame draws.
fn label_text(cx: &App, key: &str, kind: LabelKind) -> SharedString {
    match kind {
        LabelKind::Country => SharedString::from(t!(cx, key).to_uppercase()),
        LabelKind::Water | LabelKind::City => t!(cx, key),
    }
}

/// How wide the hover card is.
///
/// The widest label the card draws sets it: the two update times are labelled with a
/// sentence rather than the one or two words the other rows use, and each draws its stamp
/// on a line of its own, so the label is what has to fit.
const CARD_WIDTH: f32 = 228.0;

/// How far the hover card is kept off the hull it describes, and off the pane's edges.
const CARD_MARGIN: f32 = 12.0;

/// The height the hover card is kept inside the pane by, before it has been measured.
///
/// Eighteen lines of text at the small size — sixteen rows, two of which stack their value
/// under the label — the fifteen gaps between the rows, and the card's padding and border.
/// A line is a 0.75 rem line box at the default 1.618 line height, so about 20 px; the
/// 10 px of slack keeps the estimate an upper bound, which is the safe direction: an
/// underestimate would let the last rows fall outside the pane, while an overestimate only
/// lifts the card a little.
const CARD_HEIGHT: f32 = 18.0 * 20.0 + 15.0 * 4.0 + 2.0 * 4.0 + 10.0;

/// The page the map's ODbL credit opens.
///
/// OSMF's guidance asks for "OpenStreetMap" to be reachable from the credit itself, so the
/// label is a link to the copyright page rather than plain text.
const ATTRIBUTION_URL: &str = "https://www.openstreetmap.org/copyright";

/// The sixteen labels of the hover card, in the order the design lists the items in.
const HOVER_KEYS: [&str; 16] = [
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
    "widget.global_shipping_chokepoint.hover.static_updated_at",
];

/// The two rows whose value is drawn under its label rather than beside it.
///
/// Each carries a `YYYY-MM-DD HH:MM:SS` stamp behind a label that is a sentence of its
/// own, and the two together are wider than the card: on one line it is the stamp that is
/// cut, so it gets a line of its own.
const HOVER_STACKED_KEYS: [&str; 2] = [
    "widget.global_shipping_chokepoint.hover.updated",
    "widget.global_shipping_chokepoint.hover.static_updated_at",
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

/// Why the picture on the map is no longer being continued.
///
/// Both cases are the picture stopping being live rather than the map being wrong, so the
/// vessels stay on screen under the failure. Neither can be repaired from what the pane
/// holds — a missing message is not something a later push can put back — which is why
/// they are failures with a new subscription behind the control rather than a badge the
/// pane clears by itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
enum StalePicture {
    /// A push did not continue the picture, so a message is missing from it.
    #[error("the picture is missing a push")]
    Gap,
    /// The link delivered nothing for the staleness window.
    #[error("no push arrived within the staleness window")]
    Silent,
}

impl LocalizedError for StalePicture {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Gap => t!(cx, "error.global_shipping_chokepoint.stale"),
            Self::Silent => t!(cx, "error.global_shipping_chokepoint.stale_data"),
        }
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

/// The colours one frame is drawn with.
///
/// The map, the panels and the text come from the theme; the ship palette is the
/// component's own, because the theme has six series and the map distinguishes twenty
/// types. Nothing is kept between frames: the appearance can change while the pane is
/// open, and a colour cached onto the widget would be a map that kept drawing the old
/// theme.
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
    /// One colour per ship type in [`crate::ship::SHIP_TYPES`] order.
    pub ships: &'static [Hsla; SHIP_TYPE_COUNT],
    /// The ring around the vessel under the pointer.
    pub hover: Hsla,
    /// The fill of the legend, the credit and the card.
    overlay: Hsla,
    /// The edge of those panels and of the card.
    border: Hsla,
    /// Text and the city dots.
    text: Hsla,
    /// Secondary text: the legend's title, the card's labels.
    muted: Hsla,
    /// The ring a place name is drawn with.
    ///
    /// A name is drawn straight on the picture, and the vessel markers under it are the
    /// same dark as the letters. The outline is the map's own paper colour — the same
    /// white (or night grey) the panels use — so it reads as the fill showing through
    /// rather than as a second colour on the map.
    pub halo: Hsla,
}

impl MapColors {
    pub(super) fn resolve(theme: &Theme) -> Self {
        let roles = &theme.roles;
        Self {
            ocean: roles.chart.map_ocean.hsla,
            land: roles.chart.map_land.hsla,
            map_border: roles.chart.map_border.hsla,
            road: roles.chart.map_road.hsla,
            road_minor: roles.chart.map_road_minor.hsla,
            ships: colours(theme.appearance),
            hover: roles.line.focus.hsla,
            overlay: roles.surface.overlay.hsla,
            border: roles.line.border.hsla,
            text: roles.fg.default.hsla,
            muted: roles.fg.muted.hsla,
            halo: roles.surface.overlay.hsla,
        }
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
    /// The vessels the filter draws, joined and projected when a push arrived.
    ///
    /// Building this is the one expensive step of a push, and it is what both the painters
    /// and the hit test read: a frame does not rebuild it, because a frame changes the
    /// camera rather than the picture. It holds only the vessels of the selected type, so
    /// a vessel the filter hides is neither painted nor hit.
    painted: Arc<[Vessel]>,
    /// The ship types the picture holds, in [`crate::ship::SHIP_TYPES`] order.
    ///
    /// The legend and the header's filter offer these and nothing else: a type nobody in
    /// the strait is currently reporting would be an entry that draws nothing.
    present_types: Vec<ShipType>,
    /// What the map is restricted to.
    filter: ShipTypeFilter,
    /// The painted vessels in canvas coordinates, filled when a pointer move is resolved.
    hit_points: Vec<(u32, [f32; 2])>,
    /// The vessel the pointer is on, if any.
    hovered: Option<u32>,
    /// The last pointer position, in window coordinates, which is what a drag's increment
    /// and the wheel's anchor are read from.
    pointer: Point<Pixels>,
    /// The generation of the last push, which is what makes the one-shot stale timer that
    /// a push arms stop reporting once a newer push has arrived.
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
        // The picture the retry keeps on screen is not being continued either: a
        // subscription that takes over and then delivers nothing has to fail the pane
        // the same way a link that went silent does, and this is the push it counts from.
        self.arm_stale_timer(cx);
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
    /// a picture the pane draws the map for, while a pane that has not received one yet has
    /// nothing to draw at all — not even an empty sea, which would claim the service
    /// answered.
    fn has_content(&self) -> bool {
        self.vessels.has_state()
    }

    /// The ship-type filter is the whole of what this widget puts in its header.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty().left(self.ship_type_select(cx))
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
            present_types: Vec::new(),
            filter: ShipTypeFilter::All,
            hit_points: Vec::new(),
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
    /// being abandoned from being applied to the picture that replaced it. The filter is
    /// kept: it is the reader's choice, not part of the picture.
    fn clear_picture(&mut self) {
        self.vessels.clear();
        self.painted = Arc::default();
        self.present_types.clear();
        self.hovered = None;
        self.hit_points.clear();
    }

    /// Rebuilds what the filter draws from the picture the store holds.
    ///
    /// The present types are derived from every vessel, before the filter is applied, so
    /// the legend and the header keep offering a type even while another one is selected.
    fn rebuild_painted(&mut self) {
        let vessels = self.vessels.vessels();
        self.present_types = model::present_ship_types(&vessels);
        let filter = self.filter;
        self.painted = vessels
            .into_iter()
            .filter(|vessel| filter.matches(vessel.ship_type))
            .collect::<Vec<_>>()
            .into();
        if let Some(mmsi) = self.hovered
            && !self.painted.iter().any(|vessel| vessel.mmsi == mmsi)
        {
            self.hovered = None;
        }
    }

    /// Restricts the map to one ship type, or widens it back to all of them.
    ///
    /// The type is kept even when the picture no longer holds it: the filter is the
    /// reader's decision, and a type that leaves the picture leaves an empty sea rather
    /// than silently widening the selection back to everything.
    fn select_filter(&mut self, filter: ShipTypeFilter, cx: &mut Context<Self>) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        self.rebuild_painted();
        cx.notify();
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
    ///
    /// A push that did not continue the picture is the one case where what arrived is not
    /// what the pane reports: the picture it holds is now missing a message, and no later
    /// push can repair that, so the pane reports the failure the banner is drawn for and
    /// leaves the vessels on the map under it.
    fn apply_update(&mut self, update: &GlobalShippingChokepointUpdate, cx: &mut Context<Self>) {
        self.vessels.apply(update);
        self.rebuild_painted();
        // A keyframe the server stamped with no time says it holds no state: the picture
        // is cleared and the pane waits for one instead of drawing an empty sea. It is a
        // gap as well, but there is no picture for it to be a gap in.
        if self.vessels.has_state() && self.vessels.has_gap() {
            self.fail_stale(StalePicture::Gap, cx);
            return;
        }
        if self.vessels.has_state() {
            self.status.report(WidgetState::Ready, cx);
        } else {
            self.status.report(WidgetState::Connecting, cx);
        }
        self.arm_stale_timer(cx);
    }

    /// Reports the failure a picture that stopped being continued is shown under.
    ///
    /// The recovery is the framework's own retry: the picture cannot be reconstructed
    /// from what the pane holds, so the way out is a new subscription rather than
    /// anything the pane could do to the one it has.
    fn fail_stale(&mut self, stale: StalePicture, cx: &mut Context<Self>) {
        let recovery = self.recovery.clone();
        self.status
            .report(WidgetState::failed(Arc::new(stale), recovery), cx);
    }

    /// Arms the one-shot timer that fails a link which goes silent.
    ///
    /// The timer is the whole of the staleness clock: it is armed by the push it counts
    /// from, and a push that arrives first replaces the generation, so the timer of a
    /// push that was followed by another reports nothing. A pane that never receives a
    /// push has nothing that can go stale, which is why the report is guarded on the
    /// picture rather than made unconditionally.
    fn arm_stale_timer(&mut self, cx: &mut Context<Self>) {
        self.stale_epoch += 1;
        let epoch = self.stale_epoch;
        cx.spawn(async move |this, cx| {
            // A link that went silent has no next push to arm another timer, so this one
            // is the only chance the failure has to be reported.
            cx.background_executor().timer(STALE_AFTER).await;
            let _ = this.update(cx, |this, cx| {
                // A push that arrived first has already replaced the generation, and a
                // pane with no picture has nothing that could have stopped being
                // continued: both cases report nothing.
                if this.stale_epoch == epoch && this.vessels.has_state() {
                    this.fail_stale(StalePicture::Silent, cx);
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

    /// The text the header's filter draws: `Ship type: Tanker`, or `All`.
    fn ship_type_choice(&self, cx: &App) -> SharedString {
        let value = match self.filter {
            ShipTypeFilter::All => t!(cx, "widget.global_shipping_chokepoint.filter.all"),
            ShipTypeFilter::Only(ship_type) => t!(cx, ship_type.label_key()),
        };
        t!(
            cx,
            "widget.global_shipping_chokepoint.filter.choice",
            value = value
        )
    }

    /// The ship-type filter in the pane's header.
    ///
    /// The menu offers "all" and the types the picture currently holds, in the legend's
    /// order. A chosen type that has left the picture stays in the trigger and in the
    /// filter; it is simply no longer offered until a vessel of that type returns.
    fn ship_type_select(&self, cx: &Context<Self>) -> AnyElement {
        let all = t!(cx, "widget.global_shipping_chokepoint.filter.all");
        let select = self.present_types.iter().fold(
            Select::new(ElementId::Name(SharedString::new_static(
                "GlobalShippingChokepointShipType",
            )))
            .placeholder(self.ship_type_choice(cx))
            .ghost()
            .small()
            .aria_label(t!(cx, "widget.global_shipping_chokepoint.filter.title"))
            .debug_selector(|| "TEST_CHOKEPOINT_SHIP_TYPE".into())
            .menu_item(MenuItem::Action(MenuItemAction::new(
                ShipTypeFilter::All.key(),
                all,
            ))),
            |select, ship_type| {
                select.menu_item(MenuItem::Action(MenuItemAction::new(
                    ship_type.key(),
                    t!(cx, ship_type.label_key()),
                )))
            },
        );

        select
            .on_change({
                let widget = cx.entity().downgrade();
                move |selected, _, app| {
                    let Some(key) = selected.as_deref() else {
                        return;
                    };
                    let Some(filter) = ShipTypeFilter::from_key(key) else {
                        return;
                    };
                    let _ = widget.update(app, |this, cx| this.select_filter(filter, cx));
                }
            })
            .into_any_element()
    }

    /// The map's labels: five countries, thirty-one cities, and two seas.
    ///
    /// [`place_labels`] decides which boxes a frame draws and in what order. It needs the
    /// size of each name to do it, because what must not overlap is the text and not the
    /// anchor — so every name is shaped here, in the language being read, before the
    /// placement runs. Measuring is what makes the rule language-independent in the way
    /// that matters: `迪拜` takes 26 px and `DUBAI` 38 px, and the placement is told which
    /// one this frame draws.
    ///
    /// A label is positioned by its anchor, and GPUI lays an absolute child out by its
    /// top-left corner with no transform to shift it by half its own size. Each label is
    /// therefore a box of a fixed size that is centred on the anchor, with the text
    /// centred inside it: the text's own box lands on the anchor rather than hanging
    /// down-right of it, which is what the old corner-anchored labels did. The box is
    /// wider than any name so the text never wraps, and it draws nothing of its own, so
    /// it is not the box the placement measures.
    ///
    /// The countries are drawn in the default foreground rather than the design's
    /// `fg.subtle`: subtle reads about 2:1 on the land, and these are the only words drawn
    /// straight on the fill, so they take the readable colour and clear 4.5:1 in both
    /// appearances (pinned by the test below).
    ///
    /// A country and a city carry a halo (see [`LABEL_HALO_OFFSETS`]): a name is drawn on
    /// the picture, and the anchorage it names is where the markers pile up, so the letters
    /// would otherwise sit on a field of the same dark as themselves. A sea's name is over
    /// open water, where nothing is drawn under it, so it is the one kind that takes no
    /// halo.
    ///
    /// The three kinds are told apart by size and weight as well as by colour: a country
    /// is the map's largest word, bold and italic, a city is a step under it, and water is
    /// the smallest and the only muted one. That is the order a paper atlas prints them in,
    /// and it is what makes a country read as a region rather than as another place name.
    fn labels(
        &self,
        projection: Projection,
        colors: MapColors,
        window: &Window,
        cx: &App,
    ) -> Vec<AnyElement> {
        place_labels(projection, |label, kind| {
            label_size(window, kind, &label_text(cx, label.key, kind))
        })
        .into_iter()
        .map(|label| {
            let [x, y] = label.at;
            let text = label_text(cx, label.key, label.kind);
            // One name in one colour: the halo copies and the name itself are the same
            // text in the same style, so restyling a kind cannot leave its outline behind.
            let name_in = |color: Hsla| match label.kind {
                LabelKind::Country => div()
                    .text_size(px(COUNTRY_LABEL_TEXT_SIZE))
                    .font_weight(FontWeight::BOLD)
                    .italic()
                    .text_color(color)
                    .child(text.clone()),
                LabelKind::Water => div()
                    .text_xs()
                    .italic()
                    .text_color(color)
                    .child(text.clone()),
                LabelKind::City => div()
                    .text_size(px(CITY_LABEL_TEXT_SIZE))
                    .text_color(color)
                    .child(text.clone()),
            };
            let halo = matches!(label.kind, LabelKind::Country | LabelKind::City).then(|| {
                LABEL_HALO_OFFSETS
                    .iter()
                    .map(|[dx, dy]| {
                        div()
                            .debug_selector(|| "TEST_CHOKEPOINT_LABEL_HALO".into())
                            .absolute()
                            .left(px(*dx))
                            .top(px(*dy))
                            .child(name_in(colors.halo))
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            });
            // A sea's name is the only muted one; a country's and a city's take the
            // readable foreground, because they are the words drawn over the picture.
            let color = match label.kind {
                LabelKind::Country | LabelKind::City => colors.text,
                LabelKind::Water => colors.muted,
            };
            let name = name_in(color).debug_selector(|| "TEST_CHOKEPOINT_LABEL_NAME".into());
            div()
                .absolute()
                .left(px(x - LABEL_BOX_WIDTH / 2.0))
                .top(px(y - LABEL_BOX_HEIGHT / 2.0))
                .w(px(LABEL_BOX_WIDTH))
                .h(px(LABEL_BOX_HEIGHT))
                .flex()
                .items_center()
                .justify_center()
                // The copies are positioned against the name's own box rather than against
                // the 320 px label box, which is what makes a one-pixel offset a ring
                // around the letters instead of a shift of the whole label. The name is the
                // last child, so it is painted over them.
                .child(
                    div()
                        .relative()
                        .children(halo.unwrap_or_default())
                        .child(name),
                )
                .into_any_element()
        })
        .collect()
    }

    /// The legend's line for the size of what the filter draws.
    ///
    /// The catalogue has no automatic plural rule, so the two keys are chosen here rather
    /// than interpolated into one: a filter drawing one vessel reads "1 vessel", not "1
    /// vessels".
    fn vessel_count_text(&self, cx: &App) -> SharedString {
        let count = self.painted.len();
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

    /// The legend in the pane's bottom-right corner: the ship types the picture holds and
    /// the count of what the filter draws.
    ///
    /// The types are listed in two columns because a picture can hold most of the twenty
    /// the map distinguishes, and a single column would grow past a short pane. Only the
    /// types that are present are listed: an entry nobody is reporting would draw
    /// nothing.
    ///
    /// It is lifted clear of the ODbL credit's line rather than sharing the bottom edge with it:
    /// a pane narrower than the two together would otherwise have the card painted over the
    /// credit, which is the one thing on the map that has to stay readable.
    fn legend(&self, colors: MapColors, cx: &App) -> AnyElement {
        let size = UnifiedSize::Small;
        let count_text = self.vessel_count_text(cx);
        let entries = self
            .present_types
            .iter()
            .map(|ship_type| {
                div()
                    .debug_selector({
                        let key = ship_type.key();
                        move || format!("TEST_CHOKEPOINT_LEGEND_{key}")
                    })
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex_none()
                            .size(px(8.0))
                            .rounded_sm()
                            .bg(colors.ships[ship_type.index()]),
                    )
                    .child(
                        div()
                            .text_color(colors.text)
                            .child(t!(cx, ship_type.label_key())),
                    )
            })
            .collect::<Vec<_>>();

        div()
            .debug_selector(|| "TEST_CHOKEPOINT_LEGEND".into())
            .absolute()
            .right(px(8.0))
            // Above the credit's line (6px inset + one `text_xs` line), so the two never overlap
            // however narrow the pane is.
            .bottom(px(32.0))
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
            .child(
                div()
                    .grid()
                    .grid_cols(2)
                    .gap_x(px(12.0))
                    .gap_y(size.gap())
                    .children(entries),
            )
            .child(div().text_color(colors.muted).child(count_text))
            .into_any_element()
    }

    /// The ODbL credit the map is drawn under, in the pane's bottom-left corner.
    ///
    /// The text is the default foreground rather than `fg.muted`: it is drawn straight on the
    /// land, and small text on the buff fill has to clear the 4.5:1 a paragraph is held to,
    /// which the muted step does not. The corner is the one the legend (raised above it in the
    /// bottom-right) leaves free, and the label is a link so "OpenStreetMap" is reachable
    /// from the credit, which is what OSMF's guidance asks for.
    fn attribution(&self, colors: MapColors, cx: &App) -> AnyElement {
        div()
            .id(ElementId::Name(SharedString::new_static(
                "GlobalShippingChokepointAttribution",
            )))
            .debug_selector(|| "TEST_CHOKEPOINT_ATTRIBUTION".into())
            .absolute()
            .left(px(8.0))
            .bottom(px(6.0))
            .text_xs()
            .text_color(colors.text)
            .cursor_pointer()
            .on_click(|_, _, cx| cx.open_url(ATTRIBUTION_URL))
            .child(t!(cx, "widget.global_shipping_chokepoint.attribution"))
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
            details.ship_type.map_or_else(
                || unavailable.clone(),
                |ship_type| t!(cx, ship_type.label_key()),
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
            text(details.static_updated_at.as_ref()),
        ];
        let rows = HOVER_KEYS
            .iter()
            .zip(values)
            .map(|(key, value)| {
                let label = div()
                    .flex_none()
                    .text_color(colors.muted)
                    .child(t!(cx, *key));
                let value = div()
                    // A destination or a call sign is as long as the report says: it is
                    // cut rather than allowed to widen the card off the map.
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(colors.text)
                    .child(value);
                if HOVER_STACKED_KEYS.contains(key) {
                    // The stamp takes the whole card's width under its label, which is
                    // what keeps the seconds of it off the ellipsis.
                    div()
                        .flex()
                        .flex_col()
                        .items_start()
                        .child(label)
                        .child(
                            value
                                .debug_selector(|| "TEST_CHOKEPOINT_CARD_STAMP".into())
                                .w_full(),
                        )
                        .into_any_element()
                } else {
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(8.0))
                        .child(label)
                        .child(value.min_w_0().flex_1())
                        .into_any_element()
                }
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        let labels = projection
            .filter(|projection| projection.drawable())
            .map_or_else(Vec::new, |projection| {
                self.labels(projection, colors, window, cx)
            });
        let legend = self.legend(colors, cx);
        let attribution = self.attribution(colors, cx);
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
            .child(attribution)
            .child(legend)
            .when_some(card, |this, card| this.child(card))
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
        App, AppContext, BorrowAppContext as _, Entity, IntoElement, Modifiers, MouseButton,
        ParentElement, Pixels, Render, Rgba, ScrollDelta, ScrollWheelEvent, Styled, TestAppContext,
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
        model::{CITY_LABELS, CITY_LABELS_LATE, COUNTRY_LABELS, HIT_RADIUS_PX, WATER_LABELS},
        service::{
            AisStreamError, AisStreamMessage, GlobalShippingChokepointEventSubscription,
            GlobalShippingChokepointSession,
        },
        ship::SHIP_TYPES,
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
            static_updated_at: 1_700_000_000_000,
        }
    }

    /// One vessel's ship record with the AIS type `ship_type` and every other value as
    /// [`ships`] carries it.
    fn ship_of_type(mmsi: u32, ship_type: u32) -> GlobalShippingChokepointShip {
        let mut ship = ships(mmsi);
        ship.ship_type = ship_type;
        ship
    }

    /// A representative AIS `ship_type` code for one main type.
    fn code_of(ship_type: ShipType) -> u32 {
        match ship_type {
            ShipType::Tanker => 80,
            ShipType::Cargo => 70,
            ShipType::Passenger => 60,
            ShipType::Fishing => 30,
            ShipType::Towing => 31,
            ShipType::Dredging => 33,
            ShipType::Military => 35,
            ShipType::Sailing => 36,
            ShipType::Pleasure => 37,
            ShipType::HighSpeed => 40,
            ShipType::Pilot => 50,
            ShipType::SearchAndRescue => 51,
            ShipType::PortTender => 53,
            ShipType::AntiPollution => 54,
            ShipType::LawEnforcement => 55,
            ShipType::Medical => 58,
            ShipType::Noncombatant => 59,
            ShipType::WingInGround => 20,
            ShipType::Other => 90,
            ShipType::Unknown => 0,
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

    /// A window whose top row is the widget's own header group and whose body is the pane.
    struct HeaderHarness {
        pane: Entity<GlobalShippingChokepointWidget>,
    }

    impl Render for HeaderHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let header = self.pane.update(cx, |pane, cx| pane.header(window, cx));

            div()
                .size_full()
                .min_h_0()
                .flex()
                .flex_col()
                .child(div().flex().items_center().children(header.left))
                .child(div().flex_1().min_h_0().child(self.pane.clone()))
        }
    }

    /// Mounts one pane's header and body in a window of its own, already drawn once.
    fn header_window<'a>(
        cx: &'a mut TestAppContext,
        pane: &Entity<GlobalShippingChokepointWidget>,
    ) -> &'a mut VisualTestContext {
        let pane = pane.clone();
        let (_, window) = cx.add_window_view(move |_, _| HeaderHarness { pane });
        window.simulate_resize(size(px(600.0), px(400.0)));
        window.run_until_parked();
        repaint(window);
        repaint(window);
        window
    }

    /// Presses what `selector` names, the way a window delivers a press: the button goes
    /// down, the frame that follows is drawn, and only then does it come up.
    fn press(window: &mut VisualTestContext, selector: &'static str) {
        let target = window
            .debug_bounds(selector)
            .expect("the harness draws what it is asked to press")
            .center();
        window.simulate_mouse_down(target, MouseButton::Left, Modifiers::none());
        window.run_until_parked();
        window.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
        window.run_until_parked();
        window.update(|window, _| window.refresh());
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

    /// A ship marker is drawn straight on the map's two fills, so every authored colour
    /// has to read on both of them in the appearance it belongs to.
    #[test]
    fn every_ship_type_colour_should_read_on_both_map_fills() {
        let mut failures = Vec::new();
        for appearance in [Appearance::Light, Appearance::Dark] {
            let colors = MapColors::resolve(&Theme::of(appearance));
            for ship_type in SHIP_TYPES {
                let colour = colors.ships[ship_type.index()];
                for (what, fill) in [("land", colors.land), ("sea", colors.ocean)] {
                    let ratio = wcag_contrast(colour, fill);
                    if ratio < 2.0 {
                        failures.push(format!(
                            "{ship_type:?} on the {what} in {appearance:?} reads {ratio:.2}:1"
                        ));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
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

    /// An empty picture is still a picture: the pane draws the map and its legend, rather
    /// than leaving the framework's spinner up for ever or covering the strait with a
    /// placeholder of its own.
    #[gpui_kit::test]
    fn an_empty_keyframe_should_be_ready_with_the_map_alone(cx: &mut TestAppContext) {
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
            window.debug_bounds("TEST_CHOKEPOINT_MAP").is_some(),
            "an empty strait is still drawn as the strait",
        );
        assert!(
            window.debug_bounds("TEST_CHOKEPOINT_LEGEND").is_some(),
            "the legend is part of the ready body",
        );
        assert!(
            window.debug_bounds("EMPTY_STATE").is_none(),
            "a picture with no vessel carries no placeholder of its own",
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

    /// The header's filter offers "all" and the types the picture holds, and choosing one
    /// restricts the map to that type.
    #[gpui_kit::test]
    fn the_header_should_filter_the_map_by_ship_type(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        cx.update(|cx| {
            let mut frame = keyframe(
                7,
                1,
                vec![position(1), position(2), position(3), position(4)],
            );
            frame.ships = vec![
                ship_of_type(1, 80),
                ship_of_type(2, 70),
                ship_of_type(3, 60),
            ];
            assert!(report_update(&pane, frame, cx));
        });

        let window = header_window(cx, &pane);
        assert!(
            window.debug_bounds("TEST_CHOKEPOINT_SHIP_TYPE").is_some(),
            "the filter is the header's left group",
        );

        press(window, "TEST_CHOKEPOINT_SHIP_TYPE");
        assert!(
            window.debug_bounds("TEST_DROPDOWN_PANEL").is_some(),
            "pressing the filter opens its menu",
        );
        for selector in [
            "TEST_MENU_ITEM_all",
            "TEST_MENU_ITEM_tanker",
            "TEST_MENU_ITEM_cargo",
            "TEST_MENU_ITEM_passenger",
            "TEST_MENU_ITEM_unknown",
        ] {
            assert!(
                window.debug_bounds(selector).is_some(),
                "{selector} is a type the picture holds",
            );
        }
        assert!(
            window.debug_bounds("TEST_MENU_ITEM_fishing").is_none(),
            "a type the picture does not hold is not offered",
        );

        press(window, "TEST_MENU_ITEM_tanker");
        window.update(|_, cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.filter, ShipTypeFilter::Only(ShipType::Tanker));
            assert_eq!(pane.painted.len(), 1, "only the tanker is drawn");
            assert_eq!(pane.painted[0].mmsi, 1);
            assert_eq!(
                pane.vessel_count_text(cx).as_ref(),
                t!(cx, "widget.global_shipping_chokepoint.vessel_count_one").as_ref(),
                "the legend counts what the filter draws",
            );
            assert!(
                pane.ship_type_choice(cx).contains(
                    t!(cx, "widget.global_shipping_chokepoint.ship_type.tanker").as_ref()
                ),
                "the trigger names the chosen type",
            );
        });

        press(window, "TEST_CHOKEPOINT_SHIP_TYPE");
        press(window, "TEST_MENU_ITEM_all");
        window.update(|_, cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.filter, ShipTypeFilter::All);
            assert_eq!(pane.painted.len(), 4, "all four vessels are drawn again");
        });
    }

    /// A picture holding every type gives the filter twenty entries, and the panel grows
    /// to hold them: the menu is not capped at a height its entries would spill out of.
    #[gpui_kit::test]
    fn a_full_ship_type_menu_should_hold_every_entry(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        cx.update(|cx| {
            let mut frame = keyframe(7, 1, (1..=20).map(position).collect());
            frame.ships = SHIP_TYPES
                .iter()
                .enumerate()
                .map(|(index, ship_type)| ship_of_type(index as u32 + 1, code_of(*ship_type)))
                .collect();
            assert!(report_update(&pane, frame, cx));
        });

        let window = header_window(cx, &pane);
        press(window, "TEST_CHOKEPOINT_SHIP_TYPE");
        let panel = window
            .debug_bounds("TEST_DROPDOWN_PANEL")
            .expect("pressing the filter opens its menu");
        let last = window
            .debug_bounds("TEST_MENU_ITEM_unknown")
            .expect("the menu lists every type the picture holds");
        assert!(
            last.top() >= panel.top() && last.bottom() <= panel.bottom(),
            "the last type is inside the panel: {last:?} in {panel:?}",
        );
    }

    /// The legend lists the types the picture holds and nothing else, in two columns.
    #[gpui_kit::test]
    fn the_legend_should_list_only_the_types_the_picture_holds(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        cx.update(|cx| {
            let mut frame = keyframe(7, 1, vec![position(1), position(2), position(3)]);
            frame.ships = vec![ship_of_type(1, 80), ship_of_type(2, 70)];
            assert!(report_update(&pane, frame, cx));
        });

        let window = map_window(cx, &pane);
        for selector in [
            "TEST_CHOKEPOINT_LEGEND_tanker",
            "TEST_CHOKEPOINT_LEGEND_cargo",
            "TEST_CHOKEPOINT_LEGEND_unknown",
        ] {
            assert!(
                window.debug_bounds(selector).is_some(),
                "{selector} is a type the picture holds",
            );
        }
        assert!(
            window
                .debug_bounds("TEST_CHOKEPOINT_LEGEND_passenger")
                .is_none(),
            "a type the picture does not hold has no legend entry",
        );
        window.update(|_, cx| {
            assert_eq!(
                pane.read(cx).vessel_count_text(cx).as_ref(),
                t!(
                    cx,
                    "widget.global_shipping_chokepoint.vessel_count",
                    count = 3
                )
                .as_ref(),
                "the legend counts the whole picture while the filter is all",
            );
        });
    }

    /// A vessel the filter hides is not under the pointer any more: its card goes with it.
    #[gpui_kit::test]
    fn a_filtered_out_vessel_should_not_keep_its_card(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        cx.update(|cx| {
            let mut cargo = position(2);
            cargo.latitude = 25_500_000;
            cargo.longitude = 56_800_000;
            let mut frame = keyframe(7, 1, vec![position(1), cargo]);
            frame.ships = vec![ship_of_type(1, 80), ship_of_type(2, 70)];
            assert!(report_update(&pane, frame, cx));
        });

        let window = map_window(cx, &pane);
        let at = window.update(|_, cx| {
            let pane = pane.read(cx);
            let hull = hull_of(pane, 2).expect("the cargo ship is painted");
            window_point(pane, hull).expect("a drawn map records its box")
        });
        window.simulate_mouse_move(at, None, Modifiers::none());
        repaint(window);
        assert_eq!(
            window.update(|_, cx| pane.read(cx).hovered),
            Some(2),
            "the pointer moved onto the cargo ship",
        );
        assert!(window.debug_bounds("TEST_CHOKEPOINT_CARD").is_some());

        window.update(|_, cx| {
            pane.update(cx, |pane, cx| {
                pane.select_filter(ShipTypeFilter::Only(ShipType::Tanker), cx);
            });
        });
        repaint(window);
        assert_eq!(
            window.update(|_, cx| pane.read(cx).hovered),
            None,
            "the hidden vessel is on no card",
        );
        assert!(
            window.debug_bounds("TEST_CHOKEPOINT_CARD").is_none(),
            "the card of a filtered-out vessel is gone",
        );

        window.update(|_, cx| {
            pane.update(cx, |pane, cx| pane.select_filter(ShipTypeFilter::All, cx));
        });
        repaint(window);
        let at = window.update(|_, cx| {
            let pane = pane.read(cx);
            let hull = hull_of(pane, 2).expect("the cargo ship is painted again");
            window_point(pane, hull).expect("a drawn map records its box")
        });
        window.simulate_mouse_move(at, None, Modifiers::none());
        repaint(window);
        assert_eq!(
            window.update(|_, cx| pane.read(cx).hovered),
            Some(2),
            "widening the filter brings the vessel back",
        );
    }

    /// A chosen type that leaves the picture stays chosen: the filter is the reader's
    /// decision, and the map simply draws nothing until a vessel of that type returns.
    #[gpui_kit::test]
    fn the_selected_type_should_survive_its_disappearance(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        cx.update(|cx| {
            let mut frame = keyframe(7, 1, vec![position(1), position(2)]);
            frame.ships = vec![ship_of_type(1, 80), ship_of_type(2, 70)];
            assert!(report_update(&pane, frame, cx));
            pane.update(cx, |pane, cx| {
                pane.select_filter(ShipTypeFilter::Only(ShipType::Tanker), cx);
            });

            let mut without_tanker = keyframe(7, 2, vec![position(2)]);
            without_tanker.ships = vec![ship_of_type(2, 70)];
            assert!(report_update(&pane, without_tanker, cx));

            let pane = pane.read(cx);
            assert_eq!(
                pane.filter,
                ShipTypeFilter::Only(ShipType::Tanker),
                "the choice survives the type leaving the picture",
            );
            assert!(
                pane.painted.is_empty(),
                "nothing of the chosen type is drawn"
            );
            assert_eq!(pane.present_types, [ShipType::Cargo]);
            assert_eq!(
                pane.vessel_count_text(cx).as_ref(),
                t!(
                    cx,
                    "widget.global_shipping_chokepoint.vessel_count",
                    count = 0
                )
                .as_ref(),
            );
            assert!(
                pane.ship_type_choice(cx).contains(
                    t!(cx, "widget.global_shipping_chokepoint.ship_type.tanker").as_ref()
                ),
                "the trigger still names the chosen type",
            );
        });

        cx.update(|cx| {
            let mut with_tanker = keyframe(7, 3, vec![position(1)]);
            with_tanker.ships = vec![ship_of_type(1, 80)];
            assert!(report_update(&pane, with_tanker, cx));
            assert_eq!(
                pane.read(cx).painted.len(),
                1,
                "the chosen type returns when a vessel of it does",
            );
        });
    }

    /// The count is the filtered picture's, not the store's.
    #[gpui_kit::test]
    fn the_filter_should_count_only_what_it_draws(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        cx.update(|cx| {
            let mut frame = keyframe(7, 1, vec![position(1), position(2), position(3)]);
            frame.ships = vec![
                ship_of_type(1, 80),
                ship_of_type(2, 80),
                ship_of_type(3, 70),
            ];
            assert!(report_update(&pane, frame, cx));

            let count = |cx: &mut App, filter: ShipTypeFilter| {
                pane.update(cx, |pane, cx| {
                    pane.select_filter(filter, cx);
                    pane.vessel_count_text(cx)
                })
            };

            assert_eq!(
                count(cx, ShipTypeFilter::Only(ShipType::Tanker)).as_ref(),
                t!(
                    cx,
                    "widget.global_shipping_chokepoint.vessel_count",
                    count = 2
                )
                .as_ref(),
                "two tankers",
            );
            assert_eq!(
                count(cx, ShipTypeFilter::Only(ShipType::Cargo)).as_ref(),
                t!(cx, "widget.global_shipping_chokepoint.vessel_count_one").as_ref(),
                "one cargo ship",
            );
            assert_eq!(
                count(cx, ShipTypeFilter::All).as_ref(),
                t!(
                    cx,
                    "widget.global_shipping_chokepoint.vessel_count",
                    count = 3
                )
                .as_ref(),
                "all three vessels",
            );
            assert_eq!(
                count(cx, ShipTypeFilter::Only(ShipType::Passenger)).as_ref(),
                t!(
                    cx,
                    "widget.global_shipping_chokepoint.vessel_count",
                    count = 0
                )
                .as_ref(),
                "a type the picture does not hold draws nothing",
            );
        });
    }

    /// A push that does not continue the picture leaves a message missing from it, and no
    /// later push can put that message back: the pane reports the failure the framework
    /// draws its banner for, and the vessels stay on the map under it.
    #[gpui_kit::test]
    fn a_gap_should_fail_the_pane_while_the_picture_stays(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));

        cx.update(|cx| {
            let mut frame = keyframe(7, 10, vec![position(1)]);
            frame.ships = vec![ships(1)];
            assert!(report_update(&pane, frame, cx));
            // A delta that does not continue the applied sequence is a gap rather than a
            // push to apply.
            assert!(report_update(&pane, removal(7, 5, 6, 1), cx));

            let pane = pane.read(cx);
            let state = pane.status.state(cx);
            let failure = state.failure().expect("a gap fails the pane");
            assert_eq!(
                failure.text(cx).as_ref(),
                t!(cx, "error.global_shipping_chokepoint.stale").as_ref(),
                "the banner names the picture rather than the map",
            );
            assert_eq!(
                failure.action().label_key(),
                "common.retry",
                "the way out is a new subscription",
            );
            assert!(pane.vessels.has_gap());
            assert!(
                pane.has_content(),
                "the picture stays on the map under the banner",
            );
            assert_eq!(pane.vessels.vessel_count(), 1);
        });

        let window = map_window(cx, &pane);
        assert!(window.debug_bounds("TEST_CHOKEPOINT_LEGEND").is_some());
        assert!(
            window.debug_bounds("TEST_CHOKEPOINT_STALE").is_none(),
            "the body carries no badge of its own any more",
        );
    }

    /// A stale picture is a failure rather than a warning: the pushes that follow it are
    /// the ones the pane cannot vouch for, so only the control the banner offers leaves it.
    #[gpui_kit::test]
    fn a_stale_pane_should_ignore_pushes_until_the_user_retries(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));

        cx.update(|cx| {
            let mut frame = keyframe(7, 10, vec![position(1)]);
            frame.ships = vec![ships(1)];
            assert!(report_update(&pane, frame, cx));
            assert!(report_update(&pane, removal(7, 5, 6, 1), cx));

            // A keyframe would repair the picture, but a failed pane reads nothing.
            let mut healing = keyframe(7, 11, vec![position(1), position(2)]);
            healing.ships = vec![ships(1), ships(2)];
            assert!(
                !report_update(&pane, healing, cx),
                "a failed pane applies no push",
            );
            assert_eq!(
                pane.read(cx).vessels.vessel_count(),
                1,
                "the picture the pane holds is the one it was left with",
            );

            // The control the banner offers is the framework's retry, which loads again.
            pane.update(cx, |pane, cx| pane.load(cx));
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Connecting,
                "the retry starts over",
            );

            assert!(report_update(&pane, keyframe(7, 1, vec![position(1)]), cx));
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);
        });
    }

    /// A link that delivers nothing fails the pane when the window closes, even though the
    /// last picture is still on screen: what is stale is the update, not what was received.
    #[gpui_kit::test]
    fn a_silent_link_should_fail_the_pane_when_the_window_closes(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        cx.update(|cx| {
            let mut frame = keyframe(7, 1, vec![position(1)]);
            frame.ships = vec![ships(1)];
            assert!(report_update(&pane, frame, cx));
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Ready);
        });

        // Nothing else arrives: the timer armed by that push is the whole of the window.
        cx.executor().advance_clock(STALE_AFTER);
        cx.run_until_parked();

        cx.update(|cx| {
            let pane = pane.read(cx);
            let state = pane.status.state(cx);
            let failure = state.failure().expect("a silent link fails the pane");
            assert_eq!(
                failure.text(cx).as_ref(),
                t!(cx, "error.global_shipping_chokepoint.stale_data").as_ref(),
                "the silence is what is reported, not a picture missing a push",
            );
            assert_eq!(failure.action().label_key(), "common.retry");
            assert!(
                pane.has_content(),
                "the picture stays on the map under the banner",
            );
        });
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
            SceneError::LandHasCells,
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
        // Eighteen lines of card are taller than the default test window's map, and a
        // card that cannot fit at all is a different case from the one under test:
        // grow the pane until the placement has somewhere to keep the card.
        window.simulate_resize(size(px(600.0), px(560.0)));
        window.run_until_parked();
        repaint(window);
        repaint(window);

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

    /// The two update times are stamped under their labels rather than beside them: on one
    /// line the label and the stamp together are wider than the card, and it is the stamp
    /// that is cut. Under its label the stamp has the card's whole width to itself.
    #[gpui_kit::test]
    fn the_update_times_should_be_stamped_under_their_labels(cx: &mut TestAppContext) {
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

        let card = window
            .debug_bounds("TEST_CHOKEPOINT_CARD")
            .expect("the vessel under the pointer has a card");
        let stamp = window
            .debug_bounds("TEST_CHOKEPOINT_CARD_STAMP")
            .expect("the card carries the update times");
        // The card's text box: its width less the padding and the border on both sides.
        let text_left = card.left() + UnifiedSize::Small.px() + px(1.0);
        let text_width = card.size.width - 2.0 * UnifiedSize::Small.px() - px(2.0);
        assert!(
            (f32::from(stamp.left()) - f32::from(text_left)).abs() < 0.5,
            "a stamp under its label starts at the card's text edge: {} in {card:?}",
            f32::from(stamp.left()),
        );
        assert!(
            (f32::from(stamp.size.width) - f32::from(text_width)).abs() < 0.5,
            "a stamp under its label takes the card's whole width: {} of {}",
            f32::from(stamp.size.width),
            f32::from(text_width),
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
                "error.global_shipping_chokepoint.map_asset",
                "error.global_shipping_chokepoint.stale",
                "error.global_shipping_chokepoint.stale_data",
                "widget.names.global_shipping_chokepoint",
                "widget.global_shipping_chokepoint.attribution",
                "widget.global_shipping_chokepoint.entitlement_required",
                "widget.global_shipping_chokepoint.filter.all",
                "widget.global_shipping_chokepoint.filter.choice",
                "widget.global_shipping_chokepoint.filter.title",
                "widget.global_shipping_chokepoint.legend.title",
                "widget.global_shipping_chokepoint.sign_in",
                "widget.global_shipping_chokepoint.unavailable",
                "widget.global_shipping_chokepoint.vessel_count",
                "widget.global_shipping_chokepoint.vessel_count_one",
                "widget.global_shipping_chokepoint.view_plans",
            ];
            keys.extend(HOVER_KEYS);
            keys.extend(SHIP_TYPES.iter().map(|ship_type| ship_type.label_key()));
            keys.extend(NAV_STATUS_KEYS);
            keys.extend(COUNTRY_LABELS.iter().map(|label| label.key));
            keys.extend(CITY_LABELS.iter().map(|label| label.key));
            keys.extend(CITY_LABELS_LATE.iter().map(|label| label.key));
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
                    !t!(cx, "widget.global_shipping_chokepoint.vessel_count_one").contains("%{"),
                    "the singular count carries no placeholder in {language:?}",
                );
                assert!(
                    !t!(
                        cx,
                        "widget.global_shipping_chokepoint.filter.choice",
                        value = "x"
                    )
                    .contains("%{"),
                    "the filter's value is supplied in {language:?}",
                );
            }
        });
    }

    /// The fitted view names the Gulf's coast: the cities a reader looks for are on it, the
    /// names that are drawn do not print over each other, and the two names the island
    /// cannot hold are the ones that wait for a zoom.
    ///
    /// The widths here are the shaped ones, so this is the test that measures the density
    /// the pane was designed around: `model.rs` covers the rule with the widest name of
    /// each kind, and this one covers what the font actually does in both languages.
    #[gpui_kit::test]
    fn the_fitted_view_should_name_the_gulfs_coast_cities(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        let window = map_window(cx, &pane);
        // The pane the density was measured on: 1,404 x 900 of canvas.
        window.simulate_resize(size(px(1_436.0), px(932.0)));
        window.run_until_parked();
        repaint(window);

        let (keys, viewport) = window.update(|window, cx| {
            let pane = pane.read(cx);
            let bounds = pane.frame.bounds.get().expect("the map has a box");
            let viewport = viewport_of(bounds);
            let projection = Projection::new(pane.frame.camera.get(), viewport);
            let measure = |label: &model::PlacedLabel| {
                label_size(window, label.kind, &label_text(cx, label.key, label.kind))
            };
            let placed = place_labels(projection, |label, kind| {
                label_size(window, kind, &label_text(cx, label.key, kind))
            });

            let boxes: Vec<[f32; 4]> = placed
                .iter()
                .map(|label| {
                    let size = measure(label);
                    [
                        label.at[0] - size.width / 2.0,
                        label.at[1] - size.height / 2.0,
                        label.at[0] + size.width / 2.0,
                        label.at[1] + size.height / 2.0,
                    ]
                })
                .collect();
            for (index, label) in placed.iter().enumerate() {
                for (earlier, other) in placed[..index].iter().enumerate() {
                    let (left, right) = (boxes[index], boxes[earlier]);
                    assert!(
                        left[2] + model::LABEL_GAP_PX <= right[0]
                            || right[2] + model::LABEL_GAP_PX <= left[0]
                            || left[3] + model::LABEL_GAP_PX <= right[1]
                            || right[3] + model::LABEL_GAP_PX <= left[1],
                        "{} and {} print over each other",
                        other.key,
                        label.key,
                    );
                }
            }
            (
                placed.iter().map(|label| label.key).collect::<Vec<_>>(),
                viewport,
            )
        });
        assert!(
            viewport[0] > 1_380.0 && viewport[1] > 890.0,
            "the pane is the one the density was measured on: {viewport:?}",
        );

        // Twenty-eight of the thirty-one cities fit here; the three that do not are the two
        // names a bigger name covers and the island capital.
        let cities = keys.iter().filter(|key| key.contains(".city.")).count();
        assert!(cities >= 26, "the fitted view names {cities} cities");
        for city in [
            "city.dubai",
            "city.abu_dhabi",
            "city.jebel_ali",
            "city.doha",
            "city.dammam",
            "city.bandar_abbas",
            "city.fujairah",
            "city.muscat",
            "city.sohar",
            "city.ras_laffan",
        ] {
            assert!(
                keys.iter().any(|key| key.ends_with(city)),
                "{city} is not on the fitted view",
            );
        }
        for city in ["city.sharjah", "city.mesaieed"] {
            assert!(
                !keys.iter().any(|key| key.ends_with(city)),
                "{city} has no room beside the name that covers it",
            );
        }
        assert!(
            !keys.iter().any(|key| key.ends_with("city.manama")),
            "Bahrain's own name covers its capital at the fitted zoom",
        );

        // The island has grown enough for both names by the zoom the capital waits for.
        let zoomed = window.update(|window, cx| {
            let camera = Camera {
                center: model::project(50.58, 26.22),
                zoom: model::CITY_LABEL_LATE_ZOOM,
            }
            .clamped(viewport);
            place_labels(Projection::new(camera, viewport), |label, kind| {
                label_size(window, kind, &label_text(cx, label.key, kind))
            })
        });
        assert!(
            zoomed
                .iter()
                .any(|label| label.key.ends_with("city.manama")),
            "the capital is named once the island is zoomed in",
        );
    }

    /// The words drawn straight on the map fills clear the contrast a paragraph is held to in
    /// both appearances: the country names and the ODbL credit take the default foreground,
    /// which is the only text step that does — `fg.subtle` reads about 2:1 on the land.
    #[test]
    fn the_attribution_reads_on_both_map_fills() {
        for appearance in [Appearance::Light, Appearance::Dark] {
            let colors = MapColors::resolve(&Theme::of(appearance));
            for (what, background) in [("the land", colors.land), ("the sea", colors.ocean)] {
                let ratio = wcag_contrast(colors.text, background);
                assert!(
                    ratio >= 4.5,
                    "{appearance:?}: the attribution reads {ratio:.2}:1 on {what}",
                );
            }
        }
    }

    /// A halo is only a halo if the letters stand clearly off it: the ring is drawn in the
    /// map's paper colour, so the name has to read against that colour in both appearances.
    /// An outline a step away from the letters would be a second dark tone on a dark map,
    /// which is the state the halo exists to end.
    #[test]
    fn the_halo_should_stand_off_the_letters_it_outlines() {
        for appearance in [Appearance::Light, Appearance::Dark] {
            let colors = MapColors::resolve(&Theme::of(appearance));
            let ratio = wcag_contrast(colors.text, colors.halo);
            assert!(
                ratio >= 4.5,
                "{appearance:?}: a name reads {ratio:.2}:1 on its own halo",
            );
        }
    }

    /// The halo is a ring rather than a shadow: eight offsets, each exactly one pixel from
    /// the name and each direction once. A missing diagonal is a gap the markers show
    /// through at the city's 13 px step, and a wider offset is a second, thicker copy of
    /// the letters rather than an outline.
    #[test]
    fn the_halo_should_ring_the_name_it_outlines() {
        assert_eq!(LABEL_HALO_OFFSETS.len(), 8, "eight directions are covered");

        for [dx, dy] in LABEL_HALO_OFFSETS {
            assert_eq!(
                dx.abs().max(dy.abs()),
                1.0,
                "the offset [{dx}, {dy}] is one pixel",
            );
            assert!(
                LABEL_HALO_OFFSETS.contains(&[-dx, -dy]),
                "the offset [{dx}, {dy}] has no opposite",
            );
        }
    }

    /// A copy of a name is laid out in the name's own box, one pixel off it — not in the
    /// 320 px label box the name is centred in. The difference is the whole mechanism: an
    /// offset against the label box would put the ring a hundred pixels away and read as a
    /// smear beside the name rather than as its outline.
    ///
    /// The body is cached, so the frame that answers `debug_bounds` has to be a forced
    /// repaint (`crates/widgets/AGENTS.md` §10, trap 5).
    #[gpui_kit::test]
    fn a_name_should_be_ringed_by_its_halo(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        cx.update(|cx| {
            assert!(report_update(&pane, keyframe(7, 1, vec![position(1)]), cx));
        });
        let window = map_window(cx, &pane);
        repaint(window);

        let name = window
            .debug_bounds("TEST_CHOKEPOINT_LABEL_NAME")
            .expect("the fitted view names a place");
        let halo = window
            .debug_bounds("TEST_CHOKEPOINT_LABEL_HALO")
            .expect("a city's name is ringed");
        let offset = halo.origin - name.origin;

        assert_eq!(halo.size, name.size, "a copy is the name's own box");
        assert!(
            LABEL_HALO_OFFSETS
                .iter()
                .any(|[dx, dy]| offset == point(px(*dx), px(*dy))),
            "the copy sits at one of the offsets, measured from the name: {offset:?}",
        );
    }

    /// The map is drawn under its ODbL credit: the label sits in the bottom-left corner of the
    /// map's box, below the legend.
    ///
    /// The body is the cached layer, so the frame that answers `debug_bounds` has to be a
    /// forced full repaint — which is what `repaint` is (`crates/widgets/AGENTS.md` §10, trap 5).
    ///
    /// `debug_bounds` records where an element was painted, not what was painted over it, so this
    /// pins presence and place; what keeps the credit above the map is its place in `render`
    /// (after the canvas), and no assertion here can observe that order.
    #[gpui_kit::test]
    fn the_attribution_is_drawn_on_the_map(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        cx.update(|cx| {
            assert!(report_update(&pane, keyframe(7, 1, vec![position(1)]), cx));
        });
        let window = map_window(cx, &pane);
        repaint(window);

        let attribution = window
            .debug_bounds("TEST_CHOKEPOINT_ATTRIBUTION")
            .expect("the map is drawn under its credit");
        let map = window
            .debug_bounds("TEST_CHOKEPOINT_MAP")
            .expect("the map is drawn");
        let legend = window
            .debug_bounds("TEST_CHOKEPOINT_LEGEND")
            .expect("the legend is drawn");

        assert!(
            attribution.left() >= map.left()
                && attribution.right() <= map.right()
                && attribution.top() >= map.top()
                && attribution.bottom() <= map.bottom(),
            "the credit is inside the map's box: {attribution:?} in {map:?}",
        );
        assert!(
            attribution.right() <= map.center().x && attribution.top() >= map.center().y,
            "the credit sits in the bottom-left corner: {attribution:?} in {map:?}",
        );
        assert!(
            legend.bottom() <= attribution.top(),
            "the legend clears the credit's line: {legend:?} above {attribution:?}",
        );
    }

    /// The credit is a link rather than a label: pressing it opens the copyright page OSMF's
    /// guidance points at.
    #[gpui_kit::test]
    fn the_attribution_should_open_the_copyright_page(cx: &mut TestAppContext) {
        let pane = pane(cx, RecordingSource::default(), Some(ACCOUNT));
        cx.update(|cx| {
            assert!(report_update(&pane, keyframe(7, 1, vec![position(1)]), cx));
        });
        let window = map_window(cx, &pane);
        repaint(window);

        let attribution = window
            .debug_bounds("TEST_CHOKEPOINT_ATTRIBUTION")
            .expect("the map is drawn under its credit");
        let map = window
            .debug_bounds("TEST_CHOKEPOINT_MAP")
            .expect("the map is drawn");
        window.simulate_click(map.center(), Modifiers::none());
        assert_eq!(
            window.opened_url(),
            None,
            "a press on the map itself opens nothing",
        );
        window.simulate_click(attribution.center(), Modifiers::none());

        assert_eq!(
            window.opened_url().as_deref(),
            Some(ATTRIBUTION_URL),
            "the credit opens the page the ODbL attribution points at",
        );
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
            ship_type: ShipType::Tanker,
            position: model::project(56.3, 26.5),
            cog: 900,
            heading: 87,
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
        assert!(projection.near_screen(at, 0.0));
        assert!(projection.near_screen([-4.0, 200.0], 8.0));
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

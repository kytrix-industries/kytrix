//! The canvas the Strait of Hormuz map is drawn on.
//!
//! One canvas holds the whole picture: the sea, the static coastline, the borders and
//! roads, the vessels, the place labels, the legend, the count, the credit and the
//! hover card. Those are all drawn by this module, because a canvas is the engine's
//! only self-drawing entry point and everything above it moves with the camera.
//!
//! # The engine's canvas is a value
//!
//! The map is a [`Canvas`] value the widget hands to
//! [`WidgetCx::set_canvas`](widget::cx::WidgetCx::set_canvas) when, and only when, what
//! it would draw moved. The inputs are named in one key ([`PaintedKey`]): the box the
//! node was laid out in, the camera, the picture's revision, the pointer's vessel, the
//! filter, the colours the theme resolved, the language the words are read in and the
//! raster scale the labels were shaped at. A steady frame therefore replaces nothing and
//! asks for nothing (I3), and a canvas built for another appearance or another language
//! is the wrong value rather than a value to keep.
//!
//! **The atlas lease is part of the key too.** A canvas names glyph runs leased from the
//! shared atlas; an atlas generation change or a per-tile eviction leaves those runs
//! pointing at reused texture, and the engine drops them at encode time. The shaped
//! lines the canvas was built from are kept beside it, and a dead lease is a frame that
//! rebuilds — otherwise the labels would stay missing for good.
//!
//! # The land is static geometry
//!
//! The coastline is the one part of the picture that does not change between frames: the
//! four tiers were tessellated once when the asset was parsed and bucketed into the
//! culling grid, so each tier is registered in the engine's geometry store **once**, in
//! Mercator coordinates, and every frame draws the visible cells with a per-draw affine
//! transform ([`GeometryDraw`]). A pan or a zoom changes a uniform, not a vertex: the
//! per-frame geometry work is the borders, the roads and the vessels, whose stroke widths
//! and marker sizes are in screen pixels and therefore cannot ride a transform.
//!
//! # The pointer is followed one reading behind, on purpose
//!
//! A mouse crossing the map produces one move event per pixel, so a move only records the
//! vessel and the camera it resolved to and the frame that follows draws from it: a
//! pointer that moved inside one vessel is the same drawing, and a movement that does not
//! change the hit is not a frame.
//!
//! # What a frame is clipped to
//!
//! The canvas draws in absolute window coordinates and the node's box is the map: the
//! frame is clipped to that box, so a marker at the edge is cut off at the pane's edge
//! rather than painted over a sibling.

use std::sync::Arc;

use ahash::AHashMap;
use base_ui::{kit::size::UnifiedSize, style_builder::StyleBuilder};
use bytes::Bytes;
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    math::{Point, Rect, Transform},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        canvas::Canvas,
        color::Color,
        geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
        primitive::{Border, ClipRect, GeometryDraw, Quad, QuadPaint},
    },
    text::{ShapedTextHandle, TextSlant, TextStyle},
};
use widget::cx::WidgetCx;

use crate::{
    colors::MapColors,
    model::{
        self, Camera, LabelKind, LabelSize, MAX_ZOOM, MIN_ZOOM, PlacedLabel, Projection, Vessel,
        VesselDetails,
    },
    paint::{self, Mesh},
    scene::Scene,
    ship::{SHIP_TYPES, ShipType, ShipTypeFilter},
};

/// The selector the canvas node carries, so the pane's own tests can find it.
pub(super) const MAP_SELECTOR: &str = "TEST_CHOKEPOINT_MAP";

/// How much one wheel notch scales the camera.
///
/// It is a ratio rather than a step so a notch in and a notch out are the same gesture:
/// a fixed step zooms out faster than it zooms in as soon as the camera is close.
const WHEEL_ZOOM_STEP: f32 = 1.15;

/// The size a country's name is drawn at, in pixels.
///
/// Half again the `text_sm` step the cities used to share with it: at the fitted zoom a
/// country is a region rather than a place, and the extra size is what says so. The theme
/// has control-size steps but no body/caption tokens yet (AGENTS §15.4); the map's own
/// three-step type scale is registered as a W6 difference rather than borrowing a control
/// step that would resize with every kit change.
const COUNTRY_LABEL_TEXT_SIZE: f32 = 21.0;

/// The size a city's name is drawn at, in pixels.
///
/// A step over the water's `text_xs` and well under the country's, so the three kinds of
/// word read in the order a paper atlas prints them in.
const CITY_LABEL_TEXT_SIZE: f32 = 13.0;

/// The size a body of water's name is drawn at.
///
/// The kit's small control step, which is the old `.text_xs()` at the application's
/// sixteen-pixel rem.
fn water_label_text_size() -> f32 {
    UnifiedSize::Small.text_size()
}

/// Where a country's or a city's halo is drawn, in pixels off the name's own box.
///
/// The engine has no text stroke: `TextStyle` carries no stroke and no shadow, and a
/// `Canvas` draws glyph runs rather than paths. The outline a chart gives a name that sits
/// on the picture is therefore the name itself, drawn in the map's paper colour one pixel
/// off in each direction and painted under it. Eight offsets rather than four: a cross
/// leaves the diagonals of a thin glyph uncovered at the city's 13 px step, and the cost is
/// a shaped line the cache already holds.
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

/// The width of the hover card, in pixels.
///
/// The widest label the card draws sets it: the two update times are labelled with a
/// sentence rather than the one or two words the other rows use, and each draws its stamp
/// on a line of its own, so the label is what has to fit.
const CARD_WIDTH: f32 = 228.0;

/// The gap the hover card keeps off the hull it describes and off the pane's edges.
const CARD_MARGIN: f32 = 12.0;

/// The inset of the credit from the pane's bottom-left corner.
const ATTRIBUTION_INSET: f32 = 8.0;

/// The line height a card's rows are laid out at, as a multiple of the text size.
///
/// The theme's paragraph rhythm (1.618); a card is a block of sentences and needs the
/// same rhythm the old element tree drew it with.
const CARD_LINE_RHYTHM: f32 = 1.618;

/// The gap the count card keeps above the legend, in pixels, and the height the credit's
/// line takes under both.
const CORNER_GAP: f32 = 4.0;

/// The ODbL credit the map is drawn under.
///
/// It is the widget's own attribution rather than a framework one: the base map is
/// OpenStreetMap data and ODbL requires the credit wherever the map is drawn.
const ATTRIBUTION_KEY: &str = "widget.global_shipping_chokepoint.attribution";

/// The page the map's ODbL credit opens.
///
/// OSMF's guidance asks for "OpenStreetMap" to be reachable from the credit itself, so the
/// label is a link to the copyright page rather than plain text.
pub(super) const ATTRIBUTION_URL: &str = "https://www.openstreetmap.org/copyright";

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

/// How many shaped lines the map keeps.
///
/// The labels of one frame plus the cards: at most 38 labels twice (name and halo), the
/// legend's twenty names and its title, the count and the credit, and the sixteen rows of
/// a hover card (two of them twice). A map that changes language fills it with the other
/// language's set, and the least recently used one is dropped.
const TEXT_CACHE_CAPACITY: usize = 256;

/// What the map draws, as the pane hands it over.
///
/// The revision moves whenever the pane replaced the picture, the filter or the derived
/// present-type list, which is what lets the canvas key tell a new picture from a steady
/// one without comparing every vessel.
pub(super) struct MapPicture<'a> {
    /// The vessels the filter is drawing, in the pane's own order.
    pub vessels: &'a [Vessel],
    /// The types the picture holds, in [`ShipType`] order, for the legend.
    pub present_types: &'a [ShipType],
    /// What the map is restricted to.
    pub filter: ShipTypeFilter,
    /// Moves whenever any of the above changed.
    pub revision: u64,
    /// The display zone the hover card's stamps were formatted in, in whole seconds.
    ///
    /// It is part of what the canvas is a function of: the card's two stamps are formatted
    /// when the picture is read, so a zone the settings moved is a different card over the
    /// same vessel.
    pub zone: i32,
    /// The vessel under the pointer, formatted for the zone showing now.
    pub hover: Option<VesselDetails>,
}

/// The inputs one canvas was built from.
///
/// Everything a frame reads has to be in here, or a change to it would keep serving the
/// canvas from before the change: the box, the camera, the picture, the pointer, the
/// colours (the appearance), the language the words are read in, and the scale the labels
/// were shaped at.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PaintedKey {
    bounds: Rect,
    camera: Camera,
    revision: u64,
    hovered: Option<u32>,
    filter: ShipTypeFilter,
    colors: MapColors,
    language: LanguageCode,
    text_scale: f32,
    /// The display zone the hover card's stamps were formatted in, in whole seconds.
    zone: i32,
    /// The overlay's own placement inputs, which follow the canvas box and cannot be
    /// recomputed from it alone: the pane is the same size but a legend that changed its
    /// entries is a different drawing.
    present_types: usize,
}

/// One shaped line and the style it was shaped in.
struct CachedText {
    text: SmolStr,
    style: TextStyle,
    handle: Arc<ShapedTextHandle>,
}

/// The shaped lines a map has drawn, keyed by text and style.
///
/// A cache of the labels of one language plus the cards is enough for the two adjacent
/// frames a language switch produces, and it is what makes a hover free: a hover changes
/// no label at all.
struct MapTextCache {
    entries: Vec<CachedText>,
}

impl MapTextCache {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(TEXT_CACHE_CAPACITY),
        }
    }

    /// Shapes `text` in `style`, or returns the line that was shaped before.
    ///
    /// A handle whose atlas lease is gone is dropped and shaped again: the glyphs it named
    /// have been reused, so drawing it would be drawing another line's pixels.
    fn line(&mut self, app: &mut App, text: &str, style: &TextStyle) -> Arc<ShapedTextHandle> {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.text == text && entry.style == *style)
        {
            let entry = self.entries.remove(index);
            if !entry.handle.is_stale() {
                let handle = Arc::clone(&entry.handle);
                self.entries.push(entry);
                return handle;
            }
        }
        let handle = Arc::new(app.shape_text(text, style, None, &[]).unwrap_or_else(|_| {
            // Text that cannot be shaped at all is drawn as nothing rather than failing
            // the frame: a label is not worth a dropped frame, and the next frame tries
            // again.
            app.shape_text("", style, None, &[])
                .expect("an empty line always shapes")
        }));
        if self.entries.len() == TEXT_CACHE_CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push(CachedText {
            text: SmolStr::new(text),
            style: style.clone(),
            handle: Arc::clone(&handle),
        });
        handle
    }

    /// The width `text` occupies, shaped in `style`.
    fn width(&mut self, app: &mut App, text: &str, style: &TextStyle) -> f32 {
        self.line(app, text, style).size().width
    }

    /// The line box height one line of `style` takes.
    fn height(&mut self, app: &mut App, style: &TextStyle) -> f32 {
        self.line(app, "X", style).size().height
    }

    /// Whether any line this cache holds can no longer be drawn from the atlas, dropping
    /// the ones that cannot.
    ///
    /// This is what the map asks before it decides a frame changed nothing: the engine
    /// drops stale runs at encode time, so a canvas kept over a dead lease would lose its
    /// labels permanently.
    ///
    /// The stale entries are dropped on the way out. A line the frame that follows does
    /// not draw against — the other language's labels after a switch, a card whose
    /// pointer has moved away, a legend entry the picture no longer holds — would
    /// otherwise keep this answer true for good, which turns every later frame into a
    /// rebuild and breaks the steady state (I3): the canvas, and every mesh it registers,
    /// would be rebuilt for a lease that nothing on screen names any more. The canvas
    /// that was built from them is rebuilt by the frame that asks this question, so
    /// nothing is left naming glyphs that are gone.
    fn has_stale(&mut self) -> bool {
        let stale = self.entries.iter().any(|entry| entry.handle.is_stale());
        if stale {
            self.entries.retain(|entry| !entry.handle.is_stale());
        }
        stale
    }
}

/// One land tier's registered geometry, and the ranges a frame draws from it.
struct TierGeometry {
    id: GeometryId,
    /// The tier's triangles in Mercator coordinates, in the asset's cell order.
    mesh: paint::LandMesh,
}

/// The map's canvas node, the camera it draws with, and the canvas it was built from.
pub(super) struct MapCanvas {
    node: Option<NodeId>,
    /// The box the node was last laid out in, in window coordinates.
    bounds: Option<Rect>,
    camera: Camera,
    /// The land tiers registered so far, in [`model::LAND_TIERS`] order.
    tiers: [Option<TierGeometry>; model::LAND_TIERS],
    /// The geometry the canvas on screen names, released when it is replaced.
    dynamic: Vec<GeometryId>,
    painted: Option<PaintedKey>,
    text: MapTextCache,
    /// The vessel the pointer is on, if any.
    hovered: Option<u32>,
    /// Where the credit was drawn on the frame on screen, for its click.
    attribution: Option<Rect>,
    /// The last pointer position, which is what a drag's increment is read from.
    pointer: Option<Point>,
    /// Whether the pointer went down on the map and has not come up since.
    dragging: bool,
}

impl MapCanvas {
    /// An empty canvas, with no node and the fitted camera.
    pub(super) fn new() -> Self {
        Self {
            node: None,
            bounds: None,
            camera: Camera::default(),
            tiers: std::array::from_fn(|_| None),
            dynamic: Vec::new(),
            painted: None,
            text: MapTextCache::new(),
            hovered: None,
            attribution: None,
            pointer: None,
            dragging: false,
        }
    }

    /// The canvas node, once the pane's body built it.
    #[cfg(test)]
    pub(super) fn node(&self) -> Option<NodeId> {
        self.node
    }

    /// The box the map was last laid out in, if it has been laid out yet.
    #[cfg(test)]
    pub(super) fn bounds(&self) -> Option<Rect> {
        self.bounds
    }

    /// The camera the frame draws with.
    #[cfg(test)]
    pub(super) fn camera(&self) -> Camera {
        self.camera
    }

    /// The vessel the last frame drew as hovered.
    pub(super) fn hovered(&self) -> Option<u32> {
        self.hovered
    }

    /// Builds the canvas node into `parent` and remembers it.
    ///
    /// The node starts empty (`Rect::default()`): it has no box until the first layout,
    /// and [`Self::sync`] is what fills it in. It registers two dependencies on purpose:
    /// the appearance (a canvas bakes its colours in, so an appearance switch has to
    /// schedule the frame that rebuilds it) and the language (the canvas' accessible name
    /// and every word on it are resolved in the language showing now).
    pub(super) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let label = t_smol!(translate, "widget.names.global_shipping_chokepoint");
        let node = NodeBuilder::canvas(Canvas::new(Rect::default()))
            .id(MAP_SELECTOR)
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .tab_stop(false)
            .a11y(A11yOverride {
                role: Some(A11yRole::Group),
                label: Some(label),
                ..A11yOverride::default()
            })
            .bind_a11y(move |read| {
                let _language = read.read(&language);
                A11yPatch {
                    label: Some(t_smol!(
                        translate,
                        "widget.names.global_shipping_chokepoint"
                    )),
                    ..A11yPatch::default()
                }
            })
            .bind_style(move |read| {
                let _appearance = theme.appearance(read);
                ui_core::paint::style::StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        self.node = Some(node);
        Ok(())
    }

    /// Rebuilds the canvas the map is drawn on, reporting whether it changed.
    ///
    /// The work is split in two: the *geometry* moves into the engine's store when the
    /// land tier or the picture moved, and the *canvas* is rebuilt only when what it would
    /// draw moved — the box, the camera, the picture, the pointer, the colours, the
    /// language, the raster scale, or the atlas lease of a line it was built from.
    pub(super) fn sync(
        &mut self,
        cx: &mut WidgetCx<'_>,
        picture: &MapPicture<'_>,
        scene: Option<&'static Scene>,
        translate: &Translate,
    ) -> Result<bool, EngineError> {
        let Some(node) = self.node else {
            return Ok(false);
        };
        let Ok(target) = cx.tree().get(node) else {
            return Ok(false);
        };
        let bounds = target.layout().bounds;
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            // The frame that laid the node out has not run yet; the next one will.
            return Ok(false);
        }
        self.bounds = Some(bounds);
        // The camera is kept on the map for the box it is drawn in: a pane that was
        // resized can leave the old centre outside the new cover fit.
        self.camera = self.camera.clamped([bounds.size.width, bounds.size.height]);

        // The card names a vessel the picture may no longer hold; dropping it here rather
        // than in the draw keeps a stale hit from surviving the update that removed it.
        if self
            .hovered
            .is_some_and(|mmsi| !picture.vessels.iter().any(|vessel| vessel.mmsi == mmsi))
        {
            self.hovered = None;
        }

        let appearance = cx.theme().current(cx.app());
        let colors = MapColors::resolve(cx.theme().theme_for(appearance));
        let language = *cx.read(&cx.language());
        let key = PaintedKey {
            bounds,
            camera: self.camera,
            revision: picture.revision,
            hovered: self.hovered,
            filter: picture.filter,
            colors,
            language,
            text_scale: cx.app().text_scale(),
            zone: picture.zone,
            present_types: picture.present_types.len(),
        };
        if self.painted == Some(key) && !self.text.has_stale() {
            return Ok(false);
        }

        let (canvas, changed, dynamic) =
            self.paint(cx.app_mut(), picture, scene, translate, &key)?;
        let changed = cx.set_canvas(node, canvas)? && changed;
        if changed {
            for id in std::mem::take(&mut self.dynamic) {
                cx.app_mut().geometry_mut().release(id);
            }
            self.dynamic = dynamic;
            self.painted = Some(key);
        } else {
            // The canvas the node keeps is the one on screen: the geometry it named is
            // still the geometry it names, and the one just registered has no reader.
            for id in dynamic {
                cx.app_mut().geometry_mut().release(id);
            }
        }
        Ok(changed)
    }

    /// Registers the palette of one frame and builds its canvas.
    fn paint(
        &mut self,
        app: &mut App,
        picture: &MapPicture<'_>,
        scene: Option<&'static Scene>,
        translate: &Translate,
        key: &PaintedKey,
    ) -> Result<(Canvas, bool, Vec<GeometryId>), EngineError> {
        let bounds = key.bounds;
        let viewport = [bounds.size.width, bounds.size.height];
        let projection = Projection::new(self.camera, viewport);
        let origin = [bounds.origin.x, bounds.origin.y];
        let colors = key.colors;
        let mut dynamic = Vec::with_capacity(4 + SHIP_TYPES.len() + 1);
        let mut canvas = Canvas::new(bounds);

        // The sea is the map's own fill rather than the node's background: a canvas is
        // opaque where it paints, and the pane body behind it is not the sea.
        canvas.push_quad(Quad {
            bounds,
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: colors.ocean,
                border: None,
            },
        });
        let _ = canvas.push_clip(ClipRect {
            rect: bounds,
            corner_radius: [0.0; 4],
        });

        if projection.drawable()
            && let Some(scene) = scene
        {
            let tier = model::land_tier(projection.metres_per_pixel());
            self.ensure_tier(app, scene, tier);
            if let Some(land) = self.tiers[tier].as_ref() {
                let visible = projection.visible_mercator();
                let cell_span = model::visible_cells(visible).grown();
                for range in paint::visible_land_ranges(
                    &land.mesh,
                    scene.land(tier_metres_per_pixel(tier)),
                    cell_span,
                    visible,
                ) {
                    canvas.push_geometry(GeometryDraw {
                        id: land.id,
                        transform: land_transform(projection, bounds),
                        color: colors.land,
                        range,
                    });
                }
            }

            // The per-frame layers: borders, road classes and vessels, all in screen
            // pixels because their stroke widths do not scale with the camera.
            for (mesh, color) in paint::in_paint_order(
                paint::frame_meshes(Some(scene), projection, origin, picture.vessels),
                colors,
            ) {
                if let Some(id) = register_mesh(app, &mesh) {
                    canvas.push_geometry(GeometryDraw::full(id, Transform::IDENTITY, color));
                    dynamic.push(id);
                }
            }

            // The ring around the vessel under the pointer is part of the map rather than
            // of the labels: it names a hull, and the labels are placed over it.
            if let Some(mmsi) = self.hovered
                && let Some(vessel) = picture.vessels.iter().find(|vessel| vessel.mmsi == mmsi)
                && let Some(id) = register_mesh(
                    app,
                    &paint::ring_mesh(projection.screen(vessel.position), origin),
                )
            {
                canvas.push_geometry(GeometryDraw::full(id, Transform::IDENTITY, colors.hover));
                dynamic.push(id);
            }

            if projection.drawable() {
                self.paint_labels(app, &mut canvas, projection, colors, translate);
            }
            self.paint_corner(app, &mut canvas, bounds, picture, colors, translate);
            if projection.drawable() {
                self.paint_hover_card(
                    app,
                    &mut canvas,
                    projection,
                    bounds,
                    picture,
                    colors,
                    translate,
                );
            }
        }

        let _ = canvas.pop_clip();
        Ok((canvas, true, dynamic))
    }

    /// The land tier's geometry, registered the first time a frame draws it.
    fn ensure_tier(&mut self, app: &mut App, scene: &'static Scene, tier: usize) {
        if self.tiers[tier].is_some() {
            return;
        }
        let layer = scene.land(tier_metres_per_pixel(tier));
        let mesh = paint::land_mesh(layer);
        let id = register_mesh(app, &mesh.mesh);
        if let Some(id) = id {
            self.tiers[tier] = Some(TierGeometry { id, mesh });
        }
    }

    /// Releases every geometry this canvas registered and takes its node down.
    ///
    /// The node is the canvas' own: the pane's body is built once, so a canvas that goes
    /// away takes its node with it. A node the pane already removed is not a failure — the
    /// framework tears the body down after this hook.
    pub(super) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        for id in std::mem::take(&mut self.dynamic) {
            cx.app_mut().geometry_mut().release(id);
        }
        for tier in &mut self.tiers {
            if let Some(tier) = tier.take() {
                cx.app_mut().geometry_mut().release(tier.id);
            }
        }
        if let Some(node) = self.node.take()
            && cx.tree().get(node).is_ok()
        {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, node);
        }
        self.painted = None;
        self.bounds = None;
        self.hovered = None;
        self.attribution = None;
        self.pointer = None;
        self.dragging = false;
    }

    /// Draws every place name the placement kept, with the halo a country or a city
    /// carries.
    ///
    /// A name is centred on its anchor: the placement measured the text and kept the box
    /// inside the pane, and what a frame does here is put the line's own top-left where
    /// that box says. A sea's name takes no halo — it is drawn over open water, where
    /// nothing is painted under it.
    fn paint_labels(
        &mut self,
        app: &mut App,
        canvas: &mut Canvas,
        projection: Projection,
        colors: MapColors,
        translate: &Translate,
    ) {
        let labels = placed_labels(app, &mut self.text, projection, translate, colors);
        for label in labels {
            let text = label_text(translate, label.key, label.kind);
            let style = label_style(label.kind, colors);
            let width = self.text.width(app, &text, &style);
            let origin = Point::new(label.at[0] - width / 2.0, label.at[1] - style.size / 2.0);
            if matches!(label.kind, LabelKind::Country | LabelKind::City) {
                let halo = halo_style(label.kind, colors);
                for [dx, dy] in LABEL_HALO_OFFSETS {
                    push_line(
                        app,
                        &mut self.text,
                        canvas,
                        &text,
                        Point::new(origin.x + dx, origin.y + dy),
                        &halo,
                    );
                }
            }
            push_line(app, &mut self.text, canvas, &text, origin, &style);
        }
    }

    /// Draws the two corner blocks and the credit.
    ///
    /// The count card sits over the legend, and the legend is lifted clear of the credit's
    /// line rather than sharing the bottom edge with it: a pane narrower than the two
    /// together would otherwise paint the cards over the credit, which is the one thing on
    /// the map that has to stay readable.
    fn paint_corner(
        &mut self,
        app: &mut App,
        canvas: &mut Canvas,
        bounds: Rect,
        picture: &MapPicture<'_>,
        colors: MapColors,
        translate: &Translate,
    ) {
        let size = UnifiedSize::Small;
        let pad = size.px();
        let gap = size.gap();
        let dot = 8.0;
        let dot_gap = 6.0;
        let column_gap = 12.0;
        let ink = small_style(colors.text);
        let muted = small_style(colors.muted);
        let line = self.text.height(app, &ink);

        // The legend: the types the picture holds, in two columns, each with its dot. A
        // type the filter has been narrowed away from goes to the disabled step, dot and
        // name alike, so the entry still says the picture holds it.
        let title = t_smol!(translate, "widget.global_shipping_chokepoint.legend.title");
        let entries = picture
            .present_types
            .iter()
            .map(|ship_type| {
                let name = t_smol!(translate, ship_type.label_key());
                let (dot_ink, name_ink) = if picture.filter.matches(*ship_type) {
                    (colors.ships[ship_type.index()], colors.text)
                } else {
                    (colors.disabled, colors.disabled)
                };
                (name, dot_ink, name_ink)
            })
            .collect::<Vec<_>>();
        let columns = 2usize;
        let mut column_width: f32 = 0.0;
        for (name, _, _) in &entries {
            column_width = column_width.max(dot + dot_gap + self.text.width(app, name, &ink));
        }
        let rows = entries.len().div_ceil(columns);
        let title_width = self.text.width(app, &title, &muted);
        let grid_width = columns as f32 * column_width + (columns as f32 - 1.0) * column_gap;
        let card_width = pad * 2.0 + title_width.max(grid_width);
        let card_height = pad * 2.0 + line + gap + rows as f32 * (line + gap);
        let legend = Rect::new(
            bounds.origin.x + bounds.size.width - ATTRIBUTION_INSET - card_width,
            bounds.origin.y + bounds.size.height - 32.0 - card_height,
            card_width,
            card_height,
        );
        push_panel(canvas, legend, colors);
        push_line(
            app,
            &mut self.text,
            canvas,
            &title,
            Point::new(legend.origin.x + pad, legend.origin.y + pad),
            &muted,
        );
        for (index, (name, dot_ink, name_ink)) in entries.iter().enumerate() {
            let column = index % columns;
            let row = index / columns;
            let x = legend.origin.x + pad + column as f32 * (column_width + column_gap);
            let y = legend.origin.y + pad + line + gap + row as f32 * (line + gap);
            canvas.push_quad(Quad {
                bounds: Rect::new(x, y + (line - dot) / 2.0, dot, dot),
                corner_radius: [2.0; 4],
                paint: QuadPaint::Solid {
                    fill: *dot_ink,
                    border: None,
                },
            });
            let style = small_style(*name_ink);
            push_line(
                app,
                &mut self.text,
                canvas,
                name,
                Point::new(x + dot + dot_gap, y),
                &style,
            );
        }

        // The count card: the label at its left edge, the number at its right. The number
        // is bare rather than a sentence — "3 vessels" and "1 vessel" need a plural rule
        // the catalogue has none of, and the label beside it says what is counted.
        let count_height = pad * 2.0 + line;
        let count = Rect::new(
            legend.origin.x,
            legend.origin.y - CORNER_GAP - count_height,
            card_width,
            count_height,
        );
        push_panel(canvas, count, colors);
        let label = t_smol!(
            translate,
            "widget.global_shipping_chokepoint.vessel_count_label"
        );
        push_line(
            app,
            &mut self.text,
            canvas,
            &label,
            Point::new(count.origin.x + pad, count.origin.y + pad),
            &muted,
        );
        let number = format_smolstr!("{}", picture.vessels.len());
        let number_width = self.text.width(app, &number, &ink);
        push_line(
            app,
            &mut self.text,
            canvas,
            &number,
            Point::new(
                count.origin.x + count.size.width - pad - number_width,
                count.origin.y + pad,
            ),
            &ink,
        );

        // The ODbL credit, in the corner the cards leave free; its box is kept so a click
        // on it can open the page it names.
        let credit = t_smol!(translate, ATTRIBUTION_KEY);
        let credit_width = self.text.width(app, &credit, &ink);
        let credit_height = self.text.height(app, &ink);
        let credit_rect = Rect::new(
            bounds.origin.x + ATTRIBUTION_INSET,
            bounds.origin.y + bounds.size.height - 6.0 - credit_height,
            credit_width,
            credit_height,
        );
        push_line(
            app,
            &mut self.text,
            canvas,
            &credit,
            credit_origin(credit_rect),
            &ink,
        );
        self.attribution = Some(credit_rect);
    }

    /// Draws the card of the vessel under the pointer, kept inside the pane and off its
    /// hull.
    ///
    /// A card to the right of the hull would leave the pane, so it goes to its left instead;
    /// the same for one that would leave through the bottom edge.
    fn paint_hover_card(
        &mut self,
        app: &mut App,
        canvas: &mut Canvas,
        projection: Projection,
        bounds: Rect,
        picture: &MapPicture<'_>,
        colors: MapColors,
        translate: &Translate,
    ) {
        let Some(details) = picture.hover.as_ref() else {
            return;
        };
        let Some(mmsi) = self.hovered else {
            return;
        };
        let Some(vessel) = picture.vessels.iter().find(|vessel| vessel.mmsi == mmsi) else {
            return;
        };
        let size = UnifiedSize::Small;
        let pad = size.px();
        let gap = size.gap();
        let ink = small_style(colors.text);
        let muted = small_style(colors.muted);
        let line = self.text.height(app, &ink);
        let viewport = [bounds.size.width, bounds.size.height];
        let hull = projection.screen(vessel.position);
        let height = pad * 2.0 + 18.0 * line + 15.0 * gap;
        let left = if hull[0] + CARD_MARGIN + CARD_WIDTH <= viewport[0] {
            hull[0] + CARD_MARGIN
        } else {
            (hull[0] - CARD_MARGIN - CARD_WIDTH).max(0.0)
        };
        let top =
            (hull[1] - height / 2.0).clamp(0.0, (viewport[1] - height - CARD_MARGIN).max(0.0));
        let card = Rect::new(
            bounds.origin.x + left,
            bounds.origin.y + top,
            CARD_WIDTH,
            height,
        );
        push_panel(canvas, card, colors);
        let values = hover_values(details, translate);
        let value_left = card.origin.x + pad;
        let value_right = card.origin.x + card.size.width - pad;
        let mut y = card.origin.y + pad;
        for (key, value) in HOVER_KEYS.iter().zip(values.iter()) {
            let label = t_smol!(translate, *key);
            push_line(
                app,
                &mut self.text,
                canvas,
                &label,
                Point::new(card.origin.x + pad, y),
                &muted,
            );
            let value_style = small_style(colors.text);
            if HOVER_STACKED_KEYS.contains(key) {
                // The stamp takes the whole card's width under its label, which is what
                // keeps the seconds of it off the ellipsis.
                push_line(
                    app,
                    &mut self.text,
                    canvas,
                    value,
                    Point::new(card.origin.x + pad, y + line),
                    &value_style,
                );
                y += line * 2.0 + gap;
            } else {
                // A destination or a call sign is as long as the report says; it is cut at
                // the card's edge rather than allowed to widen the card off the map.
                let width = self.text.width(app, value, &value_style);
                let x = (value_right - width)
                    .max(value_left + width_of(&label, &muted, app, &mut self.text) + gap);
                let _ = canvas.push_clip(ClipRect {
                    rect: Rect::new(x, y, value_right - x, line),
                    corner_radius: [0.0; 4],
                });
                push_line(
                    app,
                    &mut self.text,
                    canvas,
                    value,
                    Point::new(x, y),
                    &value_style,
                );
                let _ = canvas.pop_clip();
                y += line + gap;
            }
        }
    }

    /// The projection the current camera lays over the current box, if the map has one.
    pub(super) fn projection(&self) -> Option<Projection> {
        let bounds = self.bounds?;
        let projection = Projection::new(self.camera, [bounds.size.width, bounds.size.height]);
        projection.drawable().then_some(projection)
    }

    /// The vessel nearest one window position, if one is close enough to it.
    ///
    /// The painted vessels are projected at the frame's camera, which is the geometry the
    /// pointer is actually over; a picture that arrived since then is not what the reader
    /// was pointing at, and the next move resolves against it.
    pub(super) fn vessel_at(&mut self, position: Point, vessels: &[Vessel]) -> Option<u32> {
        let bounds = self.bounds?;
        if !bounds.contains(position) {
            return None;
        }
        let projection = self.projection()?;
        let canvas_point = [position.x - bounds.origin.x, position.y - bounds.origin.y];
        let points = vessels
            .iter()
            .map(|vessel| (vessel.mmsi, projection.screen(vessel.position)))
            .collect::<Vec<_>>();
        model::hit_test(&points, canvas_point)
    }

    /// Resolves the vessel under one window position, reporting whether the card moved.
    pub(super) fn track_pointer(&mut self, position: Point, vessels: &[Vessel]) -> bool {
        let hit = self.vessel_at(position, vessels);
        if hit == self.hovered {
            return false;
        }
        self.hovered = hit;
        true
    }

    /// Records that the pointer left the map, reporting whether it had been on a vessel.
    pub(super) fn clear_hover(&mut self) -> bool {
        self.hovered.take().is_some()
    }

    /// The pointer went down on the map: from here until it comes up, a move pans.
    pub(super) fn pointer_down(&mut self, position: Point) {
        self.pointer = Some(position);
        self.dragging = true;
    }

    /// The pointer came up: a move resolves a vessel again.
    pub(super) fn pointer_up(&mut self) {
        self.dragging = false;
    }

    /// Applies one pointer move, reporting whether it changed what the frame draws.
    ///
    /// A held button pans the map and drops the card — it names a vessel the map is moving
    /// out from under. Anything else resolves the vessel under the pointer, and a move
    /// inside one vessel is not a movement.
    pub(super) fn pointer_move(&mut self, position: Point, vessels: &[Vessel]) -> bool {
        let previous = self.pointer.replace(position);
        if self.dragging {
            self.hovered = None;
            let Some(previous) = previous else {
                return true;
            };
            return self.pan(Point::new(position.x - previous.x, position.y - previous.y));
        }
        self.track_pointer(position, vessels)
    }

    /// Moves the camera by one pointer increment, keeping it on the map.
    pub(super) fn pan(&mut self, delta: Point) -> bool {
        let Some(bounds) = self.bounds else {
            return false;
        };
        let viewport = [bounds.size.width, bounds.size.height];
        let scale = model::fit_px_per_merc(viewport) * self.camera.zoom;
        if !scale.is_finite() || scale <= 0.0 {
            return false;
        }
        self.set_camera(Camera {
            center: [
                self.camera.center[0] - delta.x / scale,
                self.camera.center[1] + delta.y / scale,
            ],
            zoom: self.camera.zoom,
        })
    }

    /// One wheel notch, as a zoom around the pointer.
    pub(super) fn zoom_by_wheel(&mut self, position: Point, step: f32) -> bool {
        if !step.is_finite() || step == 0.0 {
            return false;
        }
        // A wheel moved away from the reader is a wheel moved up — a positive delta — which
        // zooms in.
        let factor = if step > 0.0 {
            WHEEL_ZOOM_STEP
        } else {
            1.0 / WHEEL_ZOOM_STEP
        };
        self.zoom_at(position, factor)
    }

    /// One pinch, as a zoom around the gesture's centre.
    pub(super) fn zoom_by_pinch(&mut self, position: Point, delta: f32) -> bool {
        if !delta.is_finite() || delta == 0.0 {
            return false;
        }
        self.zoom_at(position, 1.0 + delta)
    }

    /// Scales the camera around one window position, keeping the point under it fixed.
    ///
    /// The camera moves under a stationary pointer, so which vessel it is on moves with it:
    /// the hit is resolved again against the new camera.
    pub(super) fn zoom_at(&mut self, position: Point, factor: f32) -> bool {
        let Some(bounds) = self.bounds else {
            return false;
        };
        if !bounds.contains(position) || !factor.is_finite() || factor <= 0.0 {
            return false;
        }
        let Some(projection) = self.projection() else {
            return false;
        };
        let canvas_point = [position.x - bounds.origin.x, position.y - bounds.origin.y];
        let anchor = projection.mercator(canvas_point);
        let zoom = (self.camera.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if zoom == self.camera.zoom {
            return false;
        }
        let scale = model::fit_px_per_merc(projection.viewport) * zoom;
        self.set_camera(Camera {
            center: [
                anchor[0] - (canvas_point[0] - projection.viewport[0] / 2.0) / scale,
                anchor[1] + (canvas_point[1] - projection.viewport[1] / 2.0) / scale,
            ],
            zoom,
        })
    }

    /// Keeps the camera on the map, reporting whether it moved.
    pub(super) fn set_camera(&mut self, camera: Camera) -> bool {
        let Some(bounds) = self.bounds else {
            return false;
        };
        let clamped = camera.clamped([bounds.size.width, bounds.size.height]);
        if clamped == self.camera {
            return false;
        }
        self.camera = clamped;
        true
    }

    /// Whether one window position is on the credit.
    pub(super) fn attribution_contains(&self, position: Point) -> bool {
        self.attribution.is_some_and(|rect| rect.contains(position))
    }

    /// Whether one window position is inside the box the map is drawn in.
    ///
    /// A widget's `dispatch` sees every event the pane hands it, not only the ones over
    /// its own canvas: the pointer and gesture events are gated on this so a press beside
    /// the map is not a press on it.
    pub(super) fn contains(&self, position: Point) -> bool {
        self.bounds.is_some_and(|bounds| bounds.contains(position))
    }

    /// Whether a press this map began is still held.
    ///
    /// A release or a cancel belongs to the map while this is true even when it arrives
    /// off the box: the drag a press began follows the pointer, not the canvas.
    pub(super) fn is_dragging(&self) -> bool {
        self.dragging
    }
}

/// The metres per pixel one land tier was chosen for.
///
/// The tier ladder is a property of the asset's simplification, not of the pane: the
/// registered mesh is the same one [`model::land_tier`] picks for a projection, so the
/// registration asks the asset for the tier by its own threshold band.
fn tier_metres_per_pixel(tier: usize) -> f32 {
    match tier {
        0 => 1.0,
        1 => model::LAND_TIER_FINE_METRES_PER_PIXEL,
        2 => model::LAND_TIER_MIDDLE_METRES_PER_PIXEL,
        _ => model::LAND_TIER_COARSE_METRES_PER_PIXEL,
    }
}

/// The affine transform a land tier is drawn with: Mercator coordinates to the frame.
///
/// The projection's own `screen` is the same map, and this is that map as the per-draw
/// uniform the geometry channel takes: `x` scales and shifts with the canvas box, and
/// `y` scales **negatively** because Mercator grows north while the canvas grows down.
fn land_transform(projection: Projection, bounds: Rect) -> Transform {
    let zero = projection.screen([0.0, 0.0]);
    let scale_x = projection.screen([1.0, 0.0])[0] - zero[0];
    let scale_y = projection.screen([0.0, 1.0])[1] - zero[1];
    Transform::scale(scale_x, scale_y).then(Transform::translate(
        bounds.origin.x + zero[0],
        bounds.origin.y + zero[1],
    ))
}

/// Registers one mesh in the engine's geometry store, or answers that there is nothing to
/// register.
///
/// Every vertex is marked as interior (a negative coverage distance, which the engine's
/// `Ui` pipeline reads as full coverage): these meshes are filled triangles rather than
/// anti-aliased outlines, and the engine's analysis wants one scalar distance per vertex.
fn register_mesh(app: &mut App, mesh: &Mesh) -> Option<GeometryId> {
    if mesh.is_empty() {
        return None;
    }
    let mut vertices = Vec::with_capacity(mesh.vertices.len());
    let mut vertex_bytes = Vec::with_capacity(mesh.vertices.len() * 12);
    for vertex in &mesh.vertices {
        vertices.push([vertex[0], vertex[1]]);
        vertex_bytes.extend_from_slice(&vertex[0].to_le_bytes());
        vertex_bytes.extend_from_slice(&vertex[1].to_le_bytes());
        vertex_bytes.extend_from_slice(&(-1.0f32).to_le_bytes());
    }
    let index_bytes = mesh
        .indices
        .iter()
        .flat_map(|index| index.to_le_bytes())
        .collect::<Vec<u8>>();
    let desc = GeometryDesc {
        vertex_bytes: Bytes::from(vertex_bytes),
        index_bytes: Bytes::from(index_bytes),
        vertex_stride: GeometryPipeline::Ui.vertex_stride(),
        index_type: IndexType::U32,
        topology: Topology::TriangleList,
        bounds: mesh.bounds,
        pipeline: GeometryPipeline::Ui,
    };
    match app.geometry_mut().insert(desc) {
        Ok(id) => Some(id),
        Err(error) => {
            log::warn!("the Strait of Hormuz map could not register a mesh: {error}");
            None
        }
    }
}

/// The style one label kind is shaped in.
fn label_style(kind: LabelKind, colors: MapColors) -> TextStyle {
    let size = match kind {
        LabelKind::Country => COUNTRY_LABEL_TEXT_SIZE,
        LabelKind::City => CITY_LABEL_TEXT_SIZE,
        LabelKind::Water => water_label_text_size(),
    };
    let mut style = TextStyle::new(size);
    style.weight = match kind {
        LabelKind::Country => 700,
        LabelKind::City | LabelKind::Water => 400,
    };
    style.slant = match kind {
        LabelKind::Country | LabelKind::Water => TextSlant::Italic,
        LabelKind::City => TextSlant::Normal,
    };
    style.color = match kind {
        LabelKind::Country | LabelKind::City => colors.text,
        LabelKind::Water => colors.muted,
    };
    style
}

/// The style a label's halo is shaped in: the same line, in the map's paper colour.
fn halo_style(kind: LabelKind, colors: MapColors) -> TextStyle {
    let mut style = label_style(kind, colors);
    style.color = colors.halo;
    style
}

/// The text one label reads in the language being read.
///
/// A country is upper case — an atlas prints `IRAN`, not `Iran` — and that is part of the
/// label, so the measurement and the drawing are given the same string.
fn label_text(translate: &Translate, key: &str, kind: LabelKind) -> SmolStr {
    match kind {
        LabelKind::Country => SmolStr::new(t_smol!(translate, key).to_uppercase()),
        LabelKind::Water | LabelKind::City => t_smol!(translate, key),
    }
}

/// All the label anchors of one camera, with the sizes this frame aims to draw them at.
///
/// Each name is shaped here, in the language being read, before the placement runs:
/// `迪拜` takes 26 px and `DUBAI` 38 px, and what must not overlap is the text.
fn placed_labels(
    app: &mut App,
    text: &mut MapTextCache,
    projection: Projection,
    translate: &Translate,
    colors: MapColors,
) -> Vec<PlacedLabel> {
    let mut sizes: AHashMap<(&'static str, u8), LabelSize> = AHashMap::new();
    for (kind, labels) in [
        (LabelKind::Country, &model::COUNTRY_LABELS[..]),
        (LabelKind::Water, &model::WATER_LABELS[..]),
        (LabelKind::City, &model::CITY_LABELS[..]),
        (LabelKind::City, &model::CITY_LABELS_LATE[..]),
    ] {
        for label in labels {
            let text_value = label_text(translate, label.key, kind);
            let style = label_style(kind, colors);
            let width = text.width(app, &text_value, &style);
            sizes.insert(
                (label.key, label_kind_index(kind)),
                LabelSize {
                    width,
                    height: style.size,
                },
            );
        }
    }
    // The late cities are only placed from their own zoom: the placement asks for them
    // through the list its projection gates, so measuring them here is not wasted.
    model::place_labels(projection, |label, kind| {
        sizes
            .get(&(label.key, label_kind_index(kind)))
            .copied()
            .unwrap_or(LabelSize {
                width: 0.0,
                height: 0.0,
            })
    })
}

/// The discriminant of a label kind, for the map of measured sizes.
fn label_kind_index(kind: LabelKind) -> u8 {
    match kind {
        LabelKind::Country => 0,
        LabelKind::Water => 1,
        LabelKind::City => 2,
    }
}

/// The origin a line of card text is drawn at inside a box it fills.
fn credit_origin(rect: Rect) -> Point {
    Point::new(rect.origin.x, rect.origin.y)
}

/// The width `text` takes in `style`, through the cache the drawing uses.
fn width_of(text: &str, style: &TextStyle, app: &mut App, cache: &mut MapTextCache) -> f32 {
    cache.width(app, text, style)
}

/// The style a card's rows are shaped in: the kit's small step and the paragraph rhythm.
fn small_style(color: Color) -> TextStyle {
    let mut style = TextStyle::new(UnifiedSize::Small.text_size());
    style.color = color;
    style.line_height = Some(CARD_LINE_RHYTHM);
    style
}

/// Pushes one solid panel with the cards' shared edge and corner.
fn push_panel(canvas: &mut Canvas, bounds: Rect, colors: MapColors) {
    let radius = 4.0f32
        .min(bounds.size.width / 2.0)
        .min(bounds.size.height / 2.0);
    canvas.push_quad(Quad {
        bounds,
        corner_radius: [radius; 4],
        paint: QuadPaint::Solid {
            fill: colors.overlay,
            border: Some(Border {
                width: 1.0,
                color: colors.border,
            }),
        },
    });
}

/// Pushes one shaped line at `origin`, reporting whether it reached the canvas.
///
/// A refused line is one whose atlas lease died between being shaped and being pushed: its
/// glyphs have been reused, so the run is left out rather than drawn from stale texture
/// and the frame is not one the map can keep.
fn push_line(
    app: &mut App,
    cache: &mut MapTextCache,
    canvas: &mut Canvas,
    text: &str,
    origin: Point,
    style: &TextStyle,
) -> bool {
    if text.is_empty() {
        return true;
    }
    let line = cache.line(app, text, style);
    canvas.push_shaped_text(&line, origin)
}

/// The sixteen values the hover card draws, resolved in the language showing now.
///
/// A missing value or a sentinel is named rather than printed: the card says the field is
/// unavailable instead of leaving a blank a reader would take for a zero.
fn hover_values(details: &VesselDetails, translate: &Translate) -> [SmolStr; 16] {
    let unavailable = t_smol!(translate, "widget.global_shipping_chokepoint.unavailable");
    let text = |value: Option<&SmolStr>| value.cloned().unwrap_or_else(|| unavailable.clone());
    [
        text(details.name.as_ref()),
        SmolStr::new(details.mmsi.as_str()),
        text(details.imo.as_ref()),
        text(details.call_sign.as_ref()),
        details.ship_type.map_or_else(
            || unavailable.clone(),
            |ship_type| t_smol!(translate, ship_type.label_key()),
        ),
        details
            .flag
            .map_or_else(|| unavailable.clone(), SmolStr::new_static),
        text(details.destination.as_ref()),
        text(details.eta.as_ref()),
        text(details.draught.as_ref()),
        text(details.dimensions.as_ref()),
        text(details.speed.as_ref()),
        text(details.course.as_ref()),
        text(details.heading.as_ref()),
        details.nav_status.map_or_else(
            || unavailable.clone(),
            |status| t_smol!(translate, NAV_STATUS_KEYS[usize::from(status.index())]),
        ),
        text(details.updated.as_ref()),
        text(details.static_updated_at.as_ref()),
    ]
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use base_ui::theme::appearance::Appearance;
    use i18n::{language::codes::LanguageCode, translate::Translate};
    use ui_core::{
        node::builder::NodeBuilder, paint::primitive::Primitive,
        test_support::record::PrimitiveKind,
    };
    use widget::test_support::{CanvasPrimitive, WidgetTestApp};

    use super::*;
    use crate::model::{MIN_ZOOM, project};

    /// The box the canvas is laid out in.
    const PAGE: (f32, f32) = (640.0, 400.0);

    /// One canvas, its node and the host that lays it out.
    struct CanvasHarness {
        app: WidgetTestApp,
        map: MapCanvas,
        page: NodeId,
    }

    impl CanvasHarness {
        /// Paints the map from `picture`, reporting whether it rebuilt the canvas.
        fn sync(&mut self, vessels: &[Vessel], present_types: &[ShipType]) -> bool {
            let translate = self.app.services().translate.clone();
            let picture = MapPicture {
                vessels,
                present_types,
                filter: ShipTypeFilter::All,
                revision: 1,
                zone: 0,
                hover: None,
            };
            let scene = crate::scene::scene().ok();
            let map = &mut self.map;
            self.app
                .with_cx(|cx| map.sync(cx, &picture, scene, &translate))
                .expect("the map syncs")
        }

        /// The canvas' inner primitives, from the frame the last drain committed.
        fn primitives(&self) -> Vec<CanvasPrimitive> {
            self.app.canvas_primitives(self.node())
        }

        fn node(&self) -> NodeId {
            self.map.node().expect("the canvas node was built")
        }

        fn canvas_handle(&self) -> Arc<Canvas> {
            self.app
                .canvas_handle(self.node())
                .expect("the node is a canvas")
        }

        fn bounds(&self) -> Rect {
            self.map.bounds().expect("the map was laid out")
        }
    }

    fn page(app: &mut WidgetTestApp) -> NodeId {
        app.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::boxed()
                .id("TEST_CHOKEPOINT_MAP_PAGE")
                .style(
                    StyleBuilder::new()
                        .width(PAGE.0)
                        .height(PAGE.1)
                        .flex_col()
                        .build(),
                )
                .build(cx.tree_mut(), root)
                .expect("the page builds")
        })
    }

    /// A canvas node laid out in a page of a fixed size.
    fn harness() -> CanvasHarness {
        let mut app = WidgetTestApp::new();
        let page = page(&mut app);
        let mut map = MapCanvas::new();
        app.with_cx(|cx| map.build(cx, page))
            .expect("the canvas builds");
        app.drain();
        CanvasHarness { app, map, page }
    }

    /// The translator the product ships, for the tests that read the catalogue's copy.
    fn product_translate() -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            LanguageCode::En,
        )
    }

    /// A canvas whose frame is drawn from the product catalogue, place names and all.
    ///
    /// The fixture catalogue [`harness`] composes has none of this widget's copy, so it
    /// draws a map with no names on it: the tests that are about the labels need the
    /// product's own words.
    fn labelled_harness() -> CanvasHarness {
        let mut app = WidgetTestApp::with_translate(product_translate());
        let page = page(&mut app);
        let mut map = MapCanvas::new();
        app.with_cx(|cx| map.build(cx, page))
            .expect("the canvas builds");
        app.drain();
        CanvasHarness { app, map, page }
    }

    fn vessel(mmsi: u32) -> Vessel {
        Vessel {
            mmsi,
            ship_type: ShipType::Tanker,
            position: project(56.3, 26.5),
            cog: 456,
            heading: 87,
        }
    }

    fn geometry_draws(harness: &CanvasHarness) -> usize {
        harness
            .primitives()
            .iter()
            .filter(|primitive| primitive.kind == PrimitiveKind::Geometry)
            .count()
    }

    #[test]
    fn the_frame_should_clip_to_the_canvas_box_and_paint_the_sea_first() {
        let mut harness = harness();
        harness.sync(&[], &[]);
        harness.app.drain();

        assert_eq!(
            harness.bounds().size,
            ui_core::math::Size::new(PAGE.0, PAGE.1),
            "the canvas is laid out in the page it was built into"
        );
        assert!(
            harness.app.visible(harness.page),
            "the canvas is inside the page a pane would have laid out"
        );
        let primitives = harness.primitives();
        let first = primitives.first().expect("the frame painted something");
        assert_eq!(first.kind, PrimitiveKind::Quad, "the sea is the first fill");
        let theme = harness.app.services().theme.theme_for(Appearance::Light);
        assert_eq!(
            first.color,
            Some(theme.role(base_ui::theme::role::Role::ChartMapOcean))
        );
        assert!(
            primitives
                .iter()
                .any(|primitive| primitive.kind == PrimitiveKind::ClipPush),
            "the map is clipped to its node box"
        );
        assert_eq!(
            primitives.last().map(|primitive| primitive.kind),
            Some(PrimitiveKind::ClipPop),
            "the clip is balanced"
        );
    }

    #[test]
    fn the_land_should_be_registered_once_and_drawn_per_visible_cell() {
        let mut harness = harness();
        let baseline = harness.app.app().geometry().len();
        harness.sync(&[], &[]);
        harness.app.drain();
        let registered = harness.app.app().geometry().len();
        assert!(
            registered > baseline,
            "the land tier is registered on the first painted frame"
        );
        assert!(
            geometry_draws(&harness) >= 2,
            "the visible cells and the base layers are drawn"
        );

        harness.sync(&[], &[]);
        harness.app.drain();
        assert_eq!(
            harness.app.app().geometry().len(),
            registered,
            "a second frame of the same camera registers nothing new"
        );
    }

    #[test]
    fn a_steady_frame_should_keep_the_canvas_it_painted() {
        let mut harness = harness();
        harness.sync(&[], &[]);
        harness.app.drain();
        let before = harness.canvas_handle();
        let requests = harness.app.frame_requests();

        let rebuilt = harness.sync(&[], &[]);
        harness.app.drain();

        assert!(!rebuilt, "nothing the canvas draws moved");
        assert_eq!(harness.app.frame_requests(), requests, "and nothing asks");
        assert!(
            Arc::ptr_eq(&before, &harness.canvas_handle()),
            "a steady frame keeps the canvas value"
        );
    }

    #[test]
    fn a_vessel_should_be_drawn_and_hit_where_it_is_painted() {
        let mut harness = harness();
        let vessels = [vessel(1)];
        let empty = {
            harness.sync(&vessels, &[ShipType::Tanker]);
            harness.app.drain();
            geometry_draws(&harness)
        };
        assert!(empty > 0);

        let bounds = harness.bounds();
        let projection = Projection::new(
            harness.map.camera(),
            [bounds.size.width, bounds.size.height],
        );
        let at = projection.screen(vessels[0].position);
        let window = Point::new(bounds.origin.x + at[0], bounds.origin.y + at[1]);

        assert_eq!(
            harness.map.vessel_at(window, &vessels),
            Some(1),
            "the pointer is resolved against the frame's camera"
        );
        assert_eq!(
            harness
                .map
                .vessel_at(Point::new(window.x + 100.0, window.y + 100.0), &vessels),
            None
        );
    }

    #[test]
    fn hovering_a_vessel_should_add_its_ring() {
        let mut harness = harness();
        let vessels = [vessel(1)];
        harness.sync(&vessels, &[ShipType::Tanker]);
        harness.app.drain();
        let without_ring = geometry_draws(&harness);

        let bounds = harness.bounds();
        let projection = Projection::new(
            harness.map.camera(),
            [bounds.size.width, bounds.size.height],
        );
        let at = projection.screen(vessels[0].position);
        let window = Point::new(bounds.origin.x + at[0], bounds.origin.y + at[1]);
        assert!(harness.map.track_pointer(window, &vessels));
        harness.sync(&vessels, &[ShipType::Tanker]);
        harness.app.drain();

        assert_eq!(
            geometry_draws(&harness),
            without_ring + 1,
            "the hovered hull gains one ring mesh"
        );
    }

    #[test]
    fn zoom_should_keep_the_point_under_the_pointer_and_respect_its_limits() {
        let mut harness = harness();
        harness.sync(&[], &[]);
        harness.app.drain();
        let bounds = harness.bounds();
        let at = Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let projection = harness.map.projection().expect("the map has a projection");
        let canvas_point = [at.x - bounds.origin.x, at.y - bounds.origin.y];
        let before = projection.mercator(canvas_point);

        assert!(harness.map.zoom_at(at, 1.5));
        let after = harness
            .map
            .projection()
            .expect("the map has a projection")
            .mercator(canvas_point);
        assert!(
            (before[0] - after[0]).abs() < 1e-4 && (before[1] - after[1]).abs() < 1e-4,
            "the anchored point stays where the pointer is"
        );

        // The zoom saturates instead of running past its limits: this frame runs from 1.5×
        // to the top, and the next one has nothing left to move.
        assert!(harness.map.zoom_at(at, 10_000.0));
        assert!((harness.map.camera().zoom - MAX_ZOOM).abs() < f32::EPSILON);
        assert!(!harness.map.zoom_at(at, 10_000.0));
        assert!(harness.map.zoom_at(at, 0.1));
        assert!(harness.map.camera().zoom >= MIN_ZOOM);
    }

    #[test]
    fn releasing_the_canvas_should_return_every_registered_mesh() {
        let mut harness = harness();
        let baseline = harness.app.app().geometry().len();
        harness.sync(&[vessel(1)], &[ShipType::Tanker]);
        harness.app.drain();
        assert!(harness.app.app().geometry().len() > baseline);

        let map = &mut harness.map;
        harness.app.with_cx(|cx| map.release(cx));

        assert_eq!(
            harness.app.app().geometry().len(),
            baseline,
            "the canvas returns its land tiers and its per-frame meshes"
        );
    }

    /// A camera change is one of the key's inputs, so the next frame is a new canvas.
    ///
    /// The old test of this covered only the first painted frame; a key that dropped the
    /// camera would keep the canvas of the previous one and the map would never move.
    #[test]
    fn a_moved_camera_should_repaint_the_map() {
        let mut harness = harness();
        harness.sync(&[], &[]);
        harness.app.drain();
        let before = harness.canvas_handle();

        assert!(harness.map.pan(Point::new(12.0, -8.0)), "the camera moved");
        harness.sync(&[], &[]);
        harness.app.drain();

        assert!(
            !Arc::ptr_eq(&before, &harness.canvas_handle()),
            "a moved camera is another canvas, not the one that was drawn before it"
        );
    }

    /// The land is registered once and drawn from on every painted frame.
    ///
    /// The per-frame layers own one geometry id each and are returned when they are
    /// replaced; the coastline is not rebuilt with them. A cache that forgot its tier
    /// would leak one registered land mesh per painted frame.
    #[test]
    fn a_painted_frame_should_not_register_the_land_again() {
        let mut harness = harness();
        let baseline = harness.app.app().geometry().len();
        harness.sync(&[], &[]);
        harness.app.drain();
        let registered = harness.app.app().geometry().len();
        assert!(
            registered > baseline,
            "the first painted frame registers the coastline"
        );

        assert!(harness.map.pan(Point::new(12.0, -8.0)), "the camera moved");
        harness.sync(&[], &[]);
        harness.app.drain();

        assert_eq!(
            harness.app.app().geometry().len(),
            registered,
            "a painted frame re-uses the land tier it registered and swaps only its own meshes"
        );
    }

    /// A vessel is one more mesh on the frame, not a change to a layer that was not drawn.
    #[test]
    fn a_vessel_should_be_drawn_as_its_own_mesh() {
        let mut without_vessel = harness();
        without_vessel.sync(&[], &[]);
        without_vessel.app.drain();
        let without = geometry_draws(&without_vessel);

        let mut with_vessel = harness();
        with_vessel.sync(&[vessel(1)], &[ShipType::Tanker]);
        with_vessel.app.drain();

        assert_eq!(
            geometry_draws(&with_vessel),
            without + 1,
            "the hull is the one more mesh a picture holding a vessel draws"
        );
    }

    /// The land is drawn through the projection's own transform, not in Mercator space.
    ///
    /// The static mesh holds Mercator coordinates; a draw with an identity transform would
    /// put a few units of longitude at the canvas origin. This pins the mapping the
    /// uniform is supposed to be: `(0, 0)` where the projection puts it, one degree east a
    /// step right, and one degree north a step **up** the canvas (Mercator grows north, the
    /// canvas grows down).
    #[test]
    fn the_land_should_be_drawn_where_the_projection_puts_it() {
        let mut harness = harness();
        harness.sync(&[], &[]);
        harness.app.drain();

        let bounds = harness.bounds();
        let projection = harness.map.projection().expect("the map has a projection");
        let land = harness
            .map
            .tiers
            .iter()
            .flatten()
            .next()
            .expect("the painted frame registered a land tier");
        let transform = harness
            .canvas_handle()
            .primitives()
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Geometry(draw) if draw.id == land.id => Some(draw.transform),
                _ => None,
            })
            .expect("the registered tier is drawn");

        let zero = projection.screen([0.0, 0.0]);
        let at = transform.apply(Point::new(0.0, 0.0));
        assert!(
            (at.x - (bounds.origin.x + zero[0])).abs() < 1e-3
                && (at.y - (bounds.origin.y + zero[1])).abs() < 1e-3,
            "the land's Mercator origin lands where the projection says: {at:?}"
        );
        let unit = projection.screen([1.0, 1.0]);
        let east = transform.apply(Point::new(1.0, 0.0));
        let north = transform.apply(Point::new(0.0, 1.0));
        assert!(
            (east.x - at.x - (unit[0] - zero[0])).abs() < 1e-3,
            "one degree east is one projected pixel step to the right"
        );
        assert!(
            (north.y - at.y - (unit[1] - zero[1])).abs() < 1e-3,
            "one degree north is one projected pixel step up the canvas"
        );
        assert!(
            north.y < at.y,
            "Mercator grows north while the canvas grows down"
        );
    }

    /// Every country or city name is shaped once for its halo, in the map's paper colour.
    ///
    /// The ring is eight copies of the name pushed before the name itself, each shaped as a
    /// line of its own style; the text cache is where that is observable, because the canvas
    /// only records glyph counts and not which text a run came from. (The engine's shaping
    /// cache keeps the brush of the first line shaped for a text, so those copies are drawn
    /// in the label's ink today rather than the paper colour — registered as F22 in the W6b
    /// ledger. That the copies are shaped at all is what this pins.)
    #[test]
    fn every_place_name_should_be_ringed_by_its_halo() {
        let mut harness = labelled_harness();
        harness.sync(&[], &[]);
        harness.app.drain();

        let theme = harness.app.services().theme.theme_for(Appearance::Light);
        let colors = MapColors::resolve(theme);
        let halo_lines = harness
            .map
            .text
            .entries
            .iter()
            .filter(|entry| entry.style.color == colors.halo)
            .collect::<Vec<_>>();
        assert!(
            !halo_lines.is_empty(),
            "a country or city name is shaped for its halo"
        );
        for halo in halo_lines {
            assert!(
                harness
                    .map
                    .text
                    .entries
                    .iter()
                    .any(|entry| entry.text == halo.text && entry.style.color == colors.text),
                "{} is drawn under its halo",
                halo.text
            );
        }
    }
}

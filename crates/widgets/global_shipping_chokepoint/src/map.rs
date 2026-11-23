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

pub(super) const MAP_SELECTOR: &str = "TEST_CHOKEPOINT_MAP";

const WHEEL_ZOOM_STEP: f32 = 1.15;

const COUNTRY_LABEL_TEXT_SIZE: f32 = 21.0;

const CITY_LABEL_TEXT_SIZE: f32 = 13.0;

fn water_label_text_size() -> f32 {
    UnifiedSize::Small.text_size()
}

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

const CARD_WIDTH: f32 = 228.0;

const CARD_MARGIN: f32 = 12.0;

const ATTRIBUTION_INSET: f32 = 8.0;

const CARD_LINE_RHYTHM: f32 = 1.618;

const CORNER_GAP: f32 = 4.0;

const ATTRIBUTION_KEY: &str = "widget.global_shipping_chokepoint.attribution";

pub(super) const ATTRIBUTION_URL: &str = "https://www.openstreetmap.org/copyright";

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

const HOVER_STACKED_KEYS: [&str; 2] = [
    "widget.global_shipping_chokepoint.hover.updated",
    "widget.global_shipping_chokepoint.hover.static_updated_at",
];

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

const TEXT_CACHE_CAPACITY: usize = 256;

pub(super) struct MapPicture<'a> {
    pub vessels: &'a [Vessel],
    pub present_types: &'a [ShipType],
    pub filter: ShipTypeFilter,
    pub revision: u64,
    pub zone: i32,
    pub hover: Option<VesselDetails>,
}

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
    zone: i32,
    present_types: usize,
}

struct CachedText {
    text: SmolStr,
    style: TextStyle,
    handle: Arc<ShapedTextHandle>,
}

struct MapTextCache {
    entries: Vec<CachedText>,
}

impl MapTextCache {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(TEXT_CACHE_CAPACITY),
        }
    }

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

    fn width(&mut self, app: &mut App, text: &str, style: &TextStyle) -> f32 {
        self.line(app, text, style).size().width
    }

    fn height(&mut self, app: &mut App, style: &TextStyle) -> f32 {
        self.line(app, "X", style).size().height
    }

    fn has_stale(&mut self) -> bool {
        let stale = self.entries.iter().any(|entry| entry.handle.is_stale());
        if stale {
            self.entries.retain(|entry| !entry.handle.is_stale());
        }
        stale
    }
}

struct TierGeometry {
    id: GeometryId,
    mesh: paint::LandMesh,
}

pub(super) struct MapCanvas {
    node: Option<NodeId>,
    bounds: Option<Rect>,
    camera: Camera,
    tiers: [Option<TierGeometry>; model::LAND_TIERS],
    dynamic: Vec<GeometryId>,
    painted: Option<PaintedKey>,
    text: MapTextCache,
    hovered: Option<u32>,
    attribution: Option<Rect>,
    pointer: Option<Point>,
    dragging: bool,
}

impl MapCanvas {
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

    #[cfg(test)]
    pub(super) fn node(&self) -> Option<NodeId> {
        self.node
    }

    #[cfg(test)]
    pub(super) fn bounds(&self) -> Option<Rect> {
        self.bounds
    }

    #[cfg(test)]
    pub(super) fn camera(&self) -> Camera {
        self.camera
    }

    pub(super) fn hovered(&self) -> Option<u32> {
        self.hovered
    }

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
            return Ok(false);
        }
        self.bounds = Some(bounds);
        self.camera = self.camera.clamped([bounds.size.width, bounds.size.height]);

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
            for id in dynamic {
                cx.app_mut().geometry_mut().release(id);
            }
        }
        Ok(changed)
    }

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

            for (mesh, color) in paint::in_paint_order(
                paint::frame_meshes(Some(scene), projection, origin, picture.vessels),
                colors,
            ) {
                if let Some(id) = register_mesh(app, &mesh) {
                    canvas.push_geometry(GeometryDraw::full(id, Transform::IDENTITY, color));
                    dynamic.push(id);
                }
            }

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

    pub(super) fn projection(&self) -> Option<Projection> {
        let bounds = self.bounds?;
        let projection = Projection::new(self.camera, [bounds.size.width, bounds.size.height]);
        projection.drawable().then_some(projection)
    }

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

    pub(super) fn track_pointer(&mut self, position: Point, vessels: &[Vessel]) -> bool {
        let hit = self.vessel_at(position, vessels);
        if hit == self.hovered {
            return false;
        }
        self.hovered = hit;
        true
    }

    pub(super) fn clear_hover(&mut self) -> bool {
        self.hovered.take().is_some()
    }

    pub(super) fn pointer_down(&mut self, position: Point) {
        self.pointer = Some(position);
        self.dragging = true;
    }

    pub(super) fn pointer_up(&mut self) {
        self.dragging = false;
    }

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

    pub(super) fn zoom_by_wheel(&mut self, position: Point, step: f32) -> bool {
        if !step.is_finite() || step == 0.0 {
            return false;
        }
        let factor = if step > 0.0 {
            WHEEL_ZOOM_STEP
        } else {
            1.0 / WHEEL_ZOOM_STEP
        };
        self.zoom_at(position, factor)
    }

    pub(super) fn zoom_by_pinch(&mut self, position: Point, delta: f32) -> bool {
        if !delta.is_finite() || delta == 0.0 {
            return false;
        }
        self.zoom_at(position, 1.0 + delta)
    }

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

    pub(super) fn attribution_contains(&self, position: Point) -> bool {
        self.attribution.is_some_and(|rect| rect.contains(position))
    }

    pub(super) fn contains(&self, position: Point) -> bool {
        self.bounds.is_some_and(|bounds| bounds.contains(position))
    }

    pub(super) fn is_dragging(&self) -> bool {
        self.dragging
    }
}

fn tier_metres_per_pixel(tier: usize) -> f32 {
    match tier {
        0 => 1.0,
        1 => model::LAND_TIER_FINE_METRES_PER_PIXEL,
        2 => model::LAND_TIER_MIDDLE_METRES_PER_PIXEL,
        _ => model::LAND_TIER_COARSE_METRES_PER_PIXEL,
    }
}

fn land_transform(projection: Projection, bounds: Rect) -> Transform {
    let zero = projection.screen([0.0, 0.0]);
    let scale_x = projection.screen([1.0, 0.0])[0] - zero[0];
    let scale_y = projection.screen([0.0, 1.0])[1] - zero[1];
    Transform::scale(scale_x, scale_y).then(Transform::translate(
        bounds.origin.x + zero[0],
        bounds.origin.y + zero[1],
    ))
}

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

fn halo_style(kind: LabelKind, colors: MapColors) -> TextStyle {
    let mut style = label_style(kind, colors);
    style.color = colors.halo;
    style
}

fn label_text(translate: &Translate, key: &str, kind: LabelKind) -> SmolStr {
    match kind {
        LabelKind::Country => SmolStr::new(t_smol!(translate, key).to_uppercase()),
        LabelKind::Water | LabelKind::City => t_smol!(translate, key),
    }
}

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

fn label_kind_index(kind: LabelKind) -> u8 {
    match kind {
        LabelKind::Country => 0,
        LabelKind::Water => 1,
        LabelKind::City => 2,
    }
}

fn credit_origin(rect: Rect) -> Point {
    Point::new(rect.origin.x, rect.origin.y)
}

fn width_of(text: &str, style: &TextStyle, app: &mut App, cache: &mut MapTextCache) -> f32 {
    cache.width(app, text, style)
}

fn small_style(color: Color) -> TextStyle {
    let mut style = TextStyle::new(UnifiedSize::Small.text_size());
    style.color = color;
    style.line_height = Some(CARD_LINE_RHYTHM);
    style
}

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

    const PAGE: (f32, f32) = (640.0, 400.0);

    struct CanvasHarness {
        app: WidgetTestApp,
        map: MapCanvas,
        page: NodeId,
    }

    impl CanvasHarness {
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

    fn harness() -> CanvasHarness {
        let mut app = WidgetTestApp::new();
        let page = page(&mut app);
        let mut map = MapCanvas::new();
        app.with_cx(|cx| map.build(cx, page))
            .expect("the canvas builds");
        app.drain();
        CanvasHarness { app, map, page }
    }

    fn product_translate() -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            LanguageCode::En,
        )
    }

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

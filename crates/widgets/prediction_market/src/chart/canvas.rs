//! The canvas one prediction chart is: its options, its prepared frame, and the cache
//! that keeps a pointer-only frame from preparing anything again.
//!
//! A chart owns a canvas **node** rather than a value: the engine keeps the node tree,
//! and a canvas is replaced through [`WidgetCx::set_canvas`] when what it should draw
//! moved. Everything a frame reuses — the box it was laid out in, the domains, the
//! ticks, the folded series, the registered line meshes, the shaped labels — is kept
//! here, so a frame that changed nothing rebuilds nothing (I3).
//!
//! The prepared-frame key is exactly what preparing *reads*: the snapshot by identity,
//! the box the node ended up in (rounded to whole pixels), the display time zone, the
//! language showing and the raster scale the labels will be shaped at. The pointer and
//! the pulse phase are deliberately absent, because they change every frame and
//! everything they affect is drawn by [`super::paint`] from the prepared frame. A
//! pointer that moved therefore prepares nothing again — same domains, same ticks,
//! same meshes — and only the canvas value is rebuilt.
//!
//! The painted key is what the canvas on screen was built from: the prepared frame's
//! inputs, the appearance-resolved colours, the hovered sample, the pulse phase and the
//! line meshes. A steady frame compares it, checks the atlas leases of the labels the
//! canvas was actually built from, and returns without touching the node.
//!
//! # The box the chart is given
//!
//! The engine's canvas draws in **absolute window coordinates**, so the chart is laid
//! out from the layout box the node ended up in on this frame. The box is rounded to
//! whole logical pixels before the layout is built: a canvas that drew its grid from a
//! half-pixel origin would blur every dot, and a fractional resize would move the
//! prepared key on every frame that changed the fraction rather than the box.
//!
//! # Language, appearance and colour
//!
//! The canvas is not part of the engine's style pass: it holds primitives with colours
//! and glyph runs baked in, so a language switch (which rewrites every label) or an
//! appearance switch (which changes every colour) is a *rebuild*. The node carries a
//! `bind_style` that reads the appearance signal and a `bind_a11y` that reads the
//! language signal, so both changes invalidate it and schedule the frame whose `sync`
//! rebuilds the canvas. [`ChartCanvas::sync`] never requests a frame itself: the view
//! asks for the frames the endpoint pulse animates on, and nothing else here animates.

use std::sync::Arc;

use base_ui::style_builder::StyleBuilder;
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use smol_str::SmolStr;
use time::UtcOffset;
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    error::EngineError,
    math::{Point, Rect},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{canvas::Canvas, geometry::GeometryId, style::StylePatch},
    text::ShapedTextHandle,
};
use widget::cx::WidgetCx;

use super::{
    geometry::{
        ChartLayout, Sample, ScreenPoint, TimeDomain, ValueAxis, decimate, percent, time_domain,
        time_ticks, visible_max_percent, x_for_time,
    },
    hover::{HoveredSample, nearest_timestamp},
    paint::{ChartColors, SERIES_HALO_WIDTH, SERIES_STROKE_WIDTH, line_geometry, paint},
    text::{ChartTextCache, format_axis_labels, format_percent_integer},
};
use crate::model::{MAX_LINES, PredictionMarketSeries, PredictionMarketSnapshot};

/// The debug id of the canvas node the chart draws on.
const CHART_SELECTOR: &str = "TEST_PREDICTION_MARKET_CHART";

/// One point a time axis is labelled and gridded at.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TimeTick {
    pub timestamp: i64,
    /// Where the tick is drawn, which is also where its label is centred.
    pub x: f32,
    pub label: SmolStr,
}

/// One series folded onto the plot.
///
/// The original samples are kept beside the screen points: the pointer reads a value
/// from them at any time, while the screen points are already decimated and cannot
/// answer for a time they no longer carry. Both are built once, here, and a hovering
/// frame reads them instead of rebuilding them.
pub(crate) struct PreparedSeries {
    pub label: SmolStr,
    /// The index into the theme's series palette.
    pub color: usize,
    /// Every sample of the line, ascending in time.
    pub samples: Arc<[Sample]>,
    /// The decimated points the line is drawn from, including the lead.
    pub screen: Arc<[ScreenPoint]>,
    /// The newest point inside the window, which is where the endpoint is drawn.
    pub last: Option<ScreenPoint>,
}

impl PreparedSeries {
    /// Whether the line reaches `timestamp`.
    ///
    /// A step line keeps holding its last value after its last sample, but there is no
    /// point there to mark: a hover dot or a label at a later x would belong to a part
    /// of the plot this line never crossed.
    pub(crate) fn covers(&self, timestamp: i64) -> bool {
        self.samples
            .last()
            .is_some_and(|last| timestamp <= last.timestamp)
    }
}

/// The registered meshes of one series' line, drawn in the series colour.
///
/// The halo is the same ribbon a little wider, drawn under the line in the pane body's
/// own colour so two crossing lines stay separable. Both are released and registered
/// again when the prepared frame under the canvas changes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct SeriesMeshes {
    /// The step line at the series' stroke width.
    pub line: Option<GeometryId>,
    /// The same line at the halo's width, drawn underneath.
    pub halo: Option<GeometryId>,
}

/// Everything one frame is drawn from, prepared from the node's box and the snapshot
/// under it.
pub(crate) struct PreparedChart {
    pub layout: ChartLayout,
    /// The time window, or `None` for a snapshot with no samples at all.
    pub domain: Option<TimeDomain>,
    pub axis: ValueAxis,
    /// The y-axis label of every one of [`ValueAxis::ticks`], formatted once here so
    /// no paint pass has to format a number.
    pub axis_labels: Arc<[SmolStr]>,
    pub time_ticks: Arc<[TimeTick]>,
    pub series: Arc<[PreparedSeries]>,
    /// Every sample time drawn in the window, sorted and unique, which is what the
    /// pointer snaps to.
    pub hover_times: Arc<[i64]>,
    /// The display time zone the axis labels were formatted in; the hovered time is
    /// formatted in it while the pointer moves.
    pub time_zone: UtcOffset,
}

impl PreparedChart {
    /// Folds one snapshot into the boxes, meshes and labels a frame draws.
    fn new(
        snapshot: &PredictionMarketSnapshot,
        bounds: Rect,
        time_zone: UtcOffset,
        translate: &Translate,
    ) -> Self {
        let layout = ChartLayout::new(bounds);
        let Some(newest) = snapshot.latest_timestamp() else {
            return Self {
                layout,
                domain: None,
                axis: ValueAxis::new(0.0),
                axis_labels: Arc::default(),
                time_ticks: Arc::default(),
                series: Arc::default(),
                hover_times: Arc::default(),
                time_zone,
            };
        };
        let mut oldest = i64::MAX;
        for series in snapshot.series.iter() {
            for sample in series.points.iter() {
                oldest = oldest.min(sample.timestamp);
            }
        }

        let domain = time_domain(oldest, newest, snapshot.range);
        let samples: Vec<Arc<[Sample]>> = snapshot
            .series
            .iter()
            .map(|series| Arc::<[Sample]>::from(samples_of(series)))
            .collect();
        let highest = samples
            .iter()
            .map(|samples| visible_max_percent(samples, domain))
            .fold(0.0, f64::max);
        let axis = ValueAxis::new(highest);
        let axis_labels: Arc<[SmolStr]> = axis
            .ticks
            .iter()
            .map(|tick| format_percent_integer(*tick))
            .collect();
        let chosen = time_ticks(domain, layout.plot.size.width, time_zone);
        let labels = format_axis_labels(&chosen.timestamps, chosen.label, time_zone, translate);
        let time_ticks: Vec<TimeTick> = chosen
            .timestamps
            .into_iter()
            .zip(labels)
            .map(|(timestamp, label)| TimeTick {
                timestamp,
                x: x_for_time(domain, layout.plot, timestamp),
                label,
            })
            .collect();

        let mut hover_times: Vec<i64> = Vec::new();
        let series: Arc<[PreparedSeries]> = snapshot
            .series
            .iter()
            .zip(samples.iter())
            .map(|(line, samples)| {
                let folded = decimate(samples, domain, &axis, layout.plot);
                hover_times.extend(
                    samples
                        .iter()
                        .filter(|sample| domain.contains(sample.timestamp))
                        .map(|sample| sample.timestamp),
                );
                PreparedSeries {
                    label: line.label.clone(),
                    color: line.color,
                    samples: Arc::clone(samples),
                    screen: folded.screen.into(),
                    last: folded.last,
                }
            })
            .collect();
        hover_times.sort_unstable();
        hover_times.dedup();

        Self {
            layout,
            domain: Some(domain),
            axis,
            axis_labels,
            time_ticks: time_ticks.into(),
            series,
            hover_times: hover_times.into(),
            time_zone,
        }
    }

    /// The hovered sample for a pointer position in window coordinates.
    pub(crate) fn hover_at(&self, pointer: Point) -> Option<HoveredSample> {
        let domain = self.domain?;
        if !self.layout.plot.contains(pointer) {
            return None;
        }
        let fraction = (pointer.x - self.layout.plot.origin.x) / self.layout.plot.size.width;
        let time = domain.start as f64 + f64::from(fraction) * domain.span() as f64;
        let timestamp = nearest_timestamp(&self.hover_times, time as i64)?;
        Some(HoveredSample {
            timestamp,
            x: x_for_time(domain, self.layout.plot, timestamp),
        })
    }
}

/// The samples of one series, converted to percentage points and ordered in time.
fn samples_of(series: &PredictionMarketSeries) -> Vec<Sample> {
    let mut samples: Vec<Sample> = series
        .points
        .iter()
        .map(|point| Sample {
            timestamp: point.timestamp,
            percent: percent(point.probability),
        })
        .collect();
    // The service publishes points in time order, but the hover's binary search and
    // the lead both assume it; sorting here is where that becomes true rather than a
    // convention every reader has to remember.
    samples.sort_by_key(|sample| sample.timestamp);
    samples
}

/// The inputs one prepared chart was built from.
///
/// Everything *preparing* reads has to be in here, or a change to it would keep serving
/// the frame prepared before the change: the snapshot (by identity), the box it was laid
/// out in, the zone and language its labels were written in, and the raster scale those
/// labels will be shaped at.
struct PreparedKey {
    snapshot: Arc<PredictionMarketSnapshot>,
    bounds: Rect,
    time_zone: UtcOffset,
    language: LanguageCode,
    text_scale: f32,
}

impl PreparedKey {
    /// Whether a prepared chart was built from these inputs.
    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.snapshot, &other.snapshot)
            && self.bounds == other.bounds
            && self.time_zone == other.time_zone
            && self.language == other.language
            && self.text_scale == other.text_scale
    }
}

/// The inputs one canvas was built from.
///
/// The snapshot is named by identity rather than by value: a reading that was published
/// again with the same numbers is still another reading, and comparing the data of a
/// four-thousand-point snapshot on every frame is not what a short circuit is for.
#[derive(Clone, Debug)]
struct PaintedKey {
    snapshot: Arc<PredictionMarketSnapshot>,
    bounds: Rect,
    time_zone: UtcOffset,
    language: LanguageCode,
    text_scale: f32,
    colors: ChartColors,
    hovered: Option<HoveredSample>,
    pulse: Option<f32>,
    meshes: [SeriesMeshes; MAX_LINES],
}

impl PartialEq for PaintedKey {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.snapshot, &other.snapshot)
            && self.bounds == other.bounds
            && self.time_zone == other.time_zone
            && self.language == other.language
            && self.text_scale == other.text_scale
            && self.colors == other.colors
            && self.hovered == other.hovered
            && self.pulse == other.pulse
            && self.meshes == other.meshes
    }
}

/// The chart of one pane: the canvas node the body built, the prepared frame behind it
/// and everything the next frame reuses.
pub(crate) struct ChartCanvas {
    /// The canvas node, once the body built it.
    node: Option<NodeId>,
    /// The frame prepared from the current inputs, and the inputs it was prepared from.
    prepared: Option<(PreparedKey, PreparedChart)>,
    /// What the canvas on screen was built from.
    painted: Option<PaintedKey>,
    /// The shaped labels the canvas on screen was built from.
    ///
    /// A canvas is a value and the chart keeps the one it built, but the *glyphs* a
    /// canvas names are leased from a shared atlas: an atlas generation change or a
    /// per-tile eviction leaves the handles it was built from pointing at reused
    /// texture, and the engine drops such runs at encode time. The lines recorded here
    /// are the ones whose lease a steady frame has to check — a line the cache still
    /// holds from a frame that is no longer drawn is not on the canvas, so it cannot
    /// keep a steady chart rebuilding.
    painted_text: Vec<Arc<ShapedTextHandle>>,
    /// The shaped labels, keyed by text and style.
    text: ChartTextCache,
    /// The registered meshes of the series lines, in the series order the canvas draws
    /// them.
    meshes: [SeriesMeshes; MAX_LINES],
    /// How many prepared frames this chart has built, for the cache test.
    #[cfg(test)]
    builds: usize,
    /// The sample the last painted frame hovered, for the placement tests.
    #[cfg(test)]
    hovered: Option<HoveredSample>,
}

impl ChartCanvas {
    /// A chart that has not been built into a body yet.
    pub(crate) fn new() -> Self {
        Self {
            node: None,
            prepared: None,
            painted: None,
            painted_text: Vec::new(),
            text: ChartTextCache::default(),
            meshes: [SeriesMeshes::default(); MAX_LINES],
            #[cfg(test)]
            builds: 0,
            #[cfg(test)]
            hovered: None,
        }
    }

    /// Builds the canvas node into `parent`.
    ///
    /// The node starts empty (`Rect::default()`): it has no box until the first layout,
    /// and [`Self::sync`] is what fills it in. It registers two dependencies on purpose:
    /// the appearance (a canvas bakes its colours in, so an appearance switch has to
    /// schedule the frame that rebuilds it) and the language (the labels on the canvas
    /// are written in the language showing now, and the read is what makes a language
    /// switch schedule the frame that rewrites them).
    pub(crate) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let label = t_smol!(translate, "widget.names.prediction_market");
        let node = NodeBuilder::canvas(Canvas::new(Rect::default()))
            .id(CHART_SELECTOR)
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
                    label: Some(t_smol!(translate, "widget.names.prediction_market")),
                    ..A11yPatch::default()
                }
            })
            .bind_style(move |read| {
                let _appearance = theme.appearance(read);
                StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        self.node = Some(node);
        Ok(node)
    }

    /// The node the chart draws on, once the body has been built.
    ///
    /// Only the tests read it back: the pane holds the node it built and never needs the
    /// chart's copy of it again.
    #[cfg(test)]
    pub(crate) fn node(&self) -> Option<NodeId> {
        self.node
    }

    /// Forgets the prepared frame and the shaped labels.
    ///
    /// A reading the pane can no longer vouch for is a reading whose prepared frame
    /// can never be reused (its key begins with the snapshot's identity), so it is
    /// dropped before the next one arrives. The line meshes stay registered until the
    /// next prepared frame replaces them (or [`Self::release`] hands them back): they
    /// are the engine's store, not this chart's value, and dropping the id without
    /// releasing it would leak the entry.
    pub(crate) fn clear(&mut self) {
        self.prepared = None;
        self.painted = None;
        self.painted_text.clear();
    }

    /// Draws the frame for `snapshot`, or keeps the one already drawn.
    ///
    /// Returns whether the canvas was written. The work is split in two: the
    /// *prepared* frame is rebuilt only when the box, the snapshot, the zone, the
    /// language or the raster scale moved, and the *canvas* is rebuilt only when what it
    /// would draw moved — the prepared frame, the colours, the hovered sample, the pulse
    /// phase, the line meshes, or the atlas lease of a label it was built from.
    pub(crate) fn sync(
        &mut self,
        cx: &mut WidgetCx<'_>,
        snapshot: &Arc<PredictionMarketSnapshot>,
        pointer: Option<Point>,
        pulse_phase: Option<f32>,
    ) -> Result<bool, EngineError> {
        let Some(node) = self.node else {
            return Ok(false);
        };
        let Ok(target) = cx.tree().get(node) else {
            return Ok(false);
        };
        let bounds = whole_pixel_bounds(target.layout().bounds);
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            // The frame that laid the node out has not run yet; the next one will.
            return Ok(false);
        }

        let time_zone = cx.data_time_zone();
        let language = *cx.read(&cx.language());
        let text_scale = cx.app().text_scale();
        let key = PreparedKey {
            snapshot: Arc::clone(snapshot),
            bounds,
            time_zone,
            language,
            text_scale,
        };
        if !self
            .prepared
            .as_ref()
            .is_some_and(|(cached, _)| cached.matches(&key))
        {
            let translate = cx.translate().clone();
            let prepared = PreparedChart::new(snapshot, bounds, time_zone, &translate);
            self.replace_meshes(cx, &prepared);
            #[cfg(test)]
            {
                self.builds += 1;
            }
            self.prepared = Some((key, prepared));
        }
        let Some((_, prepared)) = self.prepared.as_ref() else {
            return Ok(false);
        };

        let appearance = cx.theme().current(cx.app());
        let colors = ChartColors::resolve(cx.theme().theme_for(appearance));
        let hovered = pointer.and_then(|pointer| prepared.hover_at(pointer));
        let painted = PaintedKey {
            snapshot: Arc::clone(snapshot),
            bounds,
            time_zone,
            language,
            text_scale,
            colors,
            hovered,
            pulse: pulse_phase,
            meshes: self.meshes,
        };
        // The painted key names what the canvas was built from, but not what its labels
        // were *leased* from: a shaped handle points into a shared atlas, and an atlas
        // generation change or a per-tile eviction can take every lease the canvas holds
        // without moving anything in the key. The engine drops such runs at encode time,
        // so a frame that kept this canvas would show a chart with its labels missing —
        // for good. A stale lease is a rebuild.
        if self.painted.as_ref() == Some(&painted)
            && !self.painted_text.iter().any(|handle| handle.is_stale())
        {
            return Ok(false);
        }

        let translate = cx.translate().clone();
        let (canvas, whole, used) = paint(
            cx.app_mut(),
            &mut self.text,
            &translate,
            prepared,
            &self.meshes,
            &colors,
            hovered,
            pulse_phase,
        );
        // Shaping one label can evict another label's tiles, so a single pass is not
        // always a whole frame: a refused run is shaped again and pushed by a second
        // pass. A second refusal is left to the next frame rather than retried here, so
        // a chart whose labels do not fit the atlas cannot spin this hook.
        let (canvas, whole, used) = if whole {
            (canvas, whole, used)
        } else {
            paint(
                cx.app_mut(),
                &mut self.text,
                &translate,
                prepared,
                &self.meshes,
                &colors,
                hovered,
                pulse_phase,
            )
        };
        let changed = cx.set_canvas(node, canvas)?;
        #[cfg(test)]
        {
            self.hovered = hovered;
        }
        // An incomplete canvas is not what the chart draws: leaving the key unrecorded
        // is what makes the next frame build it again (I3 holds — a whole frame is still
        // a short circuit).
        if whole {
            self.painted = Some(painted);
            self.painted_text = used;
        }
        Ok(changed)
    }

    /// Releases the meshes of the previous frame and registers the ones this frame draws.
    fn replace_meshes(&mut self, cx: &mut WidgetCx<'_>, prepared: &PreparedChart) {
        for pair in self.meshes.iter_mut() {
            let previous = std::mem::take(pair);
            for mesh in [previous.line, previous.halo].into_iter().flatten() {
                cx.app_mut().geometry_mut().release(mesh);
            }
        }
        for (index, series) in prepared.series.iter().enumerate().take(MAX_LINES) {
            self.meshes[index] = SeriesMeshes {
                halo: register_line(cx, &series.screen, SERIES_HALO_WIDTH),
                line: register_line(cx, &series.screen, SERIES_STROKE_WIDTH),
            };
        }
    }

    /// Releases what the chart owns: its registered meshes, its node and its caches.
    ///
    /// The node is the chart's own: the pane's body is built once, and a chart that
    /// goes away removes the node it drew into, because the next chart is built into
    /// the same host. A node the pane already removed is not a failure — the framework
    /// tears the body down after this hook.
    pub(crate) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        for pair in self.meshes.iter_mut() {
            let previous = std::mem::take(pair);
            for mesh in [previous.line, previous.halo].into_iter().flatten() {
                cx.app_mut().geometry_mut().release(mesh);
            }
        }
        if let Some(node) = self.node.take()
            && cx.tree().get(node).is_ok()
        {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, node);
        }
        self.clear();
    }

    /// How many prepared frames this chart has built.
    #[cfg(test)]
    pub(crate) fn prepared_builds(&self) -> usize {
        self.builds
    }

    /// The sample the last painted frame hovered, if the pointer was over the plot.
    #[cfg(test)]
    pub(crate) fn hovered_sample(&self) -> Option<HoveredSample> {
        self.hovered
    }

    /// The canvas node's current content.
    ///
    /// This is the input side of the node tree, so it needs no frame: it is what the
    /// engine will draw next, and the glyph runs in it name the atlas lease the canvas
    /// was built from.
    #[cfg(test)]
    pub(crate) fn canvas_for_test<'a>(
        &self,
        app: &'a ui_core::app::App,
        window: ui_core::window::WindowId,
    ) -> Option<&'a Canvas> {
        let node = self.node?;
        let target = app.window(window)?.tree().get(node).ok()?;
        match target.kind() {
            ui_core::node::tree::NodeKind::Canvas(canvas) => Some(canvas),
            _ => None,
        }
    }

    /// Drops the prepared frame, the way the pane does when it can no longer vouch for
    /// the reading. The next `sync` prepares and draws from scratch.
    #[cfg(test)]
    pub(crate) fn clear_for_test(&mut self) {
        self.clear();
    }
}

/// Registers one series line at `width`, logging a store that refuses it.
///
/// A refused mesh is a line drawn without its stroke for this frame rather than a
/// dropped frame: the chart's labels and dots are still worth drawing, and the next
/// prepared frame tries again.
fn register_line(cx: &mut WidgetCx<'_>, screen: &[ScreenPoint], width: f32) -> Option<GeometryId> {
    let desc = line_geometry(screen, width)?;
    match cx.app_mut().geometry_mut().insert(desc) {
        Ok(id) => Some(id),
        Err(error) => {
            log::warn!("the prediction chart could not register a series line: {error}");
            None
        }
    }
}

/// The box the chart lays itself out in: the node's box rounded to whole pixels.
///
/// The engine places a flex child at fractional logical offsets, and a canvas draws in
/// absolute window coordinates: a half-pixel origin would put every grid dot and every
/// label on a half pixel, and a resize that only moved the fraction would move the
/// prepared key with it. Rounding the box is what makes the prepared frame survive a
/// fractional resize.
fn whole_pixel_bounds(bounds: Rect) -> Rect {
    Rect::new(
        bounds.origin.x.round(),
        bounds.origin.y.round(),
        bounds.size.width.round(),
        bounds.size.height.round(),
    )
}

//! The canvas one statistics chart draws into: its cache, its pointer, and the node.
//!
//! A chart owns a canvas **node** rather than a value: the engine keeps the node tree,
//! and a canvas is replaced through [`WidgetCx::set_canvas`] when what it should draw
//! moved. Everything a frame reuses — the box it was last laid out in, the shaped
//! labels, the prepared boxes and lines, the registered meshes of the series lines — is
//! kept here, so a frame that changed nothing rebuilds nothing (I3).
//!
//! # The pointer is followed one frame behind, on purpose
//!
//! A mouse crossing a plot produces one move event per pixel, and a chart that rebuilt a
//! canvas per pixel would spend the whole gesture drawing. A move records the reading it
//! resolved to, and the next `sync` — which the widget asks for when the reading really
//! moved — applies it. The reading is what the frame draws from, so two positions inside
//! one interval's slot are one frame.
//!
//! # The box the chart is given
//!
//! The engine's canvas draws in **absolute window coordinates**, so the chart is laid out
//! from the layout box the node ended up in on this frame, and every box and label it
//! places is derived from that. That is also why the chart is not put inside a scroll
//! container: scrolling moves a node without invalidating the canvas that draws in window
//! coordinates, so the drawing would stay where the node used to be. A pane narrower than
//! the content clips it instead of scrolling to it (registered as a W5a difference: the
//! old two-axis scroll containers are gone, as in W3c's ETF chart).
//!
//! # Language, appearance and colour
//!
//! The canvas is not part of the engine's style pass: a canvas holds primitives with
//! colours baked in, so a language switch (which rewrites every label) or an appearance
//! switch (which changes every colour) is a *rebuild*. The node carries a `bind_style`
//! that reads the appearance signal and a `bind_a11y` that reads the language signal, so
//! both changes invalidate it and schedule the frame whose `sync` rebuilds the canvas.

use std::sync::Arc;

use base_ui::{style_builder::StyleBuilder, theme::appearance::Appearance};
use i18n::{t_smol, translate::Translate};
use market_participants_model::listing::rules::ListingPrecision;
use time::UtcOffset;
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    error::EngineError,
    math::{Point, Rect},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{canvas::Canvas, geometry::GeometryId, style::StylePatch},
};
use widget::cx::WidgetCx;

use super::{
    axes::{AxisScale, ChartLayout, nearest_index},
    paint::{Frame, Hover, Measurements, Prepared, Tooltip, line_geometry_mesh, measure, paint},
    palette::SeriesLabel,
    text::ChartTextCache,
};
use crate::{
    model::{MetricKind, StatisticsPoints},
    service::StatisticsStreamMessage,
};

/// The inputs one prepared frame was built from.
///
/// Everything *preparing* a frame reads has to be in here, or a change to it would keep
/// serving the frame from before the change: the box the chart node ended up with, and
/// the chart's own revision — a new window, another zone or another language all rebuild
/// the resolved view and move the revision with it. A colour is not one of them: nothing
/// the preparation keeps is coloured, and every colour a frame paints with is read from
/// the active theme as it draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CacheKey {
    pub bounds: Rect,
    pub revision: u64,
}

/// Everything one chart reuses between frames.
#[derive(Default)]
struct ChartCache {
    /// The layout of the frame that was prepared from the current inputs.
    layout: Option<ChartLayout>,
    /// The prepared frame, the measurements it used, and the inputs it was prepared
    /// from.
    prepared: Option<(CacheKey, Measurements, Prepared)>,
    /// What the canvas on screen was built from.
    painted: Option<PaintedKey>,
    /// The shaped labels, keyed by text and style.
    text: ChartTextCache,
    /// The registered meshes of the series lines, in the series order the canvas draws
    /// them.
    meshes: [Option<GeometryId>; 2],
}

/// The inputs one canvas was built from.
///
/// A canvas has every colour baked into it, so the appearance is part of the key; the
/// hover is part of it because the crosshair and the tooltip are; the meshes are part of
/// it because a canvas names the geometry it draws.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PaintedKey {
    cache: CacheKey,
    hover: Option<Hover>,
    meshes: [Option<GeometryId>; 2],
    appearance: Appearance,
}

/// One chart: the view it resolved, the cache a frame reuses, and the node it draws
/// into.
pub(crate) struct Chart<D: ChartData> {
    data: D,
    /// Moves whenever `data` is rebuilt, which is how a prepared frame knows the view
    /// under it changed.
    revision: u64,
    cache: ChartCache,
    /// The reading the pointer is over, if it is over the plot.
    hover: Option<Hover>,
    /// The canvas node, once the chart has been built into the pane's body.
    node: Option<NodeId>,
}

impl<D: ChartData> Chart<D> {
    /// Wraps a resolved view in an empty chart.
    pub(crate) fn new(data: D) -> Self {
        Self {
            data,
            revision: 1,
            cache: ChartCache::default(),
            hover: None,
            node: None,
        }
    }

    /// The revision of the window this chart is showing.
    pub(crate) fn revision(&self) -> u64 {
        self.data.revision()
    }

    /// How many points the window this chart is showing holds.
    pub(crate) fn point_count(&self) -> usize {
        self.data.point_count()
    }

    /// Builds the chart's canvas node into `parent`.
    pub(crate) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let label = t_smol!(translate, "widget.perpetual_statistics.chart");
        let node = NodeBuilder::canvas(Canvas::new(Rect::default()))
            .id("TEST_PERPETUAL_STATISTICS_CHART")
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
                    label: Some(t_smol!(translate, "widget.perpetual_statistics.chart")),
                    ..A11yPatch::default()
                }
            })
            // A canvas bakes its colours in, so an appearance switch is a rebuild: the
            // read below is what invalidates the node and schedules the frame that
            // rebuilds it. The patch is empty on purpose — nothing about the node's own
            // style follows the theme.
            .bind_style(move |read| {
                let _appearance = theme.appearance(read);
                StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        self.node = Some(node);
        Ok(node)
    }

    /// The node the chart draws into, once it has been built.
    pub(crate) fn node(&self) -> Option<NodeId> {
        self.node
    }

    /// The layout of the last prepared frame, which is what a hover is resolved
    /// against.
    #[cfg(test)]
    pub(crate) fn layout_for_test(&self) -> Option<ChartLayout> {
        self.cache.layout
    }

    /// The titles the chart draws, which is where the units its labels name come from.
    #[cfg(test)]
    pub(crate) fn axis_titles_for_test(&self) -> (&str, &str) {
        self.data.axis_titles()
    }

    /// The pointer the chart is drawing from.
    #[cfg(test)]
    pub(crate) fn hover_for_test(&self) -> Option<Hover> {
        self.hover
    }

    /// The canvas node's current content.
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

    /// The reading a pointer position resolves to, if it is over the plot.
    pub(crate) fn hover_at(&self, cursor: Point) -> Option<Hover> {
        let layout = self.cache.layout?;
        if !layout.plot.contains(cursor) {
            return None;
        }
        let index = nearest_index(layout.plot, self.data.point_count(), cursor.x)?;
        self.data.hover_point(index).map(|index| Hover {
            index,
            cursor_y: cursor.y.round(),
        })
    }

    /// Applies a pointer position, reporting whether the reading under it moved.
    pub(crate) fn point_at(&mut self, cursor: Point) -> bool {
        let next = self.hover_at(cursor);
        if self.hover == next {
            return false;
        }
        self.hover = next;
        true
    }

    /// Records that the pointer left the plot, reporting whether it had been on it.
    pub(crate) fn clear_hover(&mut self) -> bool {
        self.hover.take().is_some()
    }

    /// Rewrites the chart's labels in another display zone, reporting whether anything
    /// moved.
    ///
    /// The prepared frame is keyed by the chart's revision, so a zone that moved is a
    /// frame that is prepared again: the labels it holds were written in the old zone.
    pub(crate) fn set_time_zone(&mut self, zone: UtcOffset) -> bool {
        if !self.data.set_time_zone(zone) {
            return false;
        }
        self.bump_revision();
        true
    }

    /// Rewrites the chart's labels in another language, reporting whether anything
    /// moved.
    pub(crate) fn set_locale(&mut self, translate: &Translate) -> bool {
        if !self.data.set_locale(translate) {
            return false;
        }
        self.bump_revision();
        true
    }

    /// Applies a window, reporting whether anything changed.
    pub(crate) fn set_snapshot(&mut self, message: &StatisticsStreamMessage) -> bool {
        let StatisticsStreamMessage::Snapshot(snapshot) = message else {
            return false;
        };
        let Some(points) = D::points(&snapshot.points) else {
            return false;
        };
        if !self
            .data
            .set_snapshot(snapshot.revision, snapshot.metadata, points)
        {
            return false;
        }
        self.bump_revision();
        // A hover that no longer has a point under it is dropped rather than left
        // naming an interval the window does not hold.
        if self
            .hover
            .is_some_and(|hover| hover.index >= self.data.point_count())
        {
            self.hover = None;
        }
        true
    }

    /// Moves the revision and drops what was prepared from the previous view.
    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1).max(1);
        self.cache.prepared = None;
    }

    /// Rebuilds the canvas the chart draws, reporting whether it changed.
    ///
    /// The work is split in two: a *prepared* frame is rebuilt only when the box or the
    /// resolved view moved, and the *canvas* is rebuilt only when what it would draw
    /// moved — the prepared frame, the hover, the appearance, the line meshes, or the
    /// atlas lease of a label it was built from.
    pub(crate) fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<bool, EngineError> {
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

        let appearance = cx.theme().current(cx.app());
        let theme = cx.theme().theme_for(appearance).clone();
        let key = CacheKey {
            bounds,
            revision: self.revision,
        };
        if self.cache.prepared.as_ref().map(|(cached, ..)| *cached) != Some(key) {
            let measurements = measure(
                cx.app_mut(),
                &theme,
                &mut self.cache.text,
                self.data.axis_titles(),
                self.data.legend(),
                self.data.scales(),
            );
            let layout = ChartLayout::new(
                bounds,
                measurements.left_axis_width,
                measurements.right_axis_width,
            );
            let prepared = self.data.prepare(layout);
            self.cache.layout = Some(layout);
            self.replace_meshes(cx, &prepared);
            self.cache.prepared = Some((key, measurements, prepared));
        }

        let painted = PaintedKey {
            cache: key,
            hover: self.hover,
            meshes: self.cache.meshes,
            appearance,
        };
        // The painted key names what the canvas was built from, but not what its labels
        // were *leased* from: a shaped handle points into a shared atlas, and an atlas
        // generation change or a per-tile eviction can take every lease the canvas holds
        // without moving anything in the key. The engine drops such runs at encode time,
        // so a frame that kept this canvas would show a chart with its axis, interval and
        // tooltip labels missing — for good. A stale cache is a rebuild.
        let painted_already = self.cache.painted == Some(painted);
        if painted_already && !self.cache.text.has_stale() {
            return Ok(false);
        }

        let Some((_, measurements, prepared)) = self.cache.prepared.as_ref() else {
            return Ok(false);
        };
        let Some(layout) = self.cache.layout else {
            return Ok(false);
        };
        let tooltip = self.hover.and_then(|hover| self.data.tooltip(hover.index));
        let frame = Frame {
            layout,
            scales: self.data.scales(),
            axis_titles: self.data.axis_titles(),
            legend: self.data.legend(),
            legend_measurements: measurements.legend,
        };
        let (canvas, whole) = paint(
            cx.app_mut(),
            &theme,
            &mut self.cache.text,
            &frame,
            prepared,
            self.hover,
            tooltip.as_ref(),
            &self.cache.meshes,
        );
        // Shaping one label can evict another label's tiles, so a single pass is not
        // always a whole frame: a refused run is shaped again and pushed by a second
        // pass. A second refusal is left to the next frame rather than retried here, so a
        // chart whose labels do not fit the atlas cannot spin this hook.
        let (canvas, whole) = if whole {
            (canvas, whole)
        } else {
            paint(
                cx.app_mut(),
                &theme,
                &mut self.cache.text,
                &frame,
                prepared,
                self.hover,
                tooltip.as_ref(),
                &self.cache.meshes,
            )
        };
        let changed = cx.set_canvas(node, canvas)?;
        // An incomplete canvas is not what the chart draws: leaving the key unrecorded is
        // what makes the next frame build it again (I3 holds — a whole frame is still a
        // short circuit).
        if whole {
            self.cache.painted = Some(painted);
        }
        Ok(changed)
    }

    /// Releases the meshes of the previous frame and registers the ones this frame draws.
    fn replace_meshes(&mut self, cx: &mut WidgetCx<'_>, prepared: &Prepared) {
        for previous in self.cache.meshes.iter_mut().filter_map(Option::take) {
            cx.app_mut().geometry_mut().release(previous);
        }
        let lines: Vec<&[Option<Point>]> = match prepared {
            Prepared::Lines { series, .. } => series
                .iter()
                .filter_map(|geometry| geometry.points.as_deref())
                .collect(),
            Prepared::Shares { line, .. } => line.as_deref().into_iter().collect(),
            Prepared::Bars { .. } | Prepared::Nothing => Vec::new(),
        };
        for (index, points) in lines.into_iter().enumerate().take(2) {
            let Some(desc) = line_geometry_mesh(points, super::palette::LINE_WIDTH) else {
                continue;
            };
            match cx.app_mut().geometry_mut().insert(desc) {
                Ok(id) => self.cache.meshes[index] = Some(id),
                Err(error) => {
                    log::warn!("the statistics chart could not register a series line: {error}");
                }
            }
        }
    }

    /// Releases the geometry this chart registered and takes its canvas node down.
    ///
    /// The node is the chart's own: a chart that is dropped and re-created removes the
    /// node it drew into, because the pane's body is built only once and the next chart is
    /// built into the same host. A node the pane already removed is not a failure — the
    /// framework tears the body down after this hook.
    pub(crate) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        for mesh in self.cache.meshes.iter_mut().filter_map(Option::take) {
            cx.app_mut().geometry_mut().release(mesh);
        }
        if let Some(node) = self.node.take()
            && cx.tree().get(node).is_ok()
        {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, node);
        }
        self.cache.prepared = None;
        self.cache.painted = None;
        self.cache.layout = None;
        self.hover = None;
    }
}

/// A chart that can be drawn.
///
/// Every method is a question about the chart's own state; everything that is the same
/// for all three charts — the canvas node, the prepared frame, the shaped labels, the
/// pointer — is [`Chart`]'s.
pub(crate) trait ChartData: Sized + 'static {
    /// The unsized slice of points this chart plots.
    type Slice: ?Sized + 'static;

    /// The revision of the window this chart is showing, which is what the pane reports
    /// readiness against.
    fn revision(&self) -> u64;

    /// The points of this chart, if a snapshot holds its series.
    fn points(snapshot: &StatisticsPoints) -> Option<Arc<Self::Slice>>;

    /// How many points the frame is drawn from.
    fn point_count(&self) -> usize;

    /// The y-axes this chart draws: `(left, right)`. The right one is not optional —
    /// every chart is read against a scale on its right — while a chart with a single
    /// series leaves the left empty.
    fn scales(&self) -> (Option<&AxisScale>, &AxisScale);

    /// The legend of this chart, in the order its series are drawn.
    fn legend(&self) -> &[SeriesLabel];

    /// The titles of the axes: `(left, right)`; an empty one is not drawn.
    fn axis_titles(&self) -> (&str, &str);

    /// Folds this chart's window into the boxes and lines one frame draws.
    fn prepare(&self, layout: ChartLayout) -> Prepared;

    /// The point a hover lands on, if `index` is a point this chart annotates.
    fn hover_point(&self, index: usize) -> Option<usize>;

    /// The tooltip of one point.
    fn tooltip(&self, index: usize) -> Option<Tooltip>;

    /// Rewrites the chart's time labels in another display zone, reporting whether
    /// anything moved.
    fn set_time_zone(&mut self, zone: UtcOffset) -> bool;

    /// Re-resolves the names the chart draws when the language moved, reporting whether
    /// anything changed.
    fn set_locale(&mut self, translate: &Translate) -> bool;

    /// Rebuilds the chart from a new window, reporting whether anything changed.
    fn set_snapshot(
        &mut self,
        revision: u64,
        metadata: Option<ListingPrecision>,
        points: Arc<Self::Slice>,
    ) -> bool;
}

/// The chart of whichever series a pane is showing.
///
/// A pane is generic over its series at the type level, but its *body* has to name one
/// value, so the three types meet here. The enum is the only place in the crate where
/// they do.
pub(crate) enum ChartDispatch {
    OpenInterest(Chart<super::open_interest::OpenInterestChart>),
    TakerVolume(Chart<super::taker_volume::TakerVolumeChart>),
    Ratio(Chart<super::ratio::RatioChart>),
}

impl ChartDispatch {
    /// Builds the chart of one instrument for `metric`, or `None` for a series this
    /// module does not draw.
    ///
    /// The instrument arrives as its two assets rather than as an `Instrument`, because
    /// the only thing a chart does with it is name it on an axis and in a tooltip.
    pub(crate) fn for_metric(
        translate: &Translate,
        base: &str,
        quote: &str,
        zone: UtcOffset,
        metric: MetricKind,
    ) -> Option<Self> {
        Some(match metric {
            MetricKind::OpenInterest => Self::OpenInterest(Chart::new(
                super::open_interest::OpenInterestChart::new(translate, base, quote, zone),
            )),
            MetricKind::TakerVolume => Self::TakerVolume(Chart::new(
                super::taker_volume::TakerVolumeChart::new(translate, base, zone),
            )),
            MetricKind::TopTraderAccountRatio => {
                Self::Ratio(Chart::new(super::ratio::RatioChart::new(
                    translate,
                    "widget.perpetual_statistics.long_accounts",
                    "widget.perpetual_statistics.short_accounts",
                    "widget.perpetual_statistics.account_ratio",
                    zone,
                )))
            }
            MetricKind::TopTraderPositionRatio => {
                Self::Ratio(Chart::new(super::ratio::RatioChart::new(
                    translate,
                    "widget.perpetual_statistics.long_positions",
                    "widget.perpetual_statistics.short_positions",
                    "widget.perpetual_statistics.position_ratio",
                    zone,
                )))
            }
            MetricKind::LongShortAccountRatio => {
                Self::Ratio(Chart::new(super::ratio::RatioChart::new(
                    translate,
                    "widget.perpetual_statistics.long_accounts",
                    "widget.perpetual_statistics.short_accounts",
                    "widget.perpetual_statistics.account_ratio",
                    zone,
                )))
            }
        })
    }

    /// Builds the chart's canvas node into `parent`.
    pub(crate) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        match self {
            Self::OpenInterest(chart) => chart.build(cx, parent),
            Self::TakerVolume(chart) => chart.build(cx, parent),
            Self::Ratio(chart) => chart.build(cx, parent),
        }
    }

    /// The node the chart draws into, once it has been built.
    pub(crate) fn node(&self) -> Option<NodeId> {
        match self {
            Self::OpenInterest(chart) => chart.node(),
            Self::TakerVolume(chart) => chart.node(),
            Self::Ratio(chart) => chart.node(),
        }
    }

    /// Rebuilds the canvas from what the pane now shows.
    pub(crate) fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<bool, EngineError> {
        match self {
            Self::OpenInterest(chart) => chart.sync(cx),
            Self::TakerVolume(chart) => chart.sync(cx),
            Self::Ratio(chart) => chart.sync(cx),
        }
    }

    /// The revision of the window this chart is showing.
    pub(crate) fn revision(&self) -> u64 {
        match self {
            Self::OpenInterest(chart) => chart.revision(),
            Self::TakerVolume(chart) => chart.revision(),
            Self::Ratio(chart) => chart.revision(),
        }
    }

    /// How many intervals the chart is drawing, which is what says whether the window it
    /// holds has anything in it.
    pub(crate) fn point_count(&self) -> usize {
        match self {
            Self::OpenInterest(chart) => chart.point_count(),
            Self::TakerVolume(chart) => chart.point_count(),
            Self::Ratio(chart) => chart.point_count(),
        }
    }

    /// Rewrites every time label in another display zone.
    pub(crate) fn set_time_zone(&mut self, zone: UtcOffset) -> bool {
        match self {
            Self::OpenInterest(chart) => chart.set_time_zone(zone),
            Self::TakerVolume(chart) => chart.set_time_zone(zone),
            Self::Ratio(chart) => chart.set_time_zone(zone),
        }
    }

    /// Re-resolves the chart's labels when the language moved under it.
    pub(crate) fn set_locale(&mut self, translate: &Translate) -> bool {
        match self {
            Self::OpenInterest(chart) => chart.set_locale(translate),
            Self::TakerVolume(chart) => chart.set_locale(translate),
            Self::Ratio(chart) => chart.set_locale(translate),
        }
    }

    /// Applies a window the stream published.
    pub(crate) fn set_snapshot(&mut self, message: &StatisticsStreamMessage) -> bool {
        match self {
            Self::OpenInterest(chart) => chart.set_snapshot(message),
            Self::TakerVolume(chart) => chart.set_snapshot(message),
            Self::Ratio(chart) => chart.set_snapshot(message),
        }
    }

    /// Applies a pointer position, reporting whether the reading under it moved.
    pub(crate) fn point_at(&mut self, cursor: Point) -> bool {
        match self {
            Self::OpenInterest(chart) => chart.point_at(cursor),
            Self::TakerVolume(chart) => chart.point_at(cursor),
            Self::Ratio(chart) => chart.point_at(cursor),
        }
    }

    /// Records that the pointer left the plot, reporting whether it had been on it.
    pub(crate) fn clear_hover(&mut self) -> bool {
        match self {
            Self::OpenInterest(chart) => chart.clear_hover(),
            Self::TakerVolume(chart) => chart.clear_hover(),
            Self::Ratio(chart) => chart.clear_hover(),
        }
    }

    /// Releases the chart's meshes and its canvas node.
    pub(crate) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        match self {
            Self::OpenInterest(chart) => chart.release(cx),
            Self::TakerVolume(chart) => chart.release(cx),
            Self::Ratio(chart) => chart.release(cx),
        }
    }

    /// The titles the chart draws, which is where the units its labels name come from.
    #[cfg(test)]
    pub(crate) fn axis_titles_for_test(&self) -> (String, String) {
        let (left, right) = match self {
            Self::OpenInterest(chart) => chart.axis_titles_for_test(),
            Self::TakerVolume(chart) => chart.axis_titles_for_test(),
            Self::Ratio(chart) => chart.axis_titles_for_test(),
        };
        (left.to_owned(), right.to_owned())
    }

    /// The layout of the last prepared frame, which a hover test resolves against.
    #[cfg(test)]
    pub(crate) fn layout_for_test(&self) -> Option<ChartLayout> {
        match self {
            Self::OpenInterest(chart) => chart.layout_for_test(),
            Self::TakerVolume(chart) => chart.layout_for_test(),
            Self::Ratio(chart) => chart.layout_for_test(),
        }
    }

    /// The chart's canvas content, as a test reads the primitives it drew.
    #[cfg(test)]
    pub(crate) fn canvas_for_test<'a>(
        &self,
        app: &'a ui_core::app::App,
        window: ui_core::window::WindowId,
    ) -> Option<&'a Canvas> {
        match self {
            Self::OpenInterest(chart) => chart.canvas_for_test(app, window),
            Self::TakerVolume(chart) => chart.canvas_for_test(app, window),
            Self::Ratio(chart) => chart.canvas_for_test(app, window),
        }
    }
}

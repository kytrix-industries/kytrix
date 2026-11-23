use std::sync::Arc;

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
use ui_kit::{style_builder::StyleBuilder, theme::appearance::Appearance};
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CacheKey {
    pub bounds: Rect,
    pub revision: u64,
}

#[derive(Default)]
struct ChartCache {
    layout: Option<ChartLayout>,
    prepared: Option<(CacheKey, Measurements, Prepared)>,
    painted: Option<PaintedKey>,
    text: ChartTextCache,
    meshes: [Option<GeometryId>; 2],
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PaintedKey {
    cache: CacheKey,
    hover: Option<Hover>,
    meshes: [Option<GeometryId>; 2],
    appearance: Appearance,
}

pub(crate) struct Chart<D: ChartData> {
    data: D,
    revision: u64,
    cache: ChartCache,
    hover: Option<Hover>,
    node: Option<NodeId>,
}

impl<D: ChartData> Chart<D> {
    pub(crate) fn new(data: D) -> Self {
        Self {
            data,
            revision: 1,
            cache: ChartCache::default(),
            hover: None,
            node: None,
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.data.revision()
    }

    pub(crate) fn point_count(&self) -> usize {
        self.data.point_count()
    }

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
            .bind_style(move |read| {
                let _appearance = theme.appearance(read);
                StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        self.node = Some(node);
        Ok(node)
    }

    pub(crate) fn node(&self) -> Option<NodeId> {
        self.node
    }

    #[cfg(test)]
    pub(crate) fn layout_for_test(&self) -> Option<ChartLayout> {
        self.cache.layout
    }

    #[cfg(test)]
    pub(crate) fn axis_titles_for_test(&self) -> (&str, &str) {
        self.data.axis_titles()
    }

    #[cfg(test)]
    pub(crate) fn hover_for_test(&self) -> Option<Hover> {
        self.hover
    }

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

    pub(crate) fn point_at(&mut self, cursor: Point) -> bool {
        let next = self.hover_at(cursor);
        if self.hover == next {
            return false;
        }
        self.hover = next;
        true
    }

    pub(crate) fn clear_hover(&mut self) -> bool {
        self.hover.take().is_some()
    }

    pub(crate) fn set_time_zone(&mut self, zone: UtcOffset) -> bool {
        if !self.data.set_time_zone(zone) {
            return false;
        }
        self.bump_revision();
        true
    }

    pub(crate) fn set_locale(&mut self, translate: &Translate) -> bool {
        if !self.data.set_locale(translate) {
            return false;
        }
        self.bump_revision();
        true
    }

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
        if self
            .hover
            .is_some_and(|hover| hover.index >= self.data.point_count())
        {
            self.hover = None;
        }
        true
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1).max(1);
        self.cache.prepared = None;
    }

    pub(crate) fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<bool, EngineError> {
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
        if whole {
            self.cache.painted = Some(painted);
        }
        Ok(changed)
    }

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

pub(crate) trait ChartData: Sized + 'static {
    type Slice: ?Sized + 'static;

    fn revision(&self) -> u64;

    fn points(snapshot: &StatisticsPoints) -> Option<Arc<Self::Slice>>;

    fn point_count(&self) -> usize;

    fn scales(&self) -> (Option<&AxisScale>, &AxisScale);

    fn legend(&self) -> &[SeriesLabel];

    fn axis_titles(&self) -> (&str, &str);

    fn prepare(&self, layout: ChartLayout) -> Prepared;

    fn hover_point(&self, index: usize) -> Option<usize>;

    fn tooltip(&self, index: usize) -> Option<Tooltip>;

    fn set_time_zone(&mut self, zone: UtcOffset) -> bool;

    fn set_locale(&mut self, translate: &Translate) -> bool;

    fn set_snapshot(
        &mut self,
        revision: u64,
        metadata: Option<ListingPrecision>,
        points: Arc<Self::Slice>,
    ) -> bool;
}

pub(crate) enum ChartDispatch {
    OpenInterest(Chart<super::open_interest::OpenInterestChart>),
    TakerVolume(Chart<super::taker_volume::TakerVolumeChart>),
    Ratio(Chart<super::ratio::RatioChart>),
}

impl ChartDispatch {
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

    pub(crate) fn node(&self) -> Option<NodeId> {
        match self {
            Self::OpenInterest(chart) => chart.node(),
            Self::TakerVolume(chart) => chart.node(),
            Self::Ratio(chart) => chart.node(),
        }
    }

    pub(crate) fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<bool, EngineError> {
        match self {
            Self::OpenInterest(chart) => chart.sync(cx),
            Self::TakerVolume(chart) => chart.sync(cx),
            Self::Ratio(chart) => chart.sync(cx),
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        match self {
            Self::OpenInterest(chart) => chart.revision(),
            Self::TakerVolume(chart) => chart.revision(),
            Self::Ratio(chart) => chart.revision(),
        }
    }

    pub(crate) fn point_count(&self) -> usize {
        match self {
            Self::OpenInterest(chart) => chart.point_count(),
            Self::TakerVolume(chart) => chart.point_count(),
            Self::Ratio(chart) => chart.point_count(),
        }
    }

    pub(crate) fn set_time_zone(&mut self, zone: UtcOffset) -> bool {
        match self {
            Self::OpenInterest(chart) => chart.set_time_zone(zone),
            Self::TakerVolume(chart) => chart.set_time_zone(zone),
            Self::Ratio(chart) => chart.set_time_zone(zone),
        }
    }

    pub(crate) fn set_locale(&mut self, translate: &Translate) -> bool {
        match self {
            Self::OpenInterest(chart) => chart.set_locale(translate),
            Self::TakerVolume(chart) => chart.set_locale(translate),
            Self::Ratio(chart) => chart.set_locale(translate),
        }
    }

    pub(crate) fn set_snapshot(&mut self, message: &StatisticsStreamMessage) -> bool {
        match self {
            Self::OpenInterest(chart) => chart.set_snapshot(message),
            Self::TakerVolume(chart) => chart.set_snapshot(message),
            Self::Ratio(chart) => chart.set_snapshot(message),
        }
    }

    pub(crate) fn point_at(&mut self, cursor: Point) -> bool {
        match self {
            Self::OpenInterest(chart) => chart.point_at(cursor),
            Self::TakerVolume(chart) => chart.point_at(cursor),
            Self::Ratio(chart) => chart.point_at(cursor),
        }
    }

    pub(crate) fn clear_hover(&mut self) -> bool {
        match self {
            Self::OpenInterest(chart) => chart.clear_hover(),
            Self::TakerVolume(chart) => chart.clear_hover(),
            Self::Ratio(chart) => chart.clear_hover(),
        }
    }

    pub(crate) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        match self {
            Self::OpenInterest(chart) => chart.release(cx),
            Self::TakerVolume(chart) => chart.release(cx),
            Self::Ratio(chart) => chart.release(cx),
        }
    }

    #[cfg(test)]
    pub(crate) fn axis_titles_for_test(&self) -> (String, String) {
        let (left, right) = match self {
            Self::OpenInterest(chart) => chart.axis_titles_for_test(),
            Self::TakerVolume(chart) => chart.axis_titles_for_test(),
            Self::Ratio(chart) => chart.axis_titles_for_test(),
        };
        (left.to_owned(), right.to_owned())
    }

    #[cfg(test)]
    pub(crate) fn layout_for_test(&self) -> Option<ChartLayout> {
        match self {
            Self::OpenInterest(chart) => chart.layout_for_test(),
            Self::TakerVolume(chart) => chart.layout_for_test(),
            Self::Ratio(chart) => chart.layout_for_test(),
        }
    }

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

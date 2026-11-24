use std::sync::Arc;

use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use smol_str::SmolStr;
use time::UtcOffset;
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    error::EngineError,
    math::{LogicalPoint, LogicalPx, LogicalRect, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{canvas::Canvas, geometry::GeometryId, style::StylePatch, style_builder::StyleBuilder},
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

const CHART_SELECTOR: &str = "TEST_PREDICTION_MARKET_CHART";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TimeTick {
    pub timestamp: i64,
    pub x: LogicalPx,
    pub label: SmolStr,
}

pub(crate) struct PreparedSeries {
    pub label: SmolStr,
    pub color: usize,
    pub samples: Arc<[Sample]>,
    pub screen: Arc<[ScreenPoint]>,
    pub last: Option<ScreenPoint>,
}

impl PreparedSeries {
    pub(crate) fn covers(&self, timestamp: i64) -> bool {
        self.samples
            .last()
            .is_some_and(|last| timestamp <= last.timestamp)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct SeriesMeshes {
    pub line: Option<GeometryId>,
    pub halo: Option<GeometryId>,
}

pub(crate) struct PreparedChart {
    pub layout: ChartLayout,
    pub domain: Option<TimeDomain>,
    pub axis: ValueAxis,
    pub axis_labels: Arc<[SmolStr]>,
    pub time_ticks: Arc<[TimeTick]>,
    pub series: Arc<[PreparedSeries]>,
    pub hover_times: Arc<[i64]>,
    pub time_zone: UtcOffset,
}

impl PreparedChart {
    fn new(
        snapshot: &PredictionMarketSnapshot,
        bounds: LogicalRect,
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

    pub(crate) fn hover_at(&self, pointer: LogicalPoint) -> Option<HoveredSample> {
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

fn samples_of(series: &PredictionMarketSeries) -> Vec<Sample> {
    let mut samples: Vec<Sample> = series
        .points
        .iter()
        .map(|point| Sample {
            timestamp: point.timestamp,
            percent: percent(point.probability),
        })
        .collect();
    samples.sort_by_key(|sample| sample.timestamp);
    samples
}

struct PreparedKey {
    snapshot: Arc<PredictionMarketSnapshot>,
    bounds: LogicalRect,
    time_zone: UtcOffset,
    language: LanguageCode,
    text_scale: f32,
}

impl PreparedKey {
    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.snapshot, &other.snapshot)
            && self.bounds == other.bounds
            && self.time_zone == other.time_zone
            && self.language == other.language
            && self.text_scale == other.text_scale
    }
}

#[derive(Clone, Debug)]
struct PaintedKey {
    snapshot: Arc<PredictionMarketSnapshot>,
    bounds: LogicalRect,
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

pub(crate) struct ChartCanvas {
    node: Option<NodeId>,
    prepared: Option<(PreparedKey, PreparedChart)>,
    painted: Option<PaintedKey>,
    painted_text: Vec<Arc<ShapedTextHandle>>,
    text: ChartTextCache,
    meshes: [SeriesMeshes; MAX_LINES],
    #[cfg(test)]
    builds: usize,
    #[cfg(test)]
    hovered: Option<HoveredSample>,
}

impl ChartCanvas {
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

    pub(crate) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let label = t_smol!(translate, "widget.names.prediction_market");
        let node = NodeBuilder::canvas(Canvas::new(LogicalRect::default()))
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

    #[cfg(test)]
    pub(crate) fn node(&self) -> Option<NodeId> {
        self.node
    }

    pub(crate) fn clear(&mut self) {
        self.prepared = None;
        self.painted = None;
        self.painted_text.clear();
    }

    pub(crate) fn sync(
        &mut self,
        cx: &mut WidgetCx<'_>,
        snapshot: &Arc<PredictionMarketSnapshot>,
        pointer: Option<LogicalPoint>,
        pulse_phase: Option<f32>,
    ) -> Result<bool, EngineError> {
        let Some(node) = self.node else {
            return Ok(false);
        };
        let Ok(target) = cx.tree().get(node) else {
            return Ok(false);
        };
        let bounds = whole_pixel_bounds(target.layout().bounds);
        if bounds.size.width <= px(0.0) || bounds.size.height <= px(0.0) {
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
        if whole {
            self.painted = Some(painted);
            self.painted_text = used;
        }
        Ok(changed)
    }

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

    #[cfg(test)]
    pub(crate) fn prepared_builds(&self) -> usize {
        self.builds
    }

    #[cfg(test)]
    pub(crate) fn hovered_sample(&self) -> Option<HoveredSample> {
        self.hovered
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

    #[cfg(test)]
    pub(crate) fn clear_for_test(&mut self) {
        self.clear();
    }
}

fn register_line(
    cx: &mut WidgetCx<'_>,
    screen: &[ScreenPoint],
    width: LogicalPx,
) -> Option<GeometryId> {
    let desc = line_geometry(screen, width)?;
    match cx.app_mut().geometry_mut().insert(desc) {
        Ok(id) => Some(id),
        Err(error) => {
            log::warn!("the prediction chart could not register a series line: {error}");
            None
        }
    }
}

fn whole_pixel_bounds(bounds: LogicalRect) -> LogicalRect {
    LogicalRect::new(
        px(bounds.origin.x.value().round()),
        px(bounds.origin.y.value().round()),
        px(bounds.size.width.value().round()),
        px(bounds.size.height.value().round()),
    )
}

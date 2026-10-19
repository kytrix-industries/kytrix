use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use gpui_kit::{
    AnyElement, App, Bounds, ElementId, InteractiveElement, IntoElement, MouseMoveEvent,
    ParentElement, Pixels, Point, ScrollHandle, SharedString, Size, StatefulInteractiveElement,
    Styled, Window, base::ElementExt, div, point, px,
};
use i18n::language::codes::LanguageCode;
use market_participants_model::listing::rules::ListingPrecision;
use time::UtcOffset;

use super::{
    axes::{AxisScale, ChartLayout, nearest_index},
    paint::{
        CacheKey, FrameKey, Hover, Measurements, PaintContext, Prepared, Tooltip, content_min_size,
        measure,
    },
    palette::SeriesLabel,
    text::ChartTextCache,
};
pub(crate) use super::{
    open_interest::OpenInterestChart, ratio::RatioChart, taker_volume::TakerVolumeChart,
};
use crate::{
    model::{MetricKind, StatisticsPoints},
    service::StatisticsStreamMessage,
};

/// How close to the end a scroll offset has to be to count as following it.
const SCROLL_END_TOLERANCE: f32 = 1.0;

/// A pointer position the chart has queued for the next frame.
///
/// Pointer moves arrive in bursts — a mouse crossing a plot produces one event per
/// pixel — so a position is stored and committed once, on the frame that follows
/// it, rather than rendering a frame per pixel.
#[derive(Default)]
struct HoverFrame {
    current: Option<Hover>,
    pending: Option<Hover>,
    scheduled: bool,
}

impl HoverFrame {
    /// Records where the pointer is, reporting whether a frame is worth asking
    /// for.
    fn queue(&mut self, next: Option<Hover>) -> bool {
        self.pending = next;
        if self.scheduled || self.current == next {
            return false;
        }
        self.scheduled = true;
        true
    }

    /// Applies the queued position, reporting whether anything moved.
    fn commit(&mut self) -> bool {
        self.scheduled = false;
        if self.current == self.pending {
            return false;
        }
        self.current = self.pending;
        true
    }

    /// Applies the queued position and reports where the pointer is now.
    ///
    /// This is the whole of the frame's pointer handling: a move records a
    /// position, and the next paint applies it. Nothing else has to know that the
    /// two are a frame apart.
    fn commit_current(&mut self) -> Option<Hover> {
        self.commit();
        self.current
    }

    /// Drops a hover that no longer has a point under it.
    fn retain(&mut self, point_count: usize) {
        for slot in [&mut self.current, &mut self.pending] {
            if slot.is_some_and(|hover| hover.index >= point_count) {
                *slot = None;
            }
        }
    }
}

/// Everything one chart reuses between frames.
///
/// It is held behind an `Rc` because the element that draws a chart is rebuilt
/// every frame while this is not: the canvas callbacks take a clone of the `Rc`,
/// and the entity keeps the original.
pub(crate) struct ChartState {
    /// The box the plot was last laid out in, which is how a pointer position
    /// becomes a point.
    layout: Cell<Option<ChartLayout>>,
    pub text: RefCell<ChartTextCache>,
    hover: RefCell<HoverFrame>,
    pub cache: RefCell<PreparedCache>,
    horizontal: ScrollHandle,
    vertical: ScrollHandle,
    initialized: Cell<bool>,
    last_horizontal_max: Cell<f32>,
    last_vertical_max: Cell<f32>,
}

impl Default for ChartState {
    fn default() -> Self {
        Self {
            layout: Cell::new(None),
            text: RefCell::new(ChartTextCache::default()),
            hover: RefCell::new(HoverFrame::default()),
            cache: RefCell::new(PreparedCache::default()),
            horizontal: ScrollHandle::new(),
            vertical: ScrollHandle::new(),
            initialized: Cell::new(false),
            last_horizontal_max: Cell::new(0.0),
            last_vertical_max: Cell::new(0.0),
        }
    }
}

impl ChartState {
    /// The box the plot was last laid out in, which is how a pointer position
    /// becomes a point.
    fn layout(&self) -> Option<ChartLayout> {
        self.layout.get()
    }

    /// The box the last frame laid the plot out in.
    #[cfg(test)]
    pub(crate) fn plot(&self) -> Option<Bounds<Pixels>> {
        self.layout().map(|layout| layout.plot)
    }

    /// Every box the last frame laid out, which is what says whether a chart
    /// filled the box it was given.
    #[cfg(test)]
    pub(crate) fn boxes(&self) -> Option<ChartLayout> {
        self.layout()
    }

    /// The point the pointer is over, which is what a hover test drives.
    #[cfg(test)]
    pub(crate) fn hovered(&self) -> Option<Hover> {
        self.hover.borrow().current
    }

    /// The room a frame has to move the chart sideways in.
    ///
    /// It is the box the chart's own scroll container was given and the offset it
    /// can reach inside it, which together say whether a pane narrower than the
    /// chart's minimum can be read at all.
    #[cfg(test)]
    pub(crate) fn horizontal_scroll(&self) -> ScrollHandle {
        self.horizontal.clone()
    }

    /// Brings the scroll offsets in range for a viewport that changed size,
    /// reporting whether anything moved.
    ///
    /// An offset sitting at the old end follows to the new one, because the newest
    /// interval is where a chart is read from; an offset the user chose is kept
    /// where it is.
    fn sync_viewport(&self, viewport: Size<Pixels>, content: Size<Pixels>) -> bool {
        let horizontal_max = f32::from(content.width - viewport.width).max(0.0);
        let vertical_max = f32::from(content.height - viewport.height).max(0.0);
        let current_horizontal = f32::from(self.horizontal.offset().x);
        let current_vertical = f32::from(self.vertical.offset().y);
        let at_newest =
            (-current_horizontal - self.last_horizontal_max.get()).abs() <= SCROLL_END_TOLERANCE;
        let horizontal = if !self.initialized.get() || at_newest {
            -horizontal_max
        } else {
            current_horizontal.clamp(-horizontal_max, 0.0)
        };
        let vertical = current_vertical.clamp(-vertical_max, 0.0);
        let changed = !self.initialized.get()
            || (horizontal - current_horizontal).abs() > f32::EPSILON
            || (vertical - current_vertical).abs() > f32::EPSILON
            || (horizontal_max - self.last_horizontal_max.get()).abs() > f32::EPSILON
            || (vertical_max - self.last_vertical_max.get()).abs() > f32::EPSILON;

        if (horizontal - current_horizontal).abs() > f32::EPSILON {
            self.horizontal.set_offset(point(px(horizontal), px(0.0)));
        }
        if (vertical - current_vertical).abs() > f32::EPSILON {
            self.vertical.set_offset(point(px(0.0), px(vertical)));
        }
        self.initialized.set(true);
        self.last_horizontal_max.set(horizontal_max);
        self.last_vertical_max.set(vertical_max);
        changed
    }

    /// Follows the newest data again, for a chart whose window was replaced.
    fn follow_newest(&self) {
        self.initialized.set(false);
    }

    /// The layout of a frame that was already prepared from these inputs.
    fn cached_layout(&self, key: &CacheKey) -> Option<ChartLayout> {
        let cache = self.cache.borrow();
        cache
            .key
            .as_ref()
            .filter(|cached| *cached == key)
            .and_then(|_| cache.layout)
    }

    /// Keeps a prepared frame until its inputs change.
    fn remember(
        &self,
        key: CacheKey,
        layout: ChartLayout,
        measurements: Measurements,
        prepared: Prepared,
    ) {
        let mut cache = self.cache.borrow_mut();
        cache.key = Some(key);
        cache.layout = Some(layout);
        cache.measurements = measurements;
        cache.prepared = prepared;
    }
}

/// The frame one chart prepared last, kept until its inputs change.
#[derive(Default)]
pub(crate) struct PreparedCache {
    key: Option<CacheKey>,
    pub measurements: Measurements,
    pub prepared: Prepared,
    pub layout: Option<ChartLayout>,
}

/// One chart: its own state, the caches a frame reuses, and the viewport it
/// scrolls in.
pub(crate) struct Chart<D: ChartData> {
    data: Rc<RefCell<D>>,
    state: Rc<ChartState>,
}

impl<D: ChartData> Chart<D> {
    fn new(data: D) -> Self {
        Self {
            data: Rc::new(RefCell::new(data)),
            state: Rc::new(ChartState::default()),
        }
    }

    /// Rewrites the chart's time labels in another display zone.
    ///
    /// The prepared frame is keyed by the zone, so the next paint rebuilds what the
    /// labels moved rather than serving the frame from before the change.
    pub(super) fn set_time_zone(&mut self, zone: UtcOffset) -> bool {
        self.data.borrow_mut().set_time_zone(zone)
    }

    /// Rebuilds the chart from a new window, reporting whether anything changed.
    pub(super) fn set_snapshot(
        &mut self,
        revision: u64,
        metadata: Option<ListingPrecision>,
        points: Arc<D::Slice>,
    ) -> bool {
        if !self
            .data
            .borrow_mut()
            .set_snapshot(revision, metadata, points)
        {
            return false;
        }
        let point_count = self.data.borrow().point_count();
        self.state.hover.borrow_mut().retain(point_count);
        self.state.follow_newest();
        true
    }

    /// The box the content needs at minimum, which is what the pane it is in
    /// scrolls to.
    ///
    /// It is the measured labels' answer rather than the pane's, so a pane that
    /// shrank gets a smaller chart back: before the first frame has measured
    /// anything it is the chart's own floor.
    fn content_min_size(&self) -> Size<Pixels> {
        content_min_size(self.state.cache.borrow().measurements)
    }

    /// Resolves where the pointer is, asking for the frame that will draw it.
    fn track_pointer(&self, cursor: Point<Pixels>, window: &mut Window) {
        let next = self.state.layout().and_then(|layout| {
            if !layout.plot.contains(&cursor) {
                return None;
            }
            let data = self.data.borrow();
            nearest_index(layout.plot, data.point_count(), cursor.x)
                .and_then(|index| data.hover_point(index, cursor, layout))
        });
        if self.state.hover.borrow_mut().queue(next) {
            window.on_next_frame(|window, _| window.refresh());
        }
    }

    /// The revision of the window this chart is showing.
    fn revision(&self) -> u64 {
        self.data.borrow().revision()
    }

    /// How many intervals the window this chart is showing holds.
    fn point_count(&self) -> usize {
        self.data.borrow().point_count()
    }

    /// The state a frame reuses, which is what a hover test drives.
    #[cfg(test)]
    fn state(&self) -> Rc<ChartState> {
        self.state.clone()
    }

    /// The entity a pane embeds, which is the chart and the pointer handling it
    /// needs.
    fn entity(&self) -> AnyElement {
        let content_min = self.content_min_size();
        chart_element(self.data.clone(), self.state.clone(), content_min)
    }

    /// Records that the pointer left the chart.
    fn clear_pointer(&self, window: &mut Window) {
        if self.state.hover.borrow_mut().queue(None) {
            window.on_next_frame(|window, _| window.refresh());
        }
    }
}

/// Builds the element of a chart from the two things its frame needs.
///
/// The element is built from a pair of `Rc`s rather than from a `Chart`, because a
/// pane reaches its chart without a `Context` for it: the same pair is what a frame
/// of either closure below captures.
fn chart_element<D: ChartData>(
    data: Rc<RefCell<D>>,
    state: Rc<ChartState>,
    content_min: Size<Pixels>,
) -> AnyElement {
    let prepaint_data = data.clone();
    let prepaint_state = state.clone();
    let paint_data = data.clone();
    let paint_state = state.clone();
    let pointer_data = data.clone();
    let pointer_state = state.clone();
    let sync_state = state.clone();
    let content_state = state.clone();
    let leave_data = data;
    let leave_state = state;
    let element = gpui_kit::canvas(
        move |bounds, window, cx| {
            let locale = *cx.global::<LanguageCode>();
            let data = prepaint_data.borrow();
            let axis_titles = data.axis_titles(cx);
            let legend = data.legend(cx);
            let key = CacheKey {
                bounds,
                locale,
                frame: data.frame_key(bounds),
            };
            if let Some(layout) = prepaint_state.cached_layout(&key) {
                prepaint_state.layout.set(Some(layout));
                return layout;
            }
            let measurements = measure(
                (&axis_titles.0, &axis_titles.1),
                &legend,
                data.scales(),
                &mut PaintContext {
                    text_cache: &mut prepaint_state.text.borrow_mut(),
                    window,
                    app: cx,
                },
            );
            let layout = ChartLayout::new(
                bounds,
                measurements.left_axis_width,
                measurements.right_axis_width,
            );
            let prepared = data.prepare(layout);
            prepaint_state.remember(key, layout, measurements, prepared);
            layout
        },
        move |_, layout, window, app| {
            // A queued pointer position is applied here, on the frame after the
            // move that queued it: a burst of moves then costs one frame rather
            // than one frame per pixel. Applying it in a paint pass is what makes
            // it happen at all — a chart is a canvas inside the pane's body, not an
            // entity of its own, so nothing renders it and nothing would ever
            // commit a hover it had queued.
            let data = paint_data.borrow();
            let scales = data.scales();
            let axis_titles = data.axis_titles(app);
            let legend = data.legend(app);
            let hover = paint_state.hover.borrow_mut().commit_current();
            let tooltip = hover.and_then(|hover| data.tooltip(hover.index));
            super::paint::paint(
                layout,
                scales,
                (&axis_titles.0, &axis_titles.1),
                &legend,
                hover,
                tooltip.as_ref(),
                paint_state.as_ref(),
                window,
                app,
            );
        },
    )
    .size_full();

    // A chart's axes need room, and a pane narrower than that scrolls rather than
    // crushing the labels. The frame fills the content box it resolved, and the
    // shell below scrolls to it.
    //
    // The two sizes on the content are not the same thing, which is why one is a
    // floor and the other a demand: `min_h` is the height below which the chart is
    // not drawn, and `h_full` is the height it actually takes. The distinction
    // matters because a scroll container lays its child out against an indefinite
    // height — a `size_full()` there resolves to the floor and the chart would keep
    // its minimum height for ever, drawing in the top of a pane ten times taller.
    let content = div()
        .id(ElementId::Name(SharedString::new_static(
            "StatisticsChartContent",
        )))
        .relative()
        .min_w(content_min.width)
        .min_h(content_min.height)
        .h_full()
        .w_full()
        .on_prepaint(move |bounds, window, _| {
            if let Some(layout) = sync_state.layout()
                && sync_state.sync_viewport(bounds.size, layout.content.size)
            {
                // The offsets moved under a stationary pane, so the frame that
                // just laid this out is already out of date.
                window.refresh();
            }
        })
        .child(element)
        .on_mouse_move(move |event: &MouseMoveEvent, window, _| {
            let chart = Chart {
                data: pointer_data.clone(),
                state: pointer_state.clone(),
            };
            chart.track_pointer(event.position, window);
        })
        .on_hover(move |hovering, window, _| {
            if !*hovering {
                let chart = Chart {
                    data: leave_data.clone(),
                    state: leave_state.clone(),
                };
                chart.clear_pointer(window);
            }
        });

    // Two scroll containers rather than one: a narrow pane has to reach both the
    // left axis and the end of the legend, and one container cannot move in two
    // directions. The vertical one is the shell, so the frame always fills the
    // pane's height and only the width scrolls when the axes need the room.
    let horizontal = content_state.horizontal.clone();
    let vertical = content_state.vertical.clone();
    div()
        .id(ElementId::Name(SharedString::new_static(
            "StatisticsChartVertical",
        )))
        .relative()
        .size_full()
        .min_h_0()
        .flex()
        .flex_col()
        .overflow_y_scroll()
        .track_scroll(&vertical)
        .child(
            div()
                .id(ElementId::Name(SharedString::new_static(
                    "StatisticsChartHorizontal",
                )))
                .min_h(content_min.height)
                // The horizontal box has to claim the height the frame draws in.
                // A scroll container lays its child out against an indefinite
                // height, so a child that asked for `h_full` there would resolve
                // to its floor and the chart would be drawn in the top of a pane
                // many times taller.
                .flex_1()
                .w_full()
                .overflow_x_scroll()
                .track_scroll(&horizontal)
                .child(content),
        )
        .into_any_element()
}

/// The chart of whichever series a pane is showing.
///
/// A pane is generic over its series at the type level, but its *body* has to name
/// one value, so the three types meet here. The enum is the only place in the
/// crate where they do.
pub(crate) enum ChartDispatch {
    OpenInterest(Chart<OpenInterestChart>),
    TakerVolume(Chart<TakerVolumeChart>),
    Ratio(Chart<RatioChart>),
}

impl ChartDispatch {
    /// Builds the chart of one instrument for `metric`, or `None` for a series this
    /// module does not draw.
    ///
    /// The instrument arrives as its two assets rather than as an `Instrument`,
    /// because the only thing a chart does with it is name it on an axis and in a
    /// tooltip.
    pub(crate) fn for_metric(
        cx: &mut App,
        base: &str,
        quote: &str,
        metric: MetricKind,
    ) -> Option<ChartDispatch> {
        match metric {
            MetricKind::OpenInterest => Some(ChartDispatch::OpenInterest(Chart::new(
                OpenInterestChart::new(cx, base, quote),
            ))),
            MetricKind::TakerVolume => Some(ChartDispatch::TakerVolume(Chart::new(
                TakerVolumeChart::new(cx, base),
            ))),
            MetricKind::TopTraderAccountRatio => {
                Some(ChartDispatch::Ratio(Chart::new(RatioChart::new(
                    cx,
                    "widget.perpetual_statistics.long_accounts",
                    "widget.perpetual_statistics.short_accounts",
                    "widget.perpetual_statistics.account_ratio",
                ))))
            }
            MetricKind::TopTraderPositionRatio => {
                Some(ChartDispatch::Ratio(Chart::new(RatioChart::new(
                    cx,
                    "widget.perpetual_statistics.long_positions",
                    "widget.perpetual_statistics.short_positions",
                    "widget.perpetual_statistics.position_ratio",
                ))))
            }
            MetricKind::LongShortAccountRatio => {
                Some(ChartDispatch::Ratio(Chart::new(RatioChart::new(
                    cx,
                    "widget.perpetual_statistics.long_accounts",
                    "widget.perpetual_statistics.short_accounts",
                    "widget.perpetual_statistics.account_ratio",
                ))))
            }
        }
    }

    fn as_chart(&self) -> ChartRef {
        match self {
            Self::OpenInterest(chart) => ChartRef {
                revision: chart.revision(),
                entity: chart.entity(),
            },
            Self::TakerVolume(chart) => ChartRef {
                revision: chart.revision(),
                entity: chart.entity(),
            },
            Self::Ratio(chart) => ChartRef {
                revision: chart.revision(),
                entity: chart.entity(),
            },
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.as_chart().revision
    }

    /// How many intervals the chart is drawing, which is what says whether the
    /// window it holds has anything in it.
    pub(crate) fn point_count(&self) -> usize {
        match self {
            Self::OpenInterest(chart) => chart.point_count(),
            Self::TakerVolume(chart) => chart.point_count(),
            Self::Ratio(chart) => chart.point_count(),
        }
    }

    pub(crate) fn entity(&self) -> AnyElement {
        self.as_chart().entity
    }

    /// The pointer state of the chart, which is what a hover test drives.
    #[cfg(test)]
    pub(crate) fn state(&self) -> Rc<ChartState> {
        match self {
            Self::OpenInterest(chart) => chart.state(),
            Self::TakerVolume(chart) => chart.state(),
            Self::Ratio(chart) => chart.state(),
        }
    }

    /// The room the chart has to move itself sideways in, which is what says how a
    /// pane narrower than the chart's minimum behaves.
    #[cfg(test)]
    pub(crate) fn horizontal_scroll(&self) -> ScrollHandle {
        self.state().horizontal_scroll()
    }

    /// Rewrites every time label in another display zone.
    ///
    /// Returns whether anything moved, which is what keeps a frame that redrew for
    /// the setting from claiming a change no label made.
    pub(crate) fn set_time_zone(&mut self, zone: UtcOffset) -> bool {
        match self {
            Self::OpenInterest(chart) => chart.set_time_zone(zone),
            Self::TakerVolume(chart) => chart.set_time_zone(zone),
            Self::Ratio(chart) => chart.set_time_zone(zone),
        }
    }

    pub(crate) fn set_snapshot(&mut self, message: &StatisticsStreamMessage) -> bool {
        let StatisticsStreamMessage::Snapshot(snapshot) = message else {
            return false;
        };
        let (revision, metadata) = (snapshot.revision, snapshot.metadata);
        match self {
            Self::OpenInterest(chart) => OpenInterestChart::points(&snapshot.points)
                .is_some_and(|points| chart.set_snapshot(revision, metadata, points)),
            Self::TakerVolume(chart) => TakerVolumeChart::points(&snapshot.points)
                .is_some_and(|points| chart.set_snapshot(revision, metadata, points)),
            Self::Ratio(chart) => RatioChart::points(&snapshot.points)
                .is_some_and(|points| chart.set_snapshot(revision, metadata, points)),
        }
    }
}

/// One chart, as the pane above it needs to see it.
struct ChartRef {
    revision: u64,
    entity: AnyElement,
}

/// A chart that can be drawn.
///
/// Every method is a question about the chart's own state; everything that is the
/// same for all three charts — the viewport, the shaped-text cache, the prepared
/// frame, the pointer handling — is [`Chart`]'s.
pub(crate) trait ChartData: Sized + 'static {
    /// The unsized slice of points this chart plots.
    type Slice: ?Sized + 'static;

    /// The revision of the window this chart is showing, which is what the pane
    /// reports readiness against.
    fn revision(&self) -> u64;

    /// The points of this chart, if a snapshot holds its series.
    fn points(snapshot: &StatisticsPoints) -> Option<Arc<Self::Slice>>;

    /// How many points the frame is drawn from.
    fn point_count(&self) -> usize;

    /// The part of this chart's own state that a prepared frame depends on beyond
    /// its box.
    fn frame_key(&self, bounds: Bounds<Pixels>) -> FrameKey;

    /// The y-axes this chart draws: `(left, right, share)`.
    fn scales(&self) -> (Option<&AxisScale>, &AxisScale, Option<&AxisScale>);

    /// The legend of this chart, in the order its series are drawn.
    fn legend(&self, cx: &App) -> Box<[SeriesLabel]>;

    /// The titles of the axes: `(left, right)`.
    fn axis_titles(&self, cx: &App) -> (Option<SharedString>, SharedString);

    /// The point a hover lands on, if `index` is a point this chart annotates.
    fn hover_point(
        &self,
        index: usize,
        cursor: Point<Pixels>,
        layout: ChartLayout,
    ) -> Option<Hover>;

    /// Folds this chart's window into the boxes and paths one frame draws.
    fn prepare(&self, layout: ChartLayout) -> Prepared;

    /// The tooltip of one point.
    fn tooltip(&self, index: usize) -> Option<Tooltip>;

    /// Rewrites the chart's time labels in another display zone, reporting
    /// whether anything moved.
    fn set_time_zone(&mut self, zone: UtcOffset) -> bool;

    /// Rebuilds the chart from a new window, reporting whether anything changed.
    fn set_snapshot(
        &mut self,
        revision: u64,
        metadata: Option<ListingPrecision>,
        points: Arc<Self::Slice>,
    ) -> bool;
}

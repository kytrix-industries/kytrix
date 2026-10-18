//! The canvas one ETF chart is: its state, its viewport, and its pointer.
//!
//! A chart is an element rather than an entity, and the difference matters. The
//! pane's body is already the frame's cache unit, so a chart that were an entity of
//! its own would add a second cache boundary for nothing; what it needs instead is
//! somewhere to keep the things a frame reuses — the box it was last laid out in,
//! the shaped text, the prepared boxes and paths — and that is [`ChartState`], held
//! behind an `Rc` because the element that draws the chart is rebuilt every frame
//! while the state is not.
//!
//! The pointer is followed one frame behind, on purpose: a mouse crossing a plot
//! produces one move event per pixel, and a chart that rendered a frame per pixel
//! would spend the whole gesture drawing. A move records where the pointer is, and
//! the next frame — which the chart asks for — applies it.
//!
//! The two scroll containers are what keeps a narrow pane readable: the axes have a
//! minimum width, so a chart narrower than that scrolls sideways rather than
//! crushing its labels, and the frame always fills the pane's height.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use gpui_kit::{
    AnyElement, App, CursorStyle, ElementId, InteractiveElement, IntoElement, MouseMoveEvent,
    ParentElement, Pixels, Point, ScrollHandle, SharedString, Size, StatefulInteractiveElement,
    Styled, Window, accesskit::Role, base::ElementExt, canvas, div, point, px,
};
use i18n::{language::codes::LanguageCode, t};

use super::{
    axes::{ChartLayout, content_min_size, nearest_index},
    data::{Metrics, PreparedData},
    paint::{CacheKey, Hover, Measurements, PaintContext, PreparedFrame, measure, paint, prepare},
    text::ChartTextCache,
};
use crate::model::{EtfDataset, EtfFundSelection, EtfWindow};

/// How close to the end a scroll offset has to be to count as following it.
const SCROLL_END_TOLERANCE: f32 = 1.0;

/// A pointer position the chart has queued for the next frame.
///
/// Only the pointed-at reading and the pointer's height are kept: the x a hover
/// resolves to is the reading's own, and the height is rounded to whole pixels so a
/// pointer that moved less than a pixel is not a change worth a frame.
#[derive(Default)]
struct HoverFrame {
    current: Option<Hover>,
    pending: Option<Hover>,
    scheduled: bool,
}

impl HoverFrame {
    /// Records where the pointer is, reporting whether a frame is worth asking for.
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
    /// The box the plot was last laid out in.
    fn layout(&self) -> Option<ChartLayout> {
        self.layout.get()
    }

    /// Every box the last frame laid out, which is what says whether a chart filled
    /// the box it was given.
    #[cfg(test)]
    pub(crate) fn boxes(&self) -> Option<ChartLayout> {
        self.layout.get()
    }

    /// The point the pointer is over, which is what a hover test drives.
    #[cfg(test)]
    pub(crate) fn hovered(&self) -> Option<Hover> {
        self.hover.borrow().current
    }

    /// Brings the scroll offsets in range for a viewport that changed size,
    /// reporting whether anything moved.
    ///
    /// An offset sitting at the old end follows to the new one, because the newest
    /// days are where a chart is read from; an offset the user chose is kept where
    /// it is.
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
        prepared: PreparedFrame,
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
    pub prepared: PreparedFrame,
    pub layout: Option<ChartLayout>,
}

/// One ETF chart: the view it resolved, the caches a frame reuses, and the
/// viewport it scrolls in.
pub(crate) struct EtfChart {
    data: Rc<RefCell<PreparedData>>,
    /// Moves whenever `data` is rebuilt, which is how a prepared frame knows the
    /// window under it changed.
    revision: u64,
    state: Rc<ChartState>,
}

impl EtfChart {
    /// Resolves one history into the view a chart draws.
    pub(crate) fn new(
        cx: &App,
        dataset: Arc<EtfDataset>,
        selection: EtfFundSelection,
        window: EtfWindow,
    ) -> Self {
        Self {
            data: Rc::new(RefCell::new(PreparedData::new(
                cx, dataset, selection, window,
            ))),
            revision: 1,
            state: Rc::new(ChartState::default()),
        }
    }

    /// Points the chart at a history, a window or a fund, reporting whether
    /// anything about it changed.
    ///
    /// The dataset is compared by pointer as well as by value: a refresh that
    /// returned the same days hands over the same allocation, and a chart that
    /// re-derived its labels from it would do work nobody can see. The locale is part
    /// of the comparison because the labels are: the same days in another language
    /// are another thing on screen, and this is the one place that decides whether to
    /// write them again.
    pub(crate) fn set_view(
        &mut self,
        cx: &App,
        dataset: Arc<EtfDataset>,
        selection: EtfFundSelection,
        window: EtfWindow,
    ) -> bool {
        let locale = *cx.global::<LanguageCode>();
        {
            let data = self.data.borrow();
            if data.locale == locale
                && Arc::ptr_eq(&data.source, &dataset)
                && data.selection == selection
                && data.window == window
            {
                return false;
            }
        }
        *self.data.borrow_mut() = PreparedData::new(cx, dataset, selection, window);
        self.revision = self.revision.wrapping_add(1).max(1);
        self.state
            .hover
            .borrow_mut()
            .retain(self.data.borrow().points.len());
        self.state.follow_newest();
        true
    }

    /// Re-resolves the labels when the language changed under a chart that is
    /// already on screen, reporting whether it did.
    ///
    /// The pane cannot wait for the next snapshot to do it: an ETF answer arrives
    /// every eight hours, and a chart that kept the labels of the language the user
    /// just left would be wrong for most of that time.
    pub(crate) fn sync_locale(&mut self, cx: &App) -> bool {
        if self.data.borrow().locale == *cx.global::<LanguageCode>() {
            return false;
        }
        let (dataset, selection, window) = {
            let data = self.data.borrow();
            (data.source.clone(), data.selection.clone(), data.window)
        };
        self.set_view(cx, dataset, selection, window)
    }

    /// Applies the pointer position the last frame queued, reporting whether
    /// anything moved.
    ///
    /// It runs before the frame is built rather than during its paint, because two
    /// things in one frame are drawn from the hover: the canvas, and the pane's own
    /// row of readings. A commit in the paint pass would leave the row a frame
    /// behind the chart it describes.
    pub(crate) fn commit_pointer(&self) -> bool {
        self.state.hover.borrow_mut().commit()
    }

    /// The point the pointer is over, if any.
    pub(crate) fn hover_index(&self) -> Option<usize> {
        self.state.hover.borrow().current.map(|hover| hover.index)
    }

    /// The readings the pane's own row shows for the point under the pointer.
    pub(crate) fn metrics(&self) -> Option<Metrics> {
        let hover = self.hover_index();
        self.data.borrow().metrics(hover)
    }

    /// The box the content needs at minimum, which is what the pane it is in
    /// scrolls to.
    ///
    /// It is the measured labels' answer rather than the pane's, so a pane that
    /// shrank gets a smaller chart back: before the first frame has measured
    /// anything it is the chart's own floor.
    fn content_min_size(&self) -> Size<Pixels> {
        let cache = self.state.cache.borrow();
        content_min_size(
            cache.measurements.left_axis_width,
            cache.measurements.right_axis_width,
        )
    }

    /// The element a pane embeds, which is the canvas and the pointer handling it
    /// needs.
    pub(crate) fn entity(&self, cx: &App) -> AnyElement {
        chart_element(
            self.data.clone(),
            self.state.clone(),
            self.content_min_size(),
            self.revision,
            aria_label(cx),
        )
    }

    /// The state a frame reuses, which is what a hover test drives.
    #[cfg(test)]
    pub(crate) fn state(&self) -> Rc<ChartState> {
        self.state.clone()
    }

    /// Resolves where the pointer is, asking for the frame that will draw it.
    fn track_pointer(&self, cursor: Point<Pixels>, window: &mut Window) {
        let next = self
            .state
            .layout()
            .and_then(|layout| hover_at(&self.data.borrow(), layout, cursor));
        if self.state.hover.borrow_mut().queue(next) {
            window.on_next_frame(|window, _| window.refresh());
        }
    }

    /// Records that the pointer left the chart.
    fn clear_pointer(&self, window: &mut Window) {
        if self.state.hover.borrow_mut().queue(None) {
            window.on_next_frame(|window, _| window.refresh());
        }
    }
}

/// The point a pointer position resolves to, if it is over the plot.
fn hover_at(data: &PreparedData, layout: ChartLayout, cursor: Point<Pixels>) -> Option<Hover> {
    if !layout.plot.contains(&cursor) {
        return None;
    }
    nearest_index(layout.plot, data.points.len(), cursor.x).map(|index| Hover {
        index,
        cursor_y: px(f32::from(cursor.y).round()),
    })
}

/// The chart's own name, for assistive technology that reaches it.
fn aria_label(cx: &App) -> SharedString {
    t!(cx, "widget.etf.chart")
}

/// Builds the element of a chart from the things its frame needs.
///
/// The element is built from a pair of `Rc`s rather than from a [`EtfChart`],
/// because the pane reaches its chart without a `Context` for it: the same pair is
/// what a frame of either closure below captures.
fn chart_element(
    data: Rc<RefCell<PreparedData>>,
    state: Rc<ChartState>,
    content_min: Size<Pixels>,
    revision: u64,
    aria_label: SharedString,
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
    let element = canvas(
        move |bounds, window, cx| {
            let key = CacheKey { bounds, revision };
            if let Some(layout) = prepaint_state.cached_layout(&key) {
                prepaint_state.layout.set(Some(layout));
                return layout;
            }
            let data = prepaint_data.borrow();
            let measurements = measure(
                &data,
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
            let prepared = prepare(&data, layout);
            prepaint_state.remember(key, layout, measurements, prepared);
            layout
        },
        move |_, layout, window, app| {
            let data = paint_data.borrow();
            let hover = paint_state.hover.borrow().current;
            paint(&data, layout, hover, paint_state.as_ref(), window, app);
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
        .id(ElementId::Name(SharedString::new_static("EtfChartContent")))
        .relative()
        .min_w(content_min.width)
        .min_h(content_min.height)
        .h_full()
        .w_full()
        .cursor(CursorStyle::Crosshair)
        .on_prepaint(move |bounds, window, _| {
            if let Some(layout) = sync_state.layout()
                && sync_state.sync_viewport(bounds.size, layout.content.size)
            {
                // The offsets moved under a stationary pane, so the frame that just
                // laid this out is already out of date.
                window.refresh();
            }
        })
        .child(element)
        .on_mouse_move(move |event: &MouseMoveEvent, window, _| {
            let chart = EtfChart {
                data: pointer_data.clone(),
                revision,
                state: pointer_state.clone(),
            };
            chart.track_pointer(event.position, window);
        })
        .on_hover(move |hovering, window, _| {
            if !*hovering {
                let chart = EtfChart {
                    data: leave_data.clone(),
                    revision,
                    state: leave_state.clone(),
                };
                chart.clear_pointer(window);
            }
        });

    // Two scroll containers rather than one: a narrow pane has to reach both the
    // left axis and the last day, and one container cannot move in two directions.
    // The vertical one is the shell, so the frame always fills the pane's height and
    // only the width scrolls when the axes need the room.
    let horizontal = content_state.horizontal.clone();
    let vertical = content_state.vertical.clone();
    div()
        .id(ElementId::Name(SharedString::new_static(
            "EtfChartVertical",
        )))
        .relative()
        .size_full()
        .min_h_0()
        .flex()
        .flex_col()
        .overflow_y_scroll()
        .track_scroll(&vertical)
        .role(Role::Group)
        .aria_label(aria_label)
        .child(
            div()
                .id(ElementId::Name(SharedString::new_static(
                    "EtfChartHorizontal",
                )))
                .min_h(content_min.height)
                // The horizontal box has to claim the height the frame draws in. A
                // scroll container lays its child out against an indefinite height,
                // so a child that asked for `h_full` there would resolve to its floor
                // and the chart would be drawn in the top of a pane many times
                // taller.
                .flex_1()
                .w_full()
                .overflow_x_scroll()
                .track_scroll(&horizontal)
                .child(content),
        )
        .into_any_element()
}

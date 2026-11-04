//! The probability chart: time on x, probability on y.
//!
//! The chart is deliberately one canvas rather than a chart engine: a prediction
//! event has at most twelve lines, the y axis is always `0–100%`, and there are no
//! candles, indicators or volume to draw. The 50% reference line is the only
//! annotation.

use std::sync::Arc;

use base_ui::style::theme::ThemeExt;
use gpui_kit::{
    AnyElement, App, Bounds, Hsla, IntoElement, PathBuilder, Pixels, Point, SharedString, Styled,
    TextAlign, TextRun, Window, canvas, point, px,
};
use market_participants_model::decimal::Fraction;

use crate::model::PredictionChartSnapshot;

const LEFT_INSET: f32 = 44.0;
const RIGHT_INSET: f32 = 12.0;
const TOP_INSET: f32 = 10.0;
const BOTTOM_INSET: f32 = 24.0;
const LINE_WIDTH: f32 = 2.0;
const FONT_SIZE: f32 = 10.0;
const LINE_HEIGHT: f32 = 14.0;

/// The element one chart draws.
pub(crate) fn chart_element(snapshot: Arc<PredictionChartSnapshot>) -> AnyElement {
    canvas(
        |bounds, _, _| bounds,
        move |bounds, _, window, cx| paint(&snapshot, bounds, window, cx),
    )
    .size_full()
    .into_any_element()
}

fn paint(
    snapshot: &PredictionChartSnapshot,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let plot = Bounds::new(
        point(
            bounds.origin.x + px(LEFT_INSET),
            bounds.origin.y + px(TOP_INSET),
        ),
        gpui_kit::size(
            (bounds.size.width - px(LEFT_INSET + RIGHT_INSET)).max(px(1.0)),
            (bounds.size.height - px(TOP_INSET + BOTTOM_INSET)).max(px(1.0)),
        ),
    );
    let grid = cx.theme().roles.chart.grid.hsla;
    let axis = cx.theme().roles.chart.axis.hsla;
    for step in 0..=4 {
        let value = f64::from(step) / 4.0;
        let y = value_y(value, plot);
        paint_line(
            window,
            point(plot.origin.x, y),
            point(plot.right(), y),
            grid,
        );
        paint_text(
            window,
            cx,
            format!("{:.0}%", value * 100.0),
            point(bounds.origin.x, y - px(7.0)),
            axis,
            px(LEFT_INSET - 6.0),
        );
    }
    // The 50% line is dashed so it reads as a reference rather than as a series.
    let fifty = value_y(0.5, plot);
    let mut x = plot.origin.x;
    while x < plot.right() {
        let end = (x + px(5.0)).min(plot.right());
        paint_line(window, point(x, fifty), point(end, fifty), grid);
        x += px(9.0);
    }
    let (start, end) = time_range(snapshot);
    if start < end {
        paint_text(
            window,
            cx,
            format_timestamp(start),
            point(plot.origin.x, plot.bottom() + px(4.0)),
            axis,
            px(90.0),
        );
        paint_text(
            window,
            cx,
            format_timestamp(end),
            point(plot.right() - px(90.0), plot.bottom() + px(4.0)),
            axis,
            px(90.0),
        );
    }
    for line in snapshot.series.iter() {
        let color = cx.theme().roles.chart.series[line.color % 6].hsla;
        let mut path: Option<PathBuilder> = None;
        for point in line.points.iter() {
            let x = time_x(point.timestamp, start, end, plot);
            let y = value_y(probability(point.probability), plot);
            match path.as_mut() {
                Some(builder) => {
                    builder.line_to(gpui_kit::point(x, y));
                }
                None => {
                    let mut builder = PathBuilder::stroke(px(LINE_WIDTH));
                    builder.move_to(gpui_kit::point(x, y));
                    path = Some(builder);
                }
            }
        }
        if let Some(path) = path.and_then(|builder| builder.build().ok()) {
            window.paint_path(path, color);
        }
    }
    // The plot frame is drawn after the lines so a line cannot bleed over it.
    let top_left = point(plot.origin.x, plot.origin.y);
    let top_right = point(plot.right(), plot.origin.y);
    let bottom_left = point(plot.origin.x, plot.bottom());
    let bottom_right = point(plot.right(), plot.bottom());
    paint_line(window, bottom_left, bottom_right, grid);
    paint_line(window, top_left, top_right, grid);
    paint_line(window, top_left, bottom_left, grid);
    paint_line(window, top_right, bottom_right, grid);
}

fn time_range(snapshot: &PredictionChartSnapshot) -> (i64, i64) {
    let mut start = i64::MAX;
    let mut end = i64::MIN;
    for line in snapshot.series.iter() {
        for point in line.points.iter() {
            start = start.min(point.timestamp);
            end = end.max(point.timestamp);
        }
    }
    if start == i64::MAX {
        (0, 0)
    } else if start == end {
        (start, end.saturating_add(1))
    } else {
        (start, end)
    }
}

fn time_x(timestamp: i64, start: i64, end: i64, plot: Bounds<Pixels>) -> Pixels {
    let span = (end - start).max(1) as f32;
    let fraction = ((timestamp - start) as f32 / span).clamp(0.0, 1.0);
    plot.origin.x + plot.size.width * fraction
}

fn value_y(value: f64, plot: Bounds<Pixels>) -> Pixels {
    plot.bottom() - plot.size.height * value.clamp(0.0, 1.0) as f32
}

fn probability(value: Fraction) -> f64 {
    value.ratio().inner() as f64 / 1e18
}

fn paint_line(window: &mut Window, start: Point<Pixels>, end: Point<Pixels>, color: Hsla) {
    let mut builder = PathBuilder::stroke(px(1.0));
    builder.move_to(start);
    builder.line_to(end);
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn paint_text(
    window: &mut Window,
    cx: &mut App,
    text: impl Into<SharedString>,
    origin: Point<Pixels>,
    color: Hsla,
    width: Pixels,
) {
    let text = text.into();
    let style = window.text_style();
    let line = window.text_system().shape_line(
        text.clone(),
        px(FONT_SIZE),
        &[TextRun {
            len: text.len(),
            font: style.font(),
            color,
            ..Default::default()
        }],
        None,
    );
    let _ = line.paint(
        origin,
        px(LINE_HEIGHT),
        TextAlign::Left,
        Some(width),
        window,
        cx,
    );
}

fn format_timestamp(millis: i64) -> String {
    let Some(seconds) = millis.checked_div(1_000) else {
        return String::new();
    };
    let Ok(date) = time::OffsetDateTime::from_unix_timestamp(seconds) else {
        return String::new();
    };
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    )
}

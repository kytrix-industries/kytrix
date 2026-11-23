use std::sync::Arc;

use ui_core::{
    error::EngineError,
    math::Rect,
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        canvas::Canvas,
        color::Color,
        primitive::{ClipRect, Quad, QuadPaint},
        style::StylePatch,
        style_builder::StyleBuilder,
    },
};
use ui_kit::theme::handle::ThemeHandle;
use widget::cx::WidgetCx;

use super::model::{GroupedLevel, WINDOW_SIZES, interpolated_ratio};

pub(super) const ROW_GAP: f32 = 1.0;

pub(super) const ROW_TEXT_PADDING: f32 = 4.0;

pub(super) fn row_extent(font_size: f32) -> f32 {
    font_size + ROW_TEXT_PADDING
}

pub(super) fn row_stride(font_size: f32) -> f32 {
    row_extent(font_size) + ROW_GAP
}

pub(super) const ASK_BARS_SELECTOR: &str = "TEST_ORDER_BOOK_ASK_BARS";
pub(super) const BID_BARS_SELECTOR: &str = "TEST_ORDER_BOOK_BID_BARS";
pub(super) const IMBALANCE_SELECTOR: &str = "TEST_ORDER_BOOK_IMBALANCE";

pub(super) fn drawable_rows(bounds: Rect, font_size: f32) -> usize {
    let stride = row_stride(font_size);
    let rows = ((bounds.size.height + ROW_GAP) / stride).floor();
    (rows.max(1.0) as usize).min(WINDOW_SIZES[WINDOW_SIZES.len() - 1])
}

pub(super) fn row_bounds(
    bounds: Rect,
    index: usize,
    font_size: f32,
    offset: f32,
    asks: bool,
) -> Rect {
    let extent = row_extent(font_size);
    let top = if asks {
        bounds.size.height - extent - index as f32 * row_stride(font_size)
    } else {
        index as f32 * row_stride(font_size)
    };
    Rect::new(
        bounds.origin.x,
        bounds.origin.y + top - offset,
        bounds.size.width,
        extent,
    )
}

pub(super) fn bar_bounds(row: Rect, ratio: f32) -> Rect {
    let ratio = if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let bar_width = row.size.width * ratio;
    Rect::new(
        row.origin.x + row.size.width - bar_width,
        row.origin.y,
        bar_width,
        row.size.height,
    )
}

pub(super) fn bars_canvas(
    bounds: Rect,
    levels: &[GroupedLevel],
    progress: f32,
    font_size: f32,
    offset: f32,
    color: Color,
    asks: bool,
) -> Canvas {
    let mut canvas = Canvas::new(bounds);
    let _ = canvas.push_clip(ClipRect {
        rect: bounds,
        corner_radius: [0.0; 4],
    });
    for (index, level) in levels.iter().enumerate() {
        let row = row_bounds(bounds, index, font_size, offset, asks);
        let ratio = interpolated_ratio(level.previous_ratio, level.current_ratio, progress);
        let bar = bar_bounds(row, ratio);
        if bar.size.width <= 0.0 {
            continue;
        }
        canvas.push_quad(Quad {
            bounds: bar,
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: color,
                border: None,
            },
        });
    }
    let _ = canvas.pop_clip();
    canvas
}

pub(super) fn imbalance_canvas(
    bounds: Rect,
    ask_ratio: f32,
    ask_color: Color,
    bid_color: Color,
) -> Canvas {
    let mut canvas = Canvas::new(bounds);
    let _ = canvas.push_clip(ClipRect {
        rect: bounds,
        corner_radius: [0.0; 4],
    });
    let ask_ratio = if ask_ratio.is_finite() {
        ask_ratio.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let ask_width = bounds.size.width * ask_ratio;
    if ask_width > 0.0 {
        canvas.push_quad(Quad {
            bounds: Rect::new(
                bounds.origin.x,
                bounds.origin.y,
                ask_width,
                bounds.size.height,
            ),
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: ask_color,
                border: None,
            },
        });
    }
    if bounds.size.width - ask_width > 0.0 {
        canvas.push_quad(Quad {
            bounds: Rect::new(
                bounds.origin.x + ask_width,
                bounds.origin.y,
                bounds.size.width - ask_width,
                bounds.size.height,
            ),
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: bid_color,
                border: None,
            },
        });
    }
    let _ = canvas.pop_clip();
    canvas
}

struct BarsKey {
    levels: Arc<[GroupedLevel]>,
    bounds: Rect,
    progress: f32,
    font_size: f32,
    offset: f32,
    color: Color,
    asks: bool,
}

impl BarsKey {
    fn matches(
        &self,
        levels: &Arc<[GroupedLevel]>,
        bounds: Rect,
        progress: f32,
        font_size: f32,
        offset: f32,
        color: Color,
        asks: bool,
    ) -> bool {
        Arc::ptr_eq(&self.levels, levels)
            && self.bounds == bounds
            && self.progress == progress
            && self.font_size == font_size
            && self.offset == offset
            && self.color == color
            && self.asks == asks
    }
}

pub(super) struct SideCanvas {
    node: Option<NodeId>,
    prepared: Option<BarsKey>,
}

impl SideCanvas {
    pub(super) const fn new() -> Self {
        Self {
            node: None,
            prepared: None,
        }
    }

    pub(super) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        selector: &'static str,
        theme: ThemeHandle,
    ) -> Result<(), EngineError> {
        let style = StyleBuilder::new()
            .absolute()
            .left(0.0)
            .top(0.0)
            .size_full()
            .build();
        let node = NodeBuilder::canvas(Canvas::new(Rect::default()))
            .id(selector)
            .style(style)
            .bind_style(move |read| {
                let _ = theme.appearance(read);
                StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        self.node = Some(node);
        Ok(())
    }

    pub(super) fn sync(
        &mut self,
        cx: &mut WidgetCx<'_>,
        levels: &Arc<[GroupedLevel]>,
        progress: f32,
        font_size: f32,
        offset: f32,
        color: Color,
        asks: bool,
    ) -> Result<(), EngineError> {
        let Some(node) = self.node else {
            return Ok(());
        };
        let Ok(target) = cx.tree().get(node) else {
            return Ok(());
        };
        let bounds = target.layout().bounds;
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            return Ok(());
        }
        if self.prepared.as_ref().is_some_and(|prepared| {
            prepared.matches(levels, bounds, progress, font_size, offset, color, asks)
        }) {
            return Ok(());
        }
        let canvas = bars_canvas(bounds, levels, progress, font_size, offset, color, asks);
        cx.set_canvas(node, canvas)?;
        self.prepared = Some(BarsKey {
            levels: levels.clone(),
            bounds,
            progress,
            font_size,
            offset,
            color,
            asks,
        });
        Ok(())
    }
}

struct ImbalanceKey {
    bounds: Rect,
    ask_ratio: f32,
    ask_color: Color,
    bid_color: Color,
}

impl ImbalanceKey {
    fn matches(&self, bounds: Rect, ask_ratio: f32, ask_color: Color, bid_color: Color) -> bool {
        self.bounds == bounds
            && self.ask_ratio == ask_ratio
            && self.ask_color == ask_color
            && self.bid_color == bid_color
    }
}

pub(super) struct Imbalance {
    node: Option<NodeId>,
    prepared: Option<ImbalanceKey>,
}

impl Imbalance {
    pub(super) const fn new() -> Self {
        Self {
            node: None,
            prepared: None,
        }
    }

    pub(super) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        theme: ThemeHandle,
    ) -> Result<(), EngineError> {
        let style = StyleBuilder::new()
            .absolute()
            .left(0.0)
            .top(0.0)
            .size_full()
            .build();
        let node = NodeBuilder::canvas(Canvas::new(Rect::default()))
            .id(IMBALANCE_SELECTOR)
            .style(style)
            .bind_style(move |read| {
                let _ = theme.appearance(read);
                StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        self.node = Some(node);
        Ok(())
    }

    pub(super) fn sync(
        &mut self,
        cx: &mut WidgetCx<'_>,
        ask_ratio: f32,
        ask_color: Color,
        bid_color: Color,
    ) -> Result<(), EngineError> {
        let Some(node) = self.node else {
            return Ok(());
        };
        let Ok(target) = cx.tree().get(node) else {
            return Ok(());
        };
        let bounds = target.layout().bounds;
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            return Ok(());
        }
        let ask_ratio = if ask_ratio.is_finite() {
            ask_ratio.clamp(0.0, 1.0)
        } else {
            0.5
        };
        if self
            .prepared
            .as_ref()
            .is_some_and(|prepared| prepared.matches(bounds, ask_ratio, ask_color, bid_color))
        {
            return Ok(());
        }
        let canvas = imbalance_canvas(bounds, ask_ratio, ask_color, bid_color);
        cx.set_canvas(node, canvas)?;
        self.prepared = Some(ImbalanceKey {
            bounds,
            ask_ratio,
            ask_color,
            bid_color,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use market_participants_model::decimal::{Price, Quantity};
    use smol_str::SmolStr;
    use ui_core::paint::primitive::Primitive;

    use super::*;

    const FONT_SIZE: f32 = 12.0;

    fn level(ratio: f32) -> GroupedLevel {
        GroupedLevel {
            price: Price::from_str("100").expect("price should parse"),
            quantity: Quantity::from_str("1").expect("quantity should parse"),
            cumulative_quantity: Quantity::from_str("1").expect("quantity should parse"),
            price_text: SmolStr::new_static("100"),
            quantity_text: SmolStr::new_static("1"),
            cumulative_quantity_text: SmolStr::new_static("1"),
            previous_ratio: ratio,
            current_ratio: ratio,
        }
    }

    fn bounds(left: f32, top: f32, width: f32, height: f32) -> Rect {
        Rect::new(left, top, width, height)
    }

    fn color() -> Color {
        Color::Srgb {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            alpha: 1.0,
        }
    }

    fn quads(canvas: &Canvas) -> Vec<Quad> {
        canvas
            .primitives()
            .iter()
            .filter_map(|primitive| match primitive {
                Primitive::Quad(quad) => Some(quad.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn drawable_rows_should_match_the_row_geometry() {
        let side = bounds(0.0, 0.0, 640.0, 25.0 * row_stride(FONT_SIZE) - ROW_GAP);

        assert_eq!(drawable_rows(side, FONT_SIZE), 25);
    }

    #[test]
    fn drawable_rows_should_never_exceed_the_widest_window() {
        let side = bounds(0.0, 0.0, 640.0, 4_000.0);

        assert_eq!(
            drawable_rows(side, FONT_SIZE),
            WINDOW_SIZES[WINDOW_SIZES.len() - 1]
        );
    }

    #[test]
    fn drawable_rows_should_keep_one_row_in_a_collapsed_pane() {
        let side = bounds(0.0, 0.0, 640.0, 0.0);

        assert_eq!(drawable_rows(side, FONT_SIZE), 1);
    }

    #[test]
    fn level_rows_should_grow_away_from_the_midpoint() {
        let side = bounds(10.0, 20.0, 300.0, 100.0);
        let best_ask = row_bounds(side, 0, FONT_SIZE, 0.0, true);
        let next_ask = row_bounds(side, 1, FONT_SIZE, 0.0, true);
        let best_bid = row_bounds(side, 0, FONT_SIZE, 0.0, false);
        let next_bid = row_bounds(side, 1, FONT_SIZE, 0.0, false);

        assert_eq!(
            best_ask.origin.y + best_ask.size.height,
            side.origin.y + side.size.height
        );
        assert!(next_ask.origin.y < best_ask.origin.y);
        assert_eq!(best_bid.origin.y, side.origin.y);
        assert!(next_bid.origin.y > best_bid.origin.y);
    }

    #[test]
    fn a_scrolled_list_should_move_the_bars_with_it() {
        let side = bounds(0.0, 0.0, 300.0, 100.0);
        let unscrolled = row_bounds(side, 3, FONT_SIZE, 0.0, false);
        let scrolled = row_bounds(side, 3, FONT_SIZE, row_stride(FONT_SIZE) * 2.0, false);

        assert_eq!(
            unscrolled.origin.y - scrolled.origin.y,
            row_stride(FONT_SIZE) * 2.0
        );
    }

    #[test]
    fn depth_bars_should_not_overlap_within_one_side() {
        let side = bounds(10.0, 20.0, 300.0, 500.0);
        let bars = [true, false].into_iter().flat_map(|asks| {
            (0..25)
                .map(move |index| bar_bounds(row_bounds(side, index, FONT_SIZE, 0.0, asks), 0.75))
        });
        let bars = bars.collect::<Vec<_>>();

        assert!(bars.as_chunks::<25>().0.iter().all(|side| {
            side.windows(2).all(|pair| {
                pair[0].origin.y + pair[0].size.height <= pair[1].origin.y
                    || pair[1].origin.y + pair[1].size.height <= pair[0].origin.y
            })
        }));
    }

    #[test]
    fn depth_bars_should_be_anchored_to_the_right_edge() {
        let row = bounds(10.0, 20.0, 300.0, 16.0);

        assert_eq!(bar_bounds(row, 0.0).size.width, 0.0);
        assert_eq!(bar_bounds(row, 1.0), row);
        let half = bar_bounds(row, 0.5);
        assert_eq!(
            half.origin.x + half.size.width,
            row.origin.x + row.size.width
        );
    }

    #[test]
    fn a_non_finite_or_out_of_range_ratio_should_not_escape_the_row() {
        let row = bounds(10.0, 20.0, 300.0, 16.0);

        assert_eq!(bar_bounds(row, 2.0), row);
        assert_eq!(bar_bounds(row, -1.0).size.width, 0.0);
        assert_eq!(bar_bounds(row, f32::NAN).size.width, 0.0);
    }

    #[test]
    fn bars_canvas_should_carry_one_quad_per_visible_level() {
        let levels: Arc<[GroupedLevel]> = Arc::from([level(0.2), level(0.5), level(1.0)]);
        let side = bounds(0.0, 0.0, 300.0, 100.0);

        let canvas = bars_canvas(side, &levels, 1.0, FONT_SIZE, 0.0, color(), true);

        assert_eq!(quads(&canvas).len(), levels.len());
    }

    #[test]
    fn an_interpolated_bar_should_sit_between_the_two_ratios_it_eases() {
        let mut easing = level(0.0);
        easing.current_ratio = 1.0;
        let levels: Arc<[GroupedLevel]> = Arc::from([easing]);
        let side = bounds(0.0, 0.0, 300.0, 100.0);

        let canvas = bars_canvas(side, &levels, 0.5, FONT_SIZE, 0.0, color(), true);

        let bar = quads(&canvas).remove(0);
        assert!((bar.bounds.size.width - side.size.width / 2.0).abs() < 0.01);
        assert_eq!(
            bar.paint,
            QuadPaint::Solid {
                fill: color(),
                border: None
            }
        );
    }

    #[test]
    fn the_imbalance_bar_should_split_the_box_between_the_two_sides() {
        let side = bounds(10.0, 20.0, 300.0, 20.0);

        let canvas = imbalance_canvas(side, 0.25, color(), color());

        let quads = quads(&canvas);
        assert_eq!(quads.len(), 2);
        assert!((quads[0].bounds.size.width - 75.0).abs() < 0.01);
        assert!((quads[1].bounds.origin.x - 85.0).abs() < 0.01);
        assert!((quads[1].bounds.size.width - 225.0).abs() < 0.01);
    }

    #[test]
    fn a_prepared_side_should_be_reused_for_a_frame_with_the_same_inputs() {
        let levels: Arc<[GroupedLevel]> = Arc::from([level(0.5)]);
        let side = bounds(0.0, 0.0, 300.0, 100.0);
        let key = BarsKey {
            levels: levels.clone(),
            bounds: side,
            progress: 1.0,
            font_size: FONT_SIZE,
            offset: 0.0,
            color: color(),
            asks: true,
        };

        assert!(key.matches(&levels, side, 1.0, FONT_SIZE, 0.0, color(), true));
        assert!(!key.matches(&levels, side, 0.5, FONT_SIZE, 0.0, color(), true));
        assert!(!key.matches(
            &Arc::from([level(0.5)]),
            side,
            1.0,
            FONT_SIZE,
            0.0,
            color(),
            true
        ));
        assert!(!key.matches(&levels, side, 1.0, FONT_SIZE, 8.0, color(), true));
    }
}

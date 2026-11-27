use smallvec::SmallVec;

use crate::{
    math::{LogicalRect, LogicalSize, px},
    popup::{
        Alignment, Placement, PlacementInput, PlacementResult, PlacementSpec, PopupError,
        ShiftPolicy, Side,
    },
};

const OVERFLOW_EPSILON: f32 = 1e-4;

const HALF_PIXEL: f32 = 0.5;

const MIN_OVERLAP: f32 = 1.0;

pub fn solve_placement(input: PlacementInput) -> Result<PlacementResult, PopupError> {
    let PlacementInput {
        anchor,
        size,
        viewport,
        avoid,
        spec,
    } = input;
    validate(anchor, size, viewport, avoid, &spec)?;

    let margin = spec.viewport_margin.value();
    let gap = spec.gap.value();
    let viewport_width = viewport.width.value();
    let viewport_height = viewport.height.value();
    let available_width = (viewport_width - 2.0 * margin).max(0.0);
    let available_height = (viewport_height - 2.0 * margin).max(0.0);

    let mut width = size.width.value();
    let content_width = width;
    let mut resized = false;
    if spec.max_size.shrinks_width() {
        width = width.min(available_width);
        resized |= content_width > available_width + 0.01;
    }
    let mut height = size.height.value();
    let content_height = height;
    if spec.max_size.shrinks_height() {
        height = height.min(available_height);
        resized |= content_height > available_height + 0.01;
    }

    if width < 0.0 || height < 0.0 {
        return Err(PopupError::InvalidGeometry {
            anchor,
            size,
            viewport,
            avoid,
        });
    }
    let selection = select(
        anchor,
        avoid,
        viewport_width,
        viewport_height,
        margin,
        gap,
        &spec,
        width,
        height,
    )?;
    let mut rect = if selection.fitted {
        selection.rect
    } else {
        clamp_primary(
            selection.rect,
            selection.side,
            viewport_width,
            viewport_height,
            margin,
        )
    };
    if rect.size.width <= px(available_width) && rect.size.height <= px(available_height) {
        rect.origin.x = px(rect.origin.x.value().clamp(
            margin,
            (viewport_width - margin - rect.size.width.value()).max(margin),
        ));
        rect.origin.y = px(rect.origin.y.value().clamp(
            margin,
            (viewport_height - margin - rect.size.height.value()).max(margin),
        ));
    }
    if !rect.is_finite() {
        return Err(PopupError::InvalidGeometry {
            anchor,
            size,
            viewport,
            avoid,
        });
    }
    Ok(PlacementResult {
        rect,
        side: selection.side,
        resized,
    })
}

fn validate(
    anchor: LogicalRect,
    size: LogicalSize,
    viewport: LogicalSize,
    avoid: Option<LogicalRect>,
    spec: &PlacementSpec,
) -> Result<(), PopupError> {
    let anchor_valid = anchor.is_convertible();
    let size_valid = size.is_finite() && size.width.value() >= 0.0 && size.height.value() >= 0.0;
    let viewport_valid =
        viewport.is_finite() && viewport.width.value() >= 0.0 && viewport.height.value() >= 0.0;
    let avoid_valid = avoid.is_none_or(LogicalRect::is_convertible);
    if !anchor_valid || !size_valid || !viewport_valid || !avoid_valid {
        return Err(PopupError::InvalidGeometry {
            anchor,
            size,
            viewport,
            avoid,
        });
    }
    let gap = spec.gap;
    let margin = spec.viewport_margin;
    if !gap.is_finite() || gap.value() < 0.0 || !margin.is_finite() || margin.value() < 0.0 {
        return Err(PopupError::InvalidPlacement { gap, margin });
    }
    Ok(())
}

fn candidate_placements(spec: &PlacementSpec) -> SmallVec<[Placement; 5]> {
    let mut placements: SmallVec<[Placement; 5]> =
        SmallVec::with_capacity(spec.fallbacks.len() + 1);
    placements.push(Placement::new(spec.side, spec.align));
    for fallback in &spec.fallbacks {
        if !placements.contains(fallback) {
            placements.push(*fallback);
        }
    }
    placements
}

struct Selection {
    rect: LogicalRect,
    side: Side,
    fitted: bool,
}

struct Candidate {
    rect: LogicalRect,
    side: Side,
    overflow: f32,
    overlaps_avoid: bool,
    fitted: bool,
}

fn select(
    anchor: LogicalRect,
    avoid: Option<LogicalRect>,
    viewport_width: f32,
    viewport_height: f32,
    margin: f32,
    gap: f32,
    spec: &PlacementSpec,
    width: f32,
    height: f32,
) -> Result<Selection, PopupError> {
    let mut best: Option<Candidate> = None;
    for placement in candidate_placements(spec) {
        let candidate = evaluate(
            placement,
            anchor,
            avoid,
            viewport_width,
            viewport_height,
            margin,
            gap,
            spec.shift,
            width,
            height,
        );
        if candidate.fitted {
            return Ok(Selection {
                rect: candidate.rect,
                side: candidate.side,
                fitted: true,
            });
        }
        if best.as_ref().is_none_or(|current| {
            candidate.overflow < current.overflow
                || (candidate.overflow == current.overflow
                    && current.overlaps_avoid
                    && !candidate.overlaps_avoid)
        }) {
            best = Some(candidate);
        }
    }
    let Some(best) = best else {
        return Err(PopupError::InvalidGeometry {
            anchor,
            size: LogicalSize::new(px(width), px(height)),
            viewport: LogicalSize::new(px(viewport_width), px(viewport_height)),
            avoid,
        });
    };
    Ok(Selection {
        rect: best.rect,
        side: best.side,
        fitted: false,
    })
}

fn evaluate(
    placement: Placement,
    anchor: LogicalRect,
    avoid: Option<LogicalRect>,
    viewport_width: f32,
    viewport_height: f32,
    margin: f32,
    gap: f32,
    shift: ShiftPolicy,
    width: f32,
    height: f32,
) -> Candidate {
    let (base_x, base_y) = place(anchor, avoid, gap, placement, width, height);
    let (x, y) = if placement.side.is_vertical() {
        let delta = clamp_shift(base_x, width, viewport_width, margin);
        let delta = limited_shift(
            delta,
            base_x,
            anchor.origin.x.value(),
            anchor.size.width.value(),
            width,
            shift,
        );
        (base_x + delta, base_y)
    } else {
        let delta = clamp_shift(base_y, height, viewport_height, margin);
        let delta = limited_shift(
            delta,
            base_y,
            anchor.origin.y.value(),
            anchor.size.height.value(),
            height,
            shift,
        );
        (base_x, base_y + delta)
    };
    let rect = LogicalRect::new(px(x), px(y), px(width), px(height));
    let overflow = overflow_axis(x, width, viewport_width, margin)
        + overflow_axis(y, height, viewport_height, margin);
    let overlaps_avoid = avoid.is_some_and(|bounds| rect.intersects(bounds));
    Candidate {
        rect,
        side: placement.side,
        overflow,
        overlaps_avoid,
        fitted: overflow <= OVERFLOW_EPSILON && !overlaps_avoid,
    }
}

fn place(
    anchor: LogicalRect,
    avoid: Option<LogicalRect>,
    gap: f32,
    placement: Placement,
    width: f32,
    height: f32,
) -> (f32, f32) {
    let anchor_x = anchor.origin.x.value();
    let anchor_y = anchor.origin.y.value();
    let anchor_width = anchor.size.width.value();
    let anchor_height = anchor.size.height.value();
    match placement.side {
        Side::Bottom => (
            aligned(anchor_x, anchor_width, width, placement.align),
            anchor_y + anchor_height + gap,
        ),
        Side::Top => (
            aligned(anchor_x, anchor_width, width, placement.align),
            anchor_y - gap - height,
        ),
        Side::Right => (
            avoid.map_or(anchor_x + anchor_width, |bounds| {
                bounds.origin.x.value() + bounds.size.width.value()
            }) + gap,
            aligned(anchor_y, anchor_height, height, placement.align),
        ),
        Side::Left => (
            avoid.map_or(anchor_x, |bounds| bounds.origin.x.value()) - gap - width,
            aligned(anchor_y, anchor_height, height, placement.align),
        ),
    }
}

fn aligned(start: f32, extent: f32, size: f32, align: Alignment) -> f32 {
    match align {
        Alignment::Start => start,
        Alignment::Center => start + (extent - size) / 2.0,
        Alignment::End => start + extent - size,
    }
}

fn clamp_shift(start: f32, size: f32, total: f32, margin: f32) -> f32 {
    let target = start.clamp(margin, (total - margin - size).max(margin));
    target - start
}

fn limited_shift(
    desired: f32,
    base: f32,
    anchor_start: f32,
    anchor_extent: f32,
    cross_size: f32,
    shift: ShiftPolicy,
) -> f32 {
    if shift == ShiftPolicy::None || cross_size <= 0.0 {
        return desired;
    }
    let anchor_end = anchor_start + anchor_extent;
    let (low, high) = if anchor_extent >= MIN_OVERLAP {
        (anchor_start, anchor_end)
    } else {
        let center = anchor_start + anchor_extent / 2.0;
        (center - HALF_PIXEL, center + HALF_PIXEL)
    };
    let min_shift = low - base - cross_size + MIN_OVERLAP;
    let max_shift = high - base - MIN_OVERLAP;
    if min_shift > max_shift {
        return desired;
    }
    desired.clamp(min_shift, max_shift)
}

fn overflow_axis(start: f32, size: f32, total: f32, margin: f32) -> f32 {
    let leading = (margin - start).max(0.0);
    let trailing = (start + size - (total - margin)).max(0.0);
    leading + trailing
}

fn clamp_primary(
    rect: LogicalRect,
    side: Side,
    viewport_width: f32,
    viewport_height: f32,
    margin: f32,
) -> LogicalRect {
    if side.is_vertical() {
        let y = clamp_axis(
            rect.origin.y.value(),
            rect.size.height.value(),
            viewport_height,
            margin,
        );
        LogicalRect::new(rect.origin.x, px(y), rect.size.width, rect.size.height)
    } else {
        let x = clamp_axis(
            rect.origin.x.value(),
            rect.size.width.value(),
            viewport_width,
            margin,
        );
        LogicalRect::new(px(x), rect.origin.y, rect.size.width, rect.size.height)
    }
}

fn clamp_axis(start: f32, size: f32, total: f32, margin: f32) -> f32 {
    start.clamp(margin, (total - margin - size).max(margin))
}
#[cfg(test)]
mod tests {
    use smallvec::SmallVec;

    use super::solve_placement;
    use crate::{
        math::{LogicalRect, LogicalSize, px},
        popup::{
            Alignment, MaxSize, Placement, PlacementInput, PlacementResult, PlacementSpec,
            PopupError, ShiftPolicy, Side,
        },
    };

    fn rect(x: f32, y: f32, width: f32, height: f32) -> LogicalRect {
        LogicalRect::new(px(x), px(y), px(width), px(height))
    }

    fn size(width: f32, height: f32) -> LogicalSize {
        LogicalSize::new(px(width), px(height))
    }

    fn viewport() -> LogicalSize {
        size(400.0, 300.0)
    }

    fn spec(side: Side, align: Alignment) -> PlacementSpec {
        PlacementSpec {
            side,
            align,
            gap: px(4.0),
            fallbacks: SmallVec::new(),
            viewport_margin: px(4.0),
            shift: ShiftPolicy::Limit,
            max_size: MaxSize::Viewport,
        }
    }

    fn with_fallbacks(mut spec: PlacementSpec, fallbacks: &[Placement]) -> PlacementSpec {
        spec.fallbacks = fallbacks.iter().copied().collect();
        spec
    }

    fn bottom() -> PlacementSpec {
        spec(Side::Bottom, Alignment::Start)
    }

    fn assert_origin(rect: LogicalRect, origin: (f32, f32)) {
        assert_eq!(
            (rect.origin.x, rect.origin.y),
            (px(origin.0), px(origin.1)),
            "{rect:?}"
        );
    }

    fn assert_size(rect: LogicalRect, expected: (f32, f32)) {
        assert_eq!(
            (rect.size.width, rect.size.height),
            (px(expected.0), px(expected.1)),
            "{rect:?}"
        );
    }

    fn solve(
        anchor: LogicalRect,
        size: LogicalSize,
        viewport: LogicalSize,
        spec: &PlacementSpec,
    ) -> Result<PlacementResult, PopupError> {
        solve_placement(PlacementInput {
            anchor,
            size,
            viewport,
            avoid: None,
            spec: spec.clone(),
        })
    }

    fn solve_avoiding(
        anchor: LogicalRect,
        size: LogicalSize,
        viewport: LogicalSize,
        avoid: LogicalRect,
        spec: &PlacementSpec,
    ) -> Result<PlacementResult, PopupError> {
        solve_placement(PlacementInput {
            anchor,
            size,
            viewport,
            avoid: Some(avoid),
            spec: spec.clone(),
        })
    }
    #[test]
    fn bottom_start_positions_below_the_anchor() {
        let result = solve(
            rect(100.0, 100.0, 80.0, 30.0),
            size(120.0, 80.0),
            viewport(),
            &bottom(),
        )
        .expect("placement");
        assert_eq!(result.side, Side::Bottom);
        assert!(!result.resized);
        assert_origin(result.rect, (100.0, 134.0));
        assert_size(result.rect, (120.0, 80.0));
    }

    #[test]
    fn top_positions_above_the_anchor() {
        let result = solve(
            rect(100.0, 100.0, 80.0, 30.0),
            size(120.0, 80.0),
            viewport(),
            &spec(Side::Top, Alignment::Start),
        )
        .expect("placement");
        assert_eq!(result.side, Side::Top);
        assert_origin(result.rect, (100.0, 16.0));
    }

    #[test]
    fn horizontal_sides_position_on_the_horizontal_axis() {
        let anchor = rect(200.0, 100.0, 80.0, 30.0);
        let right = solve(
            anchor,
            size(60.0, 80.0),
            viewport(),
            &spec(Side::Right, Alignment::Start),
        )
        .expect("placement");
        assert_eq!(right.side, Side::Right);
        assert_origin(right.rect, (284.0, 100.0));

        let left_anchor = rect(100.0, 100.0, 80.0, 30.0);
        let left = solve(
            left_anchor,
            size(60.0, 80.0),
            viewport(),
            &spec(Side::Left, Alignment::Start),
        )
        .expect("placement");
        assert_eq!(left.side, Side::Left);
        assert_origin(left.rect, (36.0, 100.0));
    }

    #[test]
    fn alignments_place_the_popup_along_the_anchor() {
        let anchor = rect(100.0, 100.0, 80.0, 30.0);
        let centered = solve(
            anchor,
            size(120.0, 80.0),
            viewport(),
            &spec(Side::Bottom, Alignment::Center),
        )
        .expect("placement");
        assert_origin(centered.rect, (80.0, 134.0));

        let ended = solve(
            anchor,
            size(120.0, 80.0),
            viewport(),
            &spec(Side::Bottom, Alignment::End),
        )
        .expect("placement");
        assert_origin(ended.rect, (60.0, 134.0));

        let right_centered = solve(
            anchor,
            size(40.0, 80.0),
            viewport(),
            &spec(Side::Right, Alignment::Center),
        )
        .expect("placement");
        assert_origin(right_centered.rect, (184.0, 75.0));

        let right_ended = solve(
            anchor,
            size(40.0, 80.0),
            viewport(),
            &spec(Side::Right, Alignment::End),
        )
        .expect("placement");
        assert_origin(right_ended.rect, (184.0, 50.0));
    }

    #[test]
    fn flips_to_the_fallback_when_the_primary_side_does_not_fit() {
        let spec = with_fallbacks(bottom(), &[Placement::new(Side::Top, Alignment::Start)]);
        let result = solve(
            rect(100.0, 260.0, 80.0, 30.0),
            size(120.0, 80.0),
            viewport(),
            &spec,
        )
        .expect("placement");
        assert_eq!(result.side, Side::Top);
        assert_origin(result.rect, (100.0, 176.0));
    }

    #[test]
    fn a_flip_requires_strictly_less_overflow() {
        let spec = with_fallbacks(bottom(), &[Placement::new(Side::Top, Alignment::Start)]);
        let result = solve(
            rect(100.0, 140.0, 80.0, 20.0),
            size(120.0, 150.0),
            viewport(),
            &spec,
        )
        .expect("placement");
        assert_eq!(result.side, Side::Bottom, "溢出相等时保持首选方向");
        assert_origin(result.rect, (100.0, 146.0));
    }

    #[test]
    fn best_fit_picks_the_strictly_smaller_overflow() {
        let spec = with_fallbacks(bottom(), &[Placement::new(Side::Top, Alignment::Start)]);
        let result = solve(
            rect(0.0, 60.0, 10.0, 10.0),
            size(20.0, 80.0),
            size(100.0, 100.0),
            &PlacementSpec {
                viewport_margin: px(0.0),
                gap: px(0.0),
                ..spec
            },
        )
        .expect("placement");
        assert_eq!(result.side, Side::Top);
        assert_origin(result.rect, (0.0, 0.0));
    }

    #[test]
    fn shifts_the_cross_axis_into_the_viewport() {
        let result = solve(
            rect(300.0, 100.0, 80.0, 30.0),
            size(120.0, 80.0),
            viewport(),
            &bottom(),
        )
        .expect("placement");
        assert_origin(result.rect, (276.0, 134.0));
    }

    #[test]
    fn unlimited_shift_detaches_from_an_offscreen_anchor() {
        let spec = PlacementSpec {
            gap: px(0.0),
            viewport_margin: px(0.0),
            shift: ShiftPolicy::None,
            ..bottom()
        };
        let result = solve(
            rect(-50.0, 100.0, 10.0, 10.0),
            size(20.0, 20.0),
            size(300.0, 300.0),
            &spec,
        )
        .expect("placement");
        assert_origin(result.rect, (0.0, 110.0));
    }

    #[test]
    fn limit_shift_clamps_into_the_viewport_margin() {
        let spec = PlacementSpec {
            gap: px(0.0),
            viewport_margin: px(0.0),
            shift: ShiftPolicy::Limit,
            ..bottom()
        };
        let anchor = rect(-50.0, 100.0, 10.0, 10.0);
        let result = solve(anchor, size(20.0, 20.0), size(300.0, 300.0), &spec).expect("placement");
        assert_eq!(result.rect.origin.x, px(0.0));
        assert!(result.rect.origin.y >= px(100.0), "{:?}", result.rect);
        assert!(
            result.rect.origin.x >= px(0.0) && result.rect.max().x <= px(300.0),
            "viewport margin 是硬约束：{:?}",
            result.rect
        );
    }

    #[test]
    fn limit_shift_keeps_a_degenerate_anchor_rect_inside_the_viewport() {
        let spec = PlacementSpec {
            gap: px(0.0),
            viewport_margin: px(0.0),
            shift: ShiftPolicy::Limit,
            ..bottom()
        };
        let anchor = rect(-50.0, 100.0, 0.0, 10.0);
        let result = solve(anchor, size(20.0, 20.0), size(300.0, 300.0), &spec).expect("placement");
        assert_eq!(result.rect.origin.x, px(0.0), "{:?}", result.rect);
        assert!(result.rect.max().x <= px(300.0));
    }

    #[test]
    fn extreme_finite_inputs_never_produce_a_non_finite_rect() {
        let spec = PlacementSpec {
            gap: px(f32::MAX),
            ..bottom()
        };
        let result = solve(
            rect(10.0, 10.0, 10.0, 10.0),
            size(20.0, 20.0),
            viewport(),
            &spec,
        );
        match result {
            Ok(result) => {
                assert!(result.rect.is_finite(), "极端输入不得产生 NaN/inf rect");
                assert!(result.rect.origin.x >= px(0.0));
                assert!(result.rect.max().x <= px(400.0));
            }
            Err(error) => assert!(matches!(error, PopupError::InvalidGeometry { .. })),
        }
    }

    #[test]
    fn resizes_a_popup_that_is_larger_than_the_viewport() {
        let spec = PlacementSpec {
            gap: px(0.0),
            viewport_margin: px(0.0),
            ..bottom()
        };
        let result = solve(
            rect(0.0, 0.0, 10.0, 10.0),
            size(400.0, 300.0),
            size(200.0, 100.0),
            &spec,
        )
        .expect("placement");
        assert!(result.resized);
        assert_size(result.rect, (200.0, 100.0));
        assert_origin(result.rect, (0.0, 0.0));
    }

    #[test]
    fn max_size_width_only_constrains_the_width() {
        let spec = PlacementSpec {
            gap: px(0.0),
            viewport_margin: px(0.0),
            max_size: MaxSize::Width,
            ..bottom()
        };
        let result = solve(
            rect(0.0, 0.0, 10.0, 10.0),
            size(400.0, 300.0),
            size(200.0, 100.0),
            &spec,
        )
        .expect("placement");
        assert!(result.resized);
        assert_size(result.rect, (200.0, 300.0));
    }

    #[test]
    fn max_size_height_only_constrains_the_height() {
        let spec = PlacementSpec {
            gap: px(0.0),
            viewport_margin: px(0.0),
            max_size: MaxSize::Height,
            ..bottom()
        };
        let result = solve(
            rect(0.0, 0.0, 10.0, 10.0),
            size(400.0, 300.0),
            size(200.0, 100.0),
            &spec,
        )
        .expect("placement");
        assert!(result.resized);
        assert_size(result.rect, (400.0, 100.0));
    }

    #[test]
    fn a_submenu_falls_back_to_the_left_when_the_right_side_overflows() {
        let spec = PlacementSpec {
            side: Side::Right,
            align: Alignment::Start,
            gap: px(0.0),
            fallbacks: SmallVec::from_slice(&[Placement::new(Side::Left, Alignment::Start)]),
            viewport_margin: px(0.0),
            shift: ShiftPolicy::Limit,
            max_size: MaxSize::Viewport,
        };
        let result = solve(
            rect(250.0, 0.0, 20.0, 100.0),
            size(120.0, 50.0),
            size(300.0, 300.0),
            &spec,
        )
        .expect("placement");
        assert_eq!(result.side, Side::Left);
        assert_origin(result.rect, (130.0, 0.0));
    }

    #[test]
    fn a_submenu_flips_right_to_left_when_the_primary_side_overflows() {
        let spec = PlacementSpec {
            side: Side::Right,
            fallbacks: SmallVec::from_slice(&[Placement::new(Side::Left, Alignment::Start)]),
            ..bottom()
        };
        let result = solve(
            rect(340.0, 100.0, 20.0, 20.0),
            size(120.0, 60.0),
            viewport(),
            &spec,
        )
        .expect("placement");
        assert_eq!(result.side, Side::Left, "右溢必须翻到左，而不是垂直方向");
        assert_origin(result.rect, (340.0 - 4.0 - 120.0, 100.0));
    }

    #[test]
    fn a_horizontal_side_shifts_along_the_vertical_axis() {
        let result = solve(
            rect(100.0, 280.0, 40.0, 20.0),
            size(80.0, 80.0),
            viewport(),
            &spec(Side::Right, Alignment::Start),
        )
        .expect("placement");
        assert_origin(result.rect, (144.0, 216.0));
    }

    #[test]
    fn clamps_the_primary_axis_into_the_margin_box() {
        let result = solve(
            rect(100.0, 50.0, 40.0, 20.0),
            size(120.0, 100.0),
            viewport(),
            &spec(Side::Top, Alignment::Start),
        )
        .expect("placement");
        assert_eq!(result.side, Side::Top);
        assert_origin(result.rect, (100.0, 4.0));
    }

    #[test]
    fn the_first_fitting_candidate_wins() {
        let spec = with_fallbacks(bottom(), &[Placement::new(Side::Right, Alignment::Start)]);
        let result = solve(
            rect(100.0, 100.0, 80.0, 30.0),
            size(50.0, 50.0),
            viewport(),
            &spec,
        )
        .expect("placement");
        assert_eq!(result.side, Side::Bottom);
    }

    #[test]
    fn duplicate_candidates_do_not_change_the_outcome() {
        let spec = with_fallbacks(
            bottom(),
            &[
                Placement::new(Side::Bottom, Alignment::Start),
                Placement::new(Side::Top, Alignment::Start),
            ],
        );
        let result = solve(
            rect(100.0, 260.0, 80.0, 30.0),
            size(120.0, 80.0),
            viewport(),
            &spec,
        )
        .expect("placement");
        assert_eq!(result.side, Side::Top);
    }

    #[test]
    fn a_zero_sized_popup_is_placed_on_the_anchor_edge() {
        let result = solve(
            rect(100.0, 100.0, 80.0, 30.0),
            size(0.0, 0.0),
            viewport(),
            &bottom(),
        )
        .expect("placement");
        assert_origin(result.rect, (100.0, 134.0));
        assert!(!result.resized);
    }

    #[test]
    fn a_narrow_viewport_keeps_the_popup_inside_the_margin_box() {
        let result = solve(
            rect(0.0, 0.0, 10.0, 10.0),
            size(200.0, 20.0),
            size(100.0, 100.0),
            &bottom(),
        )
        .expect("placement");
        assert!(result.resized);
        assert_origin(result.rect, (4.0, 14.0));
        assert_size(result.rect, (92.0, 20.0));
    }

    #[test]
    fn a_viewport_smaller_than_the_margins_still_returns_finite_geometry() {
        let result = solve(
            rect(0.0, 0.0, 10.0, 10.0),
            size(10.0, 10.0),
            size(4.0, 4.0),
            &bottom(),
        )
        .expect("placement");
        assert!(result.resized);
        assert!(result.rect.is_finite());
        assert_size(result.rect, (0.0, 0.0));
    }

    #[test]
    fn rejects_non_finite_geometry() {
        let error = solve(
            rect(f32::NAN, 0.0, 10.0, 10.0),
            size(10.0, 10.0),
            viewport(),
            &bottom(),
        )
        .expect_err("NaN anchor must be rejected");
        assert!(matches!(error, PopupError::InvalidGeometry { .. }));

        let error = solve(
            rect(0.0, 0.0, 10.0, 10.0),
            size(f32::INFINITY, 10.0),
            viewport(),
            &bottom(),
        )
        .expect_err("infinite size must be rejected");
        assert!(matches!(error, PopupError::InvalidGeometry { .. }));

        let error = solve(
            rect(0.0, 0.0, 10.0, 10.0),
            size(10.0, 10.0),
            size(400.0, f32::NAN),
            &bottom(),
        )
        .expect_err("NaN viewport must be rejected");
        assert!(matches!(error, PopupError::InvalidGeometry { .. }));

        let error = solve(
            rect(0.0, 0.0, 10.0, -10.0),
            size(10.0, 10.0),
            viewport(),
            &bottom(),
        )
        .expect_err("negative anchor size must be rejected");
        assert!(matches!(error, PopupError::InvalidGeometry { .. }));
    }

    #[test]
    fn rejects_invalid_placement_values() {
        let mut invalid_gap = bottom();
        invalid_gap.gap = px(f32::NAN);
        let error = solve(
            rect(0.0, 0.0, 10.0, 10.0),
            size(10.0, 10.0),
            viewport(),
            &invalid_gap,
        )
        .expect_err("NaN gap must be rejected");
        assert!(matches!(
            error,
            PopupError::InvalidPlacement { gap, .. } if gap.value().is_nan()
        ));

        let mut negative_margin = bottom();
        negative_margin.viewport_margin = px(-1.0);
        let error = solve(
            rect(0.0, 0.0, 10.0, 10.0),
            size(10.0, 10.0),
            viewport(),
            &negative_margin,
        )
        .expect_err("negative margin must be rejected");
        assert!(matches!(
            error,
            PopupError::InvalidPlacement { margin, .. } if margin == px(-1.0)
        ));
    }

    #[test]
    fn solve_placement_is_pure_across_calls() {
        let spec = bottom();
        let anchor = rect(100.0, 100.0, 80.0, 30.0);
        let first = solve(anchor, size(120.0, 80.0), viewport(), &spec).expect("first");
        let second = solve(anchor, size(120.0, 80.0), viewport(), &spec).expect("second");
        assert_eq!(first, second);
    }

    #[test]
    fn without_an_avoid_rect_horizontal_placement_uses_the_anchor_edge() {
        let result = solve(
            rect(120.0, 120.0, 160.0, 24.0),
            size(150.0, 100.0),
            size(800.0, 600.0),
            &spec(Side::Right, Alignment::Start),
        )
        .expect("placement");
        assert_eq!(result.side, Side::Right);
        assert_origin(result.rect, (284.0, 120.0));
    }

    #[test]
    fn an_avoid_rect_moves_the_horizontal_candidate_to_its_outer_edge() {
        let panel = rect(100.0, 100.0, 200.0, 300.0);
        let item = rect(120.0, 120.0, 160.0, 24.0);
        let result = solve_avoiding(
            item,
            size(150.0, 100.0),
            size(800.0, 600.0),
            panel,
            &spec(Side::Right, Alignment::Start),
        )
        .expect("placement");
        assert_eq!(result.side, Side::Right);
        assert_origin(result.rect, (304.0, 120.0));
        assert!(
            !result.rect.intersects(panel),
            "子菜单不得覆盖父菜单面板：{:?}",
            result.rect
        );
    }

    #[test]
    fn a_submenu_flips_to_the_left_when_the_right_side_would_cover_the_parent() {
        let panel = rect(300.0, 100.0, 150.0, 200.0);
        let item = rect(320.0, 120.0, 100.0, 24.0);
        let spec = with_fallbacks(
            spec(Side::Right, Alignment::Start),
            &[Placement::new(Side::Left, Alignment::Start)],
        );
        let result = solve_avoiding(item, size(200.0, 80.0), size(500.0, 400.0), panel, &spec)
            .expect("placement");
        assert_eq!(result.side, Side::Left, "右侧放不下时必须翻到父菜单左侧");
        assert_origin(result.rect, (96.0, 120.0));
        assert!(
            !result.rect.intersects(panel),
            "翻左后不得覆盖父菜单面板：{:?}",
            result.rect
        );
    }

    #[test]
    fn a_flipped_submenu_still_shifts_along_the_vertical_axis() {
        let panel = rect(600.0, 400.0, 150.0, 150.0);
        let item = rect(620.0, 520.0, 100.0, 24.0);
        let spec = with_fallbacks(
            spec(Side::Right, Alignment::Start),
            &[Placement::new(Side::Left, Alignment::Start)],
        );
        let result = solve_avoiding(item, size(200.0, 80.0), size(800.0, 600.0), panel, &spec)
            .expect("placement");
        assert_eq!(result.side, Side::Left);
        assert_origin(result.rect, (396.0, 516.0));
        assert!(
            !result.rect.intersects(panel),
            "翻左后不得覆盖父菜单面板：{:?}",
            result.rect
        );
    }

    #[test]
    fn a_flipped_submenu_clamps_when_neither_side_fits() {
        let panel = rect(100.0, 100.0, 150.0, 200.0);
        let item = rect(120.0, 120.0, 100.0, 24.0);
        let spec = with_fallbacks(
            spec(Side::Right, Alignment::Start),
            &[Placement::new(Side::Left, Alignment::Start)],
        );
        let result = solve_avoiding(
            item,
            size(200.0, 80.0),
            size(300.0, 300.0),
            panel,
            &PlacementSpec {
                viewport_margin: px(0.0),
                ..spec
            },
        )
        .expect("placement");
        assert_eq!(result.side, Side::Left);
        assert_eq!(
            result.rect.origin.x,
            px(0.0),
            "双侧都放不下时必须 clamp 进视口"
        );
        assert!(result.rect.is_finite());
    }

    #[test]
    fn an_avoid_rect_rejects_a_vertical_candidate_that_would_cover_it() {
        let avoid = rect(100.0, 150.0, 200.0, 100.0);
        let item = rect(120.0, 260.0, 80.0, 20.0);
        let spec = with_fallbacks(
            spec(Side::Top, Alignment::Start),
            &[Placement::new(Side::Bottom, Alignment::Start)],
        );
        let result = solve_avoiding(item, size(120.0, 60.0), size(400.0, 600.0), avoid, &spec)
            .expect("placement");
        assert_eq!(result.side, Side::Bottom, "覆盖 avoid 的候选必须被拒绝");
        assert!(
            !result.rect.intersects(avoid),
            "最终矩形不得覆盖 avoid：{:?}",
            result.rect
        );
    }

    #[test]
    fn a_non_finite_avoid_rect_is_invalid_geometry() {
        let error = solve_placement(PlacementInput {
            anchor: rect(0.0, 0.0, 10.0, 10.0),
            size: size(10.0, 10.0),
            viewport: viewport(),
            avoid: Some(rect(f32::NAN, 0.0, 10.0, 10.0)),
            spec: bottom(),
        })
        .expect_err("NaN avoid 必须被拒绝");
        assert!(matches!(
            error,
            PopupError::InvalidGeometry {
                avoid: Some(avoid),
                ..
            } if !avoid.is_convertible()
        ));

        let error = solve_placement(PlacementInput {
            anchor: rect(0.0, 0.0, 10.0, 10.0),
            size: size(10.0, 10.0),
            viewport: viewport(),
            avoid: Some(rect(0.0, 0.0, -10.0, 10.0)),
            spec: bottom(),
        })
        .expect_err("负尺寸 avoid 必须被拒绝");
        assert!(matches!(
            error,
            PopupError::InvalidGeometry { avoid: Some(_), .. }
        ));
    }
}

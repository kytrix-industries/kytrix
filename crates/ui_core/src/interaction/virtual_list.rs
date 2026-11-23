use std::ops::Range;

use crate::{interaction::VirtualSlot, math::LogicalPx};

#[must_use]
pub(crate) fn visible_range(
    offset: LogicalPx,
    viewport: LogicalPx,
    extent: LogicalPx,
    count: usize,
    overscan: usize,
) -> Range<usize> {
    if count == 0 || extent <= LogicalPx::ZERO {
        return 0..0;
    }
    if viewport <= LogicalPx::ZERO {
        return 0..(1 + overscan).min(count);
    }
    let first = ((offset / extent).floor().max(0.0)) as usize;
    let first = first.saturating_sub(overscan).min(count);
    let visible = (((offset + viewport) / extent).ceil().max(0.0)) as usize;
    let last = visible.saturating_add(overscan).min(count);
    let last = last.max((first + 1).min(count));
    first..last
}

#[must_use]
pub(crate) fn visible_range_extents(
    offset: LogicalPx,
    viewport: LogicalPx,
    prefix: &[LogicalPx],
    overscan: usize,
) -> Range<usize> {
    let count = prefix.len().saturating_sub(1);
    if count == 0 || prefix[count] <= LogicalPx::ZERO {
        return 0..0;
    }
    if viewport <= LogicalPx::ZERO {
        return 0..(1 + overscan).min(count);
    }
    let tops = &prefix[..count];
    let first = if offset >= prefix[count] {
        count
    } else {
        tops.partition_point(|top| *top <= offset).saturating_sub(1)
    };
    let first = first.saturating_sub(overscan).min(count);
    let last = tops.partition_point(|top| *top < offset + viewport);
    let last = last.saturating_add(overscan).min(count);
    let last = last.max((first + 1).min(count));
    first..last
}

#[must_use]
pub(crate) fn visible_window(
    slot: &VirtualSlot,
    offset: LogicalPx,
    viewport: LogicalPx,
) -> Range<usize> {
    match slot.active_extents() {
        Some(_) => visible_range_extents(offset, viewport, slot.prefix(), slot.overscan),
        None => visible_range(offset, viewport, slot.extent, slot.count, slot.overscan),
    }
}

#[must_use]
pub(crate) fn row_top_height(slot: &VirtualSlot, index: usize) -> (LogicalPx, LogicalPx) {
    match slot.active_extents() {
        Some(extents) => (slot.prefix()[index], extents[index]),
        None => (slot.extent * (index as f32), slot.extent),
    }
}

#[must_use]
pub(crate) fn content_height(slot: &VirtualSlot) -> LogicalPx {
    match slot.active_extents() {
        Some(_) => slot.prefix()[slot.count],
        None => slot.extent * (slot.count as f32),
    }
}

#[must_use]
pub(crate) fn offset_anchor(slot: &VirtualSlot, offset: LogicalPx) -> (usize, LogicalPx) {
    if slot.count == 0 {
        return (0, LogicalPx::ZERO);
    }
    match slot.active_extents() {
        Some(_) => {
            if slot.prefix()[slot.count] <= LogicalPx::ZERO {
                return (0, LogicalPx::ZERO);
            }
            let tops = &slot.prefix()[..slot.count];
            let first = tops
                .partition_point(|top| *top <= offset)
                .saturating_sub(1)
                .min(slot.count - 1);
            (first, (offset - tops[first]).max(LogicalPx::ZERO))
        }
        None => {
            let first = (((offset / slot.extent).floor().max(0.0)) as usize).min(slot.count - 1);
            (
                first,
                (offset - slot.extent * (first as f32)).max(LogicalPx::ZERO),
            )
        }
    }
}

#[must_use]
pub(crate) fn offset_for_anchor(slot: &VirtualSlot, first: usize, inner: LogicalPx) -> LogicalPx {
    if slot.count == 0 {
        return LogicalPx::ZERO;
    }
    let index = first.min(slot.count - 1);
    let top = match slot.active_extents() {
        Some(_) => slot.prefix()[index],
        None => slot.extent * (index as f32),
    };
    (top + inner).max(LogicalPx::ZERO)
}

#[cfg(test)]
mod tests {
    use super::{
        content_height, offset_anchor, offset_for_anchor, row_top_height, visible_range,
        visible_range_extents, visible_window,
    };
    use crate::{
        interaction::VirtualSlot,
        math::{LogicalPx, px},
    };

    fn slot(
        extent: LogicalPx,
        count: usize,
        overscan: usize,
        extents: Option<Vec<LogicalPx>>,
    ) -> VirtualSlot {
        let mut slot = VirtualSlot::new(extent, overscan);
        slot.count = count;
        if let Some(extents) = extents {
            slot.set_extents(extents.into_boxed_slice());
        }
        slot
    }

    #[test]
    fn visible_range_math() {
        assert_eq!(visible_range(px(0.0), px(400.0), px(100.0), 100, 0), 0..4);
        assert_eq!(visible_range(px(250.0), px(400.0), px(100.0), 100, 0), 2..7);
        assert_eq!(visible_range(px(150.0), px(100.0), px(100.0), 100, 0), 1..3);
    }

    #[test]
    fn overscan_expands_both_ends() {
        assert_eq!(visible_range(px(250.0), px(400.0), px(100.0), 100, 2), 0..9);
        assert_eq!(visible_range(px(0.0), px(100.0), px(100.0), 100, 2), 0..3);
        assert_eq!(
            visible_range(px(9_000.0), px(400.0), px(100.0), 100, 2),
            88..96
        );
    }

    #[test]
    fn empty_inputs_yield_an_empty_range() {
        assert_eq!(visible_range(px(0.0), px(400.0), px(100.0), 0, 0), 0..0);
        assert_eq!(visible_range(px(0.0), px(400.0), px(0.0), 10, 0), 0..0);
        assert_eq!(visible_range(px(0.0), px(0.0), px(100.0), 10, 0), 0..1);
        assert_eq!(visible_range(px(0.0), px(0.0), px(100.0), 10, 2), 0..3);
    }

    #[test]
    fn a_single_slot_is_always_mounted() {
        assert_eq!(visible_range(px(0.0), px(10.0), px(100.0), 50, 0), 0..1);
        assert_eq!(
            visible_range(px(4_900.0), px(10.0), px(100.0), 50, 0),
            49..50
        );
    }

    #[test]
    fn a_uniform_extents_list_matches_the_single_extent_math() {
        let heights = vec![px(25.0); 100];
        let slot = slot(px(25.0), 100, 0, Some(heights));
        for overscan in [0_usize, 2] {
            for viewport in [px(0.0), px(10.0), px(100.0), px(400.0)] {
                for offset in [
                    px(0.0),
                    px(10.0),
                    px(25.0),
                    px(250.0),
                    px(1_000.0),
                    px(2_475.0),
                ] {
                    assert_eq!(
                        visible_range_extents(offset, viewport, slot.prefix(), overscan),
                        visible_range(offset, viewport, px(25.0), 100, overscan),
                        "offset={offset:?} viewport={viewport:?} overscan={overscan}"
                    );
                }
            }
        }
    }

    #[test]
    fn variable_heights_pick_the_window_from_the_prefix() {
        let slot = slot(
            px(1.0),
            5,
            0,
            Some(vec![px(50.0), px(50.0), px(0.0), px(100.0), px(200.0)]),
        );
        assert_eq!(
            visible_window(&slot, px(0.0), px(100.0)),
            0..2,
            "首行起：两行 50 铺满视口"
        );
        assert_eq!(
            visible_window(&slot, px(100.0), px(100.0)),
            3..4,
            "零高行与上一行同 top：锚到首个非空行"
        );
        assert_eq!(
            visible_window(&slot, px(150.0), px(100.0)),
            3..5,
            "行内偏移落在第三行"
        );
        assert_eq!(visible_window(&slot, px(300.0), px(100.0)), 4..5, "末行");
        assert_eq!(
            visible_window(&slot, px(0.0), px(0.0)),
            0..1,
            "视口未知时挂首行"
        );
    }

    #[test]
    fn variable_range_handles_empty_and_overscan() {
        assert_eq!(
            visible_range_extents(px(0.0), px(400.0), &[px(0.0)], 0),
            0..0,
            "空列表"
        );
        assert_eq!(visible_range_extents(px(0.0), px(400.0), &[], 0), 0..0);
        let prefix = [px(0.0), px(50.0), px(150.0), px(250.0), px(350.0)];
        assert_eq!(
            visible_range_extents(px(100.0), px(100.0), &prefix, 0),
            1..3
        );
        assert_eq!(
            visible_range_extents(px(100.0), px(100.0), &prefix, 2),
            0..4,
            "overscan 两端各扩 2 并 clamp 到 count"
        );
        assert_eq!(
            visible_range_extents(px(0.0), px(0.0), &prefix, 2),
            0..3,
            "视口未知时挂首行 + overscan"
        );
    }

    #[test]
    fn anchor_roundtrips_for_both_layouts() {
        let uniform = slot(px(20.0), 100, 0, None);
        assert_eq!(offset_anchor(&uniform, px(0.0)), (0, px(0.0)));
        assert_eq!(offset_anchor(&uniform, px(120.0)), (6, px(0.0)));
        assert_eq!(offset_anchor(&uniform, px(130.5)), (6, px(10.5)));
        for offset in [px(0.0), px(10.5), px(120.0), px(130.5), px(1_980.0)] {
            let (first, inner) = offset_anchor(&uniform, offset);
            assert_eq!(
                offset_for_anchor(&uniform, first, inner),
                offset,
                "{offset:?}"
            );
        }

        let variable = slot(
            px(20.0),
            6,
            0,
            Some(vec![
                px(50.0),
                px(50.0),
                px(50.0),
                px(200.0),
                px(20.0),
                px(100.0),
            ]),
        );
        for (offset, expected) in [
            (px(0.0), (0, px(0.0))),
            (px(49.5), (0, px(49.5))),
            (px(50.0), (1, px(0.0))),
            (px(120.0), (2, px(20.0))),
            (px(150.0), (3, px(0.0))),
            (px(360.0), (4, px(10.0))),
        ] {
            assert_eq!(offset_anchor(&variable, offset), expected, "{offset:?}");
            assert_eq!(
                offset_for_anchor(&variable, expected.0, expected.1),
                offset,
                "{offset:?}"
            );
        }

        let shortened = slot(px(20.0), 3, 0, None);
        assert_eq!(
            offset_for_anchor(&shortened, 99, px(10.0)),
            px(50.0),
            "越界锚点退到末项"
        );
        assert_eq!(
            offset_for_anchor(&slot(px(20.0), 0, 0, None), 5, px(10.0)),
            px(0.0)
        );
    }

    #[test]
    fn a_zero_content_list_anchors_at_the_top() {
        let folded = slot(px(10.0), 10, 0, Some(vec![px(0.0); 10]));
        assert_eq!(content_height(&folded), px(0.0));
        assert_eq!(offset_anchor(&folded, px(0.0)), (0, px(0.0)));
        assert_eq!(offset_for_anchor(&folded, 0, px(0.0)), px(0.0));
        let mut measured = slot(px(10.0), 10, 0, None);
        measured.set_extents(vec![px(100.0); 10].into_boxed_slice());
        assert_eq!(offset_for_anchor(&measured, 0, px(0.0)), px(0.0));
    }

    #[test]
    fn row_geometry_and_content_height_follow_the_active_layout() {
        let variable = slot(px(10.0), 3, 0, Some(vec![px(0.0), px(30.0), px(70.0)]));
        assert_eq!(row_top_height(&variable, 0), (px(0.0), px(0.0)), "零高行");
        assert_eq!(row_top_height(&variable, 1), (px(0.0), px(30.0)));
        assert_eq!(row_top_height(&variable, 2), (px(30.0), px(70.0)));
        assert_eq!(content_height(&variable), px(100.0));

        let uniform = slot(px(25.0), 4, 0, None);
        assert_eq!(row_top_height(&uniform, 3), (px(75.0), px(25.0)));
        assert_eq!(content_height(&uniform), px(100.0));

        let stale = slot(
            px(25.0),
            5,
            0,
            Some(vec![px(10.0), px(10.0), px(10.0), px(10.0)]),
        );
        assert_eq!(stale.active_extents(), None);
        assert_eq!(row_top_height(&stale, 2), (px(50.0), px(25.0)));
        assert_eq!(content_height(&stale), px(125.0));
    }

    #[test]
    fn prefix_cache_is_rebuilt_on_every_extents_change() {
        let mut slot = slot(px(10.0), 2, 0, None);
        slot.set_extents(vec![px(40.0), px(60.0)].into_boxed_slice());
        assert_eq!(slot.active_extents(), Some(&[px(40.0), px(60.0)][..]));
        assert_eq!(slot.prefix(), &[px(0.0), px(40.0), px(100.0)]);
        assert_eq!(visible_window(&slot, px(0.0), px(50.0)), 0..2);

        slot.set_extents(vec![px(50.0), px(50.0)].into_boxed_slice());
        assert_eq!(slot.prefix(), &[px(0.0), px(50.0), px(100.0)]);
        assert_eq!(visible_window(&slot, px(0.0), px(50.0)), 0..1);

        slot.clear_extents();
        assert_eq!(slot.active_extents(), None);
        assert!(slot.prefix().is_empty());
        assert_eq!(content_height(&slot), px(20.0));
        assert_eq!(visible_window(&slot, px(0.0), px(20.0)), 0..2);
    }
}

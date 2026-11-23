use std::ops::Range;

use crate::interaction::VirtualSlot;

#[must_use]
pub(crate) fn visible_range(
    offset: f32,
    viewport: f32,
    extent: f32,
    count: usize,
    overscan: usize,
) -> Range<usize> {
    if count == 0 || extent <= 0.0 {
        return 0..0;
    }
    if viewport <= 0.0 {
        return 0..(1 + overscan).min(count);
    }
    let first = (offset / extent).floor().max(0.0) as usize;
    let first = first.saturating_sub(overscan).min(count);
    let visible = ((offset + viewport) / extent).ceil().max(0.0) as usize;
    let last = visible.saturating_add(overscan).min(count);
    let last = last.max((first + 1).min(count));
    first..last
}

#[must_use]
pub(crate) fn visible_range_extents(
    offset: f32,
    viewport: f32,
    prefix: &[f32],
    overscan: usize,
) -> Range<usize> {
    let count = prefix.len().saturating_sub(1);
    if count == 0 || prefix[count] <= 0.0 {
        return 0..0;
    }
    if viewport <= 0.0 {
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
pub(crate) fn visible_window(slot: &VirtualSlot, offset: f32, viewport: f32) -> Range<usize> {
    match slot.active_extents() {
        Some(_) => visible_range_extents(offset, viewport, slot.prefix(), slot.overscan),
        None => visible_range(offset, viewport, slot.extent, slot.count, slot.overscan),
    }
}

#[must_use]
pub(crate) fn row_top_height(slot: &VirtualSlot, index: usize) -> (f32, f32) {
    match slot.active_extents() {
        Some(extents) => (slot.prefix()[index], extents[index]),
        None => (index as f32 * slot.extent, slot.extent),
    }
}

#[must_use]
pub(crate) fn content_height(slot: &VirtualSlot) -> f32 {
    match slot.active_extents() {
        Some(_) => slot.prefix()[slot.count],
        None => slot.count as f32 * slot.extent,
    }
}

#[must_use]
pub(crate) fn offset_anchor(slot: &VirtualSlot, offset: f32) -> (usize, f32) {
    if slot.count == 0 {
        return (0, 0.0);
    }
    match slot.active_extents() {
        Some(_) => {
            if slot.prefix()[slot.count] <= 0.0 {
                return (0, 0.0);
            }
            let tops = &slot.prefix()[..slot.count];
            let first = tops
                .partition_point(|top| *top <= offset)
                .saturating_sub(1)
                .min(slot.count - 1);
            (first, (offset - tops[first]).max(0.0))
        }
        None => {
            let first = ((offset / slot.extent).floor().max(0.0) as usize).min(slot.count - 1);
            (first, (offset - first as f32 * slot.extent).max(0.0))
        }
    }
}

#[must_use]
pub(crate) fn offset_for_anchor(slot: &VirtualSlot, first: usize, inner: f32) -> f32 {
    if slot.count == 0 {
        return 0.0;
    }
    let index = first.min(slot.count - 1);
    let top = match slot.active_extents() {
        Some(_) => slot.prefix()[index],
        None => index as f32 * slot.extent,
    };
    (top + inner).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::{
        content_height, offset_anchor, offset_for_anchor, row_top_height, visible_range,
        visible_range_extents, visible_window,
    };
    use crate::interaction::VirtualSlot;

    fn slot(extent: f32, count: usize, overscan: usize, extents: Option<Vec<f32>>) -> VirtualSlot {
        let mut slot = VirtualSlot::new(extent, overscan);
        slot.count = count;
        if let Some(extents) = extents {
            slot.set_extents(extents.into_boxed_slice());
        }
        slot
    }

    #[test]
    fn visible_range_math() {
        assert_eq!(visible_range(0.0, 400.0, 100.0, 100, 0), 0..4);
        assert_eq!(visible_range(250.0, 400.0, 100.0, 100, 0), 2..7);
        assert_eq!(visible_range(150.0, 100.0, 100.0, 100, 0), 1..3);
    }

    #[test]
    fn overscan_expands_both_ends() {
        assert_eq!(visible_range(250.0, 400.0, 100.0, 100, 2), 0..9);
        assert_eq!(visible_range(0.0, 100.0, 100.0, 100, 2), 0..3);
        assert_eq!(visible_range(9_000.0, 400.0, 100.0, 100, 2), 88..96);
    }

    #[test]
    fn empty_inputs_yield_an_empty_range() {
        assert_eq!(visible_range(0.0, 400.0, 100.0, 0, 0), 0..0);
        assert_eq!(visible_range(0.0, 400.0, 0.0, 10, 0), 0..0);
        assert_eq!(visible_range(0.0, 0.0, 100.0, 10, 0), 0..1);
        assert_eq!(visible_range(0.0, 0.0, 100.0, 10, 2), 0..3);
    }

    #[test]
    fn a_single_slot_is_always_mounted() {
        assert_eq!(visible_range(0.0, 10.0, 100.0, 50, 0), 0..1);
        assert_eq!(visible_range(4_900.0, 10.0, 100.0, 50, 0), 49..50);
    }

    #[test]
    fn a_uniform_extents_list_matches_the_single_extent_math() {
        let heights = vec![25.0; 100];
        let slot = slot(25.0, 100, 0, Some(heights));
        for overscan in [0_usize, 2] {
            for viewport in [0.0_f32, 10.0, 100.0, 400.0] {
                for offset in [0.0_f32, 10.0, 25.0, 250.0, 1_000.0, 2_475.0] {
                    assert_eq!(
                        visible_range_extents(offset, viewport, slot.prefix(), overscan),
                        visible_range(offset, viewport, 25.0, 100, overscan),
                        "offset={offset} viewport={viewport} overscan={overscan}"
                    );
                }
            }
        }
    }

    #[test]
    fn variable_heights_pick_the_window_from_the_prefix() {
        let slot = slot(1.0, 5, 0, Some(vec![50.0, 50.0, 0.0, 100.0, 200.0]));
        assert_eq!(
            visible_window(&slot, 0.0, 100.0),
            0..2,
            "首行起：两行 50 铺满视口"
        );
        assert_eq!(
            visible_window(&slot, 100.0, 100.0),
            3..4,
            "零高行与上一行同 top：锚到首个非空行"
        );
        assert_eq!(
            visible_window(&slot, 150.0, 100.0),
            3..5,
            "行内偏移落在第三行"
        );
        assert_eq!(visible_window(&slot, 300.0, 100.0), 4..5, "末行");
        assert_eq!(visible_window(&slot, 0.0, 0.0), 0..1, "视口未知时挂首行");
    }

    #[test]
    fn variable_range_handles_empty_and_overscan() {
        assert_eq!(visible_range_extents(0.0, 400.0, &[0.0], 0), 0..0, "空列表");
        assert_eq!(visible_range_extents(0.0, 400.0, &[], 0), 0..0);
        let prefix = [0.0, 50.0, 150.0, 250.0, 350.0];
        assert_eq!(visible_range_extents(100.0, 100.0, &prefix, 0), 1..3);
        assert_eq!(
            visible_range_extents(100.0, 100.0, &prefix, 2),
            0..4,
            "overscan 两端各扩 2 并 clamp 到 count"
        );
        assert_eq!(
            visible_range_extents(0.0, 0.0, &prefix, 2),
            0..3,
            "视口未知时挂首行 + overscan"
        );
    }

    #[test]
    fn anchor_roundtrips_for_both_layouts() {
        let uniform = slot(20.0, 100, 0, None);
        assert_eq!(offset_anchor(&uniform, 0.0), (0, 0.0));
        assert_eq!(offset_anchor(&uniform, 120.0), (6, 0.0));
        assert_eq!(offset_anchor(&uniform, 130.5), (6, 10.5));
        for offset in [0.0_f32, 10.5, 120.0, 130.5, 1_980.0] {
            let (first, inner) = offset_anchor(&uniform, offset);
            assert_eq!(
                offset_for_anchor(&uniform, first, inner),
                offset,
                "{offset}"
            );
        }

        let variable = slot(20.0, 6, 0, Some(vec![50.0, 50.0, 50.0, 200.0, 20.0, 100.0]));
        for (offset, expected) in [
            (0.0, (0, 0.0)),
            (49.5, (0, 49.5)),
            (50.0, (1, 0.0)),
            (120.0, (2, 20.0)),
            (150.0, (3, 0.0)),
            (360.0, (4, 10.0)),
        ] {
            assert_eq!(offset_anchor(&variable, offset), expected, "{offset}");
            assert_eq!(
                offset_for_anchor(&variable, expected.0, expected.1),
                offset,
                "{offset}"
            );
        }

        let shortened = slot(20.0, 3, 0, None);
        assert_eq!(
            offset_for_anchor(&shortened, 99, 10.0),
            50.0,
            "越界锚点退到末项"
        );
        assert_eq!(offset_for_anchor(&slot(20.0, 0, 0, None), 5, 10.0), 0.0);
    }

    #[test]
    fn a_zero_content_list_anchors_at_the_top() {
        let folded = slot(10.0, 10, 0, Some(vec![0.0; 10]));
        assert_eq!(content_height(&folded), 0.0);
        assert_eq!(offset_anchor(&folded, 0.0), (0, 0.0));
        assert_eq!(offset_for_anchor(&folded, 0, 0.0), 0.0);
        let mut measured = slot(10.0, 10, 0, None);
        measured.set_extents(vec![100.0; 10].into_boxed_slice());
        assert_eq!(offset_for_anchor(&measured, 0, 0.0), 0.0);
    }

    #[test]
    fn row_geometry_and_content_height_follow_the_active_layout() {
        let variable = slot(10.0, 3, 0, Some(vec![0.0, 30.0, 70.0]));
        assert_eq!(row_top_height(&variable, 0), (0.0, 0.0), "零高行");
        assert_eq!(row_top_height(&variable, 1), (0.0, 30.0));
        assert_eq!(row_top_height(&variable, 2), (30.0, 70.0));
        assert_eq!(content_height(&variable), 100.0);

        let uniform = slot(25.0, 4, 0, None);
        assert_eq!(row_top_height(&uniform, 3), (75.0, 25.0));
        assert_eq!(content_height(&uniform), 100.0);

        let stale = slot(25.0, 5, 0, Some(vec![10.0, 10.0, 10.0, 10.0]));
        assert_eq!(stale.active_extents(), None);
        assert_eq!(row_top_height(&stale, 2), (50.0, 25.0));
        assert_eq!(content_height(&stale), 125.0);
    }

    #[test]
    fn prefix_cache_is_rebuilt_on_every_extents_change() {
        let mut slot = slot(10.0, 2, 0, None);
        slot.set_extents(vec![40.0, 60.0].into_boxed_slice());
        assert_eq!(slot.active_extents(), Some(&[40.0, 60.0][..]));
        assert_eq!(slot.prefix(), &[0.0, 40.0, 100.0]);
        assert_eq!(visible_window(&slot, 0.0, 50.0), 0..2);

        slot.set_extents(vec![50.0, 50.0].into_boxed_slice());
        assert_eq!(slot.prefix(), &[0.0, 50.0, 100.0]);
        assert_eq!(visible_window(&slot, 0.0, 50.0), 0..1);

        slot.clear_extents();
        assert_eq!(slot.active_extents(), None);
        assert!(slot.prefix().is_empty());
        assert_eq!(content_height(&slot), 20.0);
        assert_eq!(visible_window(&slot, 0.0, 20.0), 0..2);
    }
}

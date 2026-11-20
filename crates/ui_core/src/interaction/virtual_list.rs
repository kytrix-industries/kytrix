//! 虚拟列表：可见窗口计算（纯函数）+ 重开窗的边界判定。
//!
//! 结构：容器（滚动容器 + `Clip::Bounds` + `position: relative`）直接挂**绝对定位的行**，`content` 由
//! 引擎显式给出（taffy 没有「可滚动内容盒」）。挂载/回收的写入口是
//! `App::set_virtual_items` / `App::set_virtual_extents` / `App::scroll_to`。
//!
//! 两套行布局（§11.3 E3）：
//! - 单一 `extent`（`extents = None`）：行 `i` 的 top = `i * extent`、内容高度 = `count * extent`；
//! - 逐项高度（`extents = Some`）：行 `i` 的 top / height 来自前缀和 `prefix`（`prefix[0] = 0`、
//!   `prefix[i + 1] = prefix[i] + extents[i]`），可见区间在 `prefix` 上二分，内容高度 = `prefix[count]`。
//!
//! 前缀和只在 `VirtualSlot::set_extents` 里重建（O(n)）；滚动帧只做 O(log n) 查询。

use std::ops::Range;

use crate::interaction::VirtualSlot;

/// 可见窗口（半开区间）。
///
/// `offset` / `viewport` / `extent` 都是逻辑像素；`overscan` 是上下各多挂的行数。
/// `extent <= 0` 或 `count == 0` 时返回空区间。
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
    // 视口未知（首次布局前）：挂首行（+ overscan），否则「建列表 → 布局」之间会永久空窗。
    if viewport <= 0.0 {
        return 0..(1 + overscan).min(count);
    }
    let first = (offset / extent).floor().max(0.0) as usize;
    let first = first.saturating_sub(overscan).min(count);
    let visible = ((offset + viewport) / extent).ceil().max(0.0) as usize;
    let last = visible.saturating_add(overscan).min(count);
    // 至少一个（`last` 不能回到 `first` 之前），否则「视口落在条目缝隙」时会挂 0 行。
    let last = last.max((first + 1).min(count));
    first..last
}

/// 变高可见窗口（半开区间）：在 `prefix`（长度 `count + 1`）上二分。
///
/// 与 [`visible_range`] 同语义（含 `overscan`、视口未知时挂首行、至少一行）；`offset` 必须已按
/// `content - viewport` 夹紧（与调用方 `ScrollSlot` 的约定一致）。
#[must_use]
pub(crate) fn visible_range_extents(
    offset: f32,
    viewport: f32,
    prefix: &[f32],
    overscan: usize,
) -> Range<usize> {
    let count = prefix.len().saturating_sub(1);
    // 空列表 / 全部零高（内容高度 0）没有可见行 —— 与等宽路径 `extent <= 0 ⇒ 0..0` 同口径。
    if count == 0 || prefix[count] <= 0.0 {
        return 0..0;
    }
    // 视口未知（首次布局前）：挂首行（+ overscan），与等宽路径一致。
    if viewport <= 0.0 {
        return 0..(1 + overscan).min(count);
    }
    let tops = &prefix[..count];
    // 首个可见行：`top <= offset` 的最后一个（`offset >= content` 时退到 `count`，与等宽路径的
    // `floor(offset / extent)` 在可达区间内一致）。
    let first = if offset >= prefix[count] {
        count
    } else {
        tops.partition_point(|top| *top <= offset).saturating_sub(1)
    };
    let first = first.saturating_sub(overscan).min(count);
    // 尾后行：`top < offset + viewport` 的行数（偏移正好落在行边界时不含下一行）。
    let last = tops.partition_point(|top| *top < offset + viewport);
    let last = last.saturating_add(overscan).min(count);
    let last = last.max((first + 1).min(count));
    first..last
}

/// 当前生效行布局下的可见窗口。
///
/// 消费者：`app::virtual_list::{rewindow_virtual, virtual_list_state}`。
#[must_use]
pub(crate) fn visible_window(slot: &VirtualSlot, offset: f32, viewport: f32) -> Range<usize> {
    match slot.active_extents() {
        Some(_) => visible_range_extents(offset, viewport, slot.prefix(), slot.overscan),
        None => visible_range(offset, viewport, slot.extent, slot.count, slot.overscan),
    }
}

/// 第 `index` 行的 `(top, height)`（逻辑像素，内容坐标）。
///
/// 消费者：`app::virtual_list::{rewindow_virtual, set_virtual_extents, scroll_to_item}`。
#[must_use]
pub(crate) fn row_top_height(slot: &VirtualSlot, index: usize) -> (f32, f32) {
    match slot.active_extents() {
        Some(extents) => (slot.prefix()[index], extents[index]),
        None => (index as f32 * slot.extent, slot.extent),
    }
}

/// 内容总高度（逻辑像素）。
///
/// 消费者：`app::virtual_list::{rewindow_virtual, set_virtual_extents, scroll_to_item}`。
#[must_use]
pub(crate) fn content_height(slot: &VirtualSlot) -> f32 {
    match slot.active_extents() {
        Some(_) => slot.prefix()[slot.count],
        None => slot.count as f32 * slot.extent,
    }
}

/// 偏移锚点：`(首个可见 index, 该行内偏移)`。
///
/// **锚点规则**（§11.3）：逐项高度整体替换时，先按旧布局取锚点，替换后用
/// [`offset_for_anchor`] 重算偏移，从而保持「同一 first index + 同一内偏移」。
///
/// 消费者：`app::virtual_list::{set_virtual_items, set_virtual_extents}`。
#[must_use]
pub(crate) fn offset_anchor(slot: &VirtualSlot, offset: f32) -> (usize, f32) {
    if slot.count == 0 {
        return (0, 0.0);
    }
    match slot.active_extents() {
        Some(_) => {
            // 全零内容（尚未测量 / 全部折叠）没有可分辨的锚点：所有 top 并列 0，
            // `partition_point` 会选到末项，首次拿到真实行高时视图从顶部跳到底部。
            // 与 `visible_range_extents` 的零内容早返回同口径 ⇒ 固定 (0, 0)。
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

/// 按锚点反算偏移：`prefix'[first] + inner`（`count == 0` ⇒ `0`），按新行数 clamp。
///
/// 消费者：`app::virtual_list::{set_virtual_items, set_virtual_extents}`。
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
        // 视口 400、行高 100、无 overscan ⇒ 0..4（恰好铺满）。
        assert_eq!(visible_range(0.0, 400.0, 100.0, 100, 0), 0..4);
        // 滚 250 ⇒ 从 2 开始，到 ceil(650/100)=7。
        assert_eq!(visible_range(250.0, 400.0, 100.0, 100, 0), 2..7);
        // 视口中间落在缝隙上：ceil 仍把最后一行算进来。
        assert_eq!(visible_range(150.0, 100.0, 100.0, 100, 0), 1..3);
    }

    #[test]
    fn overscan_expands_both_ends() {
        assert_eq!(visible_range(250.0, 400.0, 100.0, 100, 2), 0..9);
        assert_eq!(visible_range(0.0, 100.0, 100.0, 100, 2), 0..3);
        // 尾部夹到 count。
        // 尾部夹到 count：first=90−2=88，last=ceil(9400/100)+2=96。
        assert_eq!(visible_range(9_000.0, 400.0, 100.0, 100, 2), 88..96);
    }

    #[test]
    fn empty_inputs_yield_an_empty_range() {
        assert_eq!(visible_range(0.0, 400.0, 100.0, 0, 0), 0..0);
        assert_eq!(visible_range(0.0, 400.0, 0.0, 10, 0), 0..0);
        // 视口未知（首次布局前）⇒ 至少挂首行，避免「建列表 → 布局」之间永久空窗。
        assert_eq!(visible_range(0.0, 0.0, 100.0, 10, 0), 0..1);
        assert_eq!(visible_range(0.0, 0.0, 100.0, 10, 2), 0..3);
    }

    #[test]
    fn a_single_slot_is_always_mounted() {
        // 视口比行高小 ⇒ 至少 1 行。
        assert_eq!(visible_range(0.0, 10.0, 100.0, 50, 0), 0..1);
        // 偏移正好落在行边界且 count 已到尾部。
        assert_eq!(visible_range(4_900.0, 10.0, 100.0, 50, 0), 49..50);
    }

    /// 逐项高度全部相等 ⇒ 可见区间与等宽路径逐点一致（`extents = None` 的回归基准）。
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

    /// 变高列表的首 / 中 / 尾边界：`top = prefix[i]`，窗口在 `prefix` 上二分。
    #[test]
    fn variable_heights_pick_the_window_from_the_prefix() {
        // tops = 0, 50, 100, 100, 200；content = 400。
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
        // count = 4；tops = 0, 50, 150, 250。
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

    /// 锚点往返：`prefix[first] + inner == offset`（行数变化时 clamp 到末项）。
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

        // tops = 0, 50, 100, 150, 350, 370；content = 470。
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

    /// F1 回归：全零内容（未测量 / 全折叠）没有可分辨的锚点 ⇒ 固定 (0,0)，不得选到末项。
    #[test]
    fn a_zero_content_list_anchors_at_the_top() {
        let folded = slot(10.0, 10, 0, Some(vec![0.0; 10]));
        assert_eq!(content_height(&folded), 0.0);
        assert_eq!(offset_anchor(&folded, 0.0), (0, 0.0));
        assert_eq!(offset_for_anchor(&folded, 0, 0.0), 0.0);
        // 拿到真实行高后从 (0, 0) 反算仍是顶部（修复前零高并列会让锚点落到末项 ⇒ 900）。
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

        // 预置高度与条目数不一致 ⇒ 按单一 extent（make → set_virtual_items 的过渡窗口）。
        let stale = slot(25.0, 5, 0, Some(vec![10.0, 10.0, 10.0, 10.0]));
        assert_eq!(stale.active_extents(), None);
        assert_eq!(row_top_height(&stale, 2), (50.0, 25.0));
        assert_eq!(content_height(&stale), 125.0);
    }

    /// 前缀和缓存只在 extents 变化时重建：换值 / 清空后立刻反映新布局。
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

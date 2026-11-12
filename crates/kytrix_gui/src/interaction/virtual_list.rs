//! 虚拟列表：可见窗口计算（纯函数）+ 重开窗的边界判定。
//!
//! 结构：容器（滚动容器 + `Clip::Bounds` + `position: relative`）直接挂**绝对定位的行**，`content` 由
//! `count × extent` 显式给出（taffy 没有「可滚动内容盒」）。挂载/回收的写入口是
//! `App::set_virtual_items` / `App::scroll_to`（`design/phase-10-interaction.md` §8）。

use std::ops::Range;

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

#[cfg(test)]
mod tests {
    use super::visible_range;

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
}

//! 行内流：`InlineBoxSpec → parley::InlineBox` 与 `(anchor, focus)` 选区的几何映射（§12）。
//!
//! Markdown AST → `Vec<InlineFragment>` 的**数据管线不在本阶段**（P15 的消费者还不存在）；
//! 本模块只把「盒的输入面」和「选区的输出面」钉死。

use parley::{
    Affinity, BoundingBox, InlineBox, InlineBoxKind, VerticalAlign,
    editing::{Cursor, Selection},
};

use super::{InlineAlign, InlineBoxSpec, TextDiagnostic};
use crate::{
    math::{Point, Rect},
    paint::color::ColorBrush,
};

/// `InlineBoxSpec` → parley `InlineBox`（逻辑像素 → 物理像素）。
///
/// `index` 必须落在 char boundary（parley 的硬要求，`inline_box.rs:14-16`）：非法时吸附到**前一个**
/// 合法边界并返回诊断（不 panic —— 应用给错偏移不该炸窗口）。
pub(crate) fn to_inline_box(
    spec: &InlineBoxSpec,
    text: &str,
) -> (InlineBox, Option<TextDiagnostic>) {
    let (index, diagnostic) = if spec.index <= text.len() && text.is_char_boundary(spec.index) {
        (spec.index, None)
    } else {
        (
            floor_boundary(spec.index, text),
            Some(TextDiagnostic::InlineBoxOffBoundary {
                id: spec.id,
                index: spec.index,
            }),
        )
    };
    // 逻辑坐标（R12-4）：行内盒尺寸/基线与文本一样用逻辑像素，scale 只在光栅/落点阶段相乘。
    let inline_box = InlineBox {
        id: spec.id,
        kind: InlineBoxKind::InFlow,
        index,
        width: spec.size.width,
        height: spec.size.height,
        baseline: spec.baseline,
        vertical_align: vertical_align(spec.align),
    };
    (inline_box, diagnostic)
}

/// 向下取最近的合法字节边界；`index > len` 时取 `len`。
fn floor_boundary(index: usize, text: &str) -> usize {
    let mut candidate = index.min(text.len());
    while candidate > 0 && !text.is_char_boundary(candidate) {
        candidate -= 1;
    }
    candidate
}

/// `InlineAlign` → parley `VerticalAlign`（1:1 的 8 个常量）。
pub(crate) fn vertical_align(align: InlineAlign) -> VerticalAlign {
    match align {
        InlineAlign::Baseline => VerticalAlign::BASELINE,
        InlineAlign::Sub => VerticalAlign::SUB,
        InlineAlign::Super => VerticalAlign::SUPER,
        InlineAlign::TextTop => VerticalAlign::TEXT_TOP,
        InlineAlign::TextBottom => VerticalAlign::TEXT_BOTTOM,
        InlineAlign::Middle => VerticalAlign::MIDDLE,
        InlineAlign::Top => VerticalAlign::TOP,
        InlineAlign::Bottom => VerticalAlign::BOTTOM,
    }
}

/// `(anchor, focus)` 字节偏移 → 每行的选区矩形（文本盒局部逻辑像素）。
///
/// 用 parley 的 `Selection` 而不是自算：bidi/连字/簇边界只有它知道（§12.5）。
pub(crate) fn selection_rects(
    layout: &parley::Layout<ColorBrush>,
    anchor: usize,
    focus: usize,
) -> Vec<Rect> {
    let anchor_cursor = Cursor::from_byte_index(layout, anchor, Affinity::Downstream);
    let focus_cursor = Cursor::from_byte_index(layout, focus, Affinity::Upstream);
    let selection = Selection::new(anchor_cursor, focus_cursor);
    if selection.is_collapsed() {
        return Vec::new();
    }
    selection
        .geometry(layout)
        .into_iter()
        .map(|(bounds, _line): (BoundingBox, usize)| to_rect(bounds))
        .collect()
}

/// `byte_index` 处的 caret 矩形（文本盒局部逻辑像素，宽 1 逻辑像素）。
pub(crate) fn caret_rect(layout: &parley::Layout<ColorBrush>, byte_index: usize) -> Rect {
    let cursor = Cursor::from_byte_index(layout, byte_index, Affinity::Downstream);
    to_rect(cursor.geometry(layout, 1.0))
}

/// 文本盒局部逻辑坐标 → 最近的**合法字节边界**（[`caret_rect`] 的逆向，E5/§11.5）。
///
/// 簇边界 / bidi / 行内盒偏移只有 parley 知道，因此命中统一走 `Cursor::from_point`：
/// 同一行内水平越界由其收敛到该行首/行尾（半簇取整）。**垂直**越界按规格显式 clamp ——
/// 文本盒上方 `0`、下方 `text.len()`；盒内取最近行（`line_for_offset` 的行命中）。
/// 返回索引再做一次 char boundary 收敛（parley 已保证，这里只是契约的兜底）。
///
/// `text` 必须与该 `layout` 的源文本是同一份（调用方保证），仅用于 clamp 上界与边界收敛。
pub(crate) fn byte_index_at(
    layout: &parley::Layout<ColorBrush>,
    text: &str,
    point: Point,
) -> usize {
    if point.y < 0.0 {
        return 0;
    }
    if point.y > layout.height() {
        return text.len();
    }
    floor_boundary(Cursor::from_point(layout, point.x, point.y).index(), text)
}

/// parley 的 bounding box → 逻辑像素矩形（布局本身已是逻辑坐标，R12-4）。
fn to_rect(bounds: BoundingBox) -> Rect {
    Rect::new(
        bounds.x0 as f32,
        bounds.y0 as f32,
        (bounds.x1 - bounds.x0) as f32,
        (bounds.y1 - bounds.y0) as f32,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        byte_index_at, caret_rect, floor_boundary, selection_rects, to_inline_box, vertical_align,
    };
    use crate::{
        math::{Point, Size},
        paint::color::ColorBrush,
        text::{InlineAlign, InlineBoxSpec, TextDiagnostic},
    };

    fn spec(index: usize) -> InlineBoxSpec {
        InlineBoxSpec {
            id: 5,
            index,
            size: Size::new(10.0, 10.0),
            baseline: None,
            align: InlineAlign::Baseline,
            paint: None,
        }
    }

    /// 直接建一个 parley 布局（不经过 `ShapedText`，避免测试互相依赖）；逻辑坐标（scale = 1）。
    fn layout_of(text: &str) -> parley::Layout<ColorBrush> {
        layout_of_width(text, 400.0)
    }

    /// 指定断行宽度的布局（多行用例需要窄宽度）。
    fn layout_of_width(text: &str, width: f32) -> parley::Layout<ColorBrush> {
        let mut font_cx = parley::FontContext::new();
        let mut layout_cx: parley::LayoutContext<ColorBrush> = parley::LayoutContext::new();
        let mut builder = layout_cx.ranged_builder(&mut font_cx, text, 1.0, true);
        builder.push_default(parley::StyleProperty::FontSize(16.0));
        builder.push_default(parley::StyleProperty::Brush(ColorBrush::default()));
        let mut layout = builder.build(text);
        layout.break_all_lines(Some(width));
        layout.align(
            parley::Alignment::Start,
            parley::AlignmentOptions::default(),
        );
        layout
    }

    /// `byte_index` 处 caret 几何的中心点（文本盒局部）—— 命中它的返回值必须回到同一索引。
    fn caret_center(layout: &parley::Layout<ColorBrush>, byte_index: usize) -> Point {
        let rect = caret_rect(layout, byte_index);
        Point::new(rect.origin.x + 0.5, rect.origin.y + rect.size.height * 0.5)
    }

    #[test]
    fn floor_boundary_only_returns_char_boundaries() {
        let text = "a中b"; // 边界：0, 1, 4, 5

        assert_eq!(floor_boundary(0, text), 0);
        assert_eq!(floor_boundary(1, text), 1);
        assert_eq!(floor_boundary(2, text), 1);
        assert_eq!(floor_boundary(3, text), 1);
        assert_eq!(floor_boundary(4, text), 4);
        assert_eq!(floor_boundary(9, text), 5, "past the end clamps to len");
        for index in 0..=text.len() {
            assert!(text.is_char_boundary(floor_boundary(index, text)));
        }
    }

    #[test]
    fn inline_box_converts_units_and_reports_off_boundary() {
        let (inline_box, diagnostic) = to_inline_box(&spec(2), "a中b");

        assert_eq!(inline_box.width, 10.0, "逻辑像素：不再乘 scale（R12-4）");
        assert_eq!(inline_box.height, 10.0);
        assert_eq!(inline_box.index, 1, "snapped back to a boundary");
        assert!(matches!(
            diagnostic,
            Some(TextDiagnostic::InlineBoxOffBoundary { id: 5, index: 2 })
        ));
    }

    #[test]
    fn inline_box_keeps_baseline_scaled_and_needs_no_diagnostic() {
        let mut spec = spec(0);
        spec.baseline = Some(4.0);
        let (inline_box, diagnostic) = to_inline_box(&spec, "x");

        assert_eq!(inline_box.baseline, Some(4.0), "基线也是逻辑像素");
        assert_eq!(inline_box.index, 0);
        assert!(diagnostic.is_none());
    }

    #[test]
    fn vertical_align_maps_all_eight_variants() {
        assert_eq!(
            vertical_align(InlineAlign::Baseline),
            parley::VerticalAlign::BASELINE
        );
        assert_eq!(vertical_align(InlineAlign::Sub), parley::VerticalAlign::SUB);
        assert_eq!(
            vertical_align(InlineAlign::Super),
            parley::VerticalAlign::SUPER
        );
        assert_eq!(
            vertical_align(InlineAlign::TextTop),
            parley::VerticalAlign::TEXT_TOP
        );
        assert_eq!(
            vertical_align(InlineAlign::TextBottom),
            parley::VerticalAlign::TEXT_BOTTOM
        );
        assert_eq!(
            vertical_align(InlineAlign::Middle),
            parley::VerticalAlign::MIDDLE
        );
        assert_eq!(vertical_align(InlineAlign::Top), parley::VerticalAlign::TOP);
        assert_eq!(
            vertical_align(InlineAlign::Bottom),
            parley::VerticalAlign::BOTTOM
        );
    }

    #[test]
    fn selection_rects_cover_the_requested_range() {
        let layout = layout_of("abcdef");
        let rects = selection_rects(&layout, 0, 3);

        assert_eq!(rects.len(), 1);
        assert!(rects[0].origin.x.abs() < 0.01);
        assert!(rects[0].size.width > 0.0);
        assert!(rects[0].size.height > 0.0);
    }

    #[test]
    fn selection_rects_are_empty_for_a_collapsed_range() {
        let layout = layout_of("abcdef");

        assert!(selection_rects(&layout, 2, 2).is_empty());
    }

    #[test]
    fn selection_rects_are_order_independent() {
        let layout = layout_of("abcdef");

        assert_eq!(
            selection_rects(&layout, 0, 4),
            selection_rects(&layout, 4, 0)
        );
    }

    /// R12-4：布局是逻辑坐标 ⇒ 选区几何**不再除以 scale**（旧实现把物理坐标除回逻辑）。
    ///
    /// 这里用「同一布局的 caret 位移」做交叉验证：选区右边界必须等于 focus 处的 caret x。
    #[test]
    fn selection_geometry_matches_caret_offsets() {
        let layout = layout_of("abcdef");
        let rects = selection_rects(&layout, 0, 3);
        let caret = caret_rect(&layout, 3);

        assert_eq!(rects.len(), 1);
        assert!(
            (rects[0].origin.x + rects[0].size.width - caret.origin.x).abs() < 0.01,
            "{:?} vs {caret:?}",
            rects[0]
        );
    }

    #[test]
    fn caret_rect_reaches_the_end_of_the_text() {
        let layout = layout_of("abc");

        let rect = caret_rect(&layout, 3);

        assert!((rect.size.width - 1.0).abs() < 0.01);
        assert!(
            (rect.origin.x - layout.width()).abs() < 0.01,
            "caret x {} must equal line width {}",
            rect.origin.x,
            layout.width()
        );
    }

    #[test]
    fn caret_rect_handles_cjk_bytes() {
        let layout = layout_of("你好");

        let rect = caret_rect(&layout, "你好".len());

        assert!(rect.origin.x > 0.0);
        assert!(rect.size.height > 0.0);
    }

    /// R12-4：caret 宽度恒为 1 **逻辑**像素（布局逻辑化后不再有 `scale` 参数）。
    #[test]
    fn caret_rect_is_one_logical_pixel_wide() {
        let layout = layout_of("abc");

        let rect = caret_rect(&layout, 3);

        assert!((rect.size.width - 1.0).abs() < 0.01, "{rect:?}");
        assert!((rect.origin.x - layout.width()).abs() < 0.01, "{rect:?}");
    }

    #[test]
    fn caret_rect_for_empty_text_still_has_a_line_height() {
        let layout = layout_of("");

        let rect = caret_rect(&layout, 0);

        assert_eq!(rect.origin.x, 0.0);
        assert!((rect.size.width - 1.0).abs() < 0.01, "{rect:?}");
        assert!(rect.size.height > 0.0, "{rect:?}");
    }

    // ── E5（§11.5）：点 → 字节索引 ─────────────────────────────────────────

    /// 单行 LTR：caret 几何的中心点必须命中回同一个字节索引（行内任意位置）。
    #[test]
    fn byte_index_at_maps_line_points_to_caret_boundaries() {
        let layout = layout_of("abcdef");

        for index in 0..="abcdef".len() {
            let point = caret_center(&layout, index);
            assert_eq!(
                byte_index_at(&layout, "abcdef", point),
                index,
                "caret 中心 {point:?} 必须命中索引 {index}"
            );
        }
    }

    /// 盒外四方向 clamp：上方 `0`、下方 `len`、同一行内水平越界取行首/行尾。
    #[test]
    fn byte_index_at_clamps_points_outside_the_text_box() {
        let text = "abcdef";
        let layout = layout_of(text);
        let height = layout.height();
        let width = layout.width();
        assert!(width > 0.0 && height > 0.0);

        let line_y = caret_center(&layout, 0).y;
        assert_eq!(
            byte_index_at(&layout, text, Point::new(0.0, -1.0)),
            0,
            "文本盒上方 → 0"
        );
        assert_eq!(
            byte_index_at(&layout, text, Point::new(0.0, height + 1.0)),
            text.len(),
            "文本盒下方 → text.len()"
        );
        assert_eq!(
            byte_index_at(&layout, text, Point::new(-50.0, line_y)),
            0,
            "行首左侧越界 → 行首"
        );
        assert_eq!(
            byte_index_at(&layout, text, Point::new(width + 50.0, line_y)),
            text.len(),
            "行尾右侧越界 → 行尾"
        );
        assert_eq!(byte_index_at(&layout, text, Point::new(-50.0, -50.0)), 0);
        assert_eq!(
            byte_index_at(&layout, text, Point::new(width + 50.0, height + 50.0)),
            text.len()
        );
    }

    /// 多行：每行行首的 caret 中心命中该行首（最近行 = 点所在行）；点在行间取最近行。
    #[test]
    fn byte_index_at_picks_the_line_under_the_point() {
        let text = "alpha beta gamma delta";
        let layout = layout_of_width(text, 60.0);
        let line_starts: Vec<usize> = layout.lines().map(|line| line.text_range().start).collect();
        assert!(
            line_starts.len() >= 3,
            "fixture 必须折成多行：{line_starts:?}"
        );

        for start in &line_starts {
            let point = caret_center(&layout, *start);
            assert_eq!(
                byte_index_at(&layout, text, point),
                *start,
                "行首 {start} 的 caret 中心 {point:?} 必须命中该行"
            );
        }

        // 行间点（每行垂直中心 + 行首 x）落在该行的文本范围内。
        for (line_index, line) in layout.lines().enumerate() {
            let metrics = line.metrics();
            let point = Point::new(
                metrics.inline_min_coord.max(0.0) + 0.5,
                (metrics.block_min_coord + metrics.block_max_coord) * 0.5,
            );
            let index = byte_index_at(&layout, text, point);
            let range = line.text_range();
            assert!(
                range.start <= index && index <= range.end,
                "第 {line_index} 行的点必须命中该行范围 {range:?}，实际 {index}"
            );
        }
    }

    /// 显式换行：第二行行首的 caret 中心命中换行后的字节（不是换行符前）。
    #[test]
    fn byte_index_at_maps_explicit_line_breaks_to_the_next_line_start() {
        let text = "ab\ncd";
        let layout = layout_of_width(text, 400.0);
        assert_eq!(layout.lines().count(), 2, "fixture 必须两行");

        let second_start = layout.lines().nth(1).expect("第二行").text_range().start;
        assert_eq!(second_start, 3);
        let point = caret_center(&layout, second_start);
        assert_eq!(
            byte_index_at(&layout, text, point),
            second_start,
            "换行后行首 {point:?}"
        );
        // 第一行的行中点仍落在第一行（不会跳过换行符）。
        let first_line = layout.lines().next().expect("第一行");
        let first_point = Point::new(
            first_line.metrics().inline_min_coord + 0.5,
            (first_line.metrics().block_min_coord + first_line.metrics().block_max_coord) * 0.5,
        );
        assert!(byte_index_at(&layout, text, first_point) <= 2);
    }

    /// CJK：返回的索引必须是 char boundary，绝不能落进 3 字节字符内部。
    #[test]
    fn byte_index_at_never_splits_cjk_characters() {
        let text = "a中b";
        let layout = layout_of(text);
        let cjk_start = text.find('中').expect("含 CJK");
        let cjk_end = cjk_start + '中'.len_utf8();
        let middle = Point::new(
            (caret_rect(&layout, cjk_start).origin.x + caret_rect(&layout, cjk_end).origin.x) * 0.5,
            caret_center(&layout, cjk_start).y,
        );

        let index = byte_index_at(&layout, text, middle);
        assert!(
            index == cjk_start || index == cjk_end,
            "CJK 中点必须吸附到簇边界，实际 {index}"
        );
        let mut x = -1.0;
        while x <= layout.width() + 1.0 {
            let index = byte_index_at(&layout, text, Point::new(x, caret_center(&layout, 0).y));
            assert!(text.is_char_boundary(index), "x={x} 返回非边界 {index}");
            assert!(
                index != cjk_start + 1 && index != cjk_start + 2,
                "x={x} 必须落在 char boundary，实际 {index}"
            );
            x += 0.25;
        }
    }

    /// emoji：多码元字素簇（含 ZWJ 序列）不可被拆开，返回值必须是 char boundary 且落在簇边界。
    #[test]
    fn byte_index_at_never_splits_emoji_grapheme_clusters() {
        let text = "a👨\u{200D}👩\u{200D}👧b";
        let layout = layout_of(text);
        let emoji_start = text.find('👨').expect("含 emoji");
        let emoji_end = text.find('b').expect("emoji 之后还有文本");
        assert!(emoji_end > emoji_start);
        let y = caret_center(&layout, 0).y;
        let middle = Point::new(
            (caret_rect(&layout, emoji_start).origin.x + caret_rect(&layout, emoji_end).origin.x)
                * 0.5,
            y,
        );

        let index = byte_index_at(&layout, text, middle);
        assert!(
            index == emoji_start || index == emoji_end,
            "ZWJ 簇中点必须吸附到簇边界，实际 {index}"
        );
        let mut x = -1.0;
        while x <= layout.width() + 1.0 {
            let index = byte_index_at(&layout, text, Point::new(x, y));
            assert!(text.is_char_boundary(index), "x={x} 返回非边界 {index}");
            assert!(
                index <= emoji_start || index >= emoji_end,
                "x={x} 不得落进 emoji 簇内部，实际 {index}"
            );
            x += 0.25;
        }
    }

    /// 空文本：恒返回 0（盒外也一样，`text.len() == 0`）。
    #[test]
    fn byte_index_at_returns_zero_for_empty_text() {
        let layout = layout_of("");

        for point in [
            Point::new(0.0, 0.0),
            Point::new(-10.0, -10.0),
            Point::new(100.0, 100.0),
        ] {
            assert_eq!(byte_index_at(&layout, "", point), 0, "{point:?}");
        }
    }

    /// 多行：同一行内水平越界必须收敛到**该行**行首/行尾，不得退化为整段 `0`/`len`。
    #[test]
    fn byte_index_at_clamps_horizontally_within_each_wrapped_line() {
        let text = "alpha beta gamma delta";
        let layout = layout_of_width(text, 60.0);
        let lines: Vec<_> = layout.lines().collect();
        assert!(lines.len() >= 3, "fixture 必须折成多行");
        let width = layout.width();

        let mut left_hits = Vec::with_capacity(lines.len());
        let mut right_hits = Vec::with_capacity(lines.len());
        for (line_index, line) in lines.iter().enumerate() {
            let metrics = line.metrics();
            let y = (metrics.block_min_coord + metrics.block_max_coord) * 0.5;
            let range = line.text_range();
            let left = byte_index_at(&layout, text, Point::new(-50.0, y));
            let right = byte_index_at(&layout, text, Point::new(width + 50.0, y));
            for (side, index) in [("左", left), ("右", right)] {
                assert!(
                    range.start <= index && index <= range.end,
                    "第 {line_index} 行{side}侧越界的命中 {index} 必须落在该行范围 {range:?}"
                );
            }
            left_hits.push(left);
            right_hits.push(right);
        }

        // 中间行左右越界必须收敛到该行行首/行尾：若退化成「整段 0/len」，中间行会失守。
        assert!(
            left_hits
                .iter()
                .enumerate()
                .any(|(index, &hit)| index + 1 < left_hits.len() && hit > 0),
            "左侧越界不得全部收敛到整段 0：{left_hits:?}"
        );
        assert!(
            right_hits
                .iter()
                .enumerate()
                .any(|(index, &hit)| index + 1 < right_hits.len() && hit < text.len()),
            "右侧越界不得全部收敛到整段 len：{right_hits:?}"
        );
    }

    // ── bidi 命中（§11.5 测试要求：bidi 文本）─────────────────────────────
    //
    // 口径：命中点是 caret 的**精确边界 x**（`caret_rect(..).origin.x`），断言返回值与该点
    // **共享视觉位置**（某个 affinity 的 caret x 相同）。不能用 `caret.center()`（+0.5px）：
    // bidi 重排后同一视觉 x 可对应两个逻辑边界，+0.5px 会落进相邻 run，往返本身就无定义。

    /// 指定段落基准方向的布局（bidi 用例需要显式 RTL/LTR 段落方向）。
    fn layout_of_direction(
        text: &str,
        width: f32,
        direction: parley::BaseDirection,
    ) -> parley::Layout<ColorBrush> {
        layout_of_direction_with_run(text, width, direction, 0..0)
    }

    /// 指定段落方向 + 一个额外 run（字号不同 ⇒ 切成两个 run）的布局。
    fn layout_of_direction_with_run(
        text: &str,
        width: f32,
        direction: parley::BaseDirection,
        run: core::ops::Range<usize>,
    ) -> parley::Layout<ColorBrush> {
        let mut font_cx = parley::FontContext::new();
        let mut layout_cx: parley::LayoutContext<ColorBrush> = parley::LayoutContext::new();
        let mut builder = layout_cx.ranged_builder(&mut font_cx, text, 1.0, true);
        builder.push_default(parley::StyleProperty::FontSize(16.0));
        builder.push_default(parley::StyleProperty::Brush(ColorBrush::default()));
        builder.set_base_direction(direction);
        if !run.is_empty() {
            builder.push(parley::StyleProperty::FontSize(20.0), run);
        }
        let mut layout = builder.build(text);
        layout.break_all_lines(Some(width));
        layout.align(
            parley::Alignment::Start,
            parley::AlignmentOptions::default(),
        );
        layout
    }

    /// 同一逻辑边界的两个视觉 caret x（bidi 边界上 Downstream/Upstream 可能落在不同 x）。
    fn caret_xs(layout: &parley::Layout<ColorBrush>, byte_index: usize) -> [f32; 2] {
        [parley::Affinity::Downstream, parley::Affinity::Upstream].map(|affinity| {
            parley::editing::Cursor::from_byte_index(layout, byte_index, affinity)
                .geometry(layout, 1.0)
                .x0 as f32
        })
    }

    /// 点在 caret 的精确边界 x：返回值必须是 char boundary，且与点共享视觉位置。
    fn assert_exact_boundary_hit(
        layout: &parley::Layout<ColorBrush>,
        text: &str,
        byte_index: usize,
    ) {
        let rect = caret_rect(layout, byte_index);
        let point = Point::new(rect.origin.x, rect.origin.y + rect.size.height * 0.5);
        let hit = byte_index_at(layout, text, point);
        assert!(
            text.is_char_boundary(hit),
            "caret({byte_index}) 的命中 {hit} 不是 char boundary"
        );
        let xs = caret_xs(layout, hit);
        assert!(
            xs.iter().any(|x| (x - point.x).abs() <= 0.05),
            "caret({byte_index}) 精确边界 x={} 的命中 {hit} 必须共享该视觉位置（caret x = {xs:?}）",
            point.x
        );
    }

    /// 纯 RTL 段落：bidi 重排后逻辑递增的字符视觉 x 递减；每个 caret 精确边界都能命中同一视觉位置。
    #[test]
    fn byte_index_at_handles_a_pure_rtl_paragraph() {
        let text = "אבג abc";
        let layout = layout_of_direction(text, 400.0, parley::BaseDirection::Rtl);
        assert_eq!(layout.lines().count(), 1, "fixture 必须单行");
        let x = |index: usize| caret_rect(&layout, index).origin.x;

        // 希伯来簇（逻辑 0/2/4）在 RTL 段落里视觉反转：x(0) > x(2) > x(4)。
        assert!(
            x(0) > x(2) + 1.0 && x(2) > x(4) + 1.0,
            "RTL 段落必须视觉反转：x = {:?}",
            [x(0), x(2), x(4)]
        );

        for index in 0..=text.len() {
            if text.is_char_boundary(index) {
                assert_exact_boundary_hit(&layout, text, index);
            }
        }

        // 段落末尾（视觉最左）的精确边界：命中必须落在末尾附近，而不是逻辑 0。
        let last = caret_rect(&layout, text.len());
        let hit = byte_index_at(
            &layout,
            text,
            Point::new(last.origin.x, last.origin.y + last.size.height * 0.5),
        );
        assert!(
            hit >= 6,
            "RTL 段落视觉最左端必须命中逻辑末尾附近（不是 0），实际 {hit}"
        );
    }

    /// LTR 段落内的 RTL run：run 内视觉反转，精确边界命中同一视觉位置。
    #[test]
    fn byte_index_at_handles_an_rtl_run_inside_an_ltr_paragraph() {
        let text = "abc אבג xyz";
        let layout = layout_of_direction(text, 400.0, parley::BaseDirection::Ltr);
        assert_eq!(layout.lines().count(), 1, "fixture 必须单行");
        let x = |index: usize| caret_rect(&layout, index).origin.x;

        // 逻辑 4/6/8 是 RTL run 的簇起点：视觉 x 递减（run 内反转），且整段仍是单行。
        assert!(
            x(4) > x(6) + 1.0 && x(6) > x(8) + 1.0,
            "LTR 段内的 RTL run 必须视觉反转：x = {:?}",
            [x(4), x(6), x(8)]
        );

        for index in 0..=text.len() {
            if text.is_char_boundary(index) {
                assert_exact_boundary_hit(&layout, text, index);
            }
        }
    }

    /// RTL 跨 run：同一 RTL 段落切成两个 run，视觉顺序仍按 bidi 重排；命中按视觉位置而非 run 顺序。
    #[test]
    fn byte_index_at_handles_rtl_text_across_runs() {
        let text = "אבגדהו";
        // 两个 run（字号不同）：0..4 与 4..12 都是合法 char boundary。
        let layout = layout_of_direction_with_run(text, 400.0, parley::BaseDirection::Rtl, 0..4);
        assert_eq!(layout.lines().count(), 1, "fixture 必须单行");
        let run_count = layout.lines().next().expect("单行").runs().count();
        assert!(run_count >= 2, "fixture 必须切成多个 run，实际 {run_count}");
        let x = |index: usize| caret_rect(&layout, index).origin.x;

        // RTL 段落：逻辑递增 ⇒ 视觉 x 递减（跨 run 仍然成立）。
        assert!(
            x(0) > x(4) + 1.0 && x(4) > x(8) + 1.0 && x(8) > x(12) + 1.0,
            "跨 run 的 RTL 段落必须视觉反转：x = {:?}",
            [x(0), x(4), x(8), x(12)]
        );

        for index in 0..=text.len() {
            if text.is_char_boundary(index) {
                assert_exact_boundary_hit(&layout, text, index);
            }
        }

        // 视觉右半（逻辑 0..4 的 run）的中间点必须命中该 run 的逻辑区间，而不是另一个 run。
        let y = caret_center(&layout, 0).y;
        let right_mid = Point::new((x(0) + x(4)) * 0.5, y);
        let right_hit = byte_index_at(&layout, text, right_mid);
        assert!(
            right_hit <= 4,
            "视觉右半必须命中逻辑 0..=4 的 run，实际 {right_hit}"
        );
        // 视觉左半（逻辑 4..12 的 run）同理。
        let left_mid = Point::new((x(4) + x(12)) * 0.5, y);
        let left_hit = byte_index_at(&layout, text, left_mid);
        assert!(
            left_hit >= 4,
            "视觉左半必须命中逻辑 4..=12 的 run，实际 {left_hit}"
        );
    }
}

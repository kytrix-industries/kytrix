//! 行内流：`InlineBoxSpec → parley::InlineBox` 与 `(anchor, focus)` 选区的几何映射（§12）。
//!
//! Markdown AST → `Vec<InlineFragment>` 的**数据管线不在本阶段**（P15 的消费者还不存在）；
//! 本模块只把「盒的输入面」和「选区的输出面」钉死。

use parley::{
    Affinity, BoundingBox, InlineBox, InlineBoxKind, VerticalAlign,
    editing::{Cursor, Selection},
};

use super::{InlineAlign, InlineBoxSpec, TextDiagnostic};
use crate::{math::Rect, paint::color::ColorBrush};

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
    use super::{caret_rect, floor_boundary, selection_rects, to_inline_box, vertical_align};
    use crate::{
        math::Size,
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
        let mut font_cx = parley::FontContext::new();
        let mut layout_cx: parley::LayoutContext<ColorBrush> = parley::LayoutContext::new();
        let mut builder = layout_cx.ranged_builder(&mut font_cx, text, 1.0, true);
        builder.push_default(parley::StyleProperty::FontSize(16.0));
        builder.push_default(parley::StyleProperty::Brush(ColorBrush::default()));
        let mut layout = builder.build(text);
        layout.break_all_lines(Some(400.0));
        layout.align(
            parley::Alignment::Start,
            parley::AlignmentOptions::default(),
        );
        layout
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
}

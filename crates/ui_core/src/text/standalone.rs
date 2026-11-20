//! E1：**无节点归属**的文本 shaping（`App::shape_text` 的实现，`§11.1`）。
//!
//! 与节点路径（`App::refresh_text` → `Window::compute_layout` → `App::layout_text`）的分工：
//! 节点路径的内容宽来自 taffy 布局；canvas 内文本没有节点，宽度由应用显式给出
//! （`max_width = None` ⇒ 单行、按 max-content 宽断行）。两条路径共用同一份形状缓存
//! （`text::cache::ShapeKey`）、同一条光栅/装箱路径（`ShapedText::rasterize`）与同一个字形
//! 图集，因此同一文本 + 同一 shaping 输入重复 shape 命中缓存、不重复塑形。
//!
//! # 图集代际（D-5 的等价面）
//!
//! 句柄记录构造时的 `AtlasAllocator::generation`；`App` 侧另存一份**当前**代际（代际租约），
//! 任何可能换代的入口（`App::{layout_text, set_font_stack, reload_system_fonts}` 等）都会同步
//! 它。换代后 [`super::ShapedTextHandle::is_stale`] 为真，`Canvas::push_shaped_text` 不编码旧
//! UV —— 这正是 D-5 修掉的花字问题。本次 shape 自身触发的扩容 / 清空会让**本进程其它窗口**的
//! 文本行一起失效，调用方（`App::shape_text`）负责把它们重新入队。
//!
//! # 设备 scale
//!
//! `§11.1` 的 `App::shape_text` 签名没有 window / scale 参数：本实现按**逻辑像素 1:1**
//! （[`STANDALONE_SCALE`]）光栅，`ShapedLine::scale` 因此恒为 1.0；paint 阶段仍按窗口
//! `Scale` 放大。hidpi 下要按物理 ppem 光栅需要规格补充「scale 从哪来」（显式参数 / App 级
//! 默认 scale），不在 E1 范围内（见本批次提交说明的偏差项）。

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use smol_str::{SmolStr, format_smolstr};

use super::{
    InlineBoxSpec, ShapedInlineBox, ShapedTextHandle, ShapedTextItem, TextContent, TextStyle,
    atlas::GlyphAtlas, context::TextContext, raster::GlyphRasterizer, shape::RasterCtx,
};
use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{AtlasAllocator, DirtyRects},
};

/// 无窗口 shaping 的光栅 scale（逻辑像素 1:1；理由见模块文档「设备 scale」）。
const STANDALONE_SCALE: f32 = 1.0;

/// 单次 `shape_text` 因图集换代（阶梯扩容 / 容量逃逸清空）重跑光栅的最大轮数。
///
/// 与 `app::text_pass::MAX_LAYOUT_TEXT_PASSES` 同量级：换代只由扩容（512→1024→2048，最多 2 次）
/// 或满页清空触发，正常一轮即收敛；超过上限说明图集在持续换代，按容量失败上报。
const MAX_SHAPE_ATTEMPTS: u32 = 4;

/// 无节点归属的 shaping：校验 → 形状缓存 → 光栅/装箱 → 带代际租约的句柄。
///
/// `pending_dirty` 是图集脏矩形（`App::pending_dirty`）：新装箱的字形必须进入下一帧的
/// 图集页上传，否则 GPU 纹理里对应 tile 是未定义内容。
pub(crate) fn shape(
    text_context: &mut TextContext,
    raster: &mut GlyphRasterizer,
    atlas: &mut AtlasAllocator,
    glyph_atlas: &mut GlyphAtlas,
    pending_dirty: &mut DirtyRects,
    lease: &Arc<AtomicU32>,
    text: &str,
    style: &TextStyle,
    max_width: Option<f32>,
    inline_boxes: &[InlineBoxSpec],
) -> Result<ShapedTextHandle, EngineError> {
    // 入口校验：宽度约束必须是有限非负值（NaN 会让 parley 的断行结果不可复现）。
    if let Some(width) = max_width
        && (!width.is_finite() || width < 0.0)
    {
        return Err(EngineError::TextShapeStandalone {
            detail: format_smolstr!("max_width {width} must be finite and non-negative"),
        });
    }
    // 空字体栈没有任何候选：与节点路径不同，这里没有节点槽可挂诊断，按 `§11.1` 的错误表
    // 返回强类型错误（非空栈但家族名不存在 ⇒ 走 fontique 回退，不报错）。
    if text_context.font_stack().families().len() == 0 {
        return Err(EngineError::TextShapeStandalone {
            detail: SmolStr::new_static("font stack is empty"),
        });
    }
    // 字体换代（set_font_stack / reload_system_fonts）后光栅器的字体 epoch 可能尚未同步
    // （节点路径由 `refresh_text` 每帧同步；canvas 文本可能在任何时刻被 shape）。
    let font_epoch = text_context.font_epoch();
    raster.set_font_epoch(font_epoch);

    let content =
        TextContent::new(text, style.clone()).with_inline_boxes(inline_boxes.iter().copied());
    // 形状缓存（`ShapeKey`：文本 + shaping 输入的样式 + 行内盒 + font_stack_version）；
    // 命中路径不重建 parley Layout，只按新的内容宽断行 + 光栅。
    let (shaped, _hit) = text_context.shape_cached(&content);
    let width = max_width.unwrap_or_else(|| shaped.max_content_size().width);
    let ellipsis = text_context.ellipsis_for(style);

    // 本 pass 的字形 tile 在本 pass 内不可被 LRU 淘汰（与 `App::layout_text` 同口径）。
    glyph_atlas.begin_pass();
    let mut attempt = 0u32;
    loop {
        let generation = atlas.generation();
        let (line, diagnostics, stats) = {
            let mut ctx = RasterCtx {
                raster,
                glyph_atlas,
                atlas,
                generation,
                scale: STANDALONE_SCALE,
            };
            shaped.rasterize(
                width,
                style.alignment,
                style.color,
                &mut ctx,
                ellipsis.as_deref(),
            )?
        };
        text_context.push_diagnostics(diagnostics);
        pending_dirty.merge(&mut atlas.take_dirty_rects());
        // 软失败（整页都是本次 shape 的工作集）：canvas 没有「下一帧自动重排」的节点槽，
        // 少画字形不可接受 —— 按 `§11.1` 的错误表上报容量失败（可见失败优于静默缺字）。
        if stats.skipped > 0 {
            return Err(EngineError::AtlasExhausted {
                kind: AtlasKind::Glyph,
            });
        }
        if atlas.generation() == generation {
            let inline_boxes: Box<[ShapedInlineBox]> = line
                .lines
                .iter()
                .flat_map(|line| line.items.iter())
                .filter_map(|item| match item {
                    ShapedTextItem::InlineBox(inline) => Some(*inline),
                    ShapedTextItem::GlyphRun(_) => None,
                })
                .collect();
            // 句柄记录上**最终**代际：换代在 pass 内发生后已重跑，产出的 tile 属于这一代。
            lease.store(generation, Ordering::Relaxed);
            return Ok(ShapedTextHandle::new(
                Arc::new(line),
                inline_boxes,
                generation,
                Arc::clone(lease),
            ));
        }
        // 本次 rasterize 触发了图集换代（扩容 / 清空）：已产出的 `ShapedLine` 烘焙的是旧 UV，
        // 必须用新代际重跑（与 `App::layout_text` 的重跑语义一致）。
        attempt = attempt.saturating_add(1);
        if attempt >= MAX_SHAPE_ATTEMPTS {
            return Err(EngineError::AtlasExhausted {
                kind: AtlasKind::Glyph,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::App,
        math::{Point, Rect, Size},
        paint::{canvas::Canvas, primitive::Primitive},
        text::{FontStack, InlineAlign, ShapedLine, TextOverflow, TextStyle},
    };

    fn new_app() -> (tokio::runtime::Runtime, App) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let app = App::new(runtime.handle().clone());
        (runtime, app)
    }

    /// 快照里的字形总数（`GlyphRun` 逐个累加；空 run 计 0）。
    fn glyph_count(line: &ShapedLine) -> usize {
        line.glyph_runs().map(|run| run.glyphs.len()).sum()
    }

    fn canvas() -> Canvas {
        Canvas::new(Rect::new(0.0, 0.0, 200.0, 120.0))
    }

    #[test]
    fn empty_text_shapes_to_an_empty_handle() {
        let (_runtime, mut app) = new_app();
        let handle = app
            .shape_text("", &TextStyle::new(16.0), None, &[])
            .expect("空文本不是错误：与节点路径一致，产出 0 行的快照");
        assert_eq!(handle.size(), Size::ZERO);
        assert!(handle.line().lines.is_empty());
        assert_eq!(glyph_count(handle.line()), 0);
        assert!(!handle.is_stale());
        // 空句柄仍然有效：push 返回 true，只是没有可画内容。
        let mut canvas = canvas();
        assert!(canvas.push_shaped_text(&handle, Point::ZERO));
        assert!(canvas.primitives().is_empty());
    }

    #[test]
    fn ascii_and_cjk_mix_shape_into_visible_glyphs() {
        let (_runtime, mut app) = new_app();
        let style = TextStyle::new(16.0);
        let mixed = app
            .shape_text("Hello 世界", &style, None, &[])
            .expect("ASCII + CJK 必须能塑形");
        assert_eq!(mixed.line().lines.len(), 1, "max_width = None ⇒ 单行");
        assert!(mixed.size().width > 0.0);
        assert!(mixed.size().height > 0.0);
        // 空格没有覆盖度位图（不产生 PositionedGlyph）：5 个 Latin + 2 个 CJK = 7。
        assert_eq!(
            glyph_count(mixed.line()),
            7,
            "ASCII + CJK 混排的每个非空白字符都要有字形：{:?}",
            mixed.line()
        );
        // 测宽：更长的文本更宽、更大的字号更宽。
        let longer = app
            .shape_text("Hello 世界!!!", &style, None, &[])
            .expect("更长文本");
        assert!(
            longer.size().width > mixed.size().width,
            "更长的文本必须更宽：{} vs {}",
            longer.size().width,
            mixed.size().width
        );
        let bigger = app
            .shape_text("Hello 世界", &TextStyle::new(32.0), None, &[])
            .expect("更大字号");
        assert!(
            bigger.size().width > mixed.size().width,
            "字号翻倍必须更宽：{} vs {}",
            bigger.size().width,
            mixed.size().width
        );
    }

    #[test]
    fn handle_size_matches_the_shaped_line() {
        let (_runtime, mut app) = new_app();
        let handle = app
            .shape_text("line metrics", &TextStyle::new(15.0), None, &[])
            .expect("塑形");
        let line = handle.line();
        assert_eq!(handle.size(), line.size());
        assert_eq!(handle.size(), Size::new(line.width, line.height));
        assert!(line.first_baseline.is_some_and(|baseline| baseline > 0.0));
        assert_eq!(line.scale, STANDALONE_SCALE);
        assert_eq!(line.text.as_str(), "line metrics");
    }

    #[test]
    fn max_width_breaks_the_line_and_grows_the_height() {
        let (_runtime, mut app) = new_app();
        let style = TextStyle::new(16.0);
        let text = "the quick brown fox jumps over the lazy dog";
        let single = app.shape_text(text, &style, None, &[]).expect("不限宽");
        assert_eq!(single.line().lines.len(), 1);
        let wrapped = app
            .shape_text(text, &style, Some(80.0), &[])
            .expect("限宽断行");
        assert!(
            wrapped.line().lines.len() > 1,
            "max_width 必须触发断行：{:?}",
            wrapped.line().lines
        );
        // 尾随空白**允许悬挂**（CSS hanging whitespace）：行推进 = 可见内容 + 行末空格，
        // 因此上限是 `max_width + 一个空格宽`（16 px 字号下 < 8 px）。
        let hanging = 8.0;
        assert!(
            wrapped.size().width <= 80.0 + hanging,
            "断行后内容宽不得超过 max_width + 行末空白：{}",
            wrapped.size().width
        );
        assert!(
            wrapped.size().height > single.size().height,
            "多行必须比单行高：{} vs {}",
            wrapped.size().height,
            single.size().height
        );
        // 每一行的可见推进都放得进 max_width（单词都能断开）。
        for line in &wrapped.line().lines {
            assert!(
                line.advance <= 80.0 + hanging,
                "行推进 {} 超过 max_width + 行末空白",
                line.advance
            );
        }
    }

    /// 超宽截断：`max_lines + Ellipsis` 在末行保留前缀 + 省略号，行推进不超过 `max_width`
    /// （省略号重排逻辑本身由 `text/shape.rs` 的单测覆盖，这里只断言 canvas 句柄上的可观测结果）。
    #[test]
    fn max_lines_with_ellipsis_truncates_overflow() {
        let (_runtime, mut app) = new_app();
        let mut style = TextStyle::new(16.0);
        style.overflow = TextOverflow::Ellipsis;
        style.max_lines = 1;
        let text = "the quick brown fox jumps over the lazy dog";
        let shaped = app
            .shape_text(text, &style, Some(120.0), &[])
            .expect("截断塑形");
        assert_eq!(
            shaped.line().lines.len(),
            1,
            "max_lines = 1 只能留一行：{:?}",
            shaped.line().lines
        );
        let line = &shaped.line().lines[0];
        assert!(
            line.advance <= 120.0 + 1.0,
            "截断后的行推进不得超过内容宽：{}",
            line.advance
        );
        assert!(glyph_count(shaped.line()) > 0, "截断后仍要有可见字形");
        // 同样的文本在「不限行数」下必须断成多行，否则上面的断言没有区分力。
        let untruncated = app
            .shape_text(text, &TextStyle::new(16.0), Some(120.0), &[])
            .expect("不截断");
        assert!(untruncated.line().lines.len() > 1);
    }

    #[test]
    fn inline_box_landings_are_readable_in_order() {
        let (_runtime, mut app) = new_app();
        let specs = [
            InlineBoxSpec {
                id: 7,
                index: 2,
                size: Size::new(12.0, 12.0),
                baseline: None,
                align: InlineAlign::Baseline,
            },
            InlineBoxSpec {
                id: 9,
                index: 4,
                size: Size::new(8.0, 8.0),
                baseline: Some(1.0),
                align: InlineAlign::Middle,
            },
        ];
        let handle = app
            .shape_text("ab cd", &TextStyle::new(16.0), None, &specs)
            .expect("带行内盒塑形");
        let boxes = app.shaped_inline_boxes(&handle);
        assert_eq!(boxes.len(), 2, "落点数量必须与传入的 inline_boxes 一致");
        assert_eq!(boxes[0].id, 7);
        assert_eq!(boxes[1].id, 9);
        assert_eq!(boxes[0].rect.size, Size::new(12.0, 12.0));
        assert_eq!(boxes[1].rect.size, Size::new(8.0, 8.0));
        assert_eq!(boxes[0].baseline, None);
        assert_eq!(boxes[1].baseline, Some(1.0));
        assert!(
            boxes[0].rect.origin.x < boxes[1].rect.origin.x,
            "LTR 文本里先出现的行内盒必须在左边：{:?}",
            boxes
        );
        // 快照内部项的顺序与落点顺序同源。
        let item_boxes: Vec<ShapedInlineBox> = handle
            .line()
            .lines
            .iter()
            .flat_map(|line| line.items.iter())
            .filter_map(|item| match item {
                ShapedTextItem::InlineBox(inline) => Some(*inline),
                ShapedTextItem::GlyphRun(_) => None,
            })
            .collect();
        assert_eq!(item_boxes, boxes);
    }

    #[test]
    fn push_shaped_text_emits_one_glyph_run_per_non_empty_run() {
        let (_runtime, mut app) = new_app();
        let handle = app
            .shape_text("push me", &TextStyle::new(16.0), None, &[])
            .expect("塑形");
        let mut canvas = canvas();
        assert!(canvas.push_shaped_text(&handle, Point::new(3.0, 4.0)));
        let expected_runs = handle
            .line()
            .glyph_runs()
            .filter(|run| !run.glyphs.is_empty())
            .count();
        let expected_glyphs = glyph_count(handle.line());
        let expected_color = handle
            .line()
            .glyph_runs()
            .next()
            .expect("至少一个 run")
            .color;
        let mut pushed_runs = 0usize;
        let mut pushed_glyphs = 0usize;
        for primitive in canvas.primitives() {
            let Primitive::GlyphRun(run) = primitive else {
                panic!("push_shaped_text 只能产生 GlyphRun 图元，实际 {primitive:?}");
            };
            pushed_runs += 1;
            pushed_glyphs += run.glyphs.len();
            assert_eq!(
                run.color, expected_color,
                "图元颜色必须来自 shaping 的 style.color"
            );
        }
        assert_eq!(pushed_runs, expected_runs);
        assert_eq!(pushed_glyphs, expected_glyphs);
        assert!(expected_glyphs > 0, "「push me」必须有可见字形");
    }

    #[test]
    fn atlas_generation_change_makes_handles_stale_and_push_refuses() {
        let (_runtime, mut app) = new_app();
        let style = TextStyle::new(16.0);
        let handle = app.shape_text("Stale", &style, None, &[]).expect("塑形");
        assert!(!handle.is_stale(), "刚产出的句柄不得失活");
        let mut canvas = canvas();
        assert!(canvas.push_shaped_text(&handle, Point::ZERO));
        let before = canvas.primitives().len();
        assert!(before > 0);

        // 换字体栈 = 清空字形图集并换代（§11.1 的图集换代路径之一）。
        app.set_font_stack(FontStack::new(["Kytrix No Such Family"]));
        assert!(handle.is_stale(), "图集换代后句柄必须失活");
        assert!(
            !canvas.push_shaped_text(&handle, Point::ZERO),
            "stale 句柄不得编码旧 UV"
        );
        assert_eq!(
            canvas.primitives().len(),
            before,
            "stale push 不得产生任何图元"
        );

        let fresh = app
            .shape_text("Stale", &style, None, &[])
            .expect("重新 shape 必须成功");
        assert!(!fresh.is_stale(), "重新 shape 的句柄属于当前代际");
        assert!(canvas.push_shaped_text(&fresh, Point::ZERO));
    }

    #[test]
    fn invalid_widths_are_typed_errors() {
        let (_runtime, mut app) = new_app();
        let style = TextStyle::new(16.0);
        for width in [f32::NAN, f32::INFINITY, -1.0] {
            match app.shape_text("x", &style, Some(width), &[]) {
                Err(EngineError::TextShapeStandalone { detail }) => {
                    assert!(
                        detail.contains("max_width"),
                        "错误 detail 必须点名 max_width：{detail}"
                    );
                }
                other => panic!("宽度 {width} 必须返回 TextShapeStandalone，实际 {other:?}"),
            }
        }
        // 零宽是合法约束（断行到 0 宽），不是错误：与「非有限 / 负值」分开。
        assert!(app.shape_text("x", &style, Some(0.0), &[]).is_ok());
    }

    #[test]
    fn empty_font_stack_is_a_typed_error() {
        let (_runtime, mut app) = new_app();
        app.set_font_stack(FontStack::new(std::iter::empty::<&'static str>()));
        match app.shape_text("x", &TextStyle::new(16.0), None, &[]) {
            Err(EngineError::TextShapeStandalone { detail }) => {
                assert!(detail.contains("font stack"), "{detail}");
            }
            other => panic!("空字体栈必须返回 TextShapeStandalone，实际 {other:?}"),
        }
    }

    /// 非空但没有任何候选命中的字体栈走 fontique 回退（不静默换字体、也不报错）；
    /// 节点路径的同一语义由 `TextDiagnostic::FontStackMiss` 可观测。
    #[test]
    fn unknown_family_falls_back_instead_of_failing() {
        let (_runtime, mut app) = new_app();
        let style = TextStyle::new(16.0).with_family("Kytrix No Such Family");
        let handle = app
            .shape_text("Fallback", &style, None, &[])
            .expect("非空字体栈必须回退而不是报错");
        assert!(glyph_count(handle.line()) > 0, "回退后仍要画出字形");
        assert!(handle.size().width > 0.0);
    }

    #[test]
    fn repeated_shape_reuses_the_cached_snapshot() {
        let (_runtime, mut app) = new_app();
        let style = TextStyle::new(16.0);
        let first = app.shape_text("Cache me", &style, None, &[]).expect("塑形");
        let second = app.shape_text("Cache me", &style, None, &[]).expect("塑形");
        assert_eq!(
            first.line(),
            second.line(),
            "同一文本 + 同一 shaping 输入必须走同一份形状缓存（字形/落点/行盒逐位相同）"
        );
        assert!(!first.is_stale());
        assert!(!second.is_stale());
    }
}

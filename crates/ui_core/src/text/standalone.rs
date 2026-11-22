//! E1：**无节点归属**的文本 shaping（`App::shape_text` 的实现，`§11.1`）。
//!
//! 与节点路径（`App::refresh_text` → `Window::compute_layout` → `App::layout_text`）的分工：
//! 节点路径的内容宽来自 taffy 布局；canvas 内文本没有节点，宽度由应用显式给出
//! （`max_width = None` ⇒ 单行、按 max-content 宽断行）。两条路径共用同一份形状缓存
//! （`text::cache::ShapeKey`）、同一条光栅/装箱路径（`ShapedText::rasterize`）与同一个字形
//! 图集，因此同一文本 + 同一 shaping 输入重复 shape 命中缓存、不重复塑形。颜色不进缓存键
//! （R12-11）：同文本换色同样命中布局、**不重复 shape**，但光栅按本次 `style.color` 重写基础
//! run（E24 / W6b F22）—— Canvas 文本没有节点绘制期的重上色通道。
//!
//! # 图集租约（D-5 的等价面）
//!
//! 句柄记录构造时的图集租约 `(AtlasAllocator::generation, GlyphAtlas::eviction_epoch)`；
//! `App` 侧另存一份**当前**租约（打包的 `AtomicU64`），任何可能改变图集状态的入口
//! （`App::{layout_text, set_font_stack, reload_system_fonts, shape_text}` 等）都会同步它。
//! 换代**或**任何 tile 被 LRU 淘汰后 [`super::ShapedTextHandle::is_stale`] 为真，
//! `Canvas::push_shaped_text` 不编码旧 UV —— 这正是 D-5 修掉的花字问题。本次 shape 自身触发
//! 的扩容 / 清空 / 淘汰会让**本进程其它窗口**的文本行与 Canvas 一起失效，调用方
//! （`App::shape_text`）负责把它们重新入队。
//!
//! # 设备 scale
//!
//! `§11.1` 的 `App::shape_text` 签名没有 window / scale 参数，因此本实现使用 **App 级默认
//! scale**（v2.1 E1b 用户裁决）：`App` 跟踪当前焦点窗口的 DPI 作为默认值，并提供显式覆盖
//! （`App::set_text_scale`）供离屏 / 测试 / 固定 DPI 场景使用。`ShapedLine::scale` 与句柄的
//! [`super::ShapedTextHandle::scale`] 都是该值；paint 阶段仍按窗口 `Scale` 放大。
//!
//! **应用义务**：句柄的 tile 位图按 shape 时刻的 scale 光栅。多窗口不同 DPI、或窗口被拖到
//! 另一块屏（DPI 变化）后，应用必须丢弃句柄并重新 `shape_text` —— 引擎不会自动重光栅。

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use smol_str::{SmolStr, format_smolstr};

use super::{
    InlineBoxSpec, ShapedInlineBox, ShapedTextHandle, ShapedTextItem, TextContent, TextStyle,
    atlas::GlyphAtlas, context::TextContext, pack_atlas_lease, raster::GlyphRasterizer,
    shape::RasterCtx,
};
use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{AtlasAllocator, DirtyRects},
};

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
    lease: &Arc<AtomicU64>,
    text: &str,
    style: &TextStyle,
    max_width: Option<f32>,
    inline_boxes: &[InlineBoxSpec],
    scale: f32,
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
    // 命中路径不重建 parley Layout，只按新的内容宽断行 + 光栅（颜色不在键里，由
    // `ShapedText::rasterize` 按本次 `style.color` 重写基础画刷；E24）。
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
                scale,
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
            // 句柄记录上**最终**租约：换代在 pass 内发生后已重跑，产出的 tile 属于这一代；
            // 本次 pass 自身造成的逐 tile 淘汰也把纪元带到最新（本 pass 用过的 tile 不会被
            // 自己淘汰，因此这份快照对刚产出的行仍然有效）。
            let eviction_epoch = glyph_atlas.eviction_epoch();
            lease.store(
                pack_atlas_lease(generation, eviction_epoch),
                Ordering::Relaxed,
            );
            return Ok(ShapedTextHandle::new(
                Arc::new(line),
                inline_boxes,
                generation,
                eviction_epoch,
                scale,
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
        event::Event,
        math::{Point, Rect, Scale, Size},
        node::builder::NodeBuilder,
        paint::{canvas::Canvas, color::Color, primitive::Primitive},
        text::{FontStack, InlineAlign, ShapedLine, TextOverflow, TextStyle},
        window::WindowOptions,
    };
    #[cfg(feature = "test-support")]
    use crate::{node::id::NodeId, window::WindowId};

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
        assert_eq!(line.scale, app.text_scale());
        assert_eq!(handle.scale(), app.text_scale(), "句柄必须记录光栅 scale");
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
                paint: None,
            },
            InlineBoxSpec {
                id: 9,
                index: 4,
                size: Size::new(8.0, 8.0),
                baseline: Some(1.0),
                align: InlineAlign::Middle,
                paint: None,
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
        let shaped_before = app.shape_count_for_test();
        let first = app.shape_text("Cache me", &style, None, &[]).expect("塑形");
        let second = app.shape_text("Cache me", &style, None, &[]).expect("塑形");
        assert_eq!(
            app.shape_count_for_test() - shaped_before,
            1,
            "同色重复调用必须命中形状缓存（不重复 shape）"
        );
        assert_eq!(
            first.line(),
            second.line(),
            "同一文本 + 同一 shaping 输入必须走同一份形状缓存（字形/落点/行盒逐位相同）"
        );
        assert!(!first.is_stale());
        assert!(!second.is_stale());
    }

    /// E24（W6b 审查 F22）：Canvas 文本换色必须真的换色。
    ///
    /// `ShapeKey` 不含 `color`（R12-11：颜色不是 shaping 输入）：同文本第二次以不同颜色 shape
    /// 时命中同一份布局快照、**不重复塑形**；但光栅必须按本次 `style.color` 重写基础 run ——
    /// Canvas 文本没有节点绘制期的 `foreground` 重上色，旧实现里 halo 副本永远画首次的颜色
    /// （chokepoint 的 halo 8 副本环视觉不存在的根因）。
    #[test]
    fn canvas_text_recolors_on_a_shaping_cache_hit() {
        let red = Color::Srgb {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            alpha: 1.0,
        };
        let blue = Color::Srgb {
            r: 0.0,
            g: 0.2,
            b: 1.0,
            alpha: 1.0,
        };
        let (_runtime, mut app) = new_app();
        let text = "Halo";
        let shaped_before = app.shape_count_for_test();
        let red_handle = app
            .shape_text(text, &TextStyle::new(16.0).with_color(red), None, &[])
            .expect("塑形");
        let blue_handle = app
            .shape_text(text, &TextStyle::new(16.0).with_color(blue), None, &[])
            .expect("塑形");
        assert_eq!(
            app.shape_count_for_test() - shaped_before,
            1,
            "同文本换色必须命中形状缓存（颜色不是 shaping 输入），只准塑形一次"
        );

        let colors = |handle: &ShapedTextHandle| -> Vec<Color> {
            handle.line().glyph_runs().map(|run| run.color).collect()
        };
        let red_colors = colors(&red_handle);
        assert!(!red_colors.is_empty(), "「Halo」必须有可见字形 run");
        assert!(red_colors.iter().all(|color| *color == red));
        let blue_colors = colors(&blue_handle);
        assert_eq!(
            blue_colors.len(),
            red_colors.len(),
            "同一份布局快照的 run 数不变"
        );
        assert!(
            blue_colors.iter().all(|color| *color == blue),
            "缓存命中后 run 的颜色必须来自本次 style.color：{blue_colors:?}"
        );

        // 布局缓存复用：字形落点逐位相同（只换颜色）。
        let positions = |handle: &ShapedTextHandle| -> Vec<(f32, f32)> {
            handle
                .line()
                .glyph_runs()
                .flat_map(|run| run.glyphs.iter())
                .map(|glyph| (glyph.position.x, glyph.position.y))
                .collect()
        };
        assert_eq!(positions(&red_handle), positions(&blue_handle));
        assert_eq!(red_handle.size(), blue_handle.size());

        // Canvas 图元（编码期的最终颜色）：Canvas 文本没有绘制期重上色，必须直接是本次颜色。
        let mut canvas = canvas();
        assert!(canvas.push_shaped_text(&blue_handle, Point::ZERO));
        let mut pushed = 0usize;
        for primitive in canvas.primitives() {
            let Primitive::GlyphRun(run) = primitive else {
                panic!("push_shaped_text 只能产生 GlyphRun 图元，实际 {primitive:?}");
            };
            pushed += 1;
            assert_eq!(run.color, blue, "Canvas 文本必须用本次 shape 的颜色");
        }
        assert!(pushed > 0, "「Halo」必须至少推一条字形 run");
    }

    // ── E1b：App 级默认 scale（hidpi 用户裁决）────────────────────────────────

    #[test]
    fn default_text_scale_is_one() {
        let (_runtime, mut app) = new_app();
        assert_eq!(app.text_scale(), 1.0, "默认 scale = 1.0（逻辑像素 1:1）");
        let handle = app
            .shape_text("scale", &TextStyle::new(16.0), None, &[])
            .expect("塑形");
        assert_eq!(handle.scale(), 1.0);
        assert_eq!(handle.line().scale, 1.0, "快照与句柄记录的 scale 同源");
    }

    /// 显式覆盖：进入光栅（同一文本在 2.0 下按 32 ppem 出 tile，与 1.0 的 16 ppem 不同），
    /// 且非法值返回强类型错误、不改变当前值。
    #[test]
    fn explicit_text_scale_override_enters_the_raster_and_is_validated() {
        let (_runtime, mut app) = new_app();
        let style = TextStyle::new(16.0);
        let one = app.shape_text("Hi", &style, None, &[]).expect("1.0");
        assert_eq!(one.scale(), 1.0);

        app.set_text_scale(2.0).expect("有限正数是合法的");
        assert_eq!(app.text_scale(), 2.0);
        let two = app.shape_text("Hi", &style, None, &[]).expect("2.0");
        assert_eq!(two.scale(), 2.0);
        assert_eq!(two.line().scale, 2.0);

        // `ShapeKey` 不含 scale（§4.5，逻辑坐标 shaping）：命中同一份快照，但 tile 属于不同 ppem。
        let first_tile = |handle: &ShapedTextHandle| {
            handle
                .line()
                .glyph_runs()
                .next()
                .and_then(|run| run.glyphs.first())
                .map(|glyph| glyph.tile)
                .expect("至少一个可见字形")
        };
        assert_ne!(
            first_tile(&one),
            first_tile(&two),
            "scale 必须经 ppem（16 vs 32）进入字形 tile"
        );

        for invalid in [f32::NAN, f32::INFINITY, 0.0, -1.0] {
            assert!(
                matches!(
                    app.set_text_scale(invalid),
                    Err(EngineError::InvalidTextScale { .. })
                ),
                "scale {invalid} 必须返回 InvalidTextScale"
            );
            assert_eq!(app.text_scale(), 2.0, "非法值不得改变当前 scale");
        }
    }

    /// 焦点窗口 DPI → App 级默认 scale：获得焦点 / DPI 变化时跟随；失焦保留上次值；
    /// 显式覆盖优先；取消覆盖后恢复跟随（多窗口不同 DPI 下应用义务见 `App::text_scale` 文档）。
    #[test]
    fn focused_window_dpi_drives_the_default_text_scale() {
        let (_runtime, mut app) = new_app();
        let window = app.create_window(WindowOptions {
            scale: Scale::new(1.5).expect("有限正数"),
            ..WindowOptions::default()
        });
        assert_eq!(app.text_scale(), 1.0, "还没有焦点事件 ⇒ 保持默认 1.0");

        app.dispatch(window, Event::FocusChanged { focused: true });
        assert_eq!(app.text_scale(), 1.5, "焦点窗口的 DPI 成为默认 scale");
        let handle = app
            .shape_text("dpi", &TextStyle::new(16.0), None, &[])
            .expect("塑形");
        assert_eq!(handle.scale(), 1.5);
        assert_eq!(handle.line().scale, 1.5);

        // 窗口被拖到另一块屏：`DpiChanged` 自带新 scale（Win/mac 先镜像 `Window::scale` 再派发；
        // Wayland 在派发之后镜像 —— 事件值才是权威，见下一条用例）。
        let two = Scale::new(2.0).expect("有限正数");
        app.window_mut(window).expect("窗口存在").set_scale(two);
        app.dispatch(
            window,
            Event::DpiChanged {
                dpi: 192,
                scale: two,
            },
        );
        assert_eq!(app.text_scale(), 2.0);

        // 失焦保留上次值（「无焦点窗口时保持上次值」）。
        app.dispatch(window, Event::FocusChanged { focused: false });
        assert_eq!(app.text_scale(), 2.0);

        // 显式覆盖优先于 DPI 跟随；取消覆盖后回到焦点窗口 DPI。
        app.set_text_scale(1.0).expect("合法覆盖");
        app.dispatch(window, Event::FocusChanged { focused: true });
        assert_eq!(app.text_scale(), 1.0, "覆盖期间不跟随焦点窗口 DPI");
        app.clear_text_scale_override();
        assert_eq!(app.text_scale(), 2.0);
    }

    /// F14（复核轮次 2）：Wayland 的 `DpiChanged` 在事件循环镜像 `Window::set_scale` **之前**
    /// 派发（`platform/linux/event_loop.rs` 的「① 事件 → ② 平台真值镜像」顺序）。默认 scale
    /// 必须取事件自带的 `scale`，不能读还停在旧值的 `Window::scale`（否则 text_scale 落后一次
    /// DPI，与 Windows / macOS 不一致）。
    #[test]
    fn dpi_event_scale_is_authoritative_before_the_window_mirror() {
        let (_runtime, mut app) = new_app();
        let window = app.create_window(WindowOptions {
            scale: Scale::ONE,
            ..WindowOptions::default()
        });
        app.dispatch(window, Event::FocusChanged { focused: true });
        assert_eq!(app.text_scale(), 1.0);

        // Wayland 顺序：先派发事件；窗口镜像在同一轮事件循环稍后发生。
        let two = Scale::new(2.0).expect("有限正数");
        app.dispatch(
            window,
            Event::DpiChanged {
                dpi: 192,
                scale: two,
            },
        );
        assert_eq!(
            app.window(window).expect("窗口存在").scale(),
            Scale::ONE,
            "本用例必须复现「事件先于镜像」的顺序：此时 `Window::scale` 仍是旧值"
        );
        assert_eq!(
            app.text_scale(),
            2.0,
            "默认 scale 必须取事件自带的 scale，而不是旧的 `Window::scale`"
        );

        // 事件循环随后镜像 `Window::scale`：同一事件不得再改变默认 scale。
        app.window_mut(window).expect("窗口存在").set_scale(two);
        assert_eq!(app.text_scale(), 2.0);

        // 显式覆盖期间 DPI 事件不跟随（三端语义一致）。
        app.set_text_scale(1.25).expect("合法覆盖");
        let three = Scale::new(3.0).expect("有限正数");
        app.dispatch(
            window,
            Event::DpiChanged {
                dpi: 288,
                scale: three,
            },
        );
        assert_eq!(app.text_scale(), 1.25, "覆盖期间不跟随 DPI 事件");
    }

    // ── E1b F1/F3：淘汰纪元与错误路径收尾 ────────────────────────────────────

    /// F1：长期句柄的 tile 在容量压力下被 LRU 淘汰 ⇒ `is_stale()`（代际不变、淘汰纪元前进）⇒
    /// push 拒绝 ⇒ 重新 shape 恢复。
    #[test]
    fn tile_eviction_makes_long_lived_handles_stale_and_reshaping_recovers() {
        let (_runtime, mut app) = new_app();
        // 32² 的页只放得下一两个 16 ppem 字形 ⇒ 十几个单字 shape 就必然触发逐 tile LRU 淘汰。
        app.cap_glyph_atlas_page_for_test(32);
        let generation = app.glyph_atlas_generation_for_test();
        let first = app
            .shape_text("A", &TextStyle::new(16.0), None, &[])
            .expect("首个句柄");
        assert_eq!(app.glyph_atlas_eviction_epoch_for_test(), 0);
        assert!(!first.is_stale());
        let mut canvas = canvas();
        assert!(canvas.push_shaped_text(&first, Point::ZERO));
        let before = canvas.primitives().len();
        assert!(before > 0);

        for glyph in ["B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M"] {
            app.shape_text(glyph, &TextStyle::new(16.0), None, &[])
                .expect("小页只能靠淘汰腾位置，单字 shape 必须成功");
        }
        assert_eq!(
            app.glyph_atlas_generation_for_test(),
            generation,
            "逐 tile 淘汰不换代（本用例只走淘汰路径）"
        );
        assert!(
            app.glyph_atlas_eviction_epoch_for_test() > 0,
            "淘汰必须前进纪元"
        );
        assert!(first.is_stale(), "代际不变、tile 已被复用 ⇒ 句柄也必须失效");
        assert!(!canvas.push_shaped_text(&first, Point::ZERO));
        assert_eq!(
            canvas.primitives().len(),
            before,
            "stale 句柄不得产生任何图元"
        );

        // 重新 shape：在淘汰腾出的空间里重新装箱，句柄属于当前租约。
        let fresh = app
            .shape_text("A", &TextStyle::new(16.0), None, &[])
            .expect("重新 shape");
        assert!(!fresh.is_stale());
        assert!(canvas.push_shaped_text(&fresh, Point::new(1.0, 2.0)));
    }

    /// F3：`App::shape_text` 的失败路径同样必须完成「淘汰失效 + 租约同步」收尾。
    ///
    /// 变异守卫：把收尾改回 `?` 早退（返回前跳过 `take_evicted` / lease 同步）本用例必须变红
    /// （句柄假活 + 节点文本行不失效）。
    #[test]
    fn failed_shape_still_invalidates_text_lines_and_syncs_the_lease() {
        let (_runtime, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let node = {
            let target = app.window_mut(window).expect("窗口存在");
            let root = target.tree().root();
            NodeBuilder::text("node label", TextStyle::new(16.0))
                .build(target.tree_mut(), root)
                .expect("文本节点")
        };
        // 先跑一帧：节点文本的 `ShapedLine` 落进槽位（后续断言「失效」的前置）。
        app.frame(window).expect("窗口存在").expect("首帧成功");
        assert!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .text_line(node)
                .is_some(),
            "前置：节点文本必须已排版"
        );

        // 小页：cap 自身换代后两级缓存清空，之后 32² 页只放得下一个 tile。
        app.cap_glyph_atlas_page_for_test(32);
        let handle = app
            .shape_text("A", &TextStyle::new(16.0), None, &[])
            .expect("首个句柄");
        assert!(!handle.is_stale());
        let generation = app.glyph_atlas_generation_for_test();

        // 一串互不相同的字形：先淘汰旧 tile（句柄 / 节点文本），随后以 AtlasExhausted 失败。
        let error = app
            .shape_text(
                "abcdefghijklmnopqrstuvwxyz",
                &TextStyle::new(16.0),
                None,
                &[],
            )
            .expect_err("32² 页放不下 26 个字形，必须报容量失败");
        assert!(matches!(
            error,
            EngineError::AtlasExhausted {
                kind: AtlasKind::Glyph
            }
        ));
        assert_eq!(
            app.glyph_atlas_generation_for_test(),
            generation,
            "失败路径只发生逐 tile 淘汰，不换代"
        );
        assert!(app.glyph_atlas_eviction_epoch_for_test() > 0);
        assert!(handle.is_stale(), "失败路径也必须同步租约：旧句柄不能假活");
        let tree = app.window(window).expect("窗口存在").tree();
        assert!(
            tree.text_line(node).is_none(),
            "失败路径发生的淘汰必须让节点文本行失效（否则下一帧复用旧 UV）"
        );
        assert!(
            tree.text_layout_dirty_len() > 0,
            "节点文本必须重新入队等待重排"
        );
    }

    // ── E1b F5：§11.1 明列的 emoji 与 L2 FrameRecord 断言 ─────────────────────

    /// §11.1 测试要求「CJK + emoji」：emoji 走彩色字形（COLR/CBDT）的 alpha 压平路径
    /// （`TextDiagnostic::ColorGlyphMonochrome`），这里只要求不 panic、度量单调（宿主字体不一定
    /// 带 emoji 字形，缺失走 tofu / 回退也必须成功）。
    #[test]
    fn cjk_and_emoji_mix_shapes_with_monotonic_metrics() {
        let (_runtime, mut app) = new_app();
        let style = TextStyle::new(16.0);
        let mixed = app
            .shape_text("Hello 🌍🚀 世界", &style, None, &[])
            .expect("CJK + emoji 必须能塑形");
        assert!(mixed.size().width > 0.0);
        assert!(mixed.size().height > 0.0);
        assert!(glyph_count(mixed.line()) > 0, "emoji 行必须有可见字形");
        let latin = app.shape_text("Hello ", &style, None, &[]).expect("Latin");
        assert!(
            mixed.size().width > latin.size().width,
            "emoji + CJK 必须比纯 Latin 前缀更宽"
        );
        let more = app
            .shape_text("Hello 🌍🚀🌍 世界", &style, None, &[])
            .expect("更多 emoji");
        assert!(
            more.size().width > mixed.size().width,
            "多一个 emoji 必须更宽（度量单调）"
        );
        // 限宽断行也不得 panic。
        let wrapped = app
            .shape_text("Hello 🌍🚀 世界", &style, Some(60.0), &[])
            .expect("限宽");
        assert!(wrapped.line().lines.len() > 1);
    }

    /// E1b F2：canvas 编码期安全网。push 之后同帧换代（换字体栈 ⇒ 图集换代），编码输出不得
    /// 含失效 run、`FrameRecord::stale_canvas_runs` 逐条计数；同代际帧正常编码。
    #[cfg(feature = "test-support")]
    #[test]
    fn stale_canvas_glyph_runs_are_dropped_at_encode_time_and_counted() {
        use crate::{
            paint::{
                color::Color,
                primitive::{Quad, QuadPaint},
            },
            test_support::{app::TestApp, record::PrimitiveKind},
            text::TextDiagnostic,
        };

        let mut app = TestApp::new();
        let window = app.window_id();
        let handle = app
            .app_mut()
            .shape_text("Canvas text", &TextStyle::new(16.0), None, &[])
            .expect("塑形");
        assert!(!handle.is_stale());
        let run_count = handle
            .line()
            .glyph_runs()
            .filter(|run| !run.glyphs.is_empty())
            .count();
        assert!(run_count > 0);
        let mut canvas = Canvas::new(Rect::new(0.0, 0.0, 200.0, 40.0));
        // 一个非文本图元：过滤只应删失效 run，Canvas 本身仍要上屏。
        canvas.push_quad(Quad {
            bounds: Rect::new(0.0, 0.0, 200.0, 40.0),
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: Color::WHITE,
                border: None,
            },
        });
        assert!(canvas.push_shaped_text(&handle, Point::new(2.0, 3.0)));
        let canvas_node = {
            let target = app.app_mut().window_mut(window).expect("窗口存在");
            let root = target.tree().root();
            NodeBuilder::canvas(canvas)
                .layout_override(Rect::new(0.0, 0.0, 200.0, 40.0))
                .id("canvas.text")
                .build(target.tree_mut(), root)
                .expect("Canvas 节点")
        };

        // 同代际：正常编码，Canvas 内的 GlyphRun 数 = 非空 run 数。
        let record = app.drain().expect("帧成功");
        assert_eq!(record.stale_canvas_runs, 0, "同代际不得丢弃任何 run");
        let rendered = record.frame.as_ref().expect("已提交帧");
        assert!(
            rendered
                .primitives
                .iter()
                .any(|primitive| primitive.node == canvas_node
                    && primitive.kind == PrimitiveKind::Canvas),
            "Canvas 节点必须以一个 Canvas 图元进帧记录"
        );
        assert_eq!(
            canvas_scene_glyph_run_count(app.app(), window),
            run_count,
            "同代际：Canvas 内 GlyphRun 图元数 = 非空 run 数"
        );

        // 换字体栈 ⇒ 图集换代；应用持有的 Canvas 仍是旧 tile（没有重建）。应用侧标重绘
        //（本测试没有 a11y 消费者，单靠唤醒会被 a11y-only 快速路径吞掉）后引擎重绘。
        app.app_mut()
            .set_font_stack(FontStack::new(["Kytrix No Such Family"]));
        assert!(handle.is_stale(), "换代后句柄必须失活");
        assert!(
            app.app_mut()
                .request_node_repaint(window, canvas_node)
                .expect("请求重绘")
        );
        let record = app.drain().expect("帧成功");
        assert_eq!(
            record.stale_canvas_runs, run_count,
            "失效 run 必须逐条计入诊断（少字优于花字）"
        );
        assert!(
            app.app()
                .text_diagnostics()
                .iter()
                .any(|diagnostic| matches!(
                    diagnostic,
                    TextDiagnostic::CanvasTextStale { runs } if *runs == run_count
                )),
            "少字必须经 `TextDiagnostic::CanvasTextStale` 可观测（D17 不静默）：{:?}",
            app.app().text_diagnostics()
        );
        assert_eq!(
            canvas_scene_glyph_run_count(app.app(), window),
            0,
            "编码输出不得含任何失效 run"
        );
    }

    /// E1d（F15 方案①）：push 之后发生的**逐 tile LRU 淘汰**（`generation` 不变）同样会让
    /// Canvas 里已写入的 `AtlasTile` 失效；编码期逐 tile `is_alive` 必须在编码前过滤并计数。
    ///
    /// 这正是 F15 记录的场景：旧实现只比「tile 代际 == 当前代际」，逐 tile 淘汰不换代 ⇒
    /// 过滤 0 条、把被复用的旧 UV 编码上屏（花字 + 无诊断）。本用例断言：代际不变、淘汰纪元
    /// 前进的帧里，Canvas 的旧 run 被**全部**丢弃、计数 = run 数、诊断可观测。
    ///
    /// 变异守卫：`AtlasAllocator::is_alive` 恒真 ⇒ `stale_canvas_runs == 0` ⇒ 本用例 RED。
    #[cfg(feature = "test-support")]
    #[test]
    fn evicted_canvas_tiles_are_filtered_at_encode_time_without_a_generation_bump() {
        use crate::{
            paint::{
                color::Color,
                primitive::{Quad, QuadPaint},
            },
            test_support::app::TestApp,
            text::TextDiagnostic,
        };

        let mut app = TestApp::new();
        let window = app.window_id();
        // 32² 的小页只放得下一两块 16 ppem 字形 ⇒ 十几个单字 shape 就必然触发 LRU 淘汰。
        app.app_mut().cap_glyph_atlas_page_for_test(32);
        let generation = app.app().glyph_atlas_generation_for_test();
        let handle = app
            .app_mut()
            .shape_text("A", &TextStyle::new(16.0), None, &[])
            .expect("首个小页字形");
        assert!(!handle.is_stale());
        let run_count = handle
            .line()
            .glyph_runs()
            .filter(|run| !run.glyphs.is_empty())
            .count();
        assert!(run_count > 0);
        let mut canvas = Canvas::new(Rect::new(0.0, 0.0, 200.0, 40.0));
        // 非文本图元：编码期安全网只丢弃失效 run，Canvas 本身仍要上屏。
        canvas.push_quad(Quad {
            bounds: Rect::new(0.0, 0.0, 200.0, 40.0),
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: Color::WHITE,
                border: None,
            },
        });
        // push 时句柄有效：旧实现的「正常路径」把 tile 写进 Canvas 值。
        assert!(canvas.push_shaped_text(&handle, Point::new(2.0, 3.0)));
        let canvas_node = {
            let target = app.app_mut().window_mut(window).expect("窗口存在");
            let root = target.tree().root();
            NodeBuilder::canvas(canvas)
                .layout_override(Rect::new(0.0, 0.0, 200.0, 40.0))
                .id("canvas.evicted")
                .build(target.tree_mut(), root)
                .expect("Canvas 节点")
        };

        // 淘汰前：租约未变 ⇒ 正常编码、零过滤（回归：正常路径零额外过滤）。
        let record = app.drain().expect("帧成功");
        assert_eq!(record.stale_canvas_runs, 0, "淘汰前不得丢弃任何 run");
        assert_eq!(
            canvas_scene_glyph_run_count(app.app(), window),
            run_count,
            "淘汰前 Canvas 文本必须正常编码"
        );

        // 容量压力：小页 + 一串互不相同的单字 ⇒ 逐 tile LRU 淘汰（不换代）。句柄因此失活，
        // 但 Canvas 里已经写好的 tile 仍指向被复用的矩形 —— F15 的失效面。
        for glyph in ["B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M"] {
            app.app_mut()
                .shape_text(glyph, &TextStyle::new(16.0), None, &[])
                .expect("小页只能靠逐 tile 淘汰腾位置，单字 shape 必须成功");
        }
        assert_eq!(
            app.app().glyph_atlas_generation_for_test(),
            generation,
            "本用例只走淘汰路径（generation 不变）"
        );
        assert!(
            app.app().glyph_atlas_eviction_epoch_for_test() > 0,
            "必须真的发生过逐 tile 淘汰"
        );
        assert!(handle.is_stale(), "淘汰后旧句柄必须失活");

        // 编码：`App::shape_text` 的淘汰收尾已把 Canvas 节点标 PAINT，本帧重建 lane。
        let record = app.drain().expect("帧成功");
        assert_eq!(
            record.stale_canvas_runs, run_count,
            "F15：淘汰后的旧 tile 必须在编码期被过滤（旧实现只比代际 ⇒ 这里会是 0）"
        );
        assert_eq!(
            canvas_scene_glyph_run_count(app.app(), window),
            0,
            "编码输出不得含任何被复用的旧 UV（无花字）"
        );
        assert!(
            app.app()
                .text_diagnostics()
                .iter()
                .any(|diagnostic| matches!(
                    diagnostic,
                    TextDiagnostic::CanvasTextStale { runs } if *runs == run_count
                )),
            "淘汰导致的少字必须经 `TextDiagnostic::CanvasTextStale` 可观测（D17 不静默）"
        );
        assert!(
            app.app()
                .window(window)
                .expect("窗口存在")
                .tree()
                .get(canvas_node)
                .is_ok(),
            "过滤只丢弃失效 run，Canvas 节点本身必须在场"
        );
    }
    /// F13（复核轮次 2）：Canvas 失效 run 在编码期被丢弃后必须**有界补帧**。
    ///
    /// 静态窗口不会因为「应用还没重建 Canvas」而自然再出帧：不补帧 ⇒ 应用义务（在下一帧的
    /// `on_frame` 钩子里重新 `shape_text` + `set_canvas`）不可执行，canvas 文本会无限期缺失。
    /// 补帧必须按图集代际有界：同一代际至多一次，应用忽略义务时不能变成 I3 空转。
    ///
    /// 变异守卫：删掉 `App::frame` 里 `stale_canvas_runs > 0` 分支的补帧请求 ⇒ 本用例变红。
    #[cfg(feature = "test-support")]
    #[test]
    fn stale_canvas_runs_request_exactly_one_bounded_follow_up_frame() {
        use crate::{
            paint::{
                color::Color,
                primitive::{Quad, QuadPaint},
            },
            test_support::app::TestApp,
        };

        let mut app = TestApp::new();
        let window = app.window_id();
        let style = TextStyle::new(16.0);
        let handle = app
            .app_mut()
            .shape_text("Canvas follow-up", &style, None, &[])
            .expect("塑形");
        let run_count = handle
            .line()
            .glyph_runs()
            .filter(|run| !run.glyphs.is_empty())
            .count();
        assert!(run_count > 0);
        let mut canvas = Canvas::new(Rect::new(0.0, 0.0, 240.0, 40.0));
        // 非文本图元：编码期安全网只丢弃失效 run，Canvas 本身仍要上屏。
        canvas.push_quad(Quad {
            bounds: Rect::new(0.0, 0.0, 240.0, 40.0),
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: Color::WHITE,
                border: None,
            },
        });
        assert!(canvas.push_shaped_text(&handle, Point::new(2.0, 3.0)));
        let canvas_node = {
            let target = app.app_mut().window_mut(window).expect("窗口存在");
            let root = target.tree().root();
            NodeBuilder::canvas(canvas)
                .layout_override(Rect::new(0.0, 0.0, 240.0, 40.0))
                .id("canvas.followup")
                .build(target.tree_mut(), root)
                .expect("Canvas 节点")
        };

        // 无失效 run：静态窗口不产生任何额外帧（I3）。
        let record = app.drain().expect("帧成功");
        assert_eq!(record.stale_canvas_runs, 0);
        let requests = app.app().frame_requests();
        let record = app.drain().expect("帧成功");
        assert!(!record.rendered, "无失效 run 的静态窗口必须停在 Idle");
        assert_eq!(
            app.app().frame_requests(),
            requests,
            "无失效 run 时不得请求补帧（0 额外帧）"
        );

        // 换代（换字体栈）⇒ 已 push 的 tile 失活；应用侧请求一次重绘（生产里同构）。
        app.app_mut()
            .set_font_stack(FontStack::new(["Kytrix No Such Family"]));
        assert!(handle.is_stale());
        app.app_mut()
            .request_node_repaint(window, canvas_node)
            .expect("请求重绘");
        let requests = app.app().frame_requests();
        let record = app.drain().expect("帧成功");
        assert_eq!(
            record.stale_canvas_runs, run_count,
            "换代的失效 run 必须被编码期安全网丢弃（少字优于花字）"
        );
        assert_eq!(
            app.app().frame_requests(),
            requests + 1,
            "丢弃失效 run 后必须恰好补一帧：应用义务只有在下一帧的钩子里可执行"
        );
        assert_eq!(
            canvas_scene_glyph_run_count(app.app(), window),
            0,
            "本帧不得编码任何失效 run"
        );

        // 补帧之前（= 应用在补帧的 `on_frame` 钩子里）：重新 shape + 重建 Canvas。
        let fresh = app
            .app_mut()
            .shape_text("Canvas follow-up", &style, None, &[])
            .expect("重新塑形");
        assert!(!fresh.is_stale(), "新句柄属于当前图集租约");
        let mut rebuilt = Canvas::new(Rect::new(0.0, 0.0, 240.0, 40.0));
        rebuilt.push_quad(Quad {
            bounds: Rect::new(0.0, 0.0, 240.0, 40.0),
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: Color::WHITE,
                border: None,
            },
        });
        assert!(rebuilt.push_shaped_text(&fresh, Point::new(2.0, 3.0)));
        assert!(
            app.app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .set_canvas(canvas_node, rebuilt)
                .expect("节点存活")
        );

        // 引擎请求的补帧：重建后的文本恢复上屏，且不再丢弃 run。
        let record = app.drain().expect("帧成功");
        assert!(record.rendered, "补帧必须真的运行（应用义务依赖它）");
        assert_eq!(record.stale_canvas_runs, 0);
        assert_eq!(
            canvas_scene_glyph_run_count(app.app(), window),
            run_count,
            "重 push 后 canvas 文本必须恢复"
        );
        // 同一图集代际只补一帧：补帧后窗口重新回到 Idle（不空转）。
        let record = app.drain().expect("帧成功");
        assert!(!record.rendered, "同一图集代际只补一帧（I3）");
    }

    /// 场景里所有 Canvas 图元的内部 `GlyphRun` 数（E1b 的 L2 断言面：FrameRecord 把 Canvas 当
    /// 不透明图元，内层图元数只能在场景上核对）。
    #[cfg(feature = "test-support")]
    fn canvas_scene_glyph_run_count(app: &App, window: WindowId) -> usize {
        let target = app.window(window).expect("窗口存在");
        let scene = target.scene();
        let mut runs = 0usize;
        for lane in scene.order() {
            for primitive in scene.lane_primitives(*lane).unwrap_or(&[]) {
                if let Primitive::Canvas(canvas) = primitive {
                    runs += canvas
                        .primitives()
                        .iter()
                        .filter(|inner| matches!(inner, Primitive::GlyphRun(_)))
                        .count();
                }
            }
        }
        runs
    }

    /// L2（§11.1）：节点文本走帧后，`FrameRecord` 里的 `Primitive::GlyphRun` 全部落在节点盒内，
    /// 且 lane 的 GlyphRun 图元数 = 非空 run 数（与 canvas 的 `push_shaped_text` 同一套 run 语义）。
    #[cfg(feature = "test-support")]
    #[test]
    fn frame_record_glyph_runs_stay_inside_the_text_node_box() {
        use crate::test_support::{app::TestApp, record::PrimitiveKind};

        let mut app = TestApp::new();
        let window = app.window_id();
        let node = {
            let target = app.app_mut().window_mut(window).expect("窗口存在");
            let root = target.tree().root();
            NodeBuilder::text("Frame L2 text", TextStyle::new(16.0))
                .layout_override(Rect::new(10.0, 20.0, 220.0, 40.0))
                .id("text.l2")
                .build(target.tree_mut(), root)
                .expect("文本节点")
        };
        let record = app.drain().expect("帧成功");
        // 先把帧记录里的断言面拷出来，结束对 `app` 的独占借用。
        let glyph_runs: Vec<(NodeId, Rect)> = record
            .frame
            .as_ref()
            .expect("已提交帧")
            .primitives
            .iter()
            .filter(|primitive| primitive.kind == PrimitiveKind::GlyphRun)
            .map(|primitive| (primitive.node, primitive.bounds))
            .collect();
        let target = app.app().window(window).expect("窗口存在");
        let node_bounds = target.tree().get(node).expect("节点").layout().bounds;
        let line = target.tree().text_line(node).expect("文本行").clone();
        let expected_runs = line
            .glyph_runs()
            .filter(|run| !run.glyphs.is_empty())
            .count();
        let node_runs: Vec<Rect> = glyph_runs
            .into_iter()
            .filter(|(owner, _)| *owner == node)
            .map(|(_, bounds)| bounds)
            .collect();
        assert_eq!(
            node_runs.len(),
            expected_runs,
            "节点 lane 的 GlyphRun 图元数 = 非空 run 数"
        );
        assert!(!node_runs.is_empty());
        for bounds in node_runs {
            assert!(
                bounds.origin.x >= node_bounds.origin.x - 0.5
                    && bounds.origin.y >= node_bounds.origin.y - 0.5
                    && bounds.origin.x + bounds.size.width
                        <= node_bounds.origin.x + node_bounds.size.width + 0.5
                    && bounds.origin.y + bounds.size.height
                        <= node_bounds.origin.y + node_bounds.size.height + 0.5,
                "GlyphRun bounds {bounds:?} 必须 ⊆ 节点盒 {node_bounds:?}"
            );
        }
    }
}

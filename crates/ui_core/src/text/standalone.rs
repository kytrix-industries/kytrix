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

const MAX_SHAPE_ATTEMPTS: u32 = 4;

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
    if let Some(width) = max_width
        && (!width.is_finite() || width < 0.0)
    {
        return Err(EngineError::TextShapeStandalone {
            detail: format_smolstr!("max_width {width} must be finite and non-negative"),
        });
    }
    if text_context.font_stack().families().len() == 0 {
        return Err(EngineError::TextShapeStandalone {
            detail: SmolStr::new_static("font stack is empty"),
        });
    }
    let font_epoch = text_context.font_epoch();
    raster.set_font_epoch(font_epoch);

    let content =
        TextContent::new(text, style.clone()).with_inline_boxes(inline_boxes.iter().copied());
    let (shaped, _hit) = text_context.shape_cached(&content);
    let width = max_width.unwrap_or_else(|| shaped.max_content_size().width);
    let ellipsis = text_context.ellipsis_for(style);

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
        assert_eq!(
            glyph_count(mixed.line()),
            7,
            "ASCII + CJK 混排的每个非空白字符都要有字形：{:?}",
            mixed.line()
        );
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
        for line in &wrapped.line().lines {
            assert!(
                line.advance <= 80.0 + hanging,
                "行推进 {} 超过 max_width + 行末空白",
                line.advance
            );
        }
    }

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

        app.dispatch(window, Event::FocusChanged { focused: false });
        assert_eq!(app.text_scale(), 2.0);

        app.set_text_scale(1.0).expect("合法覆盖");
        app.dispatch(window, Event::FocusChanged { focused: true });
        assert_eq!(app.text_scale(), 1.0, "覆盖期间不跟随焦点窗口 DPI");
        app.clear_text_scale_override();
        assert_eq!(app.text_scale(), 2.0);
    }

    #[test]
    fn dpi_event_scale_is_authoritative_before_the_window_mirror() {
        let (_runtime, mut app) = new_app();
        let window = app.create_window(WindowOptions {
            scale: Scale::ONE,
            ..WindowOptions::default()
        });
        app.dispatch(window, Event::FocusChanged { focused: true });
        assert_eq!(app.text_scale(), 1.0);

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

        app.window_mut(window).expect("窗口存在").set_scale(two);
        assert_eq!(app.text_scale(), 2.0);

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

    #[test]
    fn tile_eviction_makes_long_lived_handles_stale_and_reshaping_recovers() {
        let (_runtime, mut app) = new_app();
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

        let fresh = app
            .shape_text("A", &TextStyle::new(16.0), None, &[])
            .expect("重新 shape");
        assert!(!fresh.is_stale());
        assert!(canvas.push_shaped_text(&fresh, Point::new(1.0, 2.0)));
    }

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
        app.frame(window).expect("窗口存在").expect("首帧成功");
        assert!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .text_line(node)
                .is_some(),
            "前置：节点文本必须已排版"
        );

        app.cap_glyph_atlas_page_for_test(32);
        let handle = app
            .shape_text("A", &TextStyle::new(16.0), None, &[])
            .expect("首个句柄");
        assert!(!handle.is_stale());
        let generation = app.glyph_atlas_generation_for_test();

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
        let wrapped = app
            .shape_text("Hello 🌍🚀 世界", &style, Some(60.0), &[])
            .expect("限宽");
        assert!(wrapped.line().lines.len() > 1);
    }

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
                .id("canvas.evicted")
                .build(target.tree_mut(), root)
                .expect("Canvas 节点")
        };

        let record = app.drain().expect("帧成功");
        assert_eq!(record.stale_canvas_runs, 0, "淘汰前不得丢弃任何 run");
        assert_eq!(
            canvas_scene_glyph_run_count(app.app(), window),
            run_count,
            "淘汰前 Canvas 文本必须正常编码"
        );

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

        let record = app.drain().expect("帧成功");
        assert!(record.rendered, "补帧必须真的运行（应用义务依赖它）");
        assert_eq!(record.stale_canvas_runs, 0);
        assert_eq!(
            canvas_scene_glyph_run_count(app.app(), window),
            run_count,
            "重 push 后 canvas 文本必须恢复"
        );
        let record = app.drain().expect("帧成功");
        assert!(!record.rendered, "同一图集代际只补一帧（I3）");
    }

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

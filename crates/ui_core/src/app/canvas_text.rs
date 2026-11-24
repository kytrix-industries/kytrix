use std::rc::Rc;

use super::App;
use crate::{
    error::EngineError,
    math::{LogicalPx, LogicalSize},
    node::id::NodeId,
    paint::{atlas::AtlasKind, canvas::Canvas, primitive::Primitive},
    text::{TextContent, TextDiagnostic, shape::RasterCtx},
    window::{
        WindowId,
        canvas_text::{CanvasTextKey, ResolvedCanvasText, lines_are_live},
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CanvasTextReport {
    pub evicted: bool,
    pub pending: bool,
}

pub(crate) fn resolve_canvas_text(
    app: &mut App,
    window: WindowId,
) -> Option<Result<CanvasTextReport, EngineError>> {
    let App {
        text,
        raster,
        atlas,
        glyph_atlas,
        windows,
        pending_dirty,
        ..
    } = app;
    let target = windows.get_mut(&window)?;
    let scale = target.scale();
    let dpr_bits = scale.factor().to_bits();
    let font_stack_version = text.font_stack_version();
    let nodes: Vec<NodeId> = target.tree().canvas_nodes().to_vec();
    let node_set: ahash::HashSet<NodeId> = nodes.iter().copied().collect();
    target
        .canvas_text_mut()
        .retain(|node, _| node_set.contains(node));

    let mut report = CanvasTextReport::default();
    let mut rounds = 0usize;
    loop {
        let generation = atlas.generation();
        let eviction_epoch = glyph_atlas.eviction_epoch();
        for node in &nodes {
            let Some(canvas) = target.tree().canvas(*node).cloned() else {
                continue;
            };
            let Some(canvas_generation) = target.tree().canvas_generation(*node) else {
                continue;
            };
            let key = CanvasTextKey {
                canvas_generation,
                font_stack_version,
                dpr_bits,
                atlas_generation: generation,
                glyph_eviction_epoch: eviction_epoch,
            };
            if target
                .canvas_text()
                .get(node)
                .is_some_and(|existing| existing.key == key && !existing.pending)
            {
                continue;
            }
            let mut recipes: Vec<TextContent> = Vec::new();
            collect_recipes(&canvas, &mut recipes);
            let mut lines: Vec<Rc<crate::text::ShapedLine>> = Vec::with_capacity(recipes.len());
            let mut skipped = 0usize;
            for recipe in recipes {
                let (shaped, _hit) = text.shape_cached(&recipe);
                let width = shaped.max_content_size().width;
                let ellipsis = text.ellipsis_for(&recipe.style);
                let raster_generation = atlas.generation();
                let mut context = RasterCtx {
                    raster,
                    glyph_atlas,
                    atlas,
                    generation: raster_generation,
                    scale,
                };
                let (line, diagnostics, stats) = match shaped.rasterize(
                    width,
                    recipe.style.alignment,
                    recipe.style.color,
                    &mut context,
                    ellipsis.as_deref(),
                ) {
                    Ok(value) => value,
                    Err(error) => return Some(Err(error)),
                };
                skipped += stats.skipped;
                text.push_diagnostics(diagnostics);
                pending_dirty.merge(&mut atlas.take_dirty_rects());
                lines.push(Rc::new(line));
            }
            if skipped > 0 {
                text.push_diagnostics(vec![TextDiagnostic::AtlasFull { skipped }]);
            }
            let key = CanvasTextKey {
                atlas_generation: atlas.generation(),
                glyph_eviction_epoch: glyph_atlas.eviction_epoch(),
                ..key
            };
            let live = lines_are_live(&lines, atlas);
            let previous_live = target
                .canvas_text()
                .get(node)
                .is_some_and(|existing| lines_are_live(&existing.lines, atlas));
            let pending = skipped > 0 || !live;
            if live || !previous_live {
                target.canvas_text_mut().insert(
                    *node,
                    ResolvedCanvasText {
                        key,
                        lines,
                        pending,
                    },
                );
            } else if let Some(existing) = target.canvas_text_mut().get_mut(node) {
                existing.pending = true;
            }
            let _ = target.tree_mut().mark_paint(*node);
        }
        let evicted = glyph_atlas.take_evicted() > 0;
        let generation_changed = atlas.generation() != generation;
        if !evicted && !generation_changed {
            break;
        }
        report.evicted = true;
        target.tree_mut().invalidate_text_lines();
        target.tree_mut().mark_canvas_nodes_paint();
        rounds += 1;
        if rounds >= super::text_pass::MAX_LAYOUT_TEXT_PASSES {
            break;
        }
    }
    report.pending = nodes.iter().any(|node| {
        target
            .canvas_text()
            .get(node)
            .is_some_and(|entry| entry.pending)
    });
    let escalated = if report.pending {
        if target.note_canvas_text_pending() {
            target.reset_canvas_text_pending();
            true
        } else {
            false
        }
    } else {
        target.reset_canvas_text_pending();
        false
    };
    if report.evicted {
        for (id, other) in windows.iter_mut() {
            if *id == window {
                continue;
            }
            other.tree_mut().invalidate_text_lines();
            other.tree_mut().mark_canvas_nodes_paint();
        }
    }
    if escalated {
        return Some(Err(EngineError::AtlasExhausted {
            kind: AtlasKind::Glyph,
        }));
    }
    Some(Ok(report))
}

fn collect_recipes(canvas: &Canvas, out: &mut Vec<TextContent>) {
    for primitive in canvas.primitives() {
        match primitive {
            Primitive::Text(text) => out.push(text.content.clone()),
            Primitive::Canvas(inner) => collect_recipes(inner, out),
            _ => {}
        }
    }
}

pub(crate) fn measure_text(
    app: &mut App,
    content: &TextContent,
    max_width: Option<LogicalPx>,
) -> LogicalSize {
    let max_width = match max_width {
        Some(width) if width.is_finite() && width >= LogicalPx::ZERO => Some(width),
        Some(width) => {
            app.text
                .push_diagnostics(vec![TextDiagnostic::InvalidMeasureWidth { width }]);
            None
        }
        None => None,
    };
    let (shaped, _hit) = app.text.shape_cached(content);
    let known = taffy::Size {
        width: max_width.map(LogicalPx::value),
        height: None,
    };
    let available = taffy::Size {
        width: max_width.map_or(taffy::AvailableSpace::MaxContent, |width| {
            taffy::AvailableSpace::Definite(width.value())
        }),
        height: taffy::AvailableSpace::MaxContent,
    };
    shaped.measure(known, available)
}

#[cfg(all(test, feature = "test-support"))]
pub(crate) fn resolved_lines(
    app: &App,
    window: WindowId,
    node: NodeId,
) -> Option<Vec<Rc<crate::text::ShapedLine>>> {
    let entries = app.window(window)?.canvas_text().get(&node)?;
    Some(entries.lines.clone())
}
#[cfg(all(test, feature = "test-support"))]
mod tests {
    use std::sync::Arc;

    use super::{App, resolved_lines};
    use crate::{
        error::EngineError,
        math::{DevicePixelRatio, LogicalPoint, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            canvas::Canvas,
            color::Color,
            primitive::Primitive,
        },
        test_support::app::TestApp,
        text::{ShapedLine, ShapedTextItem, TextContent, TextDiagnostic, TextStyle},
        window::{FrameOutcome, WindowId},
    };

    fn bounds() -> LogicalRect {
        LogicalRect::from_origin_size(LogicalPoint::ZERO, LogicalSize::new(px(200.0), px(100.0)))
    }

    fn styled(color: Color) -> TextStyle {
        let mut style = TextStyle::new(px(16.0));
        style.color = color;
        style
    }

    fn styled_at(size: f32) -> TextStyle {
        let mut style = TextStyle::new(px(size));
        style.color = Color::WHITE;
        style
    }

    fn text_canvas(entries: &[(&str, LogicalPoint)]) -> Canvas {
        let mut canvas = Canvas::new(bounds());
        for (text, origin) in entries {
            assert!(
                canvas.push_text(TextContent::new(*text, styled(Color::WHITE)), *origin),
                "non-empty recipes are recorded"
            );
        }
        canvas
    }

    fn mount(app: &mut TestApp, canvas: Canvas) -> NodeId {
        let window = app.window_id();
        let tree = app
            .app_mut()
            .window_mut(window)
            .expect("window exists")
            .tree_mut();
        let root = tree.root();
        NodeBuilder::canvas(canvas)
            .layout_override(bounds())
            .build(tree, root)
            .expect("canvas mounts")
    }

    fn canvas_of(app: &App, window: WindowId, node: NodeId) -> Arc<Canvas> {
        Arc::clone(
            app.window(window)
                .expect("window exists")
                .tree()
                .get(node)
                .expect("canvas node exists")
                .canvas()
                .expect("node is a canvas"),
        )
    }

    fn first_tile(line: &ShapedLine) -> AtlasTile {
        line.lines
            .iter()
            .flat_map(|shaped| shaped.items.iter())
            .find_map(|item| match item {
                ShapedTextItem::GlyphRun(run) => run.glyphs.first().map(|glyph| glyph.tile),
                ShapedTextItem::InlineBox(_) => None,
            })
            .expect("shaped text carries glyphs")
    }

    fn first_position(line: &ShapedLine) -> LogicalPoint {
        line.lines
            .iter()
            .flat_map(|shaped| shaped.items.iter())
            .find_map(|item| match item {
                ShapedTextItem::GlyphRun(run) => run.glyphs.first().map(|glyph| glyph.position),
                ShapedTextItem::InlineBox(_) => None,
            })
            .expect("shaped text carries glyphs")
    }

    fn glyph_count(line: &ShapedLine) -> usize {
        line.lines
            .iter()
            .flat_map(|shaped| shaped.items.iter())
            .filter_map(|item| match item {
                ShapedTextItem::GlyphRun(run) => Some(run.glyphs.len()),
                ShapedTextItem::InlineBox(_) => None,
            })
            .sum()
    }

    fn assert_rendered_tiles_live(app: &TestApp, node: NodeId) {
        let Some(rendered) = app.rendered() else {
            return;
        };
        let Some(entry) = rendered.canvases.iter().find(|entry| entry.node == node) else {
            return;
        };
        for primitive in entry.canvas.primitives() {
            let Primitive::GlyphRun(run) = primitive else {
                continue;
            };
            for glyph in &run.glyphs {
                assert!(
                    app.app().atlas.is_alive(&glyph.tile),
                    "a stale glyph tile reached the scene"
                );
            }
        }
    }
    fn constrain_glyph_atlas(app: &mut TestApp) {
        let mut atlas = crate::render::AtlasAllocator::with_page(AtlasKind::Glyph, (32, 32));
        atlas.cap_page_for_test(32);
        let app_mut = app.app_mut();
        app_mut.atlas = atlas;
        app_mut.glyph_atlas.clear();
        app_mut.request_frame();
    }

    fn free_glyph_atlas(app: &mut TestApp) {
        let app_mut = app.app_mut();
        app_mut.atlas = crate::render::AtlasAllocator::with_page(AtlasKind::Glyph, (512, 512));
        app_mut.glyph_atlas.clear();
        app_mut.request_frame();
    }

    fn has_system_fonts(app: &mut TestApp) -> bool {
        app.app_mut().text.family_count() > 0
    }

    fn drain_main(app: &mut TestApp, window: WindowId) -> Result<FrameOutcome, EngineError> {
        let _ = app.app_mut().a11y_take(window);
        app.window().drain()
    }

    fn size_bits(size: LogicalSize) -> (u32, u32) {
        (size.width.value().to_bits(), size.height.value().to_bits())
    }

    #[test]
    fn measure_text_returns_the_unwrapped_size() {
        let mut app = TestApp::new();
        let content = TextContent::new("Hello world", styled(Color::WHITE));
        let size = app.app_mut().measure_text(&content, None);
        assert!(size.width > px(0.0));
        assert!(size.height > px(0.0));
        let empty = app
            .app_mut()
            .measure_text(&TextContent::new("", styled(Color::WHITE)), None);
        assert_eq!(empty, LogicalSize::ZERO, "empty content has no extent");
    }

    #[test]
    fn measure_text_wraps_at_the_requested_width() {
        let mut app = TestApp::new();
        let content = TextContent::new(
            "the quick brown fox jumps over the lazy dog",
            styled(Color::WHITE),
        );
        let single = app.app_mut().measure_text(&content, None);
        let wrapped = app.app_mut().measure_text(&content, Some(px(60.0)));
        assert!(
            wrapped.height > single.height,
            "a narrow measure grows the height"
        );
        assert!(
            wrapped.width <= px(68.0),
            "the wrapped width stays inside the request plus the trailing word"
        );
    }

    #[test]
    fn measure_text_is_bit_identical_at_every_dpr() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let content = TextContent::new("Hello 1234567890", styled(Color::WHITE));
        let mut sizes = Vec::new();
        for factor in [1.0, 1.5, 2.0] {
            app.app_mut()
                .window_mut(window)
                .expect("window exists")
                .set_scale(DevicePixelRatio::new(factor).expect("valid DPR"));
            sizes.push(size_bits(app.app_mut().measure_text(&content, None)));
        }
        assert_eq!(
            sizes[0], sizes[1],
            "logical measurement is independent of the DPR"
        );
        assert_eq!(sizes[0], sizes[2], "1.0x and 2.0x measure identically");
    }

    #[test]
    fn measure_text_treats_invalid_widths_as_max_content() {
        let mut app = TestApp::new();
        let content = TextContent::new("Hello", styled(Color::WHITE));
        let baseline = app.app_mut().measure_text(&content, None);
        for width in [px(-1.0), px(-0.0), px(f32::NAN), px(f32::INFINITY)] {
            let measured = app.app_mut().measure_text(&content, Some(width));
            assert_eq!(
                size_bits(measured),
                size_bits(baseline),
                "invalid max width {width:?} must fall back to max-content"
            );
        }
        assert!(
            app.app().text_diagnostics().iter().any(|diagnostic| matches!(
                diagnostic,
                TextDiagnostic::InvalidMeasureWidth { width } if width.value().is_nan() || width.value() <= 0.0
            )),
            "invalid widths are reported with a typed diagnostic"
        );
    }

    #[test]
    fn canvas_text_does_not_rebuild_on_dpi_change() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let node = mount(
            &mut app,
            text_canvas(&[("Hello", LogicalPoint::new(px(4.0), px(8.0)))]),
        );
        app.drain().expect("first frame");
        let before_canvas = canvas_of(app.app(), window, node);
        let before_lines = resolved_lines(app.app(), window, node).expect("canvas has resolution");
        assert_eq!(before_lines.len(), 1);
        assert_eq!(before_lines[0].scale, DevicePixelRatio::ONE);
        let before_tile = first_tile(&before_lines[0]);

        app.app_mut()
            .window_mut(window)
            .expect("window exists")
            .set_scale(DevicePixelRatio::new(2.0).expect("2x is valid"));
        let record = app.drain().expect("second frame");
        let repainted_tile = record
            .frame
            .as_ref()
            .expect("the DPI change repaints")
            .canvases
            .iter()
            .find(|entry| entry.node == node)
            .and_then(|entry| {
                entry
                    .canvas
                    .primitives()
                    .iter()
                    .find_map(|primitive| match primitive {
                        Primitive::GlyphRun(run) => run.glyphs.first().map(|glyph| glyph.tile),
                        _ => None,
                    })
            })
            .expect("the repainted canvas draws glyphs");

        let after_canvas = canvas_of(app.app(), window, node);
        assert!(
            Arc::ptr_eq(&before_canvas, &after_canvas),
            "a DPI change must not rebuild the logical canvas"
        );
        let after_lines = resolved_lines(app.app(), window, node).expect("canvas has resolution");
        assert_eq!(
            after_lines[0].scale.factor(),
            2.0,
            "the engine re-rasterized the text at the new DPR"
        );
        assert_ne!(
            first_tile(&after_lines[0]),
            before_tile,
            "the new DPR must land on freshly rasterized glyph tiles"
        );
        assert_eq!(
            repainted_tile,
            first_tile(&after_lines[0]),
            "the frame draws the freshly resolved glyph tiles"
        );
        assert!(
            crate::window::canvas_text::line_is_live(&after_lines[0], &app.app().atlas),
            "the painted line is backed by live atlas tiles"
        );
    }

    #[test]
    fn a_matching_recipe_key_does_not_shape_again() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let node = mount(&mut app, text_canvas(&[("Hello", LogicalPoint::ZERO)]));
        app.drain().expect("first frame");
        let shaped = app.app().shape_count_for_test();
        assert!(shaped > 0, "the first frame shapes the recipe");
        let key = app
            .app()
            .window(window)
            .expect("window exists")
            .canvas_text()
            .get(&node)
            .expect("resolution entry")
            .key;

        app.app_mut().text.clear_shape_cache();
        app.app_mut().request_frame();
        let _ = app.app_mut().a11y_take(window);
        app.drain().expect("second frame");
        assert_eq!(
            app.app().shape_count_for_test(),
            shaped,
            "a matching recipe key must not call parley even with an empty shape cache"
        );
        let entry = app
            .app()
            .window(window)
            .expect("window exists")
            .canvas_text()
            .get(&node)
            .expect("resolution entry");
        assert_eq!(entry.key, key, "the same recipe key is retained");
    }

    #[test]
    fn nested_canvases_resolve_their_own_text_in_paint_order() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let mut inner = Canvas::new(bounds());
        assert!(inner.push_text(
            TextContent::new("Inner", styled(Color::WHITE)),
            LogicalPoint::new(px(5.0), px(7.0)),
        ));
        let mut outer = Canvas::new(bounds());
        assert!(outer.push_text(
            TextContent::new("Outer", styled(Color::WHITE)),
            LogicalPoint::new(px(2.0), px(3.0)),
        ));
        outer.push_primitive(Primitive::Canvas(Arc::new(inner)));
        let node = mount(&mut app, outer);
        app.drain().expect("frame commits");

        let lines = resolved_lines(app.app(), window, node).expect("canvas has resolution");
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            vec!["Outer", "Inner"],
            "resolution walks the outer recipe before the nested canvas recipe"
        );
        let rendered = {
            let record = app
                .app()
                .window(window)
                .expect("window exists")
                .record()
                .expect("frame record");
            record.frame.clone().expect("the frame rendered")
        };
        let entries: Vec<_> = rendered
            .canvases
            .iter()
            .filter(|entry| entry.node == node)
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "the nested canvas is painted inside the owner"
        );
        let primitives = entries[0].canvas.primitives();
        assert_eq!(primitives.len(), 2);
        let Primitive::GlyphRun(outer_run) = &primitives[0] else {
            panic!("outer text must become a glyph run");
        };
        let Primitive::Canvas(inner_canvas) = &primitives[1] else {
            panic!("nested canvas must stay a canvas primitive");
        };
        let outer_expected = LogicalPoint::new(px(2.0), px(3.0));
        assert_eq!(
            outer_run.glyphs[0].position,
            LogicalPoint::new(
                outer_expected.x + first_position(&lines[0]).x,
                outer_expected.y + first_position(&lines[0]).y
            ),
            "outer glyphs use the outer recipe origin"
        );
        let inner_run = inner_canvas
            .primitives()
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::GlyphRun(run) => Some(run),
                _ => None,
            })
            .expect("nested text becomes a glyph run");
        let inner_expected = LogicalPoint::new(px(5.0), px(7.0));
        assert_eq!(
            inner_run.glyphs[0].position,
            LogicalPoint::new(
                inner_expected.x + first_position(&lines[1]).x,
                inner_expected.y + first_position(&lines[1]).y
            ),
            "nested glyphs use the nested recipe origin"
        );
        assert!(!inner_run.glyphs.is_empty());
    }

    #[test]
    fn changing_the_recipe_reresolves_the_canvas() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let node = mount(&mut app, text_canvas(&[("One", LogicalPoint::ZERO)]));
        app.drain().expect("first frame");
        assert_eq!(
            resolved_lines(app.app(), window, node).expect("resolution")[0].text,
            "One"
        );

        let mut next = Canvas::new(bounds());
        assert!(next.push_text(
            TextContent::new("Two", styled(Color::WHITE)),
            LogicalPoint::new(px(1.0), px(2.0))
        ));
        app.app_mut()
            .window_mut(window)
            .expect("window exists")
            .set_canvas(node, next)
            .expect("canvas replaced");
        app.drain().expect("second frame");

        let lines = resolved_lines(app.app(), window, node).expect("resolution");
        assert_eq!(lines[0].text, "Two", "new content is shaped again");
        assert_eq!(
            app.app()
                .window(window)
                .expect("window exists")
                .tree()
                .canvas_generation(node),
            Some(1),
            "replacing the canvas bumps its generation"
        );
    }

    #[test]
    fn a_font_stack_version_change_rerasterizes_canvas_text() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let node = mount(&mut app, text_canvas(&[("Hello", LogicalPoint::ZERO)]));
        app.drain().expect("first frame");
        let before =
            resolved_lines(app.app(), window, node).expect("resolution")[0].font_stack_version;
        let shaped_before = app.app().shape_count_for_test();
        let version = {
            let app_mut = app.app_mut();
            let stack = app_mut.text.font_stack().clone();
            app_mut.text.set_font_stack(stack)
        };
        assert_eq!(version, app.app().text.font_stack_version());
        assert_eq!(
            version,
            before.wrapping_add(1),
            "the test bumps the stack version without touching the atlas"
        );
        app.app_mut().request_frame();
        let _ = app.app_mut().a11y_take(window);
        app.drain().expect("second frame");
        let lines = resolved_lines(app.app(), window, node).expect("resolution");
        assert_eq!(
            lines[0].font_stack_version, version,
            "a font stack change must re-resolve the canvas text"
        );
        assert!(
            app.app().shape_count_for_test() > shaped_before,
            "the stale shape cache must be replaced by a fresh shaping call"
        );
    }

    #[test]
    fn canvas_text_followups_are_bounded_and_resettable() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let target = app.app_mut().window_mut(window).expect("window exists");
        assert!(target.note_canvas_text_skip());
        assert!(target.note_canvas_text_skip());
        assert!(target.note_canvas_text_skip());
        assert!(
            !target.note_canvas_text_skip(),
            "stale-UV follow-ups stop after bounded attempts"
        );
        target.reset_canvas_text_skip();
        assert!(
            target.note_canvas_text_skip(),
            "a clean paint restores the budget"
        );
        assert!(!target.note_canvas_text_pending());
        assert!(!target.note_canvas_text_pending());
        assert!(
            target.note_canvas_text_pending(),
            "the third consecutive pending resolution frame escalates"
        );
    }

    #[test]
    fn unloaded_canvas_nodes_are_pruned_from_the_resolution_cache() {
        let mut app = TestApp::new();
        let window = app.window_id();
        for _ in 0..3 {
            let node = mount(&mut app, text_canvas(&[("Hello", LogicalPoint::ZERO)]));
            app.drain().expect("mount frame");
            assert_eq!(
                app.app()
                    .window(window)
                    .expect("window exists")
                    .canvas_text()
                    .len(),
                1
            );
            app.app_mut()
                .remove_node(window, node)
                .expect("canvas node removal");
            app.drain().expect("unmount frame");
            assert!(
                app.app()
                    .window(window)
                    .expect("window exists")
                    .canvas_text()
                    .is_empty(),
                "removed canvas nodes must not keep resolved text alive"
            );
        }
    }

    #[test]
    fn new_glyph_tiles_are_ready_before_the_atlas_upload() {
        let mut app = TestApp::new();
        let node = mount(&mut app, text_canvas(&[("Hello", LogicalPoint::ZERO)]));
        let outcome = app.window().drain().expect("frame");
        assert!(matches!(outcome, FrameOutcome::Committed(_)));
        {
            let rendered = app.rendered().expect("the frame rendered");
            let canvas = rendered
                .canvases
                .iter()
                .find(|entry| entry.node == node)
                .expect("the canvas was painted");
            let has_glyphs = canvas.canvas.primitives().iter().any(
                |primitive| matches!(primitive, Primitive::GlyphRun(run) if !run.glyphs.is_empty()),
            );
            assert!(
                has_glyphs,
                "the frame already draws the freshly rasterized glyphs"
            );
            assert!(
                !canvas
                    .canvas
                    .primitives()
                    .iter()
                    .any(|primitive| matches!(primitive, Primitive::Text(_))),
                "no unresolved recipe reaches the scene"
            );
        }
        assert!(
            app.app().pending_dirty.rects().is_empty(),
            "resolution ran before upload_atlas_pages consumed the dirty rects"
        );
        assert!(matches!(
            app.window().drain().expect("second frame"),
            FrameOutcome::Idle
        ));
    }

    #[test]
    fn canvas_text_paints_the_recipe_color() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let red = Color::Srgb {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            alpha: 1.0,
        };
        let mut canvas = Canvas::new(bounds());
        assert!(canvas.push_text(TextContent::new("A", styled(red)), LogicalPoint::ZERO));
        let node = mount(&mut app, canvas);
        app.drain().expect("frame");
        let record = app
            .app()
            .window(window)
            .expect("window exists")
            .record()
            .expect("frame record");
        let rendered = record.frame.as_ref().expect("the frame rendered");
        let color = rendered
            .canvases
            .iter()
            .find(|entry| entry.node == node)
            .and_then(|entry| {
                entry
                    .canvas
                    .primitives()
                    .iter()
                    .find_map(|primitive| match primitive {
                        Primitive::GlyphRun(run) => Some(run.color),
                        _ => None,
                    })
            })
            .expect("the frame draws a glyph run");
        assert_eq!(color, red, "the glyph run keeps the recipe color");
    }

    #[test]
    fn canvas_text_reports_atlas_pressure_and_retries() {
        let mut app = TestApp::new();
        if !has_system_fonts(&mut app) {
            return;
        }
        let window = app.window_id();
        let mut canvas = Canvas::new(bounds());
        assert!(canvas.push_text(
            TextContent::new("ABCDEFGH", styled_at(48.0)),
            LogicalPoint::new(px(0.0), px(0.0)),
        ));
        let node = mount(&mut app, canvas);
        constrain_glyph_atlas(&mut app);
        let _ = app.app_mut().a11y_take(window);
        app.drain().expect("the first pressured frame commits");
        assert_rendered_tiles_live(&app, node);
        assert!(
            app.app()
                .text_diagnostics()
                .iter()
                .any(|diagnostic| matches!(
                    diagnostic,
                    TextDiagnostic::AtlasFull { skipped } if *skipped > 0
                )),
            "atlas pressure is reported: {:?}",
            app.app().text_diagnostics()
        );

        let mut escalated = false;
        for _ in 0..3 {
            match drain_main(&mut app, window) {
                Ok(_) => assert_rendered_tiles_live(&app, node),
                Err(EngineError::AtlasExhausted { kind }) => {
                    assert_eq!(kind, AtlasKind::Glyph);
                    escalated = true;
                    break;
                }
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert!(
            escalated,
            "three consecutive retry frames must escalate to AtlasExhausted"
        );

        free_glyph_atlas(&mut app);
        drain_main(&mut app, window).expect("a recovered frame commits");
        assert_rendered_tiles_live(&app, node);
        let lines = resolved_lines(app.app(), window, node).expect("canvas has resolution");
        let drawn: usize = lines.iter().map(|line| glyph_count(line)).sum();
        assert!(drawn >= 8, "capacity recovery draws every glyph: {drawn}");
        assert!(
            lines
                .iter()
                .all(|line| crate::window::canvas_text::line_is_live(line, &app.app().atlas)),
            "the recovered frame resolves to live tiles"
        );
    }

    #[test]
    fn canvas_text_retries_are_counted_per_frame_not_per_canvas() {
        let mut app = TestApp::new();
        if !has_system_fonts(&mut app) {
            return;
        }
        let window = app.window_id();
        let mut nodes = Vec::new();
        for text in ["ABCDEFGH", "IJKLMNOP"] {
            let mut canvas = Canvas::new(bounds());
            assert!(canvas.push_text(TextContent::new(text, styled_at(48.0)), LogicalPoint::ZERO));
            nodes.push(mount(&mut app, canvas));
        }
        constrain_glyph_atlas(&mut app);
        let _ = app.app_mut().a11y_take(window);
        app.drain().expect("the first pressured frame commits");
        for node in &nodes {
            assert_rendered_tiles_live(&app, *node);
        }
        assert!(
            drain_main(&mut app, window).is_ok(),
            "two pending canvases in one frame count as one retry frame"
        );
        match drain_main(&mut app, window) {
            Err(EngineError::AtlasExhausted { kind }) => assert_eq!(kind, AtlasKind::Glyph),
            other => panic!("the third retry frame must escalate: {other:?}"),
        }
    }

    #[test]
    fn a_clean_frame_resets_the_canvas_text_retry_budget() {
        let mut app = TestApp::new();
        if !has_system_fonts(&mut app) {
            return;
        }
        let window = app.window_id();
        let mut canvas = Canvas::new(bounds());
        assert!(canvas.push_text(
            TextContent::new("ABCDEFGH", styled_at(48.0)),
            LogicalPoint::ZERO
        ));
        mount(&mut app, canvas);
        constrain_glyph_atlas(&mut app);
        let _ = app.app_mut().a11y_take(window);
        app.drain().expect("first pressured frame");
        free_glyph_atlas(&mut app);
        drain_main(&mut app, window).expect("clean frame after freeing capacity");
        constrain_glyph_atlas(&mut app);
        for _ in 0..2 {
            match drain_main(&mut app, window) {
                Ok(_) => {}
                Err(other) => panic!("the budget must have been reset: {other:?}"),
            }
        }
        match drain_main(&mut app, window) {
            Err(EngineError::AtlasExhausted { kind }) => assert_eq!(kind, AtlasKind::Glyph),
            other => panic!("three fresh retry frames must escalate: {other:?}"),
        }
    }
}

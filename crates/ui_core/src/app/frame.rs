use std::time::Instant;

use super::App;
#[cfg(feature = "hud")]
use crate::debug_hud;
#[cfg(feature = "test-support")]
use crate::test_support::record::FrameStats;
#[cfg(feature = "test-support")]
use crate::test_support::record::LaneOutcome;
use crate::{
    error::EngineError,
    window::{FrameOutcome, FrameTicket, Window, WindowId},
};
pub(crate) fn take_wake_request(app: &mut App) -> bool {
    let requested = app.wake_requested;
    app.wake_requested = false;
    requested
}

pub(crate) fn request_frame(app: &mut App) {
    app.wake_requested = true;
    app.dispatcher.wake();
}

pub(crate) fn frame(
    app: &mut App,
    window: WindowId,
    forced: bool,
) -> Option<Result<FrameOutcome, EngineError>> {
    super::timer::run_due(app);
    super::animation::dispatch_completions(app);
    if !app.windows.contains_key(&window) {
        return None;
    }
    match crate::render::compositor_thread::compositor().supervise() {
        Ok(false) => {}
        Ok(true) => {
            let windows: Vec<WindowId> = app.windows.keys().copied().collect();
            for target in windows {
                if let Some(target) = app.windows.get_mut(&target) {
                    target.force_full_republish();
                }
            }
        }
        Err(error) => {
            app.record_idle(window);
            return Some(Err(error));
        }
    }
    if let Some(error) = crate::render::compositor_thread::compositor().take_error() {
        app.record_idle(window);
        return Some(Err(error));
    }
    app.poll_resources();
    app.events.trim();
    #[cfg(feature = "hud")]
    app.prepare_debug_hud(window);
    app.normalize_scroll_offsets(window);
    app.resolve_overlay_placements(window);
    let wake = app.take_wake_request();
    app.tick_animations(window);
    let needs_frame = app
        .windows
        .get(&window)
        .is_some_and(|candidate| candidate.needs_frame(wake));
    if !needs_frame {
        app.record_idle(window);
        return Some(Ok(FrameOutcome::Idle));
    }
    let now = Instant::now();
    let admits = app
        .windows
        .get(&window)
        .is_none_or(|candidate| candidate.frame_budget_admits(now, forced));
    if !admits {
        if let Some(target) = app.windows.get_mut(&window) {
            target.note_frame_budget_deferred();
        }
        record_budget_deferred(app, window);
        return Some(Ok(FrameOutcome::Idle));
    }
    if let Some(target) = app.windows.get_mut(&window) {
        target.note_frame_start(now);
    }
    let Some(ticket) = app.windows.get_mut(&window).and_then(Window::begin_frame) else {
        return Some(Ok(FrameOutcome::Idle));
    };
    let Some(styles) = app.refresh_styles(window) else {
        app.abort_frame(window, ticket);
        return None;
    };
    #[cfg(feature = "hud")]
    app.apply_debug_hud(window);
    if app
        .windows
        .get(&window)
        .is_some_and(Window::a11y_only_frame)
    {
        crate::text_edit::sync_ime_cursor_area(app, window);
        app.abort_frame(window, ticket);
        #[cfg(feature = "test-support")]
        if let Some(target) = app.windows.get_mut(&window) {
            target.record_a11y_only_frame();
        }
        if app
            .windows
            .get(&window)
            .is_some_and(|target| target.canvas_text_pending_frames() > 0)
        {
            app.request_frame();
        }
        return Some(Ok(FrameOutcome::Idle));
    }
    let text = match app.refresh_text(window) {
        Some(Ok(report)) => report,
        Some(Err(error)) => {
            app.abort_frame(window, ticket);
            return Some(Err(error));
        }
        None => {
            app.abort_frame(window, ticket);
            return None;
        }
    };
    app.repaint_text_if_atlas_changed();
    app.refresh_images(window);
    let layout = match app.windows.get_mut(&window).map(Window::layout_if_dirty) {
        Some(Ok(report)) => report,
        Some(Err(error)) => {
            app.abort_frame(window, ticket);
            return Some(Err(error));
        }
        None => {
            app.abort_frame(window, ticket);
            return None;
        }
    };
    app.prepare_image_variants(window);
    let mut text_layout = match app.layout_text(window) {
        Some(Ok(report)) => report,
        Some(Err(error)) => {
            app.abort_frame(window, ticket);
            return Some(Err(error));
        }
        None => {
            app.abort_frame(window, ticket);
            return None;
        }
    };
    app.rewindow_virtuals(window);
    let clamped = app.clamp_scroll_offsets(window);
    if clamped {
        app.request_frame();
    }
    let mut resolution = match app.resolve_canvas_text(window) {
        Some(Ok(report)) => report,
        Some(Err(error)) => {
            app.abort_frame(window, ticket);
            return Some(Err(error));
        }
        None => {
            app.abort_frame(window, ticket);
            return None;
        }
    };
    let mut resolve_rounds = 0usize;
    while resolution.evicted && resolve_rounds < super::text_pass::MAX_LAYOUT_TEXT_PASSES {
        text_layout = match app.layout_text(window) {
            Some(Ok(report)) => report,
            Some(Err(error)) => {
                app.abort_frame(window, ticket);
                return Some(Err(error));
            }
            None => {
                app.abort_frame(window, ticket);
                return None;
            }
        };
        resolution = match app.resolve_canvas_text(window) {
            Some(Ok(report)) => report,
            Some(Err(error)) => {
                app.abort_frame(window, ticket);
                return Some(Err(error));
            }
            None => {
                app.abort_frame(window, ticket);
                return None;
            }
        };
        resolve_rounds += 1;
    }
    if resolution.evicted || resolution.pending {
        app.request_frame();
    }
    crate::text_edit::sync_ime_cursor_area(app, window);
    app.upload_atlas_pages();
    let painted = {
        let App {
            windows,
            geometry,
            atlas,
            glyph_atlas,
            resources,
            ..
        } = app;
        let color_atlas = glyph_atlas.color_allocator();
        windows.get_mut(&window).map(|target| {
            target.paint_frame(
                geometry,
                Some(&*atlas),
                Some(color_atlas),
                Some(&*resources),
            )
        })
    };
    let mut paint = match painted {
        Some(Ok(report)) => report,
        Some(Err(error)) => {
            app.abort_frame(window, ticket);
            return Some(Err(error));
        }
        None => {
            app.abort_frame(window, ticket);
            return None;
        }
    };
    #[cfg(feature = "test-support")]
    {
        let Some(target) = app.windows.get(&window) else {
            app.abort_frame(window, ticket);
            return None;
        };
        let stats = FrameStats {
            styles,
            text: Some(text),
            text_layout: Some(text_layout),
            layout,
            painted_lanes: paint.rebuilt,
            reused_lanes: paint
                .log
                .lanes
                .iter()
                .filter(|entry| entry.outcome == LaneOutcome::Reused)
                .count(),
            primitives: target.scene().primitive_count(),
            instances: paint.instances,
            encoded_lanes: paint.encoded_lanes,
            reused_segments: paint.reused_segments,
            uploaded_bytes: paint.uploaded_bytes,
            geometry_encoded_chunks: paint.geometry_encoded_chunks,
            geometry_uploaded_bytes: paint.geometry_uploaded_bytes,
            compositor_inbox_latency: None,
            encode_on_compositor: false,
            scroll_offset_frames: 0,
            a11y_only_frames: target.a11y_only_frames(),
        };
        let Some(target) = app.windows.get_mut(&window) else {
            app.abort_frame(window, ticket);
            return None;
        };
        target.set_frame_stats(stats);
    }
    #[cfg(not(feature = "test-support"))]
    {
        let _ = (&styles, &text, &text_layout, &layout);
    }
    let Some(target) = app.windows.get_mut(&window) else {
        app.abort_frame(window, ticket);
        return None;
    };
    let stale_text = core::mem::take(&mut paint.stale_text);
    let unresolved_canvas_text = paint.unresolved_canvas_text;
    if !paint.skipped_inline_images.is_empty() {
        app.text.push_diagnostics(
            paint
                .skipped_inline_images
                .iter()
                .map(|id| crate::text::TextDiagnostic::InlineBoxImageMissing { id: *id })
                .collect(),
        );
    }
    let outcome = target.commit_frame(ticket, paint);
    #[cfg(feature = "hud")]
    if matches!(outcome, FrameOutcome::Committed(_))
        && let Some(state) = app.debug_hud.get_mut(&window)
    {
        debug_hud::finish_frame(state);
    }
    if !stale_text.is_empty() {
        if let Some(target) = app.windows.get_mut(&window) {
            let tree = target.tree_mut();
            for node in stale_text {
                tree.invalidate_text_line(node);
            }
        }
        app.request_frame();
    }
    if unresolved_canvas_text > 0 {
        app.text
            .push_diagnostics(vec![crate::text::TextDiagnostic::CanvasTextUnresolved {
                recipes: unresolved_canvas_text,
            }]);
        let follow_up = app
            .windows
            .get_mut(&window)
            .is_some_and(Window::note_canvas_text_skip);
        if follow_up {
            app.request_frame();
        }
    } else if let Some(target) = app.windows.get_mut(&window) {
        target.reset_canvas_text_skip();
    }
    let glyph_extent = app.atlas.page().0;
    let color_glyph_spec =
        app.color_glyph_page_extent
            .map(|extent| crate::render::AtlasArraySpec {
                page: extent,
                layers: 1,
            });
    let image_pages = app.resources.page_count();
    let image_spec = (image_pages > 0).then(|| crate::render::AtlasArraySpec {
        page: app.resources.page_extent(),
        layers: u32::try_from(image_pages).expect("image atlas page count fits u32"),
    });
    let glyph_spec = crate::render::AtlasArraySpec {
        page: glyph_extent,
        layers: 1,
    };
    let deferred = app.windows.get(&window).is_some_and(|target| {
        use crate::paint::atlas::AtlasKind;
        target.has_renderer()
            && (target.atlas_rects_pending()
                || !target.atlas_page_matches(AtlasKind::Glyph, glyph_spec)
                || color_glyph_spec
                    .is_some_and(|spec| !target.atlas_page_matches(AtlasKind::ColorGlyph, spec))
                || image_spec.is_some_and(|spec| {
                    !target.atlas_page_matches(AtlasKind::Image, spec)
                        || (0..spec.layers as u16).any(|layer| !target.image_layer_received(layer))
                }))
    });
    if deferred {
        let retry = app
            .windows
            .get_mut(&window)
            .is_some_and(Window::note_atlas_deferral);
        if retry {
            if let Some(target) = app.windows.get_mut(&window) {
                target.request_repaint();
            }
            app.dispatcher.wake();
        }
    } else if let Some(target) = app.windows.get_mut(&window) {
        target.reset_atlas_deferrals();
    }
    Some(Ok(outcome))
}

pub(crate) fn abort_frame(app: &mut App, window: WindowId, ticket: FrameTicket) {
    if let Some(target) = app.windows.get_mut(&window) {
        target.abort_frame(ticket);
    }
}

pub(crate) fn record_idle(app: &mut App, window: WindowId) {
    #[cfg(feature = "test-support")]
    if let Some(target) = app.windows.get_mut(&window) {
        target.record_idle();
    }
    #[cfg(not(feature = "test-support"))]
    let _ = (app, window);
}

pub(crate) fn record_budget_deferred(app: &mut App, window: WindowId) {
    #[cfg(feature = "test-support")]
    if let Some(target) = app.windows.get_mut(&window) {
        target.record_budget_deferred();
    }
    #[cfg(not(feature = "test-support"))]
    let _ = (app, window);
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use std::time::Duration;

    use crate::{
        app::App,
        math::{LogicalRect, LogicalSize, px},
        node::builder::NodeBuilder,
        paint::style::{PaintSpec, StylePatch},
        window::{FrameOutcome, WindowOptions},
    };

    fn app_with_bound_node() -> (
        App,
        crate::window::WindowId,
        crate::node::signal::Signal<u8>,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: smol_str::SmolStr::new_static("frame budget"),
            size: LogicalSize::new(px(400.0), px(300.0)),
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("窗口存在").tree().root();
        let value = app.signal(0u8);
        let source = value;
        let _node = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0)))
            .paint(PaintSpec {
                background: crate::paint::color::Color::WHITE,
                ..PaintSpec::default()
            })
            .bind_style(move |cx| StylePatch {
                opacity: Some(f32::from(*cx.read(&source)) / 255.0),
                ..StylePatch::default()
            })
            .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
            .expect("绑定节点");
        (app, window, value)
    }

    fn committed(outcome: Option<Result<FrameOutcome, crate::error::EngineError>>) -> bool {
        matches!(outcome, Some(Ok(FrameOutcome::Committed(_))))
    }

    #[test]
    fn a_second_request_inside_one_refresh_period_is_deferred_then_resumed() {
        let (mut app, window, value) = app_with_bound_node();
        app.window_mut(window)
            .expect("窗口存在")
            .set_frame_budget_test_interval(Duration::from_millis(200));

        assert!(app.write(&value, 255));
        assert!(committed(app.frame(window)), "首帧必须提交");
        let first_index = app.window(window).expect("窗口存在").frame_index();
        assert_eq!(app.frame_budget_deferrals(window), 0);

        assert!(app.write(&value, 128));
        let deferred = app.frame(window);
        assert!(
            matches!(deferred, Some(Ok(FrameOutcome::Idle))),
            "同一 refresh 周期内的第二次请求必须被推迟"
        );
        assert_eq!(
            app.window(window).expect("窗口存在").frame_index(),
            first_index,
            "推迟不递增 FrameIndex（不是提交帧）"
        );
        assert_eq!(app.frame_budget_deferrals(window), 1, "推迟计数必须可观测");
        let record = app
            .window(window)
            .expect("窗口存在")
            .record()
            .expect("推迟也必须有帧记录")
            .clone();
        assert_eq!(record.skipped_by_budget, 1);
        assert!(!record.rendered, "推迟记录不是提交帧");

        #[cfg(any(windows, target_os = "macos"))]
        {
            let forced = app.frame_forced(window);
            assert!(committed(forced), "force 通道必须绕过帧预算门禁");
            assert!(
                app.window(window).expect("窗口存在").frame_index() > first_index,
                "force 帧必须真正提交"
            );
        }
        assert!(app.write(&value, 64));
        assert!(matches!(app.frame(window), Some(Ok(FrameOutcome::Idle))));

        std::thread::sleep(Duration::from_millis(220));
        assert!(
            committed(app.frame(window)),
            "下一拍必须把被推迟的状态补上（不是丢帧）"
        );

        let before = app.frame_budget_deferrals(window);
        assert!(matches!(app.frame(window), Some(Ok(FrameOutcome::Idle))));
        assert_eq!(
            app.frame_budget_deferrals(window),
            before,
            "空闲帧不是预算推迟，不得虚增计数"
        );
    }

    #[test]
    fn an_unknown_refresh_rate_leaves_the_frame_path_ungated() {
        let (mut app, window, value) = app_with_bound_node();
        assert_eq!(app.frame_refresh_hz(window), None);
        assert!(app.write(&value, 255));
        assert!(committed(app.frame(window)));
        assert!(app.write(&value, 128));
        assert!(
            committed(app.frame(window)),
            "刷新率未知时不得卡帧（退回唤醒即出帧）"
        );
        assert_eq!(app.frame_budget_deferrals(window), 0);
    }
}

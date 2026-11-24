use smallvec::SmallVec;

use super::Window;
#[cfg(feature = "test-support")]
use crate::paint::instance::InstanceBuffer;
#[cfg(feature = "test-support")]
use crate::test_support::record::{DeltaRecord, PaintLog};
use crate::{
    error::EngineError,
    math::{LogicalRect, physical_px},
    node::id::NodeId,
    paint::{
        pass::{PaintPass, PaintReport},
        primitive::Primitive,
        scene::Scene,
    },
    render::{
        compose::ComposeStats,
        delta::{FrameHeader, SceneDelta},
    },
};

pub(crate) fn paint_frame(
    window: &mut Window,
    geometry: &crate::paint::geometry_store::GeometryStore,
    atlas: Option<&crate::render::AtlasAllocator>,
    resources: Option<&crate::resources::ResourceCache>,
) -> Result<PaintReport, EngineError> {
    window.tree.reset_paint_visited();
    let mut removed: Vec<crate::paint::scene::LaneId> = Vec::new();
    for lane in window.tree.drain_removed_lanes() {
        window.scene.release_lane(lane);
        removed.push(lane);
    }
    let scale = window.scale;
    let frame = window.frame_rect();
    window.damage.reset(scale);
    if window.tree.take_geometry_damage_escalation() {
        window.full_damage_pending = true;
    }
    let has_damage = window.full_damage_pending || window.pending_damage.is_some();
    let scroll_damage_pending = window.pending_damage.is_some();
    let full_damage = window.full_damage_pending;
    if window.full_damage_pending {
        window.damage.push_full();
        window.full_damage_pending = false;
    }
    if let Some(pending) = window.pending_damage.take() {
        window.damage.push_logical(pending, physical_px(1.0), None);
    }
    if !has_damage && !window.needs_node_repaint() && removed.is_empty() {
        return Ok(PaintReport {
            rebuilt: 0,
            damage: crate::render::damage::DamageSummary::default(),
            stale_text: smallvec::SmallVec::new(),
            unresolved_canvas_text: 0,
            skipped_inline_images: smallvec::SmallVec::new(),
            #[cfg(feature = "test-support")]
            instances: 0,
            #[cfg(feature = "test-support")]
            encoded_lanes: 0,
            #[cfg(feature = "test-support")]
            reused_segments: 0,
            #[cfg(feature = "test-support")]
            uploaded_bytes: 0,
            #[cfg(feature = "test-support")]
            geometry_encoded_chunks: 0,
            #[cfg(feature = "test-support")]
            geometry_uploaded_bytes: 0,
            #[cfg(feature = "test-support")]
            scroll_slots: 0,
            #[cfg(feature = "test-support")]
            log: crate::test_support::record::PaintLog::default(),
            #[cfg(feature = "test-support")]
            delta: DeltaRecord::default(),
        });
    }
    let epoch_changed = window.tree.paint_epoch_changed();
    let report = if !full_damage
        && removed.is_empty()
        && !epoch_changed
        && !window.needs_node_repaint()
        && window.scroll_slots.len() > 1
    {
        refresh_scroll_offsets(window)
    } else {
        None
    };
    let report = match report {
        Some(damage) => {
            window.tree.note_paint_frame(false, 0);
            PaintReport {
                rebuilt: 0,
                damage,
                stale_text: smallvec::SmallVec::new(),
                unresolved_canvas_text: 0,
                skipped_inline_images: smallvec::SmallVec::new(),
                #[cfg(feature = "test-support")]
                instances: 0,
                #[cfg(feature = "test-support")]
                encoded_lanes: 0,
                #[cfg(feature = "test-support")]
                reused_segments: 0,
                #[cfg(feature = "test-support")]
                uploaded_bytes: 0,
                #[cfg(feature = "test-support")]
                geometry_encoded_chunks: 0,
                #[cfg(feature = "test-support")]
                geometry_uploaded_bytes: 0,
                #[cfg(feature = "test-support")]
                scroll_slots: 0,
                #[cfg(feature = "test-support")]
                log: crate::test_support::record::PaintLog::default(),
                #[cfg(feature = "test-support")]
                delta: DeltaRecord::default(),
            }
        }
        None => {
            let dirty = window.tree.take_paint_work();
            let mut full = full_damage || !removed.is_empty() || epoch_changed;
            if !full {
                for &node in &dirty {
                    if window.tree.get(node).is_err() {
                        continue;
                    }
                    let Some(lane) = window.tree.lane_of(node) else {
                        full = true;
                        break;
                    };
                    if window
                        .scene
                        .lane_primitives(lane)
                        .is_none_or(<[Primitive]>::is_empty)
                    {
                        full = true;
                        break;
                    }
                    if window.tree.clip_key(node).is_none() {
                        full = true;
                        break;
                    }
                }
            }
            if !full && scroll_damage_pending && !sync_scroll_offsets(window) {
                full = true;
            }
            if !full {
                window.tree.mark_paint_closure(&dirty);
            }
            window.tree.recycle(dirty);
            let focused = window.focus.focused();
            let canvas_text = &window.canvas_text;
            let mut pass = PaintPass {
                tree: &mut window.tree,
                geometry,
                scene: &mut window.scene,
                damage: &mut window.damage,
                scale,
                frame,
                order: &mut window.paint_order,
                focused,
                atlas,
                resources,
                canvas_text,
                scroll_slots: &mut window.scroll_slots,
                scroll_slot_nodes: &mut window.scroll_slot_nodes,
                #[cfg(feature = "test-support")]
                log: PaintLog::default(),
            };
            if full {
                pass.run()?
            } else {
                pass.run_incremental()?
            }
        }
    };
    let mut report = report;
    let surface = window.surface;
    let mut delta = SceneDelta::empty(FrameHeader {
        window: window.id(),
        index: crate::window::FrameIndex::default(),
        plan: crate::render::compositor::Plan::Idle,
        surface,
        scale,
        #[cfg(target_os = "linux")]
        surface_local: window.size(),
        #[cfg(target_os = "linux")]
        buffer_scale: window.buffer_scale,
        published_at: std::time::Instant::now(),
    });
    let full = window.delta_builder.full_for(scale);
    {
        let Window {
            scene,
            delta_builder,
            scroll_slots,
            ..
        } = window;
        delta_builder.collect(scene, geometry, scroll_slots, full, &mut delta);
    }
    for lane in removed {
        window.delta_builder.note_removed(lane, &mut delta);
    }
    if let Some(previous) = window.pending_delta.take() {
        crate::render::delta::absorb(previous, &mut delta);
    }
    delta.image_layers = window.image_drawable_layers;
    let scroll_pending = !delta.scroll.is_empty();
    #[cfg(feature = "test-support")]
    {
        report.delta = DeltaRecord {
            full: delta.full,
            order_some: delta.order.is_some(),
            lanes: delta.lanes.len(),
            removed: delta.removed.len(),
            scroll: delta.scroll.len(),
        };
    }
    if let super::frame::PublishDecision::Publish(_) =
        super::frame::publish_decision(window, &report, scroll_pending)
        && let Some(composed) = compose_inline(window, &delta)?
    {
        report = annotate_report(report, composed.stats);
        window.pending_payload = Some(composed);
    }
    window.pending_delta = Some(delta);
    #[cfg(feature = "hud")]
    {
        window.frame_cpu = window.frame_started_at.map(|started| started.elapsed());
    }
    Ok(report)
}

#[cfg(feature = "test-support")]
fn annotate_report(mut report: PaintReport, stats: ComposeStats) -> PaintReport {
    report.instances = stats.drawn_instances as usize;
    report.encoded_lanes = stats.encoded_lanes;
    report.reused_segments = stats.reused_segments;
    report.uploaded_bytes = stats.written_bytes;
    report.geometry_encoded_chunks = stats.geometry_encoded_chunks;
    report.geometry_uploaded_bytes = stats.geometry_uploaded_bytes;
    report.scroll_slots = stats.scroll_slots;
    report
}

#[cfg(not(feature = "test-support"))]
fn annotate_report(report: PaintReport, _stats: ComposeStats) -> PaintReport {
    report
}

fn refresh_scroll_offsets(window: &mut Window) -> Option<crate::render::damage::DamageSummary> {
    if !sync_scroll_offsets(window) {
        return None;
    }
    let frame = window.frame_rect();
    Some(window.damage.finish(frame))
}

fn sync_scroll_offsets(window: &mut Window) -> bool {
    debug_assert_eq!(
        window.scroll_slots.len(),
        window.scroll_slot_nodes.len() + 1,
        "槽位表与节点表必须平行（哨兵之外一一对应）"
    );
    let mut changed: SmallVec<[NodeId; 4]> = SmallVec::new();
    for (index, node) in window.scroll_slot_nodes.iter().copied().enumerate() {
        let Some(state) = window.tree.scroll_slot(node) else {
            return false;
        };
        let Some(slot) = window.scroll_slots.get(index + 1) else {
            return false;
        };
        if slot.0 != state.offset {
            changed.push(node);
        }
    }
    if changed
        .iter()
        .any(|&container| has_nested_scroll_slot(window, container))
    {
        return false;
    }
    for (index, node) in window.scroll_slot_nodes.iter().copied().enumerate() {
        let Some(state) = window.tree.scroll_slot(node) else {
            return false;
        };
        if let Some(slot) = window.scroll_slots.get_mut(index + 1) {
            slot.0 = state.offset;
        }
    }
    true
}

fn has_nested_scroll_slot(window: &Window, container: NodeId) -> bool {
    window
        .scroll_slot_nodes
        .iter()
        .copied()
        .any(|other| other != container && window.tree.is_ancestor(container, other))
}

fn compose_inline(
    window: &mut Window,
    delta: &SceneDelta,
) -> Result<Option<crate::render::compose::ComposedFrame>, EngineError> {
    let resolver = window.resolver;
    let Some(composer) = window.composer.as_mut() else {
        return Ok(None);
    };
    composer.compose(delta, resolver).map(Some)
}

pub(crate) fn needs_node_repaint(window: &Window) -> bool {
    window.tree.has_paint_work()
}

#[cfg(feature = "test-support")]
pub(crate) fn instance_buffer(window: &Window) -> Option<&InstanceBuffer> {
    window
        .composer
        .as_ref()
        .map(crate::render::compose::WindowComposer::buffer)
}

pub(crate) fn scene(window: &Window) -> &Scene {
    &window.scene
}

#[cfg(test)]
pub(crate) fn encoded_geometry(window: &Window) -> &[crate::paint::instance::GeometryDrawGpu] {
    window
        .composer
        .as_ref()
        .map_or(&[], |composer| composer.buffer().geometry())
}

pub(crate) fn request_damage(window: &mut Window, rect: LogicalRect) {
    window.pending_damage = Some(match window.pending_damage {
        Some(existing) => existing.union(rect),
        None => rect,
    });
}

pub(crate) fn request_repaint(window: &mut Window) {
    window.full_damage_pending = true;
}

#[cfg(test)]
mod tests {
    use crate::{
        app::App,
        math::{LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId},
        paint::{color::Color, style::PaintSpec},
        window::{FrameOutcome, WindowId, WindowOptions},
    };

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    }

    fn headless_window(app: &mut App) -> WindowId {
        app.create_window(WindowOptions {
            title: smol_str::SmolStr::new_static("paint-prune"),
            size: LogicalSize::new(px(1280.0), px(800.0)),
            ..WindowOptions::default()
        })
    }

    fn frame_once(app: &mut App, window: WindowId) -> FrameOutcome {
        app.frame(window).expect("窗口存在").expect("帧成功")
    }

    fn counters(app: &App, window: WindowId) -> crate::node::tree::PaintCounters {
        app.window(window)
            .expect("窗口存在")
            .tree()
            .paint_counters()
    }

    #[test]
    fn a_40k_tree_incremental_frame_visits_a_handful_of_nodes() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let window = headless_window(&mut app);
        let root = app.window(window).expect("窗口").tree().root();
        const LEAVES: usize = 40_000;
        let mut changed: Vec<NodeId> = Vec::with_capacity(5);
        {
            let tree = app.window_mut(window).expect("窗口").tree_mut();
            for index in 0..LEAVES {
                let node = NodeBuilder::boxed()
                    .layout_override(LogicalRect::new(
                        px((index % 40) as f32 * 32.0),
                        px((index / 40) as f32 * 8.0),
                        px(24.0),
                        px(6.0),
                    ))
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .build(tree, root)
                    .expect("叶子节点");
                if index % (LEAVES / 5) == 0 {
                    changed.push(node);
                }
            }
        }
        let node_count = app.window(window).expect("窗口").tree().len();
        assert_eq!(node_count, LEAVES + 1, "树规模 = 根 + 叶子");

        let first = frame_once(&mut app, window);
        assert!(matches!(first, FrameOutcome::Committed(_)), "首帧必须提交");
        let counters_before = counters(&app, window);
        assert!(
            counters_before.visited_nodes > LEAVES,
            "首帧是全量遍历：{}",
            counters_before.visited_nodes
        );

        for node in &changed {
            assert!(
                app.request_node_repaint(window, *node).expect("节点级重绘"),
                "可见节点必须接受重绘请求"
            );
        }
        let outcome = frame_once(&mut app, window);
        assert!(
            matches!(outcome, FrameOutcome::Committed(_)),
            "增量帧必须提交"
        );
        let counters = counters(&app, window);
        assert!(
            counters.visited_nodes <= 256,
            "改 5 个节点的访问数必须 ≤ 256（树共 {node_count} 节点）：{}",
            counters.visited_nodes
        );
        assert!(
            counters.visited_nodes * 100 < node_count,
            "访问数必须与树大小无关：{} vs {node_count}",
            counters.visited_nodes
        );
        assert_eq!(
            counters.full_frames, counters_before.full_frames,
            "没有结构 / lane 成员变化 ⇒ 不得退化为全量路径"
        );
        assert_eq!(
            counters.incremental_frames,
            counters_before.incremental_frames + 1,
            "本帧必须走增量路径"
        );
    }

    #[cfg(feature = "test-support")]
    mod differential {
        use super::*;
        use crate::{
            interaction::ScrollConfig,
            layout::LayoutOverride,
            math::{DeviceRect, LogicalPoint, LogicalPx, LogicalRect, LogicalSize},
            node::{builder::NodeBuilder, id::NodeId, signal::Signal},
            paint::{
                color::Color,
                primitive::Primitive,
                style::{Clip, PaintSpec, Style, StylePatch},
                style_builder::StyleBuilder,
            },
            test_support::record::DeltaRecord,
            window::{FrameOutcome, WindowId},
        };

        #[derive(Clone, Copy)]
        struct SharedSignals {
            color: Signal<Color>,
            opacity: Signal<f32>,
            visible: Signal<bool>,
            text: Signal<u32>,
        }

        struct Fixture {
            window: WindowId,
            grow: NodeId,
            movable: NodeId,
            scroll: crate::interaction::ScrollHandle,
            appended: Vec<NodeId>,
        }

        fn absolute_style(
            left: LogicalPx,
            top: LogicalPx,
            width: LogicalPx,
            height: LogicalPx,
        ) -> Style {
            StyleBuilder::new()
                .absolute()
                .left(left)
                .top(top)
                .size(width, height)
                .build()
        }

        fn tile(rect: LogicalRect, color: Color) -> NodeBuilder {
            NodeBuilder::boxed().layout_override(rect).paint(PaintSpec {
                background: color,
                ..PaintSpec::default()
            })
        }

        fn build_fixture(app: &mut App, signals: &SharedSignals) -> Fixture {
            let window = headless_window(app);
            let (grow, movable, scroll_container) = {
                let tree = app.window_mut(window).expect("窗口").tree_mut();
                let root = tree.root();
                let panel = NodeBuilder::boxed()
                    .style(absolute_style(px(0.0), px(0.0), px(400.0), px(240.0)))
                    .build(tree, root)
                    .expect("panel");
                for row in 0..6u32 {
                    for column in 0..6u32 {
                        let _ = tile(
                            LogicalRect::new(
                                px(column as f32 * 40.0),
                                px(row as f32 * 30.0),
                                px(36.0),
                                px(26.0),
                            ),
                            Color::WHITE,
                        )
                        .build(tree, panel)
                        .expect("static tile");
                    }
                }
                let source = signals.color;
                for index in 0..3u32 {
                    let _ = NodeBuilder::boxed()
                        .layout_override(LogicalRect::new(
                            px(index as f32 * 60.0 + 420.0),
                            px(0.0),
                            px(50.0),
                            px(40.0),
                        ))
                        .paint(PaintSpec {
                            background: Color::WHITE,
                            ..PaintSpec::default()
                        })
                        .bind_style(move |cx| StylePatch {
                            background: Some(*cx.read(&source)),
                            ..StylePatch::default()
                        })
                        .build(tree, root)
                        .expect("colored");
                }
                let opacity = signals.opacity;
                let _ = NodeBuilder::boxed()
                    .layout_override(LogicalRect::new(px(700.0), px(0.0), px(80.0), px(40.0)))
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .bind_style(move |cx| StylePatch {
                        opacity: Some(*cx.read(&opacity)),
                        ..StylePatch::default()
                    })
                    .build(tree, root)
                    .expect("faded");
                let source = signals.text;
                for index in 0..3u32 {
                    let _ = NodeBuilder::text(
                        smol_str::SmolStr::new_static("0"),
                        crate::text::TextStyle::new(px(12.0)),
                    )
                    .layout_override(LogicalRect::new(
                        px(index as f32 * 80.0),
                        px(60.0),
                        px(72.0),
                        px(18.0),
                    ))
                    .bind_text(move |cx| smol_str::SmolStr::new(format!("v{}", *cx.read(&source))))
                    .build(tree, root)
                    .expect("label");
                }
                let visible = signals.visible;
                let _hider = NodeBuilder::boxed()
                    .layout_override(LogicalRect::new(px(0.0), px(300.0), px(120.0), px(60.0)))
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .bind_style(move |cx| StylePatch {
                        visible: Some(*cx.read(&visible)),
                        ..StylePatch::default()
                    })
                    .build(tree, root)
                    .expect("hider");
                let grow = NodeBuilder::boxed()
                    .style(absolute_style(px(140.0), px(300.0), px(400.0), px(200.0)))
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .build(tree, root)
                    .expect("grow");
                let movable = tile(
                    LogicalRect::new(px(600.0), px(300.0), px(80.0), px(40.0)),
                    Color::WHITE,
                )
                .build(tree, root)
                .expect("movable");
                let mut scroll_style = absolute_style(px(0.0), px(400.0), px(300.0), px(120.0));
                scroll_style.clip = Clip::Bounds;
                let scroll_container = NodeBuilder::boxed()
                    .style(scroll_style)
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .build(tree, root)
                    .expect("scroll container");
                for index in 0..12u32 {
                    let _ = tile(
                        LogicalRect::new(px(0.0), px(index as f32 * 20.0), px(280.0), px(18.0)),
                        Color::WHITE,
                    )
                    .build(tree, scroll_container)
                    .expect("scroll child");
                }
                (grow, movable, scroll_container)
            };
            let scroll = app
                .make_scroll(window, scroll_container, ScrollConfig::default())
                .expect("滚动容器");
            app.set_scroll_content(&scroll, LogicalSize::new(px(280.0), px(12.0 * 20.0)))
                .expect("滚动内容");
            Fixture {
                window,
                grow,
                movable,
                scroll,
                appended: Vec::new(),
            }
        }

        #[derive(Debug, PartialEq)]
        struct View {
            lanes: Vec<(usize, Vec<Primitive>, u32)>,
            drawn: Vec<usize>,
            scroll: Vec<(LogicalPoint, DeviceRect)>,
            damage: Vec<DeviceRect>,
            damage_full: bool,
            delta: DeltaRecord,
        }

        fn dfs_indices(tree: &crate::node::tree::NodeTree) -> ahash::HashMap<NodeId, usize> {
            let mut indices: ahash::HashMap<NodeId, usize> = ahash::HashMap::default();
            let mut stack: Vec<NodeId> = vec![tree.root()];
            let mut next = 0usize;
            while let Some(node) = stack.pop() {
                indices.insert(node, next);
                next += 1;
                if let Ok(target) = tree.get(node) {
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            indices
        }

        fn view(app: &App, window: WindowId) -> View {
            let window = app.window(window).expect("窗口存在");
            let tree = window.tree();
            let scene = window.scene();
            let indices = dfs_indices(tree);
            let mut lane_nodes: ahash::HashMap<crate::paint::scene::LaneId, usize> =
                ahash::HashMap::default();
            let mut lanes = Vec::new();
            let mut stack = vec![tree.root()];
            while let Some(node) = stack.pop() {
                if let Ok(target) = tree.get(node) {
                    if let Some(lane) = target.lane() {
                        let index = indices[&node];
                        lane_nodes.insert(lane, index);
                        let primitives = scene
                            .lane_primitives(lane)
                            .map_or_else(Vec::new, <[Primitive]>::to_vec);
                        lanes.push((index, primitives, scene.lane_scroll(lane)));
                    }
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            lanes.sort_by_key(|entry| entry.0);
            let drawn: Vec<usize> = scene
                .order()
                .iter()
                .filter(|lane| {
                    scene
                        .lane_primitives(**lane)
                        .is_some_and(|primitives| !primitives.is_empty())
                })
                .filter_map(|lane| lane_nodes.get(lane).copied())
                .collect();
            let record = window.record().expect("提交帧必须有记录");
            View {
                lanes,
                drawn,
                scroll: window.scroll_slots().to_vec(),
                damage: record.damage.clone(),
                damage_full: record.damage_full,
                delta: record.delta,
            }
        }

        fn force_full(app: &mut App, window: WindowId) {
            app.window_mut(window)
                .expect("窗口存在")
                .tree_mut()
                .force_full_paint();
        }

        fn compare(app: &App, windows: [WindowId; 2], step: u32, label: &str) {
            let view_a = view(app, windows[0]);
            let view_b = view(app, windows[1]);
            assert_eq!(
                view_a, view_b,
                "step {step} ({label}): 增量帧与全量帧产物不一致"
            );
        }

        #[test]
        fn incremental_matches_forced_full_over_a_scripted_sequence() {
            let runtime = runtime();
            let mut app = App::new(runtime.handle().clone());
            let signals = SharedSignals {
                color: app.signal(Color::WHITE),
                opacity: app.signal(1.0f32),
                visible: app.signal(true),
                text: app.signal(0u32),
            };
            let a = build_fixture(&mut app, &signals);
            let b = build_fixture(&mut app, &signals);
            let windows = [a.window, b.window];
            let mut fixtures = [a, b];

            for window in windows {
                let outcome = frame_once(&mut app, window);
                assert!(matches!(outcome, FrameOutcome::Committed(_)));
            }
            compare(&app, windows, 0, "首帧");

            let mut seed = 0x5EED_1234_ABCD_9876u64;
            for step in 1..=30u32 {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let op = ((seed >> 33) as u32) % 9;
                match op {
                    0 => {
                        let channel = (step % 3) as u8;
                        app.write(
                            &signals.color,
                            Color::from_rgba8([0x20 + step as u8, 0x40 + channel * 40, 0x80, 0xFF]),
                        );
                    }
                    1 => {
                        app.write(&signals.opacity, 0.05 + 0.9 * ((step as f32 * 0.37) % 1.0));
                    }
                    2 => {
                        app.write(&signals.text, step);
                    }
                    3 => {
                        let offset =
                            LogicalPoint::new(px(0.0), px(((step as f32 * 13.0) % 120.0).max(1.0)));
                        for fixture in &fixtures {
                            app.scroll_to(&fixture.scroll, offset).expect("滚动");
                        }
                    }
                    4 => {
                        let index = (step as usize) % 36;
                        for fixture in &fixtures {
                            let root = app.window(fixture.window).expect("窗口").tree().root();
                            let panel = app
                                .window(fixture.window)
                                .expect("窗口")
                                .tree()
                                .get(root)
                                .expect("根")
                                .children()[0];
                            let target = app
                                .window(fixture.window)
                                .expect("窗口")
                                .tree()
                                .get(panel)
                                .expect("panel")
                                .children()[index];
                            let _ = app.request_node_repaint(fixture.window, target);
                        }
                    }
                    5 => {
                        let rect = LogicalRect::new(
                            px(600.0 + (step as f32) * 3.0),
                            px(300.0 + (step % 2) as f32 * 5.0),
                            px(80.0),
                            px(40.0),
                        );
                        for fixture in &fixtures {
                            app.window_mut(fixture.window)
                                .expect("窗口")
                                .tree_mut()
                                .set_layout_override(
                                    fixture.movable,
                                    Some(LayoutOverride::new(rect)),
                                )
                                .expect("移动节点");
                        }
                    }
                    6 => {
                        for fixture in &mut fixtures {
                            let tree = app.window_mut(fixture.window).expect("窗口").tree_mut();
                            let node = tile(
                                LogicalRect::new(
                                    px(160.0 + (fixture.appended.len() as f32) * 30.0),
                                    px(420.0),
                                    px(24.0),
                                    px(24.0),
                                ),
                                Color::WHITE,
                            )
                            .build(tree, fixture.grow)
                            .expect("追加节点");
                            fixture.appended.push(node);
                        }
                    }
                    7 => {
                        let mut removed = false;
                        for fixture in &mut fixtures {
                            if let Some(node) = fixture.appended.pop() {
                                let _ = app
                                    .window_mut(fixture.window)
                                    .expect("窗口")
                                    .tree_mut()
                                    .remove(node);
                                removed = true;
                            }
                        }
                        if !removed {
                            app.write(&signals.text, step);
                        }
                    }
                    _ => {}
                }
                let blink = op == 8;
                if blink {
                    app.write(&signals.visible, false);
                }
                let outcome = frame_once(&mut app, windows[0]);
                assert!(matches!(outcome, FrameOutcome::Committed(_)), "A 必须提交");
                force_full(&mut app, windows[1]);
                let outcome = frame_once(&mut app, windows[1]);
                assert!(matches!(outcome, FrameOutcome::Committed(_)), "B 必须提交");
                compare(&app, windows, step * 2, "op");
                if blink {
                    app.write(&signals.visible, true);
                    let outcome = frame_once(&mut app, windows[0]);
                    assert!(matches!(outcome, FrameOutcome::Committed(_)), "A 重新显示");
                    force_full(&mut app, windows[1]);
                    let outcome = frame_once(&mut app, windows[1]);
                    assert!(matches!(outcome, FrameOutcome::Committed(_)), "B 重新显示");
                    compare(&app, windows, step * 2 + 1, "show");
                }
            }
        }
    }
}

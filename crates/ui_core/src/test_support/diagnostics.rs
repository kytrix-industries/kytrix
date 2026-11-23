use std::fmt;

use smol_str::SmolStr;

use crate::{
    a11y::{A11yAction, A11yNodeId, A11yRole, A11yTreeSnapshot},
    app::App,
    math::{PhysicalRect, Rect, Scale},
    node::{id::NodeId, signal::SignalId},
    paint::{
        canvas::Canvas,
        color::Color,
        instance::{FrameOp, GpuPrimitive},
        primitive::{Quad, QuadPaint},
    },
    test_support::{app::TestApp, record::FrameRecord},
    window::WindowId,
};

#[derive(Clone, Debug, PartialEq)]
pub struct FrameDump {
    pub frame_index: u32,
    pub viewport: PhysicalRect,
    pub scale: f32,
    pub damage: Vec<PhysicalRect>,
    pub instances: Vec<InstanceDump>,
    pub ops: Vec<OpDump>,
    pub clips: Vec<ClipDump>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InstanceDump {
    pub kind: u32,
    pub atlas_page: u32,
    pub bounds: [f32; 4],
    pub corner_radius: [f32; 4],
    pub color0: [f32; 4],
    pub color1: [f32; 4],
    pub params0: [f32; 4],
    pub clip_index: u32,
    pub uv: [f32; 4],
    pub clip_rect: [f32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpDump {
    pub instances: Option<(u32, u32)>,
    pub geometry: Option<u32>,
    pub clip_index: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipDump {
    pub rect: [f32; 4],
    pub corner_radius: [f32; 4],
    pub kind: u32,
    pub parent: Option<u32>,
}

impl fmt::Display for FrameDump {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "frame {} viewport {:.0},{:.0},{:.0},{:.0} scale {:.4}",
            self.frame_index,
            self.viewport.x0,
            self.viewport.y0,
            self.viewport.x1,
            self.viewport.y1,
            self.scale
        )?;
        writeln!(f, "damage {}", self.damage.len())?;
        for rect in &self.damage {
            writeln!(f, "  rect {},{},{},{}", rect.x0, rect.y0, rect.x1, rect.y1)?;
        }
        writeln!(f, "instances {}", self.instances.len())?;
        for (index, instance) in self.instances.iter().enumerate() {
            writeln!(
                f,
                "  inst {index} kind={} page={} bounds={:.4},{:.4},{:.4},{:.4} clip={} uv={:.4},{:.4},{:.4},{:.4}",
                instance.kind,
                instance.atlas_page,
                instance.bounds[0],
                instance.bounds[1],
                instance.bounds[2],
                instance.bounds[3],
                instance.clip_index,
                instance.uv[0],
                instance.uv[1],
                instance.uv[2],
                instance.uv[3]
            )?;
        }
        writeln!(f, "ops {}", self.ops.len())?;
        for op in &self.ops {
            if let Some((start, len)) = op.instances {
                writeln!(f, "  op instances {start}..{}", start + len)?;
            } else if let Some(index) = op.geometry {
                writeln!(f, "  op geometry {index} clip={}", op.clip_index)?;
            }
        }
        writeln!(f, "clips {}", self.clips.len())?;
        for (index, clip) in self.clips.iter().enumerate() {
            writeln!(
                f,
                "  clip {index} rect={:.4},{:.4},{:.4},{:.4} r={:.4},{:.4},{:.4},{:.4} kind={} parent={}",
                clip.rect[0],
                clip.rect[1],
                clip.rect[2],
                clip.rect[3],
                clip.corner_radius[0],
                clip.corner_radius[1],
                clip.corner_radius[2],
                clip.corner_radius[3],
                clip.kind,
                clip.parent
                    .map_or_else(|| "none".to_string(), |parent| parent.to_string())
            )?;
        }
        Ok(())
    }
}

#[must_use]
pub fn frame_dump(app: &TestApp) -> FrameDump {
    let window = app
        .app()
        .window(app.window_id())
        .expect("TestApp's window must exist");
    let Some(buffer) = window.instance_buffer() else {
        panic!("frame_dump requires an inline (headless) window");
    };
    let instances: Vec<InstanceDump> = buffer
        .live_instances()
        .flatten()
        .map(instance_dump)
        .collect();
    let record = window.record();
    FrameDump {
        frame_index: record.map_or(0, |record| record.frame_index.get()),
        viewport: Rect::new(0.0, 0.0, window.size().width, window.size().height)
            .to_physical_covering(window.scale()),
        scale: window.scale().factor(),
        damage: record.map_or_else(Vec::new, |record| record.damage.clone()),
        instances,
        ops: buffer.ops().iter().map(op_dump).collect(),
        clips: buffer.clips().iter().map(clip_dump).collect(),
    }
}

fn instance_dump(instance: &GpuPrimitive) -> InstanceDump {
    InstanceDump {
        kind: quantize(instance.params1[2]),
        atlas_page: quantize(instance.params1[1]),
        bounds: instance.bounds,
        corner_radius: instance.corner_radius,
        color0: instance.color0,
        color1: instance.color1,
        params0: instance.params0,
        clip_index: quantize(instance.params1[0]),
        uv: instance.uv,
        clip_rect: instance.clip_rect,
    }
}

fn op_dump(op: &FrameOp) -> OpDump {
    match op {
        FrameOp::Instances { start, len, .. } => OpDump {
            instances: Some((*start, *len)),
            geometry: None,
            clip_index: 0,
        },
        FrameOp::Geometry {
            index, clip_index, ..
        } => OpDump {
            instances: None,
            geometry: Some(*index),
            clip_index: *clip_index,
        },
    }
}

fn clip_dump(chain: &crate::paint::instance::ClipChain) -> ClipDump {
    let entry = chain.gpu_entry();
    let parent = entry.meta[0];
    ClipDump {
        rect: entry.rect,
        corner_radius: entry.corner_radius,
        kind: entry.meta[1],
        parent: (parent != 0).then_some(parent),
    }
}

fn quantize(value: f32) -> u32 {
    if value.is_finite() && value > 0.0 {
        value as u32
    } else {
        0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignalSlotStats {
    pub slots: usize,
    pub live: usize,
    pub free: usize,
}

#[must_use]
pub fn signal_slot_stats(app: &App) -> SignalSlotStats {
    let (slots, live, free) = app.signal_slot_counts_for_test();
    SignalSlotStats { slots, live, free }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DependencyGraphDump {
    pub signals: Vec<SignalDump>,
    pub edges: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SignalDump {
    pub signal: SignalId,
    pub owners: Vec<DependencyOwnerDump>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyOwnerDump {
    Node { window: WindowId, node: NodeId },
    Derived(crate::node::signal::DerivedId),
}

#[must_use]
pub fn dependency_dump(app: &App) -> DependencyGraphDump {
    let dependents = app.dependency_graph().dependents();
    let mut signals: Vec<SignalDump> = dependents
        .iter()
        .map(|(signal, owners)| {
            let mut owners: Vec<DependencyOwnerDump> = owners
                .iter()
                .map(|owner| match *owner {
                    crate::node::dependency::Owner::Node { window, node } => {
                        DependencyOwnerDump::Node { window, node }
                    }
                    crate::node::dependency::Owner::Derived(derived) => {
                        DependencyOwnerDump::Derived(derived)
                    }
                })
                .collect();
            owners.sort_by_key(|owner| match owner {
                DependencyOwnerDump::Node { window, node } => {
                    (0u8, window.to_bits(), node.index(), node.generation())
                }
                DependencyOwnerDump::Derived(derived) => (1u8, derived.index(), 0, 0),
            });
            SignalDump {
                signal: *signal,
                owners,
            }
        })
        .collect();
    signals.sort_by_key(|signal| signal.signal.index());
    let edges = signals
        .iter()
        .map(|signal| signal.owners.len() as u32)
        .sum();
    DependencyGraphDump { signals, edges }
}

impl fmt::Display for DependencyGraphDump {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "dependency-graph signals={} edges={}",
            self.signals.len(),
            self.edges
        )?;
        for signal in &self.signals {
            writeln!(
                f,
                "  signal {} owners={}",
                signal.signal.index(),
                signal.owners.len()
            )?;
            for owner in &signal.owners {
                match owner {
                    DependencyOwnerDump::Node { window, node } => {
                        writeln!(f, "    node w{} n{}", window.to_bits(), node.index())?;
                    }
                    DependencyOwnerDump::Derived(derived) => {
                        writeln!(f, "    derived {}", derived.index())?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct A11yTreeDump {
    pub full: bool,
    pub focus: A11yNodeId,
    pub diagnostics: usize,
    pub nodes: Vec<A11yNodeDump>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct A11yNodeDump {
    pub engine: NodeId,
    pub role: A11yRole,
    pub label: Option<SmolStr>,
    pub value: Option<SmolStr>,
    pub bounds: PhysicalRect,
    pub hidden: bool,
    pub disabled: bool,
    pub actions: Vec<A11yAction>,
    pub children: Vec<A11yNodeId>,
}

#[must_use]
pub fn a11y_dump(snapshot: &A11yTreeSnapshot) -> A11yTreeDump {
    A11yTreeDump {
        full: snapshot.full,
        focus: snapshot.focus,
        diagnostics: snapshot.diagnostics,
        nodes: snapshot
            .nodes()
            .iter()
            .map(|node| A11yNodeDump {
                engine: node.engine,
                role: node.role,
                label: node.label.as_ref().map(|label| SmolStr::new(label.text())),
                value: node.value.clone(),
                bounds: node.bounds,
                hidden: node.hidden,
                disabled: node.disabled,
                actions: node.actions.clone(),
                children: node.children.clone(),
            })
            .collect(),
    }
}

impl fmt::Display for A11yTreeDump {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "a11y full={} focus={} diagnostics={} nodes={}",
            self.full,
            self.focus.get(),
            self.diagnostics,
            self.nodes.len()
        )?;
        for node in &self.nodes {
            writeln!(
                f,
                "  node n{}.{} role={:?} hidden={} disabled={} bounds={},{},{},{} actions={} label={:?}",
                node.engine.index(),
                node.engine.generation(),
                node.role,
                node.hidden,
                node.disabled,
                node.bounds.x0,
                node.bounds.y0,
                node.bounds.x1,
                node.bounds.y1,
                node.actions.len(),
                node.label
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DamageOverlay {
    pub rects: Vec<PhysicalRect>,
}

impl DamageOverlay {
    #[must_use]
    pub fn of(frame: &FrameRecord) -> Self {
        Self {
            rects: frame.damage.clone(),
        }
    }

    #[must_use]
    pub fn canvas(&self, scale: Scale) -> Canvas {
        let outline = scale.to_logical(1.0);
        let mut union: Option<Rect> = None;
        for rect in &self.rects {
            let bounds = logical_of(rect, scale).outset(outline);
            union = Some(match union {
                Some(current) => current.union(bounds),
                None => bounds,
            });
        }
        let mut canvas = Canvas::new(union.unwrap_or_default());
        for rect in &self.rects {
            let bounds = logical_of(rect, scale);
            canvas.push_quad(Quad {
                bounds,
                corner_radius: [0.0; 4],
                paint: QuadPaint::Solid {
                    fill: Color::from_rgba8([255, 0, 255, 32]),
                    border: None,
                },
            });
            let edges = [
                Rect::new(bounds.origin.x, bounds.origin.y, bounds.size.width, outline),
                Rect::new(
                    bounds.origin.x,
                    bounds.origin.y + bounds.size.height - outline,
                    bounds.size.width,
                    outline,
                ),
                Rect::new(
                    bounds.origin.x,
                    bounds.origin.y,
                    outline,
                    bounds.size.height,
                ),
                Rect::new(
                    bounds.origin.x + bounds.size.width - outline,
                    bounds.origin.y,
                    outline,
                    bounds.size.height,
                ),
            ];
            for edge in edges {
                canvas.push_quad(Quad {
                    bounds: edge,
                    corner_radius: [0.0; 4],
                    paint: QuadPaint::Solid {
                        fill: Color::from_rgba8([255, 0, 255, 255]),
                        border: None,
                    },
                });
            }
        }
        canvas
    }
}

fn logical_of(rect: &PhysicalRect, scale: Scale) -> Rect {
    Rect::new(
        scale.to_logical(rect.x0 as f32),
        scale.to_logical(rect.y0 as f32),
        scale.to_logical(rect.width() as f32),
        scale.to_logical(rect.height() as f32),
    )
}

#[cfg(target_os = "macos")]
#[must_use]
pub fn metal_gpu_core_generation() -> u64 {
    crate::render::macos::core::GpuCore::generation()
}

#[cfg(test)]
mod tests {
    use super::{
        A11yNodeDump, A11yTreeDump, DamageOverlay, DependencyGraphDump, DependencyOwnerDump,
        FrameDump, InstanceDump, OpDump, a11y_dump, dependency_dump, frame_dump,
    };
    use crate::{
        a11y::{A11yAction, A11yNodeId, A11yRole},
        math::{PhysicalRect, Scale},
        node::builder::NodeBuilder,
        paint::{color::Color, style::PaintSpec},
        test_support::{app::TestApp, record::FrameRecord},
    };

    fn absolute_style(
        rect: crate::math::Rect,
        clip: crate::paint::style::Clip,
    ) -> crate::paint::style::Style {
        crate::paint::style::Style {
            inner: taffy::Style::<smol_str::SmolStr> {
                position: taffy::style::Position::Absolute,
                size: taffy::geometry::Size {
                    width: taffy::style::Dimension::length(rect.size.width),
                    height: taffy::style::Dimension::length(rect.size.height),
                },
                inset: taffy::geometry::Rect {
                    left: taffy::style::LengthPercentageAuto::length(rect.origin.x),
                    top: taffy::style::LengthPercentageAuto::length(rect.origin.y),
                    right: taffy::style::LengthPercentageAuto::auto(),
                    bottom: taffy::style::LengthPercentageAuto::auto(),
                },
                ..taffy::Style::<smol_str::SmolStr>::DEFAULT
            },
            clip,
        }
    }

    fn app_with_boxes(count: usize) -> TestApp {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        for index in 0..count {
            let _ = NodeBuilder::boxed()
                .layout_override(crate::math::Rect::new(index as f32 * 10.0, 0.0, 8.0, 8.0))
                .paint(PaintSpec {
                    background: Color::WHITE,
                    ..PaintSpec::default()
                })
                .id("box")
                .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
                .expect("box");
        }
        let _ = app.drain().expect("frame");
        app
    }

    #[test]
    fn frame_dump_matches_the_committed_instances() {
        let app = app_with_boxes(3);
        let dump = frame_dump(&app);
        let record = app.frame_record();

        assert_eq!(dump.frame_index, record.frame_index.get());
        assert_eq!(dump.instances.len(), record.stats.instances);
        assert_eq!(dump.damage, record.damage);
        assert!(dump.instances.iter().all(|instance| instance.kind == 0));
    }

    #[test]
    fn frame_dump_text_is_stable() {
        let app = app_with_boxes(2);
        let dump = frame_dump(&app);
        let first = dump.to_string();
        let second = dump.to_string();

        assert_eq!(first, second);
        assert!(first.starts_with("frame "), "{first}");
        assert!(first.contains("instances 2"), "{first}");
        assert!(first.contains("ops 1"), "{first}");
        assert!(first.contains("clips 1"), "{first}");
        assert!(first.ends_with('\n'), "{first}");
    }

    #[test]
    fn frame_dump_on_a_fresh_app_is_empty() {
        let app = TestApp::new();
        let dump = frame_dump(&app);

        assert_eq!(dump.frame_index, 0, "无提交帧 ⇒ 帧号 0");
        assert!(dump.damage.is_empty());
        assert!(dump.instances.is_empty());
        assert_eq!(dump.clips.len(), 1, "clip 表恒有哨兵项 0");
        assert!(dump.to_string().starts_with("frame 0 "));
    }

    #[test]
    fn frame_dump_exposes_the_clip_kind_and_the_snapped_rect() {
        use crate::paint::{
            canvas::Canvas,
            primitive::{ClipRect, Quad, QuadPaint},
            style::Clip,
        };

        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let container = NodeBuilder::boxed()
            .style(absolute_style(
                crate::math::Rect::new(8.25, 4.5, 40.5, 20.25),
                Clip::Bounds,
            ))
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("container");
        let _ = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(0.0, 0.0, 10.0, 10.0))
            .paint(PaintSpec {
                background: Color::WHITE,
                ..PaintSpec::default()
            })
            .id("content")
            .build(
                app.app_mut().window_mut(window).unwrap().tree_mut(),
                container,
            )
            .expect("content");
        let mut canvas = Canvas::new(crate::math::Rect::new(8.0, 4.0, 40.0, 20.0));
        assert!(canvas.push_clip(ClipRect {
            rect: crate::math::Rect::new(8.0, 4.0, 40.0, 20.0),
            corner_radius: [3.0; 4],
        }));
        canvas.push_quad(Quad {
            bounds: crate::math::Rect::new(9.0, 5.0, 10.0, 10.0),
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: Color::WHITE,
                border: None,
            },
        });
        assert!(canvas.pop_clip(), "Canvas 必须 clip 平衡");
        let _ = NodeBuilder::canvas(canvas)
            .layout_override(crate::math::Rect::new(8.0, 4.0, 40.0, 20.0))
            .id("canvas")
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("canvas node");
        let _ = app.drain().expect("frame");

        let dump = frame_dump(&app);
        assert_eq!(dump.clips.len(), 3, "哨兵 + 矩形 + 圆角：{:?}", dump.clips);
        assert_eq!((dump.clips[0].kind, dump.clips[0].parent), (1, None));
        let rect_clip = dump.clips[1];
        assert_eq!(
            rect_clip.kind, 1,
            "矩形项的 meta.y（KYTRIX_CLIP_KIND_RECT）"
        );
        assert_eq!(rect_clip.parent, None, "矩形项是链根");
        assert_eq!(
            rect_clip.rect,
            [8.0, 4.0, 41.0, 21.0],
            "矩形项必须吸附到整数物理像素（floor(8.25)=8 / ceil(48.75)=49）"
        );
        let rounded = dump.clips[2];
        assert_eq!(
            rounded.kind, 2,
            "圆角项的 meta.y（KYTRIX_CLIP_KIND_ROUNDED）"
        );
        assert_eq!(rounded.parent, None, "根层 Canvas 的圆角 clip 没有父项");
        assert!(dump.to_string().contains("kind=2"), "{dump}");
    }

    #[test]
    fn frame_dump_handles_an_idle_frame() {
        let mut app = app_with_boxes(1);
        let _ = app.drain().expect("idle frame");
        let dump = frame_dump(&app);
        assert!(!dump.instances.is_empty());
        assert_eq!(dump.frame_index, app.frame_record().frame_index.get());
    }

    #[test]
    fn op_dump_distinguishes_instances_and_geometry() {
        let instances = OpDump {
            instances: Some((0, 5)),
            geometry: None,
            clip_index: 0,
        };
        let geometry = OpDump {
            instances: None,
            geometry: Some(2),
            clip_index: 1,
        };
        assert_ne!(instances, geometry);
        assert_eq!(instances.instances, Some((0, 5)));
        assert_eq!(geometry.geometry, Some(2));
    }

    #[test]
    fn dependency_dump_lists_bind_style_owners() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let signal = app.app_mut().signal(1u8);
        let _ = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(0.0, 0.0, 8.0, 8.0))
            .bind_style(move |cx| crate::paint::style::StylePatch {
                opacity: Some(f32::from(*cx.read(&signal))),
                ..crate::paint::style::StylePatch::default()
            })
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("box");
        let _ = app.drain().expect("frame");

        let dump = dependency_dump(app.app());
        assert_eq!(dump.edges, 1, "{dump:?}");
        assert_eq!(dump.signals.len(), 1);
        assert_eq!(dump.signals[0].signal.index(), signal.id().index());
        assert!(matches!(
            dump.signals[0].owners[0],
            DependencyOwnerDump::Node { .. }
        ));
        assert!(dump.to_string().contains("node w"));
    }

    #[test]
    fn dependency_dump_is_deterministic() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        for index in 0..3 {
            let signal = app.app_mut().signal(index as u8);
            let _ = NodeBuilder::boxed()
                .layout_override(crate::math::Rect::new(0.0, 0.0, 8.0, 8.0))
                .bind_style(move |cx| crate::paint::style::StylePatch {
                    opacity: Some(f32::from(*cx.read(&signal))),
                    ..crate::paint::style::StylePatch::default()
                })
                .id("box")
                .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
                .expect("box");
        }
        let _ = app.drain().expect("frame");

        let first = dependency_dump(app.app());
        let second = dependency_dump(app.app());
        assert_eq!(first, second);
        assert_eq!(first.edges, 3);
    }

    #[test]
    fn a11y_dump_mirrors_the_snapshot() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let button = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(0.0, 0.0, 40.0, 20.0))
            .on_click(crate::event::Action::custom("press"))
            .id("press")
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("button");
        let _ = app.drain().expect("frame");

        let snapshot = app.a11y().expect("a11y snapshot");
        let dump = a11y_dump(snapshot);
        assert!(dump.full);
        assert_eq!(dump.nodes.len(), snapshot.nodes().len());
        assert_eq!(dump.focus, snapshot.focus());
        let dumped = dump
            .nodes
            .iter()
            .find(|node| node.engine == button)
            .expect("button node");
        assert_eq!(dumped.role, A11yRole::Button);
        assert!(dumped.actions.contains(&A11yAction::Click));
        assert!(dump.to_string().starts_with("a11y full=true"));
    }

    #[test]
    fn damage_overlay_covers_every_rect() {
        let record = FrameRecord {
            frame_index: crate::window::FrameIndex::from_raw(1),
            rendered: true,
            frame: None,
            stats: crate::test_support::record::FrameStats::default(),
            damage: vec![
                PhysicalRect::new(10, 20, 30, 40),
                PhysicalRect::new(0, 0, 4, 4),
            ],
            damage_full: false,
            delta: crate::test_support::record::DeltaRecord::default(),
            scanned_nodes: 0,
            dirty_nodes: Vec::new(),
            repainted: true,
            a11y_only: false,
            skipped_by_budget: 0,
            paint_log: crate::test_support::record::PaintLog::default(),
            a11y: None,
            stale_canvas_runs: 0,
            presented_latency: None,
        };
        let overlay = DamageOverlay::of(&record);
        assert_eq!(overlay.rects, record.damage);

        let scale = Scale::new(2.0).expect("scale");
        let canvas = overlay.canvas(scale);
        assert_eq!(canvas.primitives().len(), overlay.rects.len() * 5);
        assert!(canvas.bounds().size.width > 15.0);
    }

    #[test]
    fn dto_types_do_not_borrow() {
        fn assert_static<T: 'static>() {}
        assert_static::<FrameDump>();
        assert_static::<InstanceDump>();
        assert_static::<OpDump>();
        assert_static::<DependencyGraphDump>();
        assert_static::<A11yTreeDump>();
        assert_static::<A11yNodeDump>();
        assert_static::<DamageOverlay>();
        let _ = A11yNodeId::from_stable(1);
    }
}

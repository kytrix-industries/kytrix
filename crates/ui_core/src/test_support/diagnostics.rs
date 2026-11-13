//! 诊断导出（`design/phase-13-resources-snapshot.md` §11）：`FrameDump` / `DependencyGraphDump` /
//! `A11yTreeDump` / `DamageOverlay`。
//!
//! 四条纪律：
//!
//! 1. 只挂**既有** `test-support` feature（`lib.rs` 的 `pub mod test_support` 门控），发布物零符号；
//! 2. DTO 只含标量 / `enum` / `PhysicalRect` / `NodeId` / `WindowId` / `SmolStr`，
//!    **不出现任何平台句柄**（本模块不 `use` 平台模块）；
//! 3. 文本格式稳定、逐行、可 diff（`Display`），顺序全部显式排序（HashMap 迭代序不可复现）；
//! 4. 不是 replay 引擎：`FrameDump` 只用于 bug 报告与 diff（F-145）。

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

/// 一帧的**可 diff 结构化记录**（不是 replay 引擎：v1 没有 `--replay` 工具，见 F-145）。
#[derive(Clone, Debug, PartialEq)]
pub struct FrameDump {
    /// 提交序号。
    pub frame_index: u32,
    /// 视口（物理像素）。
    pub viewport: PhysicalRect,
    /// 设备比例。
    pub scale: f32,
    /// 本帧 damage（物理像素）。
    pub damage: Vec<PhysicalRect>,
    /// 实例表（按编码序）。
    pub instances: Vec<InstanceDump>,
    /// 绘制序。
    pub ops: Vec<OpDump>,
    /// 裁剪表（表项 0 是哨兵）。
    pub clips: Vec<ClipDump>,
}

/// 一个 `GpuPrimitive` 的明文镜像（字段与 128 B 契约一一对应）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InstanceDump {
    /// `params1.z`（`1` = Glyph、`2` = Image、其余 = SDF quad/shadow）。
    pub kind: u32,
    /// `params1.y`（`0` = 字形页、`1` = 图片页）。
    pub texture: u32,
    /// `bounds`（物理像素 `x, y, w, h`）。
    pub bounds: [f32; 4],
    /// 四角半径（物理像素）。
    pub corner_radius: [f32; 4],
    /// 预乘线性色（实色 / 描边 / 阴影 / 字形 / 图片调制）。
    pub color0: [f32; 4],
    /// 渐变终点 / 描边色 / 阴影参数载体。
    pub color1: [f32; 4],
    /// `.x` = 边框宽、`.y` = 阴影 blur、`.z` = 渐变类型、`.w` = AA 外扩。
    pub params0: [f32; 4],
    /// `.x` = clip 链索引。
    pub clip_index: u32,
    /// 图集 UV（非图集为 0）。
    pub uv: [f32; 4],
    /// clip 交集 AABB（物理像素）。
    pub clip_rect: [f32; 4],
}

/// `FrameOp` 的镜像。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpDump {
    /// `Some((start, len))` = `FrameOp::Instances`。
    pub instances: Option<(u32, u32)>,
    /// `Some(index)` = `FrameOp::Geometry`。
    pub geometry: Option<u32>,
    /// 该 op 的 clip 链索引（0 = 无裁剪）。
    pub clip_index: u32,
}

/// 一条 clip 链的镜像（`GpuClipEntry` 的明文形态）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipDump {
    /// `(x, y, w, h)`，物理像素。
    pub rect: [f32; 4],
    /// `(tl, tr, br, bl)`，物理像素。
    pub corner_radius: [f32; 4],
    /// 父链索引（`None` = 无父）。
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
                "  inst {index} kind={} tex={} bounds={:.4},{:.4},{:.4},{:.4} clip={} uv={:.4},{:.4},{:.4},{:.4}",
                instance.kind,
                instance.texture,
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
                "  clip {index} rect={:.4},{:.4},{:.4},{:.4} r={:.4},{:.4},{:.4},{:.4} parent={}",
                clip.rect[0],
                clip.rect[1],
                clip.rect[2],
                clip.rect[3],
                clip.corner_radius[0],
                clip.corner_radius[1],
                clip.corner_radius[2],
                clip.corner_radius[3],
                clip.parent
                    .map_or_else(|| "none".to_string(), |parent| parent.to_string())
            )?;
        }
        Ok(())
    }
}

/// 从 `TestApp` 最近一帧捕获（**未 `drain` 时**帧号 0、damage 为空、实例为空，不 panic）。
#[must_use]
pub fn frame_dump(app: &TestApp) -> FrameDump {
    let window = app
        .app()
        .window(app.window_id())
        .expect("TestApp's window must exist");
    let buffer = window.instance_buffer();
    let instances: &[GpuPrimitive] = bytemuck::cast_slice(buffer.as_bytes());
    // 不能用 `TestApp::frame_record()`：它在从未 `drain()` 时会 panic（`record` 是 `Option`）。
    let record = window.record();
    FrameDump {
        frame_index: record.map_or(0, |record| record.frame_index.get()),
        viewport: Rect::new(0.0, 0.0, window.size().width, window.size().height)
            .to_physical_covering(window.scale()),
        scale: window.scale().factor(),
        damage: record.map_or_else(Vec::new, |record| record.damage.clone()),
        instances: instances.iter().map(instance_dump).collect(),
        ops: buffer.ops().iter().map(op_dump).collect(),
        clips: buffer.clips().iter().map(clip_dump).collect(),
    }
}

fn instance_dump(instance: &GpuPrimitive) -> InstanceDump {
    InstanceDump {
        kind: quantize(instance.params1[2]),
        texture: quantize(instance.params1[1]),
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
        FrameOp::Instances { start, len } => OpDump {
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
        parent: (parent != 0).then_some(parent),
    }
}

/// `f32` 里的整数索引（`params1`）→ `u32`；非有限/负值归 0（诊断不 panic）。
fn quantize(value: f32) -> u32 {
    if value.is_finite() && value > 0.0 {
        value as u32
    } else {
        0
    }
}

/// 依赖图（`SignalId → Owner`）的明文快照。
#[derive(Clone, Debug, PartialEq)]
pub struct DependencyGraphDump {
    /// 按 `SignalId::index()` 升序。
    pub signals: Vec<SignalDump>,
    /// 依赖边总数（= 所有 `owners` 之和）。
    pub edges: u32,
}

/// 一个信号及其读者。
#[derive(Clone, Debug, PartialEq)]
pub struct SignalDump {
    /// 信号 id。
    pub signal: SignalId,
    /// 读者（排序后）。
    pub owners: Vec<DependencyOwnerDump>,
}

/// `Owner` 的公开镜像（`Owner` 本身是 crate 私有）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyOwnerDump {
    /// 一个节点的 `bind_*` 闭包。
    Node {
        /// 所属窗口。
        window: WindowId,
        /// 节点。
        node: NodeId,
    },
    /// 另一个派生值。
    Derived(crate::node::signal::DerivedId),
}

/// 依赖图的公开捕获入口（自由函数：`impl App` 只能在 `app.rs`）。
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

/// 一笔 a11y 交付的明文快照（`full == false` 时只含变更节点 + 祖先链）。
#[derive(Clone, Debug, PartialEq)]
pub struct A11yTreeDump {
    /// 本次是否发出全树。
    pub full: bool,
    /// 焦点身份。
    pub focus: A11yNodeId,
    /// 本次交付触发的诊断条数。
    pub diagnostics: usize,
    /// 节点（`A11yTreeSnapshot::nodes` 的原序）。
    pub nodes: Vec<A11yNodeDump>,
}

/// 一个 a11y 节点的明文镜像（只保留诊断真正需要的字段）。
#[derive(Clone, Debug, PartialEq)]
pub struct A11yNodeDump {
    /// 引擎节点（诊断与节点树的唯一桥梁）。
    pub engine: NodeId,
    /// 角色。
    pub role: A11yRole,
    /// 可访问名。
    pub label: Option<SmolStr>,
    /// 值。
    pub value: Option<SmolStr>,
    /// 物理像素边界。
    pub bounds: PhysicalRect,
    /// 隐藏。
    pub hidden: bool,
    /// 禁用。
    pub disabled: bool,
    /// 支持的动作。
    pub actions: Vec<A11yAction>,
    /// 直接子节点。
    pub children: Vec<A11yNodeId>,
}

/// 从一笔 a11y 交付捕获（纯转换，不触达平台 adapter）。
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

/// 把 damage 矩形画成可叠加的 `Canvas`（洋红 1px 描边 + 12.5% 填充），走**正常管线**：
/// 没有平台 overlay 窗口，因此能截进快照、能在三平台一致。
#[derive(Clone, Debug, PartialEq)]
pub struct DamageOverlay {
    /// 本帧的 damage（物理像素）。
    pub rects: Vec<PhysicalRect>,
}

impl DamageOverlay {
    /// 从一帧记录取 damage。
    #[must_use]
    pub fn of(frame: &FrameRecord) -> Self {
        Self {
            rects: frame.damage.clone(),
        }
    }

    /// 生成叠在视口之上的 `Canvas`（1 物理像素描边，按 `scale` 反算逻辑坐标）。
    #[must_use]
    pub fn canvas(&self, scale: Scale) -> Canvas {
        let outline = scale.to_logical(1.0);
        // canvas 的 bounds 必须 ⊇ 所有图元（含描边的 1 px 外扩）：先求并集再建。
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
            // 四边各一条 1 物理像素的描边。
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

/// 物理矩形 → 逻辑矩形（逐边 `to_logical`，不取整）。
fn logical_of(rect: &PhysicalRect, scale: Scale) -> Rect {
    Rect::new(
        scale.to_logical(rect.x0 as f32),
        scale.to_logical(rect.y0 as f32),
        scale.to_logical(rect.width() as f32),
        scale.to_logical(rect.height() as f32),
    )
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
        // 评审阻塞 ①：从未 `drain()` 时 `frame_record()` 会 panic；`frame_dump` 必须安全降级。
        let app = TestApp::new();
        let dump = frame_dump(&app);

        assert_eq!(dump.frame_index, 0, "无提交帧 ⇒ 帧号 0");
        assert!(dump.damage.is_empty());
        assert!(dump.instances.is_empty());
        assert_eq!(dump.clips.len(), 1, "clip 表恒有哨兵项 0");
        assert!(dump.to_string().starts_with("frame 0 "));
    }

    #[test]
    fn frame_dump_handles_an_idle_frame() {
        let mut app = app_with_boxes(1);
        let _ = app.drain().expect("idle frame");
        let dump = frame_dump(&app);
        // 空闲帧不重新编码：实例表沿用上一提交，dump 结构必须仍然自洽。
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
                opacity: Some(f32::from(cx.read(&signal))),
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
                    opacity: Some(f32::from(cx.read(&signal))),
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
            dirty_nodes: Vec::new(),
            paint_log: crate::test_support::record::PaintLog::default(),
            a11y: None,
        };
        let overlay = DamageOverlay::of(&record);
        assert_eq!(overlay.rects, record.damage);

        let scale = Scale::new(2.0).expect("scale");
        let canvas = overlay.canvas(scale);
        // 每个 damage 矩形产出 1 个填充 + 4 条边。
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
        let _ = A11yNodeId::from_engine(crate::node::id::NodeId::for_test(1, 1));
    }
}

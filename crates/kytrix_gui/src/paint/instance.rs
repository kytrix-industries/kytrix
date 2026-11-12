//! `GpuPrimitive` 契约（128 B / 16 B 对齐）与 CPU 侧实例编码。
//!
//! 本文是 `render/shaders/contract.md`（P10）的**权威来源**（I2/I8）：字段顺序、offset、`kind`/`flags`
//! 取值表的任何改动都必须同时改 Rust、三套 shader、`contract.md`，并触发像素快照回归。
//!
//! 范围（`design/phase-06-paint-runtime.md` §7）：
//! - 8 类 `Primitive` → `GpuPrimitive`（`GlyphRun` 是唯一的 1→N）；
//! - `Path`/`Geometry` 0 实例 + `FrameOp::Geometry`；
//! - clip 链去重（`ClipChainId`，帧内索引）；
//! - `InstanceBuffer` 容量 = 历史峰值（`clear` 只截断、不 shrink、不按帧翻倍）。
//!
//! **P07 只做上传/批次**：本模块不碰 GPU、不持设备。

use smallvec::SmallVec;

use crate::{
    math::{Rect, Scale, Transform},
    paint::{
        color::{Color, ColorResolver, LinearPremulRgba},
        primitive::{ClipOp, GeometryDraw, GradientKind, Primitive, QuadPaint},
        scene::{MAX_CLIP_DEPTH, Scene},
    },
};

/// uber-quad 的单实例。**128 B / 16 B 对齐**，三平台共用同一份字节布局（P02 §8.1 / 附录 B）。
#[repr(C, align(16))]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GpuPrimitive {
    /// 物理像素 `(x, y, w, h)`；SDF 图元未取整，图集图元是整数矩形的浮点表示。
    pub bounds: [f32; 4],
    /// 四角半径（物理像素，`[tl, tr, br, bl]`）。
    pub corner_radius: [f32; 4],
    /// premultiplied linear sRGB：实色 / 渐变起色 / 阴影色 / 字形色 / 图片调制色。
    pub color0: [f32; 4],
    /// 渐变终色 **或** 边框色（互斥）；无则全 0。
    pub color1: [f32; 4],
    /// `border_width, shadow_blur, gradient_kind, aa_padding`。
    pub params0: [f32; 4],
    /// `clip_index, texture_index, kind, flags`（整数编码进 f32）。
    pub params1: [f32; 4],
    /// 图集 tile `(u0, v0, u1, v1)`。
    pub uv: [f32; 4],
    /// clip 链交集 AABB（物理浮点）。
    pub clip_rect: [f32; 4],
}

// 静态断言：布局漂移必须让编译失败，而不是让三平台花屏（P02 §8.1；probe 已实测）。
const _: () = {
    use core::mem::{align_of, offset_of, size_of};

    assert!(size_of::<GpuPrimitive>() == 128);
    assert!(align_of::<GpuPrimitive>() == 16);
    assert!(offset_of!(GpuPrimitive, bounds) == 0);
    assert!(offset_of!(GpuPrimitive, corner_radius) == 16);
    assert!(offset_of!(GpuPrimitive, color0) == 32);
    assert!(offset_of!(GpuPrimitive, color1) == 48);
    assert!(offset_of!(GpuPrimitive, params0) == 64);
    assert!(offset_of!(GpuPrimitive, params1) == 80);
    assert!(offset_of!(GpuPrimitive, uv) == 96);
    assert!(offset_of!(GpuPrimitive, clip_rect) == 112);
};

pub(crate) const KIND_QUAD: f32 = 0.0;
pub(crate) const KIND_GLYPH: f32 = 1.0;
pub(crate) const KIND_IMAGE: f32 = 2.0;
pub(crate) const KIND_SHADOW: f32 = 3.0;

/// `flags` 的 bit 0。
pub(crate) const FLAG_OPAQUE: f32 = 1.0;
pub(crate) const TEXTURE_GLYPH: f32 = 0.0;
pub(crate) const TEXTURE_IMAGE: f32 = 1.0;

pub(crate) const GRADIENT_NONE: f32 = 0.0;
pub(crate) const GRADIENT_HORIZONTAL: f32 = 1.0;
pub(crate) const GRADIENT_VERTICAL: f32 = 2.0;
pub(crate) const GRADIENT_RADIAL: f32 = 3.0;

/// `ClipChain` 的帧内索引；`0` 是「无裁剪」哨兵。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClipChainId(u32);

impl ClipChainId {
    const NONE: Self = Self(0);

    fn index(self) -> usize {
        self.0 as usize
    }
}

/// 一层 clip（P02 §8.7）。`aabb` 是从根到本层的交集，写进 `GpuPrimitive.clip_rect`。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClipChain {
    parent: Option<ClipChainId>,
    rect: [f32; 4],
    corner_radius: [f32; 4],
    aabb: [f32; 4],
}

/// GPU 侧 `ClipEntry`（P07 §2.6 / DEVIATIONS F-69）：**48 B / 16 B 对齐**。
///
/// 与 CPU 侧 [`ClipChain`] 的差别：丢弃 `aabb`（已在 `GpuPrimitive.clip_rect` 冗余），
/// `parent` 打进 `uint4.x`，`.yzw` 保留恒 0；表项 **0 恒为哨兵**（`clip_index == 0` 时 shader 直接返回 1）。
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[cfg(windows)]
pub(crate) struct GpuClipEntry {
    /// `(x, y, w, h)`，物理像素。
    pub rect: [f32; 4],
    /// `(tl, tr, br, bl)`，物理像素。
    pub corner_radius: [f32; 4],
    /// `.x = parent`（0 = 无裁剪），`.yzw` 恒 0。
    pub meta: [u32; 4],
}

#[cfg(windows)]
const _: () = {
    use core::mem::{align_of, size_of};
    assert!(size_of::<GpuClipEntry>() == 48);
    assert!(align_of::<GpuClipEntry>() == 16);
};

impl ClipChain {
    /// 转成 GPU 布局（P08 的上传源；哨兵语义照抄 CPU 值，不做数学）。
    #[cfg(windows)]
    pub(crate) fn gpu_entry(self) -> GpuClipEntry {
        GpuClipEntry {
            rect: self.rect,
            corner_radius: self.corner_radius,
            meta: [self.parent.map_or(0, |parent| parent.0), 0, 0, 0],
        }
    }
}

/// 保持绘制序的最小表达（P02 §8.8）。
///
/// `Geometry` 带本帧的 clip 前缀（`clip_index` + 交集 AABB），P07 直接消费即可，
/// 不必回查 `ClipChain` 表（裁剪不会在几何绘制上丢失）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameOp {
    /// `instances[start .. start + len]` 连续；段内 `clip_index` 可不同。
    Instances { start: u32, len: u32 },
    /// 一次几何绘制（`Primitive::Path` / `Primitive::Geometry`）。
    Geometry {
        /// `geometry` 表的下标。
        index: u32,
        /// 编码时的 clip 链索引（0 = 无裁剪）。
        clip_index: u32,
        /// 链交集 AABB（物理浮点）。
        clip_rect: [u32; 4],
    },
}

/// 一帧的编码产物（容量跨帧保留）。
pub(crate) struct InstanceBuffer {
    instances: Vec<GpuPrimitive>,
    clips: Vec<ClipChain>,
    geometry: Vec<GeometryDraw>,
    ops: Vec<FrameOp>,
}

impl InstanceBuffer {
    fn new() -> Self {
        Self {
            instances: Vec::with_capacity(256),
            clips: Vec::with_capacity(8),
            geometry: Vec::with_capacity(8),
            ops: Vec::with_capacity(16),
        }
    }

    fn clear(&mut self) {
        self.instances.clear();
        self.clips.clear();
        self.geometry.clear();
        self.ops.clear();
    }

    /// 本帧实例数（`test-support` 的 `FrameStats`；P07 会按它切批）。
    #[cfg(feature = "test-support")]
    pub(crate) fn len(&self) -> usize {
        self.instances.len()
    }

    /// `GpuPrimitive` 数组的字节视图（P02 §8.1 的 128 B 布局；P07 的上传源，零拷贝）。
    pub(crate) fn as_bytes(&self) -> &[u8] {
        bytemuck::cast_slice(&self.instances)
    }

    /// 本帧的绘制序（P08 的录制循环唯一读者；P06 遗留 ④ 的收口，DEVIATIONS F-78）。
    #[cfg(windows)]
    pub(crate) fn ops(&self) -> &[FrameOp] {
        &self.ops
    }

    /// `FrameOp::Geometry.index` 的下标表。
    #[cfg(windows)]
    pub(crate) fn geometry(&self) -> &[GeometryDraw] {
        &self.geometry
    }

    /// 裁剪表（表项 0 是哨兵；GPU 布局由 [`ClipChain::gpu_entry`] 转）。
    #[cfg(windows)]
    pub(crate) fn clips(&self) -> &[ClipChain] {
        &self.clips
    }

    /// 把裁剪表写成 GPU 布局（容量复用，不每帧分配）。
    #[cfg(windows)]
    pub(crate) fn write_clips_gpu(&self, out: &mut Vec<GpuClipEntry>) {
        out.clear();
        out.extend(self.clips().iter().copied().map(ClipChain::gpu_entry));
    }
}

/// `Primitive → GpuPrimitive` 的编码器（`Window` 持有，缓冲跨帧复用）。
pub(crate) struct InstanceEncoder {
    buffer: InstanceBuffer,
    stack: SmallVec<[ClipChainId; 8]>,
    chains: ahash::HashMap<(u32, [u32; 4], [u32; 4]), ClipChainId>,
    run_start: usize,
}

impl InstanceEncoder {
    /// 消费者：`Window::new`。
    pub(crate) fn new() -> Self {
        let mut encoder = Self {
            buffer: InstanceBuffer::new(),
            stack: SmallVec::new(),
            chains: ahash::HashMap::default(),
            run_start: 0,
        };
        encoder.buffer.clips.push(ClipChain {
            parent: None,
            rect: [0.0; 4],
            corner_radius: [0.0; 4],
            aabb: [f32::MIN, f32::MIN, f32::MAX, f32::MAX],
        });
        encoder
    }

    /// 编码整帧；内部缓冲复用。消费者：`Window::paint_frame`。
    pub(crate) fn encode(
        &mut self,
        scene: &Scene,
        scale: Scale,
        resolver: &ColorResolver,
    ) -> &InstanceBuffer {
        self.buffer.clear();
        self.buffer.clips.push(ClipChain {
            parent: None,
            rect: [0.0; 4],
            corner_radius: [0.0; 4],
            aabb: [f32::MIN, f32::MIN, f32::MAX, f32::MAX],
        });
        self.chains.clear();
        self.stack.clear();
        self.run_start = 0;
        let mut frame = FrameEncoder {
            buffer: &mut self.buffer,
            stack: &mut self.stack,
            chains: &mut self.chains,
            scale,
            resolver,
            run_start: &mut self.run_start,
        };
        frame.encode_scene(scene);
        self.flush_run();
        self.self_check();
        &self.buffer
    }

    fn flush_run(&mut self) {
        let len = self.buffer.instances.len() - self.run_start;
        if len > 0 {
            self.buffer.ops.push(FrameOp::Instances {
                start: self.run_start as u32,
                len: len as u32,
            });
            self.run_start = self.buffer.instances.len();
        }
    }

    /// 本帧的编码结果（消费者：`Window::instance_bytes`，P07 上传）。
    pub(crate) fn buffer(&self) -> &InstanceBuffer {
        &self.buffer
    }

    /// 编码器自检（P02 §8.8 的不变量；也是 `InstanceBuffer` 内部字段的非 test 读点，S-65）。
    fn self_check(&self) {
        let mut covered = 0usize;
        for op in &self.buffer.ops {
            match *op {
                FrameOp::Instances { start, len } => {
                    covered += len as usize;
                    debug_assert!(
                        (start as usize) + (len as usize) <= self.buffer.instances.len(),
                        "FrameOp::Instances out of range"
                    );
                }
                FrameOp::Geometry {
                    index,
                    clip_index,
                    clip_rect,
                } => {
                    debug_assert!(
                        (index as usize) < self.buffer.geometry.len(),
                        "FrameOp::Geometry out of range"
                    );
                    debug_assert!(
                        (clip_index as usize) < self.buffer.clips.len(),
                        "FrameOp::Geometry clip out of range"
                    );
                    debug_assert!(
                        clip_rect
                            .iter()
                            .all(|bits| f32::from_bits(*bits).is_finite()),
                        "FrameOp::Geometry clip rect must be finite"
                    );
                }
            }
        }
        debug_assert_eq!(
            covered,
            self.buffer.instances.len(),
            "FrameOp must cover every instance"
        );
        for (index, chain) in self.buffer.clips.iter().enumerate() {
            debug_assert!(
                chain.rect.iter().all(|value| value.is_finite()),
                "clip rect must be finite"
            );
            debug_assert!(
                chain.corner_radius.iter().all(|value| value.is_finite()),
                "clip corner radius must be finite"
            );
            if let Some(parent) = chain.parent {
                debug_assert!(
                    parent.index() < index,
                    "clip chain parent must precede its child"
                );
            }
        }
    }
}

/// 单帧编码的借用包（把 `&mut` 字段拆开，避免整结构可变借用冲突）。
struct FrameEncoder<'a> {
    buffer: &'a mut InstanceBuffer,
    stack: &'a mut SmallVec<[ClipChainId; 8]>,
    chains: &'a mut ahash::HashMap<(u32, [u32; 4], [u32; 4]), ClipChainId>,
    scale: Scale,
    resolver: &'a ColorResolver,
    run_start: &'a mut usize,
}

impl FrameEncoder<'_> {
    fn encode_scene(&mut self, scene: &Scene) {
        for lane in scene.order() {
            let entry_depth = self.stack.len();
            let Some(primitives) = scene.lane_primitives(*lane) else {
                continue;
            };
            for primitive in primitives {
                self.encode_primitive(primitive, 0);
            }
            debug_assert_eq!(
                self.stack.len(),
                entry_depth,
                "lane must be clip-balanced (C1)"
            );
        }
    }

    fn encode_primitive(&mut self, primitive: &Primitive, canvas_depth: u8) {
        match primitive {
            Primitive::Clip(ClipOp::Push(rect)) => {
                debug_assert!(self.stack.len() < usize::from(MAX_CLIP_DEPTH));
                let id = self.chain_of(rect.corner_radius, rect.rect);
                self.stack.push(id);
            }
            Primitive::Clip(ClipOp::Pop) => {
                debug_assert!(!self.stack.is_empty());
                let _ = self.stack.pop();
            }
            Primitive::Quad(quad) => {
                if quad_visible(quad) {
                    self.emit_quad(quad);
                }
            }
            Primitive::Shadow(shadow) => {
                if !shadow.color.is_transparent() {
                    self.emit_shadow(shadow);
                }
            }
            Primitive::Image(image) => {
                if !image.modulation.is_transparent() && !image.bounds.is_empty() {
                    self.emit_image(image);
                }
            }
            Primitive::GlyphRun(run) => {
                if !run.color.is_transparent() {
                    for glyph in &run.glyphs {
                        self.emit_glyph(run.color, glyph);
                    }
                }
            }
            Primitive::Path(path) => {
                self.flush_run();
                let index = self.push_geometry(GeometryDraw {
                    id: path.geometry,
                    transform: Transform::IDENTITY,
                });
                let (clip_index, clip_rect) = self.geometry_clip();
                self.buffer.ops.push(FrameOp::Geometry {
                    index,
                    clip_index,
                    clip_rect: clip_rect.map(f32::to_bits),
                });
            }
            Primitive::Geometry(draw) => {
                self.flush_run();
                let index = self.push_geometry(*draw);
                let (clip_index, clip_rect) = self.geometry_clip();
                self.buffer.ops.push(FrameOp::Geometry {
                    index,
                    clip_index,
                    clip_rect: clip_rect.map(f32::to_bits),
                });
            }
            Primitive::Canvas(canvas) => {
                debug_assert_eq!(canvas_depth, 0, "Canvas nesting depth must be <= 1");
                let entry_depth = self.stack.len();
                for inner in canvas.primitives() {
                    self.encode_primitive(inner, canvas_depth + 1);
                }
                debug_assert_eq!(
                    self.stack.len(),
                    entry_depth,
                    "Canvas must be clip-balanced"
                );
            }
        }
    }

    fn push_geometry(&mut self, draw: GeometryDraw) -> u32 {
        let index = self.buffer.geometry.len() as u32;
        self.buffer.geometry.push(draw);
        index
    }

    fn flush_run(&mut self) {
        let len = self.buffer.instances.len() - *self.run_start;
        if len > 0 {
            self.buffer.ops.push(FrameOp::Instances {
                start: *self.run_start as u32,
                len: len as u32,
            });
            *self.run_start = self.buffer.instances.len();
        }
    }

    fn current_chain(&self) -> ClipChainId {
        self.stack.last().copied().unwrap_or(ClipChainId::NONE)
    }

    fn chain_aabb(&self, chain: ClipChainId) -> [f32; 4] {
        self.buffer.clips[chain.index()].aabb
    }

    fn chain_of(&mut self, corner_radius: [f32; 4], rect: Rect) -> ClipChainId {
        let parent = self.current_chain();
        let factor = self.scale.factor();
        let rect_physical = rect.to_physical_f32(self.scale);
        let radii = corner_radius.map(|radius| radius * factor);
        let key = (
            parent.0,
            rect_physical.map(f32::to_bits),
            radii.map(f32::to_bits),
        );
        if let Some(existing) = self.chains.get(&key) {
            return *existing;
        }
        let parent_aabb = self.chain_aabb(parent);
        let aabb = [
            parent_aabb[0].max(rect_physical[0]),
            parent_aabb[1].max(rect_physical[1]),
            parent_aabb[2].min(rect_physical[0] + rect_physical[2]),
            parent_aabb[3].min(rect_physical[1] + rect_physical[3]),
        ];
        let id = ClipChainId(self.buffer.clips.len() as u32);
        self.buffer.clips.push(ClipChain {
            parent: if parent == ClipChainId::NONE {
                None
            } else {
                Some(parent)
            },
            rect: rect_physical,
            corner_radius: radii,
            aabb,
        });
        self.chains.insert(key, id);
        id
    }

    fn clip_index(&self) -> f32 {
        self.current_chain().0 as f32
    }

    /// 几何绘制的 clip 前缀（`(clip_index, clip_rect)`）。
    fn geometry_clip(&self) -> (u32, [f32; 4]) {
        (self.current_chain().0, self.clip_rect())
    }

    fn clip_rect(&self) -> [f32; 4] {
        self.chain_aabb(self.current_chain())
    }

    fn emit_quad(&mut self, quad: &crate::paint::primitive::Quad) {
        let bounds = quad.bounds.to_physical_f32(self.scale);
        if bounds[2] <= 0.0 || bounds[3] <= 0.0 {
            return;
        }
        let factor = self.scale.factor();
        let half_min = 0.5 * bounds[2].min(bounds[3]);
        let radii = quad
            .corner_radius
            .map(|radius| (radius * factor).clamp(0.0, half_min));
        let (color0, color1, border_width, gradient) = match &quad.paint {
            QuadPaint::Solid { fill, border } => {
                let width = border.map_or(0.0, |border| border.width * factor);
                (
                    self.resolver.resolve(*fill),
                    border.map_or(LinearPremulRgba::TRANSPARENT, |border| {
                        self.resolver.resolve(border.color)
                    }),
                    width.clamp(0.0, half_min),
                    GRADIENT_NONE,
                )
            }
            QuadPaint::Gradient(gradient) => (
                self.resolver.resolve(gradient.from),
                self.resolver.resolve(gradient.to),
                0.0,
                match gradient.kind {
                    GradientKind::Horizontal => GRADIENT_HORIZONTAL,
                    GradientKind::Vertical => GRADIENT_VERTICAL,
                    GradientKind::Radial => GRADIENT_RADIAL,
                },
            ),
        };
        let opaque = gradient == GRADIENT_NONE
            && color0.is_opaque()
            && (border_width == 0.0 || color1.is_opaque());
        let instance = GpuPrimitive {
            bounds,
            corner_radius: radii,
            color0: color0.to_array(),
            color1: color1.to_array(),
            params0: [border_width, 0.0, gradient, 1.0],
            params1: [
                self.clip_index(),
                0.0,
                KIND_QUAD,
                if opaque { FLAG_OPAQUE } else { 0.0 },
            ],
            uv: [0.0; 4],
            clip_rect: self.clip_rect(),
        };
        self.buffer.instances.push(instance);
    }

    fn emit_shadow(&mut self, shadow: &crate::paint::primitive::Shadow) {
        let factor = self.scale.factor();
        let bounds = shadow
            .rect
            .translate(shadow.offset)
            .to_physical_f32(self.scale);
        let blur = shadow.blur.max(0.0) * factor;
        let instance = GpuPrimitive {
            bounds,
            corner_radius: [0.0; 4],
            color0: self.resolver.resolve(shadow.color).to_array(),
            color1: [0.0; 4],
            params0: [0.0, blur, GRADIENT_NONE, blur + 1.0],
            params1: [self.clip_index(), 0.0, KIND_SHADOW, 0.0],
            uv: [0.0; 4],
            clip_rect: self.clip_rect(),
        };
        self.buffer.instances.push(instance);
    }

    fn emit_image(&mut self, image: &crate::paint::primitive::Image) {
        let physical = image.bounds.to_physical(self.scale);
        let color0 = self.resolver.resolve(image.modulation);
        let bounds = physical_to_bounds(physical);
        let instance = GpuPrimitive {
            bounds,
            corner_radius: [0.0; 4],
            color0: color0.to_array(),
            color1: [0.0; 4],
            params0: [0.0; 4],
            params1: [
                self.clip_index(),
                TEXTURE_IMAGE,
                KIND_IMAGE,
                if color0.is_opaque() { FLAG_OPAQUE } else { 0.0 },
            ],
            uv: image.tile.uv(),
            clip_rect: self.clip_rect(),
        };
        self.buffer.instances.push(instance);
    }

    fn emit_glyph(&mut self, color: Color, glyph: &crate::paint::primitive::PositionedGlyph) {
        let physical = Rect::from_origin_size(glyph.position, glyph.size).to_physical(self.scale);
        let bounds = physical_to_bounds(physical);
        let color0 = self.resolver.resolve(color);
        let instance = GpuPrimitive {
            bounds,
            corner_radius: [0.0; 4],
            color0: color0.to_array(),
            color1: [0.0; 4],
            params0: [0.0; 4],
            params1: [
                self.clip_index(),
                TEXTURE_GLYPH,
                KIND_GLYPH,
                if color0.is_opaque() { FLAG_OPAQUE } else { 0.0 },
            ],
            uv: glyph.tile.uv(),
            clip_rect: self.clip_rect(),
        };
        self.buffer.instances.push(instance);
    }
}

/// `PhysicalRect → [x, y, w, h]`（图集图元的整数矩形，P01 §7.1）。
fn physical_to_bounds(rect: crate::math::PhysicalRect) -> [f32; 4] {
    [
        rect.x0 as f32,
        rect.y0 as f32,
        rect.width() as f32,
        rect.height() as f32,
    ]
}

/// 「这个 Quad 会产生像素吗」（P02 §8.4 的跳过规则；与 paint 阶段的过滤同一判据）。
fn quad_visible(quad: &crate::paint::primitive::Quad) -> bool {
    if quad.bounds.is_empty() {
        return false;
    }
    match &quad.paint {
        QuadPaint::Solid { fill, border } => {
            !fill.is_transparent() || border.is_some_and(|border| !border.color.is_transparent())
        }
        QuadPaint::Gradient(gradient) => {
            !(gradient.from.is_transparent() && gradient.to.is_transparent())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FLAG_OPAQUE, GRADIENT_HORIZONTAL, GRADIENT_RADIAL, GRADIENT_VERTICAL, GpuPrimitive,
        InstanceBuffer, InstanceEncoder, KIND_GLYPH, KIND_QUAD, KIND_SHADOW, TEXTURE_GLYPH,
    };
    use crate::{
        math::{Point, Rect, Scale, Size, Transform},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            color::{Color, ColorResolver, Interpolation},
            geometry::GeometryId,
            primitive::{
                Border, ClipOp, ClipRect, GeometryDraw, GlyphRun, Gradient, GradientKind,
                PositionedGlyph, Primitive, Quad, QuadPaint, Shadow,
            },
            scene::Scene,
        },
    };

    fn scale() -> Scale {
        Scale::new(2.0).expect("合法 scale")
    }

    fn resolver() -> ColorResolver {
        ColorResolver::new(Interpolation::default())
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::new(x, y, w, h)
    }

    fn quad_primitive(paint: QuadPaint) -> Primitive {
        Primitive::Quad(Quad {
            bounds: rect(1.0, 2.0, 10.0, 20.0),
            corner_radius: [1.0, 2.0, 3.0, 4.0],
            paint,
        })
    }

    fn solid() -> QuadPaint {
        QuadPaint::Solid {
            fill: Color::WHITE,
            border: None,
        }
    }

    fn encode(scene: &Scene) -> InstanceBuffer {
        let mut encoder = InstanceEncoder::new();
        encoder.encode(scene, scale(), &resolver());
        // 取出编码结果，避免继续借用 encoder。
        encoder.buffer
    }

    fn scene_with(lane_primitives: impl FnOnce(&mut Scene, crate::paint::scene::LaneId)) -> Scene {
        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        lane_primitives(&mut scene, lane);
        scene.set_order(&[lane]);
        scene
    }

    #[test]
    fn layout_is_128_by_16() {
        use std::mem::{align_of, offset_of, size_of};

        assert_eq!(size_of::<GpuPrimitive>(), 128);
        assert_eq!(align_of::<GpuPrimitive>(), 16);
        assert_eq!(offset_of!(GpuPrimitive, bounds), 0);
        assert_eq!(offset_of!(GpuPrimitive, corner_radius), 16);
        assert_eq!(offset_of!(GpuPrimitive, color0), 32);
        assert_eq!(offset_of!(GpuPrimitive, color1), 48);
        assert_eq!(offset_of!(GpuPrimitive, params0), 64);
        assert_eq!(offset_of!(GpuPrimitive, params1), 80);
        assert_eq!(offset_of!(GpuPrimitive, uv), 96);
        assert_eq!(offset_of!(GpuPrimitive, clip_rect), 112);
    }

    #[test]
    fn params1_is_integer_encoded() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(lane, quad_primitive(solid())));
        });
        let buffer = encode(&scene);
        let params1 = buffer.instances[0].params1;
        assert_eq!(params1[0], 0.0);
        assert_eq!(params1[1], 0.0);
        assert_eq!(params1[2], KIND_QUAD);
        for value in params1 {
            assert_eq!(value.fract(), 0.0, "params1 必须是整数值");
        }
    }

    #[test]
    fn quad_solid_mapping() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                quad_primitive(QuadPaint::Solid {
                    fill: Color::srgb(1.0, 0.0, 0.0, 1.0),
                    border: None,
                }),
            ));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.bounds, [2.0, 4.0, 20.0, 40.0]);
        assert_eq!(instance.corner_radius, [2.0, 4.0, 6.0, 8.0]);
        assert_eq!(instance.params0, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(instance.clip_rect, [f32::MIN, f32::MIN, f32::MAX, f32::MAX]);
        assert_eq!(instance.uv, [0.0; 4]);
        assert_eq!(instance.color1, [0.0; 4]);
    }

    #[test]
    fn quad_border_uses_color1() {
        let border = Border {
            width: 2.0,
            color: Color::BLACK,
        };
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                quad_primitive(QuadPaint::Solid {
                    fill: Color::WHITE,
                    border: Some(border),
                }),
            ));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.params0[0], 4.0);
        assert_eq!(instance.color1, resolver().resolve(Color::BLACK).to_array());
        assert_eq!(instance.params0[3], 1.0);
    }

    #[test]
    fn quad_gradient_kinds() {
        for (kind, expected) in [
            (GradientKind::Horizontal, GRADIENT_HORIZONTAL),
            (GradientKind::Vertical, GRADIENT_VERTICAL),
            (GradientKind::Radial, GRADIENT_RADIAL),
        ] {
            let scene = scene_with(|scene, lane| {
                assert!(scene.push(
                    lane,
                    quad_primitive(QuadPaint::Gradient(Gradient {
                        from: Color::WHITE,
                        to: Color::BLACK,
                        kind,
                    })),
                ));
            });
            let buffer = encode(&scene);
            let instance = buffer.instances[0];
            assert_eq!(instance.params0[2], expected);
            assert_eq!(instance.params0[0], 0.0);
            assert_eq!(instance.color1, resolver().resolve(Color::BLACK).to_array());
        }
    }

    #[test]
    fn atlas_bounds_are_integer() {
        let tile = AtlasTile {
            atlas: AtlasKind::Glyph,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            generation: 0,
        };
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::GlyphRun(GlyphRun {
                    color: Color::WHITE,
                    glyphs: vec![PositionedGlyph {
                        tile,
                        position: Point::new(1.0, 2.0),
                        size: Size::new(3.0, 4.0),
                    }],
                }),
            ));
        });
        let buffer = encode(&scene);
        for value in buffer.instances[0].bounds {
            assert_eq!(value.fract(), 0.0);
        }
    }

    #[test]
    fn glyph_run_is_one_to_n() {
        let tile = |u0: f32| AtlasTile {
            atlas: AtlasKind::Glyph,
            u0,
            v0: 0.0,
            u1: u0 + 0.1,
            v1: 0.1,
            generation: 0,
        };
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::GlyphRun(GlyphRun {
                    color: Color::WHITE,
                    glyphs: vec![
                        PositionedGlyph {
                            tile: tile(0.0),
                            position: Point::new(0.0, 0.0),
                            size: Size::new(2.0, 2.0),
                        },
                        PositionedGlyph {
                            tile: tile(0.1),
                            position: Point::new(2.0, 0.0),
                            size: Size::new(2.0, 2.0),
                        },
                        PositionedGlyph {
                            tile: tile(0.2),
                            position: Point::new(4.0, 0.0),
                            size: Size::new(2.0, 2.0),
                        },
                    ],
                }),
            ));
        });
        let buffer = encode(&scene);
        assert_eq!(buffer.instances.len(), 3);
        assert!(
            buffer
                .instances
                .iter()
                .all(|i| i.params1[1] == TEXTURE_GLYPH)
        );
        assert!(buffer.instances.iter().all(|i| i.params1[2] == KIND_GLYPH));
        assert_ne!(buffer.instances[0].uv, buffer.instances[1].uv);
        assert_ne!(buffer.instances[0].bounds, buffer.instances[1].bounds);
    }

    #[test]
    fn shadow_padding_is_blur_plus_one() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Shadow(Shadow {
                    rect: rect(0.0, 0.0, 10.0, 10.0),
                    offset: Point::new(2.0, 3.0),
                    blur: 4.0,
                    color: Color::BLACK,
                }),
            ));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.bounds, [4.0, 6.0, 20.0, 20.0]);
        assert_eq!(instance.params0[1], 8.0);
        assert_eq!(instance.params0[3], 9.0);
        assert_eq!(instance.params1[2], KIND_SHADOW);
        assert_eq!(instance.params1[3], 0.0);
    }

    #[test]
    fn skip_transparent_and_empty() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: rect(0.0, 0.0, 10.0, 10.0),
                    corner_radius: [0.0; 4],
                    paint: QuadPaint::Solid {
                        fill: Color::TRANSPARENT,
                        border: None,
                    },
                }),
            ));
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: Rect::default(),
                    corner_radius: [0.0; 4],
                    paint: solid(),
                }),
            ));
            assert!(scene.push(
                lane,
                Primitive::GlyphRun(GlyphRun {
                    color: Color::TRANSPARENT,
                    glyphs: vec![],
                }),
            ));
        });
        let buffer = encode(&scene);
        assert!(buffer.instances.is_empty());
    }

    #[test]
    fn clip_chain_dedup() {
        let clip = ClipRect {
            rect: rect(0.0, 0.0, 30.0, 30.0),
            corner_radius: [0.0; 4],
        };
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        let b = scene.new_lane();
        for lane in [a, b] {
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Push(clip))));
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        }
        scene.set_order(&[a, b]);
        let buffer = encode(&scene);
        assert_eq!(buffer.instances.len(), 2);
        assert_eq!(
            buffer.instances[0].params1[0],
            buffer.instances[1].params1[0]
        );
        assert_eq!(buffer.instances[0].params1[0], 1.0);
        assert_eq!(buffer.clips.len(), 2, "哨兵 + 一条去重后的链");
    }

    #[test]
    fn frame_ops_preserve_order() {
        let geometry = GeometryId::for_test(0, 1);
        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(lane, quad_primitive(solid())));
        assert!(scene.push(
            lane,
            Primitive::Geometry(GeometryDraw {
                id: geometry,
                transform: Transform::IDENTITY,
            }),
        ));
        assert!(scene.push(lane, quad_primitive(solid())));
        scene.set_order(&[lane]);
        let buffer = encode(&scene);
        assert_eq!(buffer.ops.len(), 3);
        assert!(matches!(
            buffer.ops[0],
            super::FrameOp::Instances { start: 0, len: 1 }
        ));
        assert!(matches!(
            buffer.ops[1],
            super::FrameOp::Geometry { index: 0, .. }
        ));
        assert!(matches!(
            buffer.ops[2],
            super::FrameOp::Instances { start: 1, len: 1 }
        ));
    }

    #[test]
    fn image_maps_to_atlas_quad() {
        let tile = AtlasTile {
            atlas: AtlasKind::Image,
            u0: 0.1,
            v0: 0.2,
            u1: 0.3,
            v1: 0.4,
            generation: 0,
        };
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Image(crate::paint::primitive::Image {
                    bounds: rect(1.0, 2.0, 10.0, 20.0),
                    tile,
                    modulation: Color::WHITE,
                }),
            ));
        });
        let buffer = encode(&scene);
        assert_eq!(buffer.instances.len(), 1);
        let instance = buffer.instances[0];
        assert_eq!(instance.bounds, [2.0, 4.0, 20.0, 40.0]);
        assert_eq!(instance.params1[1], super::TEXTURE_IMAGE);
        assert_eq!(instance.params1[2], super::KIND_IMAGE);
        assert_eq!(instance.uv, [0.1, 0.2, 0.3, 0.4]);
        assert_eq!(instance.params1[3], FLAG_OPAQUE);
    }

    #[test]
    fn opaque_flag_and_idempotence() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(
                lane,
                quad_primitive(QuadPaint::Solid {
                    fill: Color::srgb(1.0, 1.0, 1.0, 0.5),
                    border: None,
                }),
            ));
        });
        let first = encode(&scene);
        assert_eq!(first.instances[0].params1[3], FLAG_OPAQUE);
        assert_eq!(first.instances[1].params1[3], 0.0);

        let second = encode(&scene);
        assert_eq!(first.instances[0].params1, second.instances[0].params1);
        assert_eq!(first.instances[0].color0, second.instances[0].color0);
    }
}

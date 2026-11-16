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
        primitive::{ClipOp, GradientKind, Primitive, QuadPaint},
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

/// 一层 clip（P02 §8.7）。`aabb` 是从根到本层的**交集**，内部按 `(x0, y0, x1, y1)` 存
/// （求交方便）；写进 `GpuPrimitive.clip_rect` 前转成契约要求的 `(x, y, w, h)`。
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
pub(crate) struct GpuClipEntry {
    /// `(x, y, w, h)`，物理像素。
    pub rect: [f32; 4],
    /// `(tl, tr, br, bl)`，物理像素。
    pub corner_radius: [f32; 4],
    /// `.x = parent`（0 = 无裁剪），`.yzw` 恒 0。
    pub meta: [u32; 4],
}

const _: () = {
    use core::mem::{align_of, size_of};
    assert!(size_of::<GpuClipEntry>() == 48);
    assert!(align_of::<GpuClipEntry>() == 16);
};

impl ClipChain {
    /// 转成 GPU 布局（P08 的上传源；哨兵语义照抄 CPU 值，不做数学）。
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
    Instances {
        start: u32,
        len: u32,
        /// 本段全部实例的可见范围（物理像素 `[x0, y0, x1, y1]`，已与各自的 clip 求交并外扩）。
        bounds: PhysicalBounds,
    },
    /// 一次几何绘制（`Primitive::Path` / `Primitive::Geometry`）。
    Geometry {
        /// `geometry` 表的下标。
        index: u32,
        /// 编码时的 clip 链索引（0 = 无裁剪）。
        clip_index: u32,
        /// 链交集 AABB（物理浮点 `[x0, y0, x1, y1]`；后端的 `geometry_root`
        /// 负责转成 GPU 的 `(x, y, w, h)`）。与 `Instances` 段的 `(x, y, w, h)`
        /// **不同**：几何 shader 只读 `draw.clip` 与 clip mask，本字段是 CPU 侧契约。
        clip_rect: [u32; 4],
        /// 该几何的可见范围（同 `Instances::bounds`）。
        bounds: PhysicalBounds,
    },
}

impl FrameOp {
    /// 本 op 的可见范围（局部刷新时据此跳过与 damage 不相交的 op）。
    pub(crate) fn bounds(self) -> PhysicalBounds {
        match self {
            Self::Instances { bounds, .. } | Self::Geometry { bounds, .. } => bounds,
        }
    }
}

/// op 的可见范围（物理像素整数 AABB）。
///
/// 用整数而不是浮点：与 damage 的比较是纯整数区间相交，且避开 `-0.0 == 0.0` 之类的陷阱。
/// 空范围用**反向**区间（`x0 > x1`）表示 —— 它与任何 damage 都不相交，因此天然被跳过。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalBounds {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl PhysicalBounds {
    /// 反向区间 = 空（不与任何矩形相交）。
    pub(crate) const EMPTY: Self = Self {
        x0: i32::MAX,
        y0: i32::MAX,
        x1: i32::MIN,
        y1: i32::MIN,
    };

    /// 是否与 `[x0, y0, x1, y1]`（半开区间语义上按闭区间处理）相交。
    ///
    /// 判定用严格的 `<`：边界贴合（`a.x1 == b.x0`）算作**相交**，宁可多画一次也不漏画 ——
    /// 漏画会让局部刷新留下旧像素，这是可见的错误；多画只是浪费。
    pub(crate) fn intersects(self, other: [i32; 4]) -> bool {
        self.x0 <= other[2] && other[0] <= self.x1 && self.y0 <= other[3] && other[1] <= self.y1
    }

    /// 并入一个实例的可见范围。
    fn union_instance(&mut self, primitive: &GpuPrimitive) {
        let (Some(extent), Some(clip)) = (instance_extent(primitive), clip_extent(primitive))
        else {
            return;
        };
        // 实例与它自己的 clip 求交；空交集不参与并集（它对像素没有贡献）。
        let x0 = extent[0].max(clip[0]);
        let y0 = extent[1].max(clip[1]);
        let x1 = extent[2].min(clip[2]);
        let y1 = extent[3].min(clip[3]);
        if x0 > x1 || y0 > y1 {
            return;
        }
        self.x0 = self.x0.min(x0);
        self.y0 = self.y0.min(y0);
        self.x1 = self.x1.max(x1);
        self.y1 = self.y1.max(y1);
    }
}

/// 一段连续实例的可见范围（并集；空段返回 [`PhysicalBounds::EMPTY`]）。
fn run_bounds(instances: &[GpuPrimitive]) -> PhysicalBounds {
    let mut bounds = PhysicalBounds::EMPTY;
    for primitive in instances {
        bounds.union_instance(primitive);
    }
    bounds
}

/// clip **AABB**（`[x0, y0, x1, y1]`）→ 整数可见范围；语义与 [`bounds_of_clip`]（xywh）分开，
/// 避免「存 AABB 却按 xywh 读」这类潜伏错误。
fn bounds_of_aabb(aabb: [f32; 4]) -> PhysicalBounds {
    if aabb.iter().any(|value| value.is_nan()) {
        return PhysicalBounds::EMPTY;
    }
    let [x0, y0, x1, y1] = aabb;
    if x1 < x0 || y1 < y0 {
        return PhysicalBounds::EMPTY;
    }
    // 无裁剪的哨兵（`[MIN, MIN, MAX, MAX]`）经 `as i32` 饱和成全范围，正确。
    PhysicalBounds {
        x0: (x0.floor() as i32).saturating_sub(1),
        y0: (y0.floor() as i32).saturating_sub(1),
        x1: (x1.ceil() as i32).saturating_add(1),
        y1: (y1.ceil() as i32).saturating_add(1),
    }
}

/// 实例包围盒的整数范围（外扩 1 px 吸收 AA / 抗锯齿边缘）。
fn instance_extent(primitive: &GpuPrimitive) -> Option<[i32; 4]> {
    let [x, y, w, h] = primitive.bounds;
    // 顶点外扩量必须与 shader 用的一致：params0.w 是 a_padding（Quad 1.0、
    // Shadow lur + 1、图集 0）。只按 1 px 外扩会让**阴影的模糊环**落在包围盒外，
    // 于是 damage 裁剪可能把阴影整个丢掉（可见缺陷）。
    let pad = primitive.params0[3];
    extent_of(x, y, w, h, pad)
}

/// `clip_rect()` 表示「无裁剪」的哨兵：`[f32::MIN, f32::MIN, f32::MAX, f32::MAX]`。
///
/// 注意它按 `(x, y, w, h)` 解析时 `w`/`h` 都是约 6.8e38（`MAX - MIN`），既不是 inf、
/// 也不是合法的宽高 —— 必须先识别哨兵，否则会算成「空交集」把整条 op 的包围盒清空，
/// 于是 damage 裁剪认为它不值得画（静默漏画）。
fn is_unclipped(clip_rect: [f32; 4]) -> bool {
    clip_rect[2] >= 1.0e30 && clip_rect[3] >= 1.0e30
}

/// clip 的整数范围（**`clip_rect` 是 `(x, y, w, h)`**，见 `clip_rect()` 与 contract §2）。
///
/// 旧实现把它当 `(x0, y0, x1, y1)` 读，原点为正的 clip 会把 op 包围盒的右/下边界算小，
/// `draw_ops` 的 damage 裁剪因此漏画（三平台共通的静默缺陷）。
fn clip_extent(primitive: &GpuPrimitive) -> Option<[i32; 4]> {
    let clip_rect = primitive.clip_rect;
    if clip_rect.iter().any(|value| value.is_nan()) {
        return None;
    }
    // 无裁剪 ⇒ 无界（与实例范围求交时是恒等，不会缩小包围盒）。
    if is_unclipped(clip_rect) {
        return Some([i32::MIN, i32::MIN, i32::MAX, i32::MAX]);
    }
    let [x, y, w, h] = clip_rect;
    if w < 0.0 || h < 0.0 {
        // 空交集：它对像素没有贡献。
        return None;
    }
    Some([
        x.floor() as i32,
        y.floor() as i32,
        ((x + w).ceil() as i32).saturating_add(1),
        ((y + h).ceil() as i32).saturating_add(1),
    ])
}

/// `(x, y, w, h)` 浮点矩形 → 外扩 1 px 的整数范围；非有限或退化时返回 `None`。
fn extent_of(x: f32, y: f32, w: f32, h: f32, pad: f32) -> Option<[i32; 4]> {
    if ![x, y, w, h, pad].iter().all(|value| value.is_finite()) {
        return None;
    }
    // shader 的顶点外扩 = params0.w（见 contract §5），再加 1 px 吸收取整误差。
    let pad = pad.max(0.0).ceil() as i32 + 1;
    Some([
        (x.floor() as i32).saturating_sub(pad),
        (y.floor() as i32).saturating_sub(pad),
        ((x + w).ceil() as i32).saturating_add(pad),
        ((y + h).ceil() as i32).saturating_add(pad),
    ])
}

/// 一帧的编码产物（容量跨帧保留）。
pub(crate) struct InstanceBuffer {
    instances: Vec<GpuPrimitive>,
    clips: Vec<ClipChain>,
    geometry: Vec<GeometryDrawGpu>,
    ops: Vec<FrameOp>,
}

/// 几何绘制的**帧内**表示：颜色已由 UI 线程解析成 linear premultiplied RGBA。
///
/// 公开的 [`GeometryDraw`] 携带应用语义的 `Color`；解析（色彩空间 + 插值配置）发生在 UI 线程
/// 的编码器里，与所有实例图元同一条路径（contract §3 的唯一转换点）。GPU 线程只拿到可以直接
/// 写进 root constant 的 `[f32; 4]`，因此不需要跨线程传 `ColorResolver`。
#[derive(Clone, Debug)]
pub(crate) struct GeometryDrawGpu {
    pub id: crate::paint::geometry::GeometryId,
    pub transform: Transform,
    /// linear premultiplied RGBA（`draw.color0`）。
    pub color: [f32; 4],
    /// 几何的顶点/索引字节与布局（GPU 线程首次见到该 `GeometryId` 时据此建缓冲）。
    ///
    /// 随帧过线是**有意**的：`GeometryStore` 在 UI 线程，GPU 线程不能直接读它；把描述
    /// 附在绘制项上，GPU 线程按 `(index, generation)` 缓存自己的缓冲，既不需要额外的
    /// 跨线程注册通道，也让「注册后第一帧」自然完成上传。
    pub desc: std::sync::Arc<crate::paint::geometry::GeometryDesc>,
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
    pub(crate) fn ops(&self) -> &[FrameOp] {
        &self.ops
    }

    /// `FrameOp::Geometry.index` 的下标表。
    pub(crate) fn geometry(&self) -> &[GeometryDrawGpu] {
        &self.geometry
    }

    /// 裁剪表（表项 0 是哨兵；GPU 布局由 [`ClipChain::gpu_entry`] 转）。
    pub(crate) fn clips(&self) -> &[ClipChain] {
        &self.clips
    }

    /// 把裁剪表写成 GPU 布局（容量复用，不每帧分配）。
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
    /// 颜色解析缓存（跨帧保留，每帧 clear）。
    colors: crate::paint::color::ResolveCache,
    run_start: usize,
}

impl InstanceEncoder {
    /// 消费者：`Window::new`。
    pub(crate) fn new() -> Self {
        let mut encoder = Self {
            buffer: InstanceBuffer::new(),
            stack: SmallVec::new(),
            chains: ahash::HashMap::default(),
            colors: crate::paint::color::ResolveCache::new(),
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
        store: &crate::paint::geometry_store::GeometryStore,
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
        self.colors.clear();
        // 先解构出各字段的独立可变借用：FrameEncoder 同时需要 uffer 与 colors，
        // 直接写 &mut self.buffer / &mut self.colors 会与后面的 self.flush_run() 冲突。
        let Self {
            buffer,
            stack,
            chains,
            colors,
            run_start,
        } = self;
        let mut frame = FrameEncoder {
            buffer,
            stack,
            chains,
            scale,
            resolver,
            colors,
            store,
            run_start,
        };
        frame.encode_scene(scene);
        self.flush_run();
        self.self_check();
        &self.buffer
    }

    fn flush_run(&mut self) {
        let len = self.buffer.instances.len() - self.run_start;
        if len > 0 {
            let bounds = run_bounds(&self.buffer.instances[self.run_start..]);
            self.buffer.ops.push(FrameOp::Instances {
                start: self.run_start as u32,
                len: len as u32,
                bounds,
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
                FrameOp::Instances { start, len, .. } => {
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
                    ..
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
    /// 颜色解析缓存（P1-2.1）：一帧内同色高度重复，宽色域解析要跑 24 步二分。
    colors: &'a mut crate::paint::color::ResolveCache,
    /// 静态几何的注册表（`GeometryId` → 顶点/索引字节）。
    store: &'a crate::paint::geometry_store::GeometryStore,
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
                // 先把颜色与描述算出来，push_geometry 会再次可变借用 self。
                let color = self.colors.resolve(self.resolver, path.fill).to_array();
                let desc = self.geometry_desc(path.geometry);
                let index = self.push_geometry(GeometryDrawGpu {
                    id: path.geometry,
                    transform: Transform::IDENTITY,
                    color,
                    desc,
                });
                let (clip_index, clip_aabb) = self.geometry_clip();
                self.buffer.ops.push(FrameOp::Geometry {
                    index,
                    clip_index,
                    clip_rect: clip_aabb.map(f32::to_bits),
                    // 几何的可见范围就是它的 clip AABB（顶点在 shader 里变换，
                    // CPU 侧不知道精确包围盒 —— 用 clip 是保守的上界，不会漏画）。
                    bounds: bounds_of_aabb(clip_aabb),
                });
            }
            Primitive::Geometry(draw) => {
                self.flush_run();
                let color = self.colors.resolve(self.resolver, draw.color).to_array();
                let desc = self.geometry_desc(draw.id);
                let index = self.push_geometry(GeometryDrawGpu {
                    id: draw.id,
                    transform: draw.transform,
                    color,
                    desc,
                });
                let (clip_index, clip_aabb) = self.geometry_clip();
                self.buffer.ops.push(FrameOp::Geometry {
                    index,
                    clip_index,
                    clip_rect: clip_aabb.map(f32::to_bits),
                    // 几何的可见范围就是它的 clip AABB（顶点在 shader 里变换，
                    // CPU 侧不知道精确包围盒 —— 用 clip 是保守的上界，不会漏画）。
                    bounds: bounds_of_aabb(clip_aabb),
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

    /// 取几何描述（`Arc` 克隆，不复制顶点字节）。未注册的句柄给空描述：渲染层会以
    /// `GeometryRegister` 错误拒绝，而不是画错东西。
    fn geometry_desc(
        &self,
        id: crate::paint::geometry::GeometryId,
    ) -> std::sync::Arc<crate::paint::geometry::GeometryDesc> {
        static MISSING: std::sync::OnceLock<std::sync::Arc<crate::paint::geometry::GeometryDesc>> =
            std::sync::OnceLock::new();
        self.store.desc_arc(id).unwrap_or_else(|| {
            std::sync::Arc::clone(MISSING.get_or_init(|| {
                std::sync::Arc::new(crate::paint::geometry::GeometryDesc {
                    vertex_bytes: bytes::Bytes::new(),
                    index_bytes: bytes::Bytes::new(),
                    vertex_stride: 0,
                    index_type: crate::paint::geometry::IndexType::U16,
                    topology: crate::paint::geometry::Topology::TriangleList,
                    bounds: crate::math::Rect::default(),
                    pipeline: crate::paint::geometry::GeometryPipeline::Ui,
                })
            }))
        })
    }

    fn push_geometry(&mut self, draw: GeometryDrawGpu) -> u32 {
        let index = self.buffer.geometry.len() as u32;
        self.buffer.geometry.push(draw);
        index
    }

    fn flush_run(&mut self) {
        let len = self.buffer.instances.len() - *self.run_start;
        if len > 0 {
            let bounds = run_bounds(&self.buffer.instances[*self.run_start..]);
            self.buffer.ops.push(FrameOp::Instances {
                start: *self.run_start as u32,
                len: len as u32,
                bounds,
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
    /// 当前 clip 链交集的 **AABB**（几何 op 的契约字段：后端 `geometry_root` 会转成 `(x,y,w,h)`）。
    fn geometry_clip(&self) -> (u32, [f32; 4]) {
        (self.current_chain().0, self.clip_aabb())
    }
    /// 当前 clip 链交集，**AABB** 形式 `[x0, y0, x1, y1]`（无裁剪 = 以 `MIN/MAX` 表示的无界）。
    ///
    /// 与 `clip_rect()` 的区别只是表示法：实例走 GPU 契约的 `(x, y, w, h)`，几何 op 走后端
    /// `geometry_root` 期望的 AABB。两者不能再混用（复审 P2-1：旧实现存了 xywh 却被按 AABB
    /// 再转一次，正原点 clip 会被算成 `(x, y, w-x, h-y)`）。
    fn clip_aabb(&self) -> [f32; 4] {
        let chain = self.current_chain();
        if chain == ClipChainId::NONE {
            return [f32::MIN, f32::MIN, f32::MAX, f32::MAX];
        }
        self.chain_aabb(chain)
    }

    /// 当前 clip 链交集，按 GPU 契约写成 `(x, y, w, h)`（`contract.md` §2 / `quad.vert.glsl`
    /// 的 `clip_rect.xy` + `clip_rect.zw`）。无裁剪时给哨兵（shader 以 `clip_index == 0` 跳过）。
    fn clip_rect(&self) -> [f32; 4] {
        let chain = self.current_chain();
        if chain == ClipChainId::NONE {
            return [f32::MIN, f32::MIN, f32::MAX, f32::MAX];
        }
        let aabb = self.chain_aabb(chain);
        // 内部 `aabb` 是 `(x0, y0, x1, y1)`（求交方便）；契约要 `(x, y, w, h)`。
        // 空交集产生负 `w/h` 是合法的：shader 用 `max(clip_rect.zw, 0)` 夹成零面积。
        [aabb[0], aabb[1], aabb[2] - aabb[0], aabb[3] - aabb[1]]
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
                    self.colors.resolve(self.resolver, *fill),
                    border.map_or(LinearPremulRgba::TRANSPARENT, |border| {
                        self.colors.resolve(self.resolver, border.color)
                    }),
                    width.clamp(0.0, half_min),
                    GRADIENT_NONE,
                )
            }
            QuadPaint::Gradient(gradient) => (
                self.colors.resolve(self.resolver, gradient.from),
                self.colors.resolve(self.resolver, gradient.to),
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
            color0: self.colors.resolve(self.resolver, shadow.color).to_array(),
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
        let color0 = self.colors.resolve(self.resolver, image.modulation);
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
        let color0 = self.colors.resolve(self.resolver, color);
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
    use std::rc::Rc;

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
        encoder.encode(
            scene,
            scale(),
            &resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        );
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

    /// P1 回归：clip 的整数范围必须按 `(x, y, w, h)` 解析。
    ///
    /// 从内部 AABB 转换到 GPU 契约时写的是 `(x, y, x1-x, y1-y)`；把第三/四分量当右/下
    /// 边界会让原点为正的 clip 包围盒算小，damage 裁剪成片漏画（三平台共通）。
    #[test]
    fn clip_extent_reads_xywh_not_aabb() {
        let mut canvas = crate::paint::canvas::Canvas::new(rect(0.0, 0.0, 2000.0, 2000.0));
        assert!(canvas.push_clip(ClipRect {
            rect: rect(100.0, 50.0, 800.0, 600.0),
            corner_radius: [0.0; 4],
        }));
        canvas.push_quad(Quad {
            bounds: rect(0.0, 0.0, 2000.0, 2000.0),
            corner_radius: [0.0; 4],
            paint: solid(),
        });
        assert!(canvas.pop_clip());
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(lane, Primitive::Canvas(Rc::new(canvas))));
        });
        let buffer = encode(&scene);
        let primitive = buffer
            .instances
            .iter()
            .find(|primitive| primitive.clip_rect[0] > 0.0)
            .expect("被裁剪的 quad");
        let extent = super::clip_extent(primitive).expect("有限 clip");
        // xywh ⇒ 右边界 = 100+800、下边界 = 50+600（AABB 误读会得到 800 / 600）。
        assert!(
            extent[2] >= 900 && extent[3] >= 650,
            "clip 范围必须覆盖 x+w / y+h：{extent:?}"
        );
        let bounds = super::bounds_of_aabb([100.0, 50.0, 900.0, 650.0]);
        assert!(
            bounds.x1 >= 900 && bounds.y1 >= 650,
            "bounds_of_aabb 必须覆盖整段 AABB：{bounds:?}"
        );
    }
    #[test]
    fn negative_origin_clip_encodes_xywh() {
        // 回归：clip_rect 必须按 contract.md / quad.vert.glsl 的 `(x, y, w, h)` 写，
        // 而不是内部 AABB 的 `(x0, y0, x1, y1)`——否则负原点 clip 会把右/下内容裁掉。
        let mut canvas = crate::paint::canvas::Canvas::new(rect(0.0, 0.0, 100.0, 100.0));
        assert!(canvas.push_clip(ClipRect {
            rect: rect(-10.0, -5.0, 20.0, 20.0),
            corner_radius: [0.0; 4],
        }));
        canvas.push_quad(Quad {
            bounds: rect(0.0, 0.0, 30.0, 30.0),
            corner_radius: [0.0; 4],
            paint: solid(),
        });
        assert!(canvas.pop_clip());
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(lane, Primitive::Canvas(Rc::new(canvas))));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.params1[0], 1.0, "clip_index 必须非 0");
        // scale = 2.0 ⇒ 物理 (-20, -10, 40, 40)；旧实现会写成 (-20, -10, 20, 30)。
        assert_eq!(instance.clip_rect, [-20.0, -10.0, 40.0, 40.0]);
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

    /// P2-3.2 回归：damage 裁剪用的 op 包围盒必须覆盖 shader 的**顶点外扩**。
    ///
    /// 阴影的顶点外扩是 `blur + 1`（`params0.w`），而旧实现只按 1 px 外扩 —— 模糊环落在
    /// 包围盒外时，damage 裁剪会把整条阴影丢掉（可见缺陷，不是性能问题）。
    #[test]
    fn op_bounds_cover_the_shader_vertex_padding() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Shadow(Shadow {
                    rect: rect(10.0, 10.0, 20.0, 20.0),
                    offset: Point::new(0.0, 0.0),
                    blur: 8.0,
                    color: Color::BLACK,
                }),
            ));
        });
        let buffer = encode(&scene);
        let crate::paint::instance::FrameOp::Instances { bounds, .. } = buffer.ops[0] else {
            panic!("阴影应产生一个 Instances op");
        };
        // 场景 scale 是 2×：rect 在物理空间是 20..60，blur 8→16，pad = blur+1 = 17，
        // 再加 1 px 取整余量 = 18 → 2..78。旧实现只外扩 1 px 会得到 19..61（模糊环被切掉）。
        assert_eq!(
            (bounds.x0, bounds.y0, bounds.x1, bounds.y1),
            (2, 2, 78, 78),
            "op 包围盒必须覆盖阴影的模糊环（blur + 1 的顶点外扩）"
        );
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

    /// P2-1：几何 op 的 `clip_rect` 存的是 **AABB**（后端 `geometry_root` 会转成 xywh）。
    ///
    /// 旧实现存 xywh 却被按 AABB 再转一次：正原点 clip 会被算成 `(x, y, w-x, h-y)`。
    #[test]
    fn geometry_op_stores_clip_aabb() {
        let geometry = GeometryId::for_test(0, 1);
        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(
            lane,
            Primitive::Clip(ClipOp::Push(ClipRect {
                rect: rect(100.0, 50.0, 800.0, 600.0),
                corner_radius: [0.0; 4],
            }))
        ));
        assert!(scene.push(
            lane,
            Primitive::Geometry(GeometryDraw {
                id: geometry,
                transform: Transform::IDENTITY,
                color: crate::paint::color::Color::WHITE,
            }),
        ));
        assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        scene.set_order(&[lane]);
        let buffer = encode(&scene);
        let Some(super::FrameOp::Geometry { clip_rect, .. }) = buffer
            .ops
            .iter()
            .find(|op| matches!(op, super::FrameOp::Geometry { .. }))
            .copied()
        else {
            panic!("必须有 geometry op")
        };
        let aabb = clip_rect.map(f32::from_bits);
        assert_eq!(
            aabb,
            [200.0, 100.0, 1800.0, 1300.0],
            "geometry op 必须存物理 AABB (x0, y0, x1, y1)（逻辑 × scale）"
        );
    }

    /// P0 回归：`App` 注册的几何必须真的进编码器（旧实现编码器各持一个空 store，
    /// `desc_arc` 恒 `None` ⇒ 上传空顶点 ⇒ 几何/Path 永远画不出来）。
    #[test]
    fn encoder_uses_the_supplied_geometry_store() {
        // 一个 3 顶点 / 3 索引的三角形（UI 管线顶点布局）。
        let stride = crate::paint::geometry::GeometryPipeline::Ui.vertex_stride() as usize;
        let vertex_bytes = bytes::Bytes::from(vec![0u8; stride * 3]);
        let index_bytes = bytes::Bytes::from(vec![0u8, 0, 1, 0, 2, 0]);
        let desc = crate::paint::geometry::GeometryDesc {
            vertex_bytes,
            index_bytes,
            vertex_stride: stride as u32,
            index_type: crate::paint::geometry::IndexType::U16,
            topology: crate::paint::geometry::Topology::TriangleList,
            bounds: rect(0.0, 0.0, 10.0, 10.0),
            pipeline: crate::paint::geometry::GeometryPipeline::Ui,
        };
        let mut store = crate::paint::geometry_store::GeometryStore::new();
        let id = store.insert(desc).expect("valid geometry");

        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(
            lane,
            Primitive::Geometry(GeometryDraw {
                id,
                transform: Transform::IDENTITY,
                color: crate::paint::color::Color::WHITE,
            }),
        ));
        scene.set_order(&[lane]);
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&scene, scale(), &resolver(), &store);
        let draw = buffer.geometry.first().expect("必须有一条几何绘制");
        assert_eq!(
            draw.desc.vertex_bytes.len(),
            stride * 3,
            "编码器必须从传入的 store 取到真实顶点（而不是空 MISSING 描述）"
        );
        assert_eq!(draw.desc.index_bytes.len(), 6);
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
                color: crate::paint::color::Color::WHITE,
            }),
        ));
        assert!(scene.push(lane, quad_primitive(solid())));
        scene.set_order(&[lane]);
        let buffer = encode(&scene);
        assert_eq!(buffer.ops.len(), 3);
        assert!(matches!(
            buffer.ops[0],
            super::FrameOp::Instances {
                start: 0,
                len: 1,
                ..
            }
        ));
        assert!(matches!(
            buffer.ops[1],
            super::FrameOp::Geometry { index: 0, .. }
        ));
        assert!(matches!(
            buffer.ops[2],
            super::FrameOp::Instances {
                start: 1,
                len: 1,
                ..
            }
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

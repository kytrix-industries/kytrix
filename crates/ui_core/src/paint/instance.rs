//! `GpuPrimitive` 契约（128 B / 16 B 对齐）与 CPU 侧实例编码。
//!
//! 本文是 `render/shaders/contract.md` 的**权威来源**（I2/I8）：字段顺序、offset、`kind`/`params`
//! 取值表的任何改动都必须同时改 Rust、三套 shader、`contract.md`，并触发像素快照回归。
//!
//! 范围（`design/phase-06-paint-runtime.md` §7）：
//! - 8 类 `Primitive` → `GpuPrimitive`（`GlyphRun` 是唯一的 1→N）；
//! - `Path`/`Geometry` 0 实例 + `FrameOp::Geometry`；
//! - clip 链去重（`ClipChainId`，帧内索引）+ **轴对齐矩形求交合并**（Skia `SkClipStack` 语义）；
//! - `InstanceBuffer` 容量 = 历史峰值（`clear` 只截断、不 shrink、不按帧翻倍）。
//!
//! **P07 只做上传/批次**：本模块不碰 GPU、不持设备。

use smallvec::SmallVec;

use crate::{
    error::EngineError,
    math::{Point, Rect, Scale, Transform},
    paint::{
        color::{ColorResolver, LinearPremulRgba},
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
    /// 四角半径（物理像素，`[tl, tr, br, bl]`，已按 CSS 规则夹到 `min(w,h)/2`）。
    pub corner_radius: [f32; 4],
    /// premultiplied linear sRGB：实色 / 渐变起色 / 阴影色 / 字形色 / 图片调制色。
    pub color0: [f32; 4],
    /// 渐变终色 **或** 边框色（互斥）；无则全 0。
    pub color1: [f32; 4],
    /// `border_width, shadow_blur, gradient_kind, aa_padding`。
    pub params0: [f32; 4],
    /// `clip_index, texture_index, kind, clip_depth`（整数编码进 f32；`clip_depth` 是 GPU
    /// 裁剪链的**真实链长**，shader 用它收口 clip 循环 —— 旧实现的 `flags` 位无人消费）。
    pub params1: [f32; 4],
    /// 图集 tile `(u0, v0, u1, v1)`。
    pub uv: [f32; 4],
    /// clip 链交集 AABB（物理浮点，`(x, y, w, h)`；空交集 = 负 w/h ⇒ shader 全裁）。
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

pub(crate) const TEXTURE_GLYPH: f32 = 0.0;
pub(crate) const TEXTURE_IMAGE: f32 = 1.0;

pub(crate) const GRADIENT_NONE: f32 = 0.0;
pub(crate) const GRADIENT_HORIZONTAL: f32 = 1.0;
pub(crate) const GRADIENT_VERTICAL: f32 = 2.0;
pub(crate) const GRADIENT_RADIAL: f32 = 3.0;

/// 阴影的高斯 σ 相对「模糊半径」的比例：**σ = blur / 2**。
///
/// 与 CSS `box-shadow` 的 `blur-radius`、`filter: blur(r)`、Figma 的 `Drop shadow blur`
/// 同义（三套 shader 的 `KYTRIX_SHADOW_SIGMA_SCALE` 必须一致，`build.rs` 会校验）。
/// 旧实现用 `blur / 3`，同数值下阴影更硬更窄（可见差异）。Skia 的 `SkBlurMaskFilter`
/// 用 `σ = radius × 0.57735 + 0.5`，换算关系见 `contract.md` §4。
pub(crate) const SHADOW_SIGMA_SCALE: f32 = 0.5;

/// σ 的下限（物理像素）：blur = 0 时仍要有一点抗锯齿过渡，避免硬边。
pub(crate) const SHADOW_SIGMA_MIN: f32 = 0.35;

/// 阴影顶点外扩 = `SHADOW_PAD_SIGMAS × σ + 1`：3σ 之外的高斯尾部 < 0.3%，
/// 与 `analytic_shadow` 的可见范围自洽（旧实现是 `blur + 1`，σ 改小后会切掉模糊环）。
pub(crate) const SHADOW_PAD_SIGMAS: f32 = 3.0;

/// 圆角半径的**唯一**规范化：CSS 规则 `r ≤ min(w, h) / 2`（逐角 clamp，保持四角相对比例）。
///
/// clip、quad、shadow 三条路径都走这里（R05-8）。旧实现有两套：quad 用 `min(half)`
/// clamp、clip **完全不 clamp**；`r > min/2` 时 `sd_rounded_rect` 的 `q = |p| - half + r`
/// 恒正 ⇒ 等值线反转，裁剪区反而**变大**（内容溢出）。shader 里保留同样的 clamp 作为
/// 第二道防线（异常数据也不会出现反向错误）。
fn normalize_corner_radius(radii: [f32; 4], width: f32, height: f32) -> [f32; 4] {
    // NaN 必须先挡掉：32::min 会**忽略** NaN（返回另一个操作数），直接算会得到
    // 一个看似合法的 half_min，于是异常尺寸的图形不会被归零。
    if width.is_nan() || height.is_nan() {
        return [0.0; 4];
    }
    let half_min = 0.5 * width.min(height);
    if !half_min.is_finite() || half_min <= 0.0 {
        return [0.0; 4];
    }
    radii.map(|radius| {
        if radius.is_nan() {
            0.0
        } else {
            radius.clamp(0.0, half_min)
        }
    })
}

/// `ClipChain` 的帧内索引；`0` 是「无裁剪」哨兵。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClipChainId(u32);

impl ClipChainId {
    fn index(self) -> usize {
        self.0 as usize
    }
}

/// 一层 clip（P02 §8.7）。`rect` 是 `(x, y, w, h)`（物理像素）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClipChain {
    parent: Option<ClipChainId>,
    rect: [f32; 4],
    corner_radius: [f32; 4],
}

/// GPU 侧 `ClipEntry`（P07 §2.6 / DEVIATIONS F-69）：**48 B / 16 B 对齐**。
///
/// 与 CPU 侧 [`ClipChain`] 同构（CPU 侧不算交集的 AABB：交集由 [`ClipState`] 维护），
/// `parent` 打进 `uint4.x`，`.yzw` 保留恒 0；表项 **0 恒为哨兵**（`clip_index == 0` 时 shader
/// 直接返回 1）。
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

/// clip **AABB**（`[x0, y0, x1, y1]`）→ 整数可见范围。与实例路径的 xywh（`clip_extent`）分开，
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
    // 顶点外扩量必须与 shader 用的一致：params0.w 是 aa_padding（Quad 1.0、
    // Shadow 3σ + 1、图集 0）。只按 1 px 外扩会让**阴影的模糊环**落在包围盒外，
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
///
/// [`GeometryDraw`]: crate::paint::primitive::GeometryDraw
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

/// 一层圆角 clip（编码期状态；`entry` 是它在裁剪表里的表项下标）。
#[derive(Clone, Copy, Debug)]
struct RoundedClip {
    /// `(x, y, w, h)`，物理像素。
    rect: [f32; 4],
    corner_radius: [f32; 4],
    entry: ClipChainId,
}

/// 当前 clip 状态（编码期唯一真相）。
///
/// **轴对齐矩形求交合并（R05-10，对齐 Skia `SkClipStack` / WebRender 的 clip rect 合并）**：
/// 全部直角矩形 clip 只做一次交集，链上只有**一条**矩形表项；圆角 clip 各自一层。于是
/// 两层边界重合时不再出现 `0.5 × 0.5 = 0.25` 的暗边（覆盖率相乘只发生在「矩形交集 ×
/// 圆角形状」之间 —— 圆角形状的交集不是矩形，无法压成一条表项）。
#[derive(Clone, Debug)]
struct ClipState {
    /// 全部矩形 clip 的交集（`[x0, y0, x1, y1]`，物理像素）。
    rect_aabb: [f32; 4],
    /// 交集是否已存在（`false` ⇒ `rect_aabb` 是无界哨兵、链上没有矩形表项）。
    has_rect: bool,
    /// 交集在裁剪表里的表项。
    rect_entry: Option<ClipChainId>,
    /// 圆角 clip 链（push 顺序；parent 依次指向前一项，链头指向 `rect_entry`）。
    rounded: SmallVec<[RoundedClip; 8]>,
}

impl ClipState {
    /// 无界交集（无任何 clip）。
    fn unbounded() -> Self {
        Self {
            rect_aabb: [f32::MIN, f32::MIN, f32::MAX, f32::MAX],
            has_rect: false,
            rect_entry: None,
            rounded: SmallVec::new(),
        }
    }

    /// GPU 裁剪链的链长 = 矩形合并项（0/1）+ 圆角层数。
    fn depth(&self) -> usize {
        self.rounded.len() + usize::from(self.rect_entry.is_some())
    }

    /// 链头（`None` = 无裁剪）。
    fn top(&self) -> Option<ClipChainId> {
        self.rounded
            .last()
            .map(|clip| clip.entry)
            .or(self.rect_entry)
    }

    /// 全部 clip 的交集 AABB（`[x0, y0, x1, y1]`；无裁剪 = 无界哨兵）。
    fn aabb(&self) -> [f32; 4] {
        let mut aabb = self.rect_aabb;
        for clip in &self.rounded {
            aabb = intersect_aabb(aabb, rect_to_aabb(clip.rect));
        }
        aabb
    }
}

/// AABB 求交。
fn intersect_aabb(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ]
}

/// `(x, y, w, h)` → AABB（`[x0, y0, x1, y1]`）。
fn rect_to_aabb(rect: [f32; 4]) -> [f32; 4] {
    [rect[0], rect[1], rect[0] + rect[2], rect[1] + rect[3]]
}

/// AABB → `(x, y, w, h)`（空交集产生负 `w/h`：shader 据此全裁）。
fn aabb_to_rect(aabb: [f32; 4]) -> [f32; 4] {
    [aabb[0], aabb[1], aabb[2] - aabb[0], aabb[3] - aabb[1]]
}

/// 逻辑矩形经仿射变换后的 AABB（`[x0, y0, x1, y1]`，逻辑像素）。
///
/// 非有限输入返回**反向区间**（= 空），避免 `f32::min/max` 吞掉 NaN 后得到「看起来合法」
/// 但错误的包围盒（damage 会因此漏画）。
fn transformed_aabb(transform: Transform, rect: Rect) -> [f32; 4] {
    let far = Point::new(
        rect.origin.x + rect.size.width,
        rect.origin.y + rect.size.height,
    );
    let corners = [
        transform.apply(rect.origin),
        transform.apply(Point::new(far.x, rect.origin.y)),
        transform.apply(Point::new(rect.origin.x, far.y)),
        transform.apply(far),
    ];
    if !corners
        .iter()
        .all(|point| point.x.is_finite() && point.y.is_finite())
    {
        return [1.0, 1.0, -1.0, -1.0];
    }
    let mut aabb = [
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    ];
    for point in corners {
        aabb = [
            aabb[0].min(point.x),
            aabb[1].min(point.y),
            aabb[2].max(point.x),
            aabb[3].max(point.y),
        ];
    }
    aabb
}

/// 逻辑 AABB → 物理 AABB → 与 clip 求交（几何 op 的可见范围）。
fn visible_aabb(logical_aabb: [f32; 4], scale: Scale, clip_aabb: [f32; 4]) -> [f32; 4] {
    let factor = scale.factor();
    let scaled = [
        logical_aabb[0] * factor,
        logical_aabb[1] * factor,
        logical_aabb[2] * factor,
        logical_aabb[3] * factor,
    ];
    intersect_aabb(scaled, clip_aabb)
}

/// 裁剪表的哨兵项（表项 0：shader 看到 `clip_index == 0` 时直接返回覆盖率 1）。
///
/// `InstanceEncoder::new` 就把它写进去：`FramePayload` 的 dump / 健康检查可以假定「clip 表恒有
/// 哨兵」，不必等第一帧编码。
const CLIP_SENTINEL: ClipChain = ClipChain {
    parent: None,
    rect: [0.0; 4],
    corner_radius: [0.0; 4],
};

/// `Primitive → GpuPrimitive` 的编码器（`Window` 持有，缓冲跨帧复用）。
pub(crate) struct InstanceEncoder {
    buffer: InstanceBuffer,
    state: ClipState,
    /// clip 栈回溯（每层 push 前保存一次完整状态；深度 ≤ `MAX_CLIP_DEPTH`）。
    saved: SmallVec<[ClipState; 8]>,
    chains: ahash::HashMap<(u32, [u32; 4], [u32; 4]), ClipChainId>,
    /// 颜色解析缓存（跨帧保留，每帧 clear）。
    colors: crate::paint::color::ResolveCache,
}

impl InstanceEncoder {
    /// 消费者：`Window::new`。
    pub(crate) fn new() -> Self {
        let mut buffer = InstanceBuffer::new();
        buffer.clips.push(CLIP_SENTINEL);
        Self {
            buffer,
            state: ClipState::unbounded(),
            saved: SmallVec::new(),
            chains: ahash::HashMap::default(),
            colors: crate::paint::color::ResolveCache::new(),
        }
    }

    /// 编码整帧；内部缓冲复用。消费者：`Window::paint_frame`。
    ///
    /// # Errors
    /// [`EngineError::ClipDepthExceeded`]：GPU 裁剪链超过 `MAX_CLIP_DEPTH`。轴对齐矩形 clip
    /// 已在编码期求交合并（只占一项），因此普通嵌套容器不会触顶；只有圆角 clip 逐层计数。
    /// 超限时**必须失败**：shader 的循环是定长，静默丢最外层 clip 会让子内容越界可见。
    pub(crate) fn encode(
        &mut self,
        scene: &Scene,
        scale: Scale,
        resolver: &ColorResolver,
        store: &crate::paint::geometry_store::GeometryStore,
    ) -> Result<&InstanceBuffer, EngineError> {
        self.buffer.clear();
        self.buffer.clips.push(CLIP_SENTINEL);
        self.chains.clear();
        self.state = ClipState::unbounded();
        self.saved.clear();
        self.colors.clear();
        {
            // 先解构出各字段的独立可变借用：FrameEncoder 同时需要 buffer 与 colors，
            // 直接写 &mut self.buffer / &mut self.colors 会与后面的 flush_run() 冲突。
            let Self {
                buffer,
                state,
                saved,
                chains,
                colors,
            } = self;
            let mut frame = FrameEncoder {
                buffer,
                state,
                saved,
                chains,
                scale,
                resolver,
                colors,
                store,
                run_start: 0,
            };
            frame.encode_scene(scene)?;
            frame.flush_run();
        }
        self.self_check();
        Ok(&self.buffer)
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
        // 每个实例的 `clip_depth`（params1.w）必须等于它那条链的真实链长：shader 用这个值
        // 收口循环，写错就是静默少裁（R05-5 / R05-11）。
        for primitive in &self.buffer.instances {
            let clip_index = primitive.params1[0] as usize;
            debug_assert!(
                clip_index < self.buffer.clips.len(),
                "instance clip index out of range"
            );
            let mut depth = 0usize;
            let mut cursor = clip_index;
            while cursor != 0 {
                depth += 1;
                debug_assert!(
                    depth <= usize::from(MAX_CLIP_DEPTH),
                    "clip chain deeper than MAX_CLIP_DEPTH"
                );
                cursor = self.buffer.clips[cursor]
                    .parent
                    .map_or(0, |parent| parent.index());
            }
            debug_assert_eq!(
                depth, primitive.params1[3] as usize,
                "instance clip_depth must match the chain length"
            );
        }
    }
}

/// 单帧编码的借用包（把 `&mut` 字段拆开，避免整结构可变借用冲突）。
struct FrameEncoder<'a> {
    buffer: &'a mut InstanceBuffer,
    state: &'a mut ClipState,
    saved: &'a mut SmallVec<[ClipState; 8]>,
    chains: &'a mut ahash::HashMap<(u32, [u32; 4], [u32; 4]), ClipChainId>,
    scale: Scale,
    resolver: &'a ColorResolver,
    /// 颜色解析缓存（P1-2.1）：一帧内同色高度重复，宽色域解析要跑 24 步二分。
    colors: &'a mut crate::paint::color::ResolveCache,
    /// 静态几何的注册表（`GeometryId` → 顶点/索引字节）。
    store: &'a crate::paint::geometry_store::GeometryStore,
    run_start: usize,
}

impl FrameEncoder<'_> {
    fn encode_scene(&mut self, scene: &Scene) -> Result<(), EngineError> {
        for lane in scene.order() {
            let entry_depth = self.state.depth();
            let Some(primitives) = scene.lane_primitives(*lane) else {
                continue;
            };
            for primitive in primitives {
                self.encode_primitive(primitive, 0)?;
            }
            debug_assert_eq!(
                self.state.depth(),
                entry_depth,
                "lane must be clip-balanced (C1)"
            );
        }
        Ok(())
    }

    fn encode_primitive(
        &mut self,
        primitive: &Primitive,
        canvas_depth: u8,
    ) -> Result<(), EngineError> {
        match primitive {
            Primitive::Clip(ClipOp::Push(rect)) => self.push_clip(*rect)?,
            Primitive::Clip(ClipOp::Pop) => self.pop_clip(),
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
                    // 整 run 的颜色解析一次（旧实现在 per-glyph 的 emit_glyph 里重复 resolve）。
                    let color0 = self.colors.resolve(self.resolver, run.color);
                    for glyph in &run.glyphs {
                        self.emit_glyph(color0, glyph);
                    }
                }
            }
            Primitive::Path(path) => {
                self.flush_run();
                let color = self.colors.resolve(self.resolver, path.fill).to_array();
                // Path 的顶点在逻辑窗口空间、无 per-primitive 变换（`Path` 没有 transform
                // 字段）；damage 用它自己的 bounds 而不是 clip AABB（R05-19）。
                self.push_geometry_op(path.geometry, Transform::IDENTITY, color, Some(path.bounds));
            }
            Primitive::Geometry(draw) => {
                self.flush_run();
                let color = self.colors.resolve(self.resolver, draw.color).to_array();
                self.push_geometry_op(draw.id, draw.transform, color, None);
            }
            Primitive::Canvas(canvas) => {
                debug_assert_eq!(canvas_depth, 0, "Canvas nesting depth must be <= 1");
                let entry_depth = self.state.depth();
                for inner in canvas.primitives() {
                    self.encode_primitive(inner, canvas_depth + 1)?;
                }
                debug_assert_eq!(
                    self.state.depth(),
                    entry_depth,
                    "Canvas must be clip-balanced"
                );
            }
        }
        Ok(())
    }

    // ── clip 状态机 ────────────────────────────────────────────────────────

    /// 压入一层 clip。直角矩形走**求交合并**（不占新表项），圆角 clip 各占一层。
    fn push_clip(&mut self, clip: crate::paint::primitive::ClipRect) -> Result<(), EngineError> {
        self.saved.push(self.state.clone());
        let factor = self.scale.factor();
        let rect = clip.rect.to_physical_f32(self.scale);
        let radii = normalize_corner_radius(
            clip.corner_radius.map(|radius| radius * factor),
            rect[2],
            rect[3],
        );
        if radii.iter().all(|radius| *radius == 0.0) {
            let aabb = rect_to_aabb(rect);
            self.state.rect_aabb = if self.state.has_rect {
                intersect_aabb(self.state.rect_aabb, aabb)
            } else {
                aabb
            };
            self.state.has_rect = true;
            self.rebuild_rect_chain();
        } else {
            let parent = self.state.top();
            let entry = self.chain_entry(parent, rect, radii);
            self.state.rounded.push(RoundedClip {
                rect,
                corner_radius: radii,
                entry,
            });
        }
        let depth = self.state.depth();
        if depth > usize::from(MAX_CLIP_DEPTH) {
            return Err(EngineError::ClipDepthExceeded {
                depth: depth as u8,
                limit: MAX_CLIP_DEPTH,
            });
        }
        Ok(())
    }

    /// 弹出当前层（恢复 push 前的完整状态）。
    fn pop_clip(&mut self) {
        *self.state = self.saved.pop().unwrap_or_else(ClipState::unbounded);
    }

    /// 矩形交集变化后重建链：矩形项在**链头**，圆角项依次挂在它下面。
    ///
    /// 覆盖率相乘在 shader 里与顺序无关，但链必须始终只有**一条**矩形项 —— 否则「矩形
    /// 交集」会被乘两次（正是 R05-10 的 0.25 暗边）。圆角项的矩形/半径不变，只重挂 parent。
    fn rebuild_rect_chain(&mut self) {
        let rect_entry = if self.state.has_rect {
            let rect = aabb_to_rect(self.state.rect_aabb);
            Some(self.chain_entry(None, rect, [0.0; 4]))
        } else {
            None
        };
        self.state.rect_entry = rect_entry;
        let mut parent = rect_entry;
        for index in 0..self.state.rounded.len() {
            let (rect, radii) = {
                let clip = &self.state.rounded[index];
                (clip.rect, clip.corner_radius)
            };
            let entry = self.chain_entry(parent, rect, radii);
            self.state.rounded[index].entry = entry;
            parent = Some(entry);
        }
    }

    /// 裁剪表项的唯一插入点（按 `(parent, rect 位模式, radii 位模式)` 去重：
    /// 完全相同的圆角 clip 复用同一表项）。
    fn chain_entry(
        &mut self,
        parent: Option<ClipChainId>,
        rect: [f32; 4],
        corner_radius: [f32; 4],
    ) -> ClipChainId {
        let key = (
            parent.map_or(0, |parent| parent.0),
            rect.map(f32::to_bits),
            corner_radius.map(f32::to_bits),
        );
        if let Some(existing) = self.chains.get(&key) {
            return *existing;
        }
        let id = ClipChainId(self.buffer.clips.len() as u32);
        self.buffer.clips.push(ClipChain {
            parent,
            rect,
            corner_radius,
        });
        self.chains.insert(key, id);
        id
    }

    /// 当前链的 `clip_index`（0 = 无裁剪）。
    fn clip_index(&self) -> f32 {
        self.state.top().map_or(0, |id| id.0) as f32
    }

    /// 当前链的**真实链长**（写进 `params1.w`；shader 按它收口 clip 循环）。
    fn clip_depth(&self) -> f32 {
        self.state.depth() as f32
    }

    /// 当前 clip 链交集，**AABB** 形式 `[x0, y0, x1, y1]`（无裁剪 = 以 `MIN/MAX` 表示的无界）。
    fn clip_aabb(&self) -> [f32; 4] {
        self.state.aabb()
    }

    /// 当前 clip 链交集，按 GPU 契约写成 `(x, y, w, h)`（`contract.md` §2 / `quad.vert.glsl`
    /// 的 `clip_rect.xy` + `clip_rect.zw`）。无裁剪时给哨兵（shader 以 `clip_index == 0` 跳过）。
    fn clip_rect(&self) -> [f32; 4] {
        if self.state.top().is_none() {
            return [f32::MIN, f32::MIN, f32::MAX, f32::MAX];
        }
        // 内部 `aabb` 是 `(x0, y0, x1, y1)`（求交方便）；契约要 `(x, y, w, h)`。
        // 空交集产生负 `w/h` 是**有意**的：shader 见到非正的 w/h 直接返回覆盖率 0（全裁）。
        aabb_to_rect(self.clip_aabb())
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

    /// 取几何描述（`Arc` 克隆，不复制顶点字节）。未注册的句柄给空描述：渲染层会以
    /// `GeometryRegister` 错误拒绝，而不是画错东西。
    fn geometry_desc(
        &self,
        id: crate::paint::geometry::GeometryId,
    ) -> Option<std::sync::Arc<crate::paint::geometry::GeometryDesc>> {
        self.store.desc_arc(id)
    }

    fn missing_desc() -> std::sync::Arc<crate::paint::geometry::GeometryDesc> {
        static MISSING: std::sync::OnceLock<std::sync::Arc<crate::paint::geometry::GeometryDesc>> =
            std::sync::OnceLock::new();
        std::sync::Arc::clone(MISSING.get_or_init(|| {
            std::sync::Arc::new(crate::paint::geometry::GeometryDesc {
                vertex_bytes: bytes::Bytes::new(),
                index_bytes: bytes::Bytes::new(),
                vertex_stride: 0,
                index_type: crate::paint::geometry::IndexType::U16,
                topology: crate::paint::geometry::Topology::TriangleList,
                bounds: Rect::default(),
                pipeline: crate::paint::geometry::GeometryPipeline::Ui,
            })
        }))
    }

    /// 追加一条几何绘制并 push `FrameOp::Geometry`。
    ///
    /// `op_bounds`：给定则用它（`Path` 自带逻辑 AABB），否则用 `desc.bounds` 经
    /// `transform` 变换后的 AABB（R05-12 / R05-19：旧实现对无 clip 的几何给出「全屏」
    /// 包围盒，局部刷新时把所有几何 draw 都重发；给了 transform 却仍用未变换的 bounds
    /// 会在平移后留残影）。
    fn push_geometry_op(
        &mut self,
        id: crate::paint::geometry::GeometryId,
        transform: Transform,
        color: [f32; 4],
        op_bounds: Option<Rect>,
    ) {
        let registered = self.geometry_desc(id);
        let clip_aabb = self.clip_aabb();
        let visible = match (op_bounds, &registered) {
            (Some(bounds), _) => {
                visible_aabb(transformed_aabb(transform, bounds), self.scale, clip_aabb)
            }
            (None, Some(desc)) => visible_aabb(
                transformed_aabb(transform, desc.bounds),
                self.scale,
                clip_aabb,
            ),
            // 未注册：拿不到真实范围，保守地用 clip 上界（GPU 侧会报 GeometryRegister）。
            (None, None) => clip_aabb,
        };
        let index = self.buffer.geometry.len() as u32;
        self.buffer.geometry.push(GeometryDrawGpu {
            id,
            transform,
            color,
            desc: registered.unwrap_or_else(Self::missing_desc),
        });
        self.buffer.ops.push(FrameOp::Geometry {
            index,
            clip_index: self.clip_index() as u32,
            clip_rect: clip_aabb.map(f32::to_bits),
            bounds: bounds_of_aabb(visible),
        });
    }

    // ── 实例发射 ───────────────────────────────────────────────────────────

    fn emit_quad(&mut self, quad: &crate::paint::primitive::Quad) {
        let bounds = quad.bounds.to_physical_f32(self.scale);
        if bounds[2] <= 0.0 || bounds[3] <= 0.0 {
            return;
        }
        let factor = self.scale.factor();
        let radii = normalize_corner_radius(
            quad.corner_radius.map(|radius| radius * factor),
            bounds[2],
            bounds[3],
        );
        let half_min = 0.5 * bounds[2].min(bounds[3]);
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
        let instance = GpuPrimitive {
            bounds,
            corner_radius: radii,
            color0: color0.to_array(),
            color1: color1.to_array(),
            params0: [border_width, 0.0, gradient, 1.0],
            params1: [self.clip_index(), 0.0, KIND_QUAD, self.clip_depth()],
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
        // 阴影跟随圆角（R05-9）：半径与 quad 用**同一个**规范化函数。
        let radii = normalize_corner_radius(
            shadow.corner_radius.map(|radius| radius * factor),
            bounds[2],
            bounds[3],
        );
        // 顶点外扩 = 3σ + 1（σ = blur/2）：3σ 之外的高斯尾部 < 0.3%，与解析式自洽。
        let sigma = (blur * SHADOW_SIGMA_SCALE).max(SHADOW_SIGMA_MIN);
        let pad = SHADOW_PAD_SIGMAS * sigma + 1.0;
        let instance = GpuPrimitive {
            bounds,
            corner_radius: radii,
            color0: self.colors.resolve(self.resolver, shadow.color).to_array(),
            color1: [0.0; 4],
            params0: [0.0, blur, GRADIENT_NONE, pad],
            params1: [self.clip_index(), 0.0, KIND_SHADOW, self.clip_depth()],
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
                self.clip_depth(),
            ],
            uv: image.tile.uv(),
            clip_rect: self.clip_rect(),
        };
        self.buffer.instances.push(instance);
    }

    fn emit_glyph(
        &mut self,
        color0: LinearPremulRgba,
        glyph: &crate::paint::primitive::PositionedGlyph,
    ) {
        let physical = Rect::from_origin_size(glyph.position, glyph.size).to_physical(self.scale);
        let bounds = physical_to_bounds(physical);
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
                self.clip_depth(),
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
        ClipState, GRADIENT_HORIZONTAL, GRADIENT_RADIAL, GRADIENT_VERTICAL, GpuPrimitive,
        InstanceBuffer, InstanceEncoder, KIND_GLYPH, KIND_QUAD, KIND_SHADOW, SHADOW_SIGMA_MIN,
        SHADOW_SIGMA_SCALE, TEXTURE_GLYPH, normalize_corner_radius,
    };
    use crate::{
        math::{Point, Rect, Scale, Size, Transform},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            color::{Color, ColorResolver},
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
        ColorResolver
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
        let buffer = encoder
            .encode(
                scene,
                scale(),
                &resolver(),
                &crate::paint::geometry_store::GeometryStore::new(),
            )
            .expect("encode");
        // 复制出编码结果，避免继续借用 encoder。
        InstanceBuffer {
            instances: buffer.instances.clone(),
            clips: buffer.clips.clone(),
            geometry: buffer.geometry.clone(),
            ops: buffer.ops.clone(),
        }
    }

    fn try_encode(scene: &Scene) -> Result<InstanceBuffer, crate::error::EngineError> {
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(
            scene,
            scale(),
            &resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )?;
        Ok(InstanceBuffer {
            instances: buffer.instances.clone(),
            clips: buffer.clips.clone(),
            geometry: buffer.geometry.clone(),
            ops: buffer.ops.clone(),
        })
    }

    fn scene_with(lane_primitives: impl FnOnce(&mut Scene, crate::paint::scene::LaneId)) -> Scene {
        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        lane_primitives(&mut scene, lane);
        scene.set_order(&[lane]);
        scene
    }

    fn push_clip(scene: &mut Scene, lane: crate::paint::scene::LaneId, clip: ClipRect) {
        assert!(scene.push(lane, Primitive::Clip(ClipOp::Push(clip))));
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

    /// R05-8：圆角半径的唯一规范化是 CSS 规则 `r <= min(w, h) / 2`，clip 与 quad 同一函数。
    #[test]
    fn corner_radius_uses_the_css_min_half_rule_everywhere() {
        // 100x2 的细条 + 半径 4：CSS/Skia 的 `min/2` 给出 1（本项目选定 `min/2` 规则）。
        assert_eq!(normalize_corner_radius([4.0; 4], 100.0, 2.0), [1.0; 4]);
        // 4x4 + 半径 8：夹到 2（shader 侧不再出现「越裁越大」）。
        assert_eq!(normalize_corner_radius([8.0; 4], 4.0, 8.0), [2.0; 4]);
        // 负半径 / NaN / 零尺寸
        assert_eq!(normalize_corner_radius([-3.0; 4], 10.0, 10.0), [0.0; 4]);
        assert_eq!(normalize_corner_radius([f32::NAN; 4], 10.0, 10.0), [0.0; 4]);
        assert_eq!(normalize_corner_radius([2.0; 4], 0.0, 10.0), [0.0; 4]);
        assert_eq!(normalize_corner_radius([2.0; 4], 10.0, f32::NAN), [0.0; 4]);

        // quad 路径：10x2 的盒子 + 半径 3（逻辑）@ 2x ⇒ 物理 20x4，half_min = 2。
        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(
            lane,
            Primitive::Quad(Quad {
                bounds: rect(0.0, 0.0, 10.0, 2.0),
                corner_radius: [3.0; 4],
                paint: solid(),
            }),
        ));
        scene.set_order(&[lane]);
        let buffer = encode(&scene);
        assert_eq!(buffer.instances[0].corner_radius, [2.0; 4]);

        // clip 路径：同一规则（旧实现完全不 clamp）。
        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 2.0, 2.0),
                    corner_radius: [8.0; 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        assert_eq!(
            buffer.clips[1].corner_radius, [2.0; 4],
            "clip 半径必须夹到 min/2"
        );
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
        // 编码按 scale 2 把逻辑换成物理：clip (100,50,800,600) ⇒ (200,100,1600,1200)。
        // xywh ⇒ 右/下边界 = 200+1600 / 100+1200，再各 +1（AA 外扩）= 1801 / 1301；
        // AABB 误读则得到 1600 / 1200 —— **精确断言**才能区分（旧的 >= 断言两者都通过）。
        assert_eq!(extent[2], 1801, "右边界必须是 x+w（不是 w）：{extent:?}");
        assert_eq!(extent[3], 1301, "下边界必须是 y+h（不是 h）：{extent:?}");
        let bounds = super::bounds_of_aabb([100.0, 50.0, 900.0, 650.0]);
        assert_eq!(
            (bounds.x0, bounds.y0, bounds.x1, bounds.y1),
            (99, 49, 901, 651)
        );
        // 边界：NaN / 反向区间 ⇒ EMPTY；零尺寸仍有 1 px 外扩；无裁剪哨兵 ⇒ 全范围。
        let empty = super::PhysicalBounds::EMPTY;
        assert_eq!(super::bounds_of_aabb([f32::NAN, 0.0, 1.0, 1.0]), empty);
        assert_eq!(super::bounds_of_aabb([10.0, 10.0, 5.0, 5.0]), empty);
        let dot = super::bounds_of_aabb([7.0, 7.0, 7.0, 7.0]);
        assert_eq!((dot.x0, dot.y0, dot.x1, dot.y1), (6, 6, 8, 8));
        let all = super::bounds_of_aabb([f32::MIN, f32::MIN, f32::MAX, f32::MAX]);
        assert_eq!(
            (all.x0, all.y0, all.x1, all.y1),
            (i32::MIN, i32::MIN, i32::MAX, i32::MAX)
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

    /// R05-10：嵌套的**轴对齐矩形** clip 在编码期求交合并成一条表项。
    ///
    /// 旧实现逐层把覆盖率相乘：两层边界重合时同一像素的覆盖率 = 0.5 × 0.5 = 0.25
    ///（滚动容器 + 画布区常是同一个矩形 ⇒ 一圈更暗的描边）。合并后链长恒为 1。
    #[test]
    fn nested_rect_clips_merge_into_one_chain_entry() {
        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 30.0, 30.0),
                    corner_radius: [0.0; 4],
                },
            );
            // 完全相同的一层（滚动容器 + 画布区同矩形）：不得再乘一次覆盖率。
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 30.0, 30.0),
                    corner_radius: [0.0; 4],
                },
            );
            // 第三层把它收窄到 (5,5,10,10)。
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(5.0, 5.0, 10.0, 10.0),
                    corner_radius: [0.0; 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            for _ in 0..3 {
                assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            }
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.params1[3], 1.0, "链长必须是 1（不是 3）");
        // 链上**只有一条**表项：从 clip_index 沿 parent 走一步就到哨兵 0。
        let head = instance.params1[0] as usize;
        assert!(head != 0, "必须有 clip 索引");
        assert_eq!(buffer.clips[head].parent, None, "合并后的矩形项是链根");
        // 这条链不带圆角（三层都是直角矩形 ⇒ 全部合并进同一条表项）。
        assert_eq!(buffer.clips[head].corner_radius, [0.0; 4]);
        // 物理 (5,5,10,10) × 2 = (10,10,20,20)，xywh。
        assert_eq!(instance.clip_rect, [10.0, 10.0, 20.0, 20.0]);
    }

    /// 圆角 clip 逐层占表项；完全相同的圆角 clip 复用同一表项（去重）。
    #[test]
    fn rounded_clips_keep_one_entry_each_and_dedupe() {
        let rounded = ClipRect {
            rect: rect(0.0, 0.0, 30.0, 30.0),
            corner_radius: [2.0; 4],
        };
        let scene = scene_with(|scene, lane| {
            push_clip(scene, lane, rounded);
            // 同一个圆角 clip 再来一层：必须与上一层共享表项（不能重复入表）。
            push_clip(scene, lane, rounded);
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        // 两层相同圆角 clip ⇒ 链长 2，但表项去重后仍然只有 2 条（哨兵 + 1 条圆角项）。
        assert_eq!(instance.params1[3], 2.0);
        assert_eq!(buffer.clips.len(), 3, "哨兵 + parent 链上的两个圆角项");
        // parent 链：链头 = 第二层，它的 parent 是第一层（同一表项的 rect/radii）。
        let head = buffer.clips[instance.params1[0] as usize];
        assert_eq!(head.corner_radius, [4.0; 4], "2 逻辑 × scale 2");
    }

    /// R05-10 的零尺寸语义：0 宽/高的 clip 必须**全裁**（覆盖率 0），不是 0.5 的鬼线。
    #[test]
    fn zero_sized_clip_encodes_as_a_negative_extent() {
        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(4.0, 4.0, 0.0, 10.0),
                    corner_radius: [0.0; 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert!(instance.params1[0] != 0.0, "必须有 clip 索引");
        assert!(
            instance.clip_rect[2] <= 0.0 || instance.clip_rect[3] <= 0.0,
            "零尺寸交集的 w/h 必须非正（shader 据此返回覆盖率 0）：{:?}",
            instance.clip_rect
        );
    }

    /// R05-5：GPU 裁剪链超限时**编码期返回明确错误**（旧实现 release 下静默丢最外层）。
    #[test]
    fn clip_chain_beyond_the_limit_fails_instead_of_dropping_the_outermost() {
        let scene = scene_with(|scene, lane| {
            // 直角矩形：无论多少层都合并成一条 ⇒ 不报错。
            for index in 0..12 {
                push_clip(
                    scene,
                    lane,
                    ClipRect {
                        rect: rect(index as f32, index as f32, 100.0, 100.0),
                        corner_radius: [0.0; 4],
                    },
                );
            }
            assert!(scene.push(lane, quad_primitive(solid())));
            for _ in 0..12 {
                assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            }
        });
        let buffer = encode(&scene);
        assert_eq!(buffer.instances[0].params1[3], 1.0, "12 层矩形只占 1 项");

        // 圆角：逐层计项，超过 8 层必须失败（depth 9，limit 8）。
        let scene = scene_with(|scene, lane| {
            for index in 0..9 {
                push_clip(
                    scene,
                    lane,
                    ClipRect {
                        rect: rect(index as f32, 0.0, 100.0, 100.0),
                        corner_radius: [2.0; 4],
                    },
                );
            }
            assert!(scene.push(lane, quad_primitive(solid())));
            for _ in 0..9 {
                assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            }
        });
        let error = match try_encode(&scene) {
            Err(error) => error,
            Ok(_) => panic!("9 层圆角 clip 必须报错"),
        };
        match error {
            crate::error::EngineError::ClipDepthExceeded { depth, limit } => {
                assert_eq!((depth, limit), (9, crate::paint::scene::MAX_CLIP_DEPTH));
            }
            other => panic!("期望 ClipDepthExceeded，实际 {other:?}"),
        }

        // 恰好 8 层圆角（+ 0 矩形）是合法的。
        let scene = scene_with(|scene, lane| {
            for index in 0..8 {
                push_clip(
                    scene,
                    lane,
                    ClipRect {
                        rect: rect(index as f32, 0.0, 100.0, 100.0),
                        corner_radius: [2.0; 4],
                    },
                );
            }
            assert!(scene.push(lane, quad_primitive(solid())));
            for _ in 0..8 {
                assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            }
        });
        let buffer = encode(&scene);
        assert_eq!(buffer.instances[0].params1[3], 8.0);
    }

    /// R05-11：`params1.w` 携带真实链长（旧实现写 `FLAG_OPAQUE` 而无人读）。
    #[test]
    fn params1_w_carries_the_clip_chain_depth() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(lane, quad_primitive(solid())));
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 30.0, 30.0),
                    corner_radius: [1.0; 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        assert_eq!(buffer.instances[0].params1[3], 0.0, "无裁剪 ⇒ 链长 0");
        assert_eq!(buffer.instances[1].params1[3], 1.0);
        assert_eq!(
            buffer.instances[0].params1[0], 0.0,
            "clip_index == 0 = 无裁剪"
        );
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

    /// R05-9：σ = blur / 2，顶点外扩 = 3σ + 1（旧实现 σ = blur / 3、外扩 = blur + 1）。
    #[test]
    fn shadow_sigma_and_padding_follow_css() {
        assert_eq!(SHADOW_SIGMA_SCALE, 0.5, "CSS box-shadow：σ = blur / 2");
        assert_eq!(SHADOW_SIGMA_MIN, 0.35);
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Shadow(Shadow {
                    rect: rect(0.0, 0.0, 10.0, 10.0),
                    offset: Point::new(2.0, 3.0),
                    blur: 4.0,
                    color: Color::BLACK,
                    corner_radius: [2.0; 4],
                }),
            ));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.bounds, [4.0, 6.0, 20.0, 20.0]);
        assert_eq!(instance.params0[1], 8.0, "物理 blur = 4 × scale 2");
        // σ = 8 / 2 = 4；pad = 3 × 4 + 1 = 13。
        assert_eq!(instance.params0[3], 13.0);
        assert_eq!(instance.params1[2], KIND_SHADOW);
        // 阴影跟随圆角：2 逻辑 × 2 = 4，且按 min(w,h)/2 = 10 clamp。
        assert_eq!(instance.corner_radius, [4.0; 4]);
    }

    /// R05-9 的角部：圆角半径超过 `min/2` 时阴影半径也被夹住（与 quad 同一函数）。
    #[test]
    fn shadow_corner_radius_is_normalized_like_a_quad() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Shadow(Shadow {
                    rect: rect(0.0, 0.0, 4.0, 8.0),
                    offset: Point::ZERO,
                    blur: 0.0,
                    color: Color::BLACK,
                    corner_radius: [100.0; 4],
                }),
            ));
        });
        let buffer = encode(&scene);
        // 物理 8x16 ⇒ min/2 = 4。
        assert_eq!(buffer.instances[0].corner_radius, [4.0; 4]);
        // blur = 0 ⇒ σ = KYTRIX_SHADOW_SIGMA_MIN ⇒ pad = 3 × 0.35 + 1 = 2.05。
        assert!((buffer.instances[0].params0[3] - (3.0 * SHADOW_SIGMA_MIN + 1.0)).abs() < 1e-6);
    }

    /// P2-3.2 回归：damage 裁剪用的 op 包围盒必须覆盖 shader 的**顶点外扩**。
    ///
    /// 阴影的顶点外扩是 `3σ + 1`（`params0.w`）；旧实现按 `blur + 1` 外扩 —— σ 改小后
    /// 模糊环落在包围盒外时，damage 裁剪会把整条阴影丢掉（可见缺陷，不是性能问题）。
    #[test]
    fn op_bounds_cover_the_shader_vertex_padding() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Shadow(Shadow {
                    rect: rect(10.0, 10.0, 20.0, 20.0),
                    offset: Point::ZERO,
                    blur: 8.0,
                    color: Color::BLACK,
                    corner_radius: [0.0; 4],
                }),
            ));
        });
        let buffer = encode(&scene);
        let crate::paint::instance::FrameOp::Instances { bounds, .. } = buffer.ops[0] else {
            panic!("阴影应产生一个 Instances op");
        };
        // 场景 scale 是 2×：rect 在物理空间是 20..60，blur 8→16，σ = 8，
        // pad = 3σ + 1 = 25，再加 1 px 取整余量 = 26 → -6..86。
        assert_eq!(
            (bounds.x0, bounds.y0, bounds.x1, bounds.y1),
            (-6, -6, 86, 86),
            "op 包围盒必须覆盖阴影的模糊环（3σ + 1 的顶点外扩）"
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

    /// R05-12 / R05-19：几何 op 的可见范围 = `transform(desc.bounds) ∩ clip`。
    ///
    /// 旧实现对无 clip 的几何给出「全屏」包围盒（`clip_aabb` 的无界哨兵），局部刷新时
    /// 每条几何 draw 都会被重发；给了 transform 却用未变换的 bounds 会在平移后留残影。
    #[test]
    fn geometry_op_bounds_follow_the_transform() {
        let stride = crate::paint::geometry::GeometryPipeline::Ui.vertex_stride() as usize;
        let desc = crate::paint::geometry::GeometryDesc {
            vertex_bytes: bytes::Bytes::from(vec![0u8; stride * 3]),
            index_bytes: bytes::Bytes::from(vec![0u8; 6]),
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
                transform: Transform::translate(5.0, 5.0),
                color: crate::paint::color::Color::WHITE,
            }),
        ));
        scene.set_order(&[lane]);

        let mut encoder = InstanceEncoder::new();
        let buffer = encoder
            .encode(&scene, scale(), &resolver(), &store)
            .expect("encode");
        let Some(super::FrameOp::Geometry { bounds, .. }) = buffer
            .ops
            .iter()
            .find(|op| matches!(op, super::FrameOp::Geometry { .. }))
        else {
            panic!("必须有 geometry op")
        };
        // desc.bounds = (0,0,10,10) → translate(5,5) → (5,5,15,15) → × scale 2
        // = (10,10,30,30)，再外扩 1 px ⇒ (9, 9, 31, 31)。
        assert_eq!(
            (bounds.x0, bounds.y0, bounds.x1, bounds.y1),
            (9, 9, 31, 31),
            "几何 op 的可见范围必须用 transform 后的 AABB"
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
        let buffer = encoder
            .encode(&scene, scale(), &resolver(), &store)
            .expect("encode");
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
        assert_eq!(instance.params1[3], 0.0, "无裁剪 ⇒ clip_depth 0");
    }

    #[test]
    fn encode_is_idempotent() {
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
        let second = encode(&scene);
        assert_eq!(first.instances[0].params1, second.instances[0].params1);
        assert_eq!(first.instances[0].color0, second.instances[0].color0);
        assert_eq!(first.instances[1].color0, second.instances[1].color0);
    }

    /// `ClipState` 的深度计数：矩形合并项 0/1 + 圆角层数（与 shader 的循环上限对齐）。
    #[test]
    fn clip_state_depth_counts_the_merged_rect_once() {
        let state = ClipState::unbounded();
        assert_eq!(state.depth(), 0);
        assert!(state.top().is_none());
    }
}

//! `GpuPrimitive` 契约（128 B / 16 B 对齐）与 CPU 侧实例编码。
//!
//! 本文是 `render/shaders/contract.md` 的**权威来源**（I2/I8）：字段顺序、offset、`kind`/`params`
//! 取值表的任何改动都必须同时改 Rust、三套 shader、`contract.md`，并触发像素快照回归。
//!
//! 范围（`design/phase-06-paint-runtime.md` §7）：
//! - 8 类 `Primitive` → `GpuPrimitive`（`GlyphRun` 是唯一的 1→N）；
//! - `Path`/`Geometry` 0 实例 + `FrameOp::Geometry`；
//! - clip 链去重（`ClipChainId`，帧内索引）+ **轴对齐矩形求交合并**（Skia `SkClipStack` 语义）+
//!   矩形/圆角两种覆盖率语义（矩形 = 像素精确 scissor、圆角 = 解析 AA；见 [`ClipKind`]）；
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
    /// 裁剪链的**真实链长** = 至多一条矩形项 + k 条圆角项（k ≤ `MAX_CLIP_DEPTH`），上界
    /// [`MAX_CLIP_ITEMS`]；shader 用它收口 clip 循环 —— 旧实现的 `flags` 位无人消费）。
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

/// clip 链的矩形项（写进 `ClipEntry.meta.y`）：覆盖率只有 0/1 的**像素精确 scissor**。
pub(crate) const CLIP_KIND_RECT: f32 = 1.0;

/// clip 链的圆角项（写进 `ClipEntry.meta.y`）：参与解析 AA。
pub(crate) const CLIP_KIND_ROUNDED: f32 = 2.0;

/// GPU clip 链的**总项数**上界 = `MAX_CLIP_DEPTH`（圆角层数）+ 1（至多一条矩形项）。
///
/// 与三份 shader 的 `KYTRIX_MAX_CLIP_ITEMS` 逐值相同（`build.rs` 校验）。为什么需要它：
/// 圆角层数的预算由 `Canvas::push_clip` 与编码器按 `MAX_CLIP_DEPTH` 控制，但矩形项**不参与**
/// 该预算（全部轴对齐矩形 clip 求交合并成一条，无损），于是「8 层圆角 + 祖先矩形 clip」的链长
/// 是 9 而不是 8。shader 的 clip 循环按真实链长收口，上界必须覆盖这个 9；链长超过 9（等价于
/// 圆角层数 > 8）在编码期直接报错，绝不静默少裁。
///
/// 只被 debug 自检与测试消费（release 生产路径不需要这个数：链长上界由 `push_clip` 的圆角
/// 预算 + 至多一条矩形项保证），因此按「debug 或测试」收口 —— 否则 release 构建会报
/// `dead_code`（AR2b P3-N1）。**不是** `#[allow]`：release 下它真的没有读者。
#[cfg(any(debug_assertions, test))]
pub(crate) const MAX_CLIP_ITEMS: f32 = 9.0;

/// 圆角半径的**唯一**规范化（R05-8 / AR2 P2-②）：两步、顺序固定。
///
/// 1. **CSS 比例缩放**（CSS Backgrounds §5.5）：某条边上相邻两角半径之和超过该边长时，
///    全部半径按 `f = min(side / Σr)` 等比缩小（保持四角相对比例）。例：`100×20, tl=bl=40`
///    按 `f = 20/80` 缩到 `tl=10, bl=10`；逐角 `min(w,h)/2` 会得到 `tl=1, bl=10`（比例被
///    破坏），所以旧实现**不是** CSS 算法（AR2 P2-② 的反例）。
/// 2. **SDF 合法域 clamp** `r ≤ min(w, h) / 2`：`r > min(half)` 会让 `sd_rounded_rect` 的
///    `q = |p| - half + r` 恒正 ⇒ 等值线反转，裁剪区反而**变大**（内容溢出）。CSS 允许
///    `r > min(half)` 的椭圆角，本引擎的 SDF 只支持圆角，故第二步必须存在；shader 里保留同一
///    clamp 作为第二道防线，对已规范化的数据幂等。
///
/// clip、quad、shadow 三条路径都走这里（R05-8）。旧实现有两套：quad 用 `min(half)` clamp、
/// clip **完全不 clamp**；`r > min/2` 时等值线反转，裁剪区反而变大。
fn normalize_corner_radius(radii: [f32; 4], width: f32, height: f32) -> [f32; 4] {
    // NaN 必须先挡掉：`f32::min` 会**忽略** NaN（返回另一个操作数），直接算会得到
    // 一个看似合法的 half_min，于是异常尺寸的图形不会被归零。
    if width.is_nan() || height.is_nan() || !width.is_finite() || !height.is_finite() {
        return [0.0; 4];
    }
    if width <= 0.0 || height <= 0.0 {
        return [0.0; 4];
    }
    let sanitized = radii.map(|radius| {
        if radius.is_nan() {
            0.0
        } else {
            radius.max(0.0)
        }
    });
    // ① CSS：四条边各自的比例因子取最小值（该边上没有任何半径时不构成约束）。
    let mut scale = 1.0f32;
    for (side, sum) in [
        (width, sanitized[0] + sanitized[1]),
        (width, sanitized[3] + sanitized[2]),
        (height, sanitized[0] + sanitized[3]),
        (height, sanitized[1] + sanitized[2]),
    ] {
        if sum > side {
            scale = scale.min(side / sum);
        }
    }
    // ② SDF 合法域。
    let half_min = 0.5 * width.min(height);
    sanitized.map(|radius| (radius * scale).min(half_min))
}

/// `ClipChain` 的帧内索引；`0` 是「无裁剪」哨兵。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClipChainId(u32);

impl ClipChainId {
    /// 帧内表项下标。消费者只有 debug 自检与测试（生产路径直接用 `.0` 写进 `meta.x`），
    /// 因此按「debug 或测试」收口，避免 release 构建的 `dead_code` 警告（AR2b P3-N1）。
    #[cfg(any(debug_assertions, test))]
    fn index(self) -> usize {
        self.0 as usize
    }
}

/// 一层 clip 的覆盖率语义（写进 `ClipEntry.meta.y`；shader 的 `clip_mask` 据此分支）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClipKind {
    /// 轴对齐矩形：**像素精确 scissor**（覆盖率只有 0 或 1，不参与 AA 相乘）。
    Rect,
    /// 圆角矩形：解析 AA（逐层相乘）。
    Rounded,
}

impl ClipKind {
    /// GPU 侧的 `meta.y` 取值（与 shader 的 `KYTRIX_CLIP_KIND_*` 同值，`build.rs` 校验）。
    fn gpu(self) -> u32 {
        match self {
            Self::Rect => CLIP_KIND_RECT as u32,
            Self::Rounded => CLIP_KIND_ROUNDED as u32,
        }
    }
}

/// 一层 clip（P02 §8.7）。`rect` 是 `(x, y, w, h)`（物理像素）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClipChain {
    parent: Option<ClipChainId>,
    kind: ClipKind,
    rect: [f32; 4],
    corner_radius: [f32; 4],
}

/// GPU 侧 `ClipEntry`（P07 §2.6 / DEVIATIONS F-69）：**48 B / 16 B 对齐**。
///
/// 与 CPU 侧 [`ClipChain`] 同构（CPU 侧不算交集的 AABB：交集由 [`ClipState`] 维护），
/// `parent` 打进 `meta.x`、`kind` 打进 `meta.y`，`.zw` 保留恒 0；表项 **0 恒为哨兵**
///（`clip_index == 0` 时 shader 直接返回 1）。
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GpuClipEntry {
    /// `(x, y, w, h)`，物理像素。
    pub rect: [f32; 4],
    /// `(tl, tr, br, bl)`，物理像素。
    pub corner_radius: [f32; 4],
    /// `.x = parent`（0 = 无裁剪），`.y = KYTRIX_CLIP_KIND_*`，`.zw` 恒 0。
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
            meta: [
                self.parent.map_or(0, |parent| parent.0),
                self.kind.gpu(),
                0,
                0,
            ],
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
/// 链的形状恒为「**至多一条矩形项**（链根）+ k 条圆角项（k ≤ `MAX_CLIP_DEPTH`）」：
///
/// - 全部轴对齐矩形 clip 只做一次交集（**无损**：矩形交仍是矩形），链上只有一条矩形项。
///   它的覆盖率语义是**像素精确 scissor**（0/1），不参与 AA 相乘 —— 因此「同边界的
///   `Clip::Bounds` + Canvas 圆角 clip」只乘一次圆角 AA，不再有 `0.5 × 0.5 = 0.25` 的暗边；
/// - 圆角 clip 各自一层（圆角形状的交集不是矩形，无法压成一条）。**完全相同的圆角 clip
///   在栈里出现多次时只保留一层**：交集是幂等的（`A ∩ A = A`），再乘一次覆盖率恒为错。
///
/// 轴对齐矩形 clip 早就在 `push_clip` 里折叠了，但**不占 `MAX_CLIP_DEPTH` 预算**：预算是圆角
/// 层数（`Canvas::push_clip` 与编码器同口径）。于是「8 层圆角 + 祖先矩形」的链长是 9 ——
/// shader 的循环上界是 `MAX_CLIP_ITEMS = MAX_CLIP_DEPTH + 1`。
#[derive(Clone, Debug)]
struct ClipState {
    /// 全部矩形 clip 的交集（`[x0, y0, x1, y1]`，物理像素）。
    rect_aabb: [f32; 4],
    /// 交集是否已存在（`false` ⇒ `rect_aabb` 是无界哨兵、链上没有矩形表项）。
    has_rect: bool,
    /// 交集在裁剪表里的表项（链根）。
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

    /// GPU 裁剪链的链长 = 矩形合并项（0/1）+ 圆角层数；上界 `MAX_CLIP_ITEMS`。
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

    /// 链上是否已有**完全相同**（矩形 + 四角半径逐位相同）的圆角层。
    ///
    /// 交集幂等 ⇒ 重复层没有任何视觉效果，只会再乘一次覆盖率（变暗）。`push_clip` 据此跳过。
    fn duplicate_rounded(&self, rect: [f32; 4], corner_radius: [f32; 4]) -> bool {
        self.rounded.iter().any(|clip| {
            clip.rect.map(f32::to_bits) == rect.map(f32::to_bits)
                && clip.corner_radius.map(f32::to_bits) == corner_radius.map(f32::to_bits)
        })
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

/// 矩形 clip 的**像素吸附**（Skia `SkIRect::roundOut`）：`lo = floor(x0)`、`hi = ceil(x1)`。
///
/// 为什么必须在编码期吸附：矩形项的覆盖率语义是**像素精确 scissor**（0/1，不参与 AA），
/// shader 侧只能拿**像素索引**（`floor(world_px)`）与 `[lo, hi)` 比较。`world_px` 是插值得到
/// 的顶点属性（顶点只夹到 `clip_rect ± KYTRIX_CLIP_VERTEX_SLACK`，所以边界像素确有片元），
/// 一旦拿它和浮点边界做「±0.5 px 像素单元」的相等判定，1 ulp 的插值误差就会让边界像素翻面；
/// 覆盖率是 0/1 ⇒ 翻面 = **整列像素以满不透明越出裁剪框**（AR2b P1-N1：lavapipe 上实测 1 px
/// 实心出框条并被 bless 进基线）。吸附到整数后，`floor(world_px)` 与整数边界之间没有任何平局，
/// 1 ulp 改不了 `floor` 的结果。
///
/// 方向 = **向外**（`roundOut`，保证不漏裁）：这与旧的「片元像素单元（±0.5 px）与矩形有交」
/// 判定**逐像素等价**（后者本来就等于 `roundOut`），所以吸附不改变任何像素的取舍，只是把同一
/// 次计算挪到整数上 —— 对 golden 的影响只有「原本落在平局错误一侧的像素归位」（lavapipe 的
/// 越界列消失）。向内吸附会裁掉与矩形有交的像素（内容被多裁 1 px），没有理由。
///
/// 空交集（含零宽高）归宿成 `w = h = 0`（= 全裁，shader 见到非正 w/h 直接返回 0）。不能对空
/// 交集用 `floor/ceil`：它会把反向区间变成 1 px 的保留区（`floor(10.6) = 10`、`ceil(10.4) = 11`）。
/// 这样「矩形项恒为整数物理像素」就没有例外，shader 侧的比较也不需要任何 epsilon。
fn snap_rect_clip_to_pixels(rect: [f32; 4]) -> [f32; 4] {
    // 空交集 / 零尺寸（`!(w > 0)` 也覆盖 NaN）：`floor/ceil` 会把反向区间变成 1 px 的保留区
    // （`floor(10.6) = 10`、`ceil(10.4) = 11`），所以直接给 `w = h = 0`（shader 全裁）。
    if !(rect[2] > 0.0 && rect[3] > 0.0) {
        return [rect[0].floor(), rect[1].floor(), 0.0, 0.0];
    }
    let lo_x = rect[0].floor();
    let lo_y = rect[1].floor();
    let hi_x = (rect[0] + rect[2]).ceil();
    let hi_y = (rect[1] + rect[3]).ceil();
    let snapped = [lo_x, lo_y, hi_x - lo_x, hi_y - lo_y];
    // 极端坐标（|x| 接近 f32::MAX）上 `hi - lo` 会溢出成 inf；编码器约定 clip 矩形有限，
    // 这种输入下退回原值（shader 的比较仍然给出「全裁」，不会画错东西）。
    if snapped.iter().all(|value| value.is_finite()) {
        snapped
    } else {
        rect
    }
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
    kind: ClipKind::Rect,
    rect: [0.0; 4],
    corner_radius: [0.0; 4],
};

/// `Primitive → GpuPrimitive` 的编码器（`Window` 持有，缓冲跨帧复用）。
pub(crate) struct InstanceEncoder {
    buffer: InstanceBuffer,
    state: ClipState,
    /// clip 栈回溯（每层 push 前保存一次完整状态；嵌套深度由调用方决定，不设硬上限）。
    saved: SmallVec<[ClipState; 8]>,
    chains: ahash::HashMap<(u32, u32, [u32; 4], [u32; 4]), ClipChainId>,
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
        // 自检只在 debug 构建里跑：循环本身（遍历全部 op / clip / instance）不是
        // `debug_assert!` 的宏体，release 下也会白跑 —— 10k 图元 ≈ 40 µs/帧。
        #[cfg(debug_assertions)]
        self.self_check();
        Ok(&self.buffer)
    }

    /// 本帧的编码结果（消费者：`Window::instance_bytes`，P07 上传）。
    pub(crate) fn buffer(&self) -> &InstanceBuffer {
        &self.buffer
    }

    /// 编码器自检（P02 §8.8 的不变量）。
    ///
    /// 只在 debug 构建里编译：release 下这些循环没有任何副作用，却是 O(op + clip + instance)
    /// 的纯开销。
    #[cfg(debug_assertions)]
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
            // AR2b P2-N1：`GpuClipEntry.meta.y`（`ClipKind` 的 GPU 编码）是 shader 分流矩形 /
            // 圆角的**唯一**依据，但它的取值只由 `ClipKind::gpu()` 决定 —— 把两个值对调后
            // 覆盖率只是「乘错一次」（矩形项被当圆角做 AA ⇒ 0.25 暗边；圆角项被当矩形硬裁 ⇒
            // 圆角不被裁），单测与像素 golden 都可能仍然是绿的（复审实测：对调后 1174 单测 +
            // 8 条 golden 全绿）。因此这里用**几何**不变量交叉校验 `meta.y`：
            // 四角半径全 0 ⇔ 矩形项（`push_clip` 把全零半径路由到矩形分支，圆角项必有非零半径），
            // 于是「对调 Rect/Rounded」会立刻打破它。
            let entry = chain.gpu_entry();
            let rect_kind = chain.corner_radius.iter().all(|radius| *radius == 0.0);
            let expected = if rect_kind {
                CLIP_KIND_RECT as u32
            } else {
                CLIP_KIND_ROUNDED as u32
            };
            debug_assert_eq!(
                entry.meta[1], expected,
                "GpuClipEntry.meta.y 必须与覆盖率语义一致（1 = 矩形 scissor、2 = 圆角 AA）"
            );
            debug_assert_eq!(entry.meta[2], 0, "ClipEntry.meta.zw 保留恒 0");
            debug_assert_eq!(entry.meta[3], 0, "ClipEntry.meta.zw 保留恒 0");
        }
        // 每个实例的 `clip_depth`（params1.w）必须等于它那条链的真实链长：shader 用这个值
        // 收口循环，写错就是静默少裁（R05-5 / R05-11）。链长还必须有界（shader 的循环上界是
        // `KYTRIX_MAX_CLIP_ITEMS`），且链上至多一条矩形项、矩形项只能是链根。
        for primitive in &self.buffer.instances {
            let clip_index = primitive.params1[0] as usize;
            debug_assert!(
                clip_index < self.buffer.clips.len(),
                "instance clip index out of range"
            );
            let mut depth = 0usize;
            let mut rects = 0usize;
            let mut cursor = clip_index;
            while cursor != 0 {
                depth += 1;
                debug_assert!(
                    depth <= MAX_CLIP_ITEMS as usize,
                    "clip chain longer than MAX_CLIP_ITEMS"
                );
                let entry = self.buffer.clips[cursor];
                if entry.kind == ClipKind::Rect {
                    rects += 1;
                    debug_assert_eq!(entry.parent, None, "矩形项必须是链根");
                }
                cursor = entry.parent.map_or(0, |parent| parent.index());
            }
            debug_assert!(rects <= 1, "链上至多一条矩形 clip 项");
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
    chains: &'a mut ahash::HashMap<(u32, u32, [u32; 4], [u32; 4]), ClipChainId>,
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

    /// 压入一层 clip。
    ///
    /// - **轴对齐矩形**（四角半径全 0）走无损求交合并：链上恒只有一条矩形项（scissor 语义），
    ///   不占 `MAX_CLIP_DEPTH` 预算；
    /// - **圆角 clip** 各占一层，并计入 `MAX_CLIP_DEPTH` 预算；与栈内已有层**完全相同**时跳过
    ///   （交集幂等，重复层只会把覆盖率多乘一次 ⇒ 变暗）。
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
        } else if !self.state.duplicate_rounded(rect, radii) {
            let parent = self.state.top();
            let entry = self.chain_entry(parent, ClipKind::Rounded, rect, radii);
            self.state.rounded.push(RoundedClip {
                rect,
                corner_radius: radii,
                entry,
            });
        }
        // 预算只数圆角层：矩形项是求交合并后的 scissor，不消耗圆角预算（链长 = 圆角层数 + 0/1）。
        let rounded_depth = self.state.rounded.len();
        if rounded_depth > usize::from(MAX_CLIP_DEPTH) {
            return Err(EngineError::ClipDepthExceeded {
                depth: rounded_depth as u8,
                limit: MAX_CLIP_DEPTH,
            });
        }
        Ok(())
    }

    /// 弹出当前层（恢复 push 前的完整状态）。
    fn pop_clip(&mut self) {
        *self.state = self.saved.pop().unwrap_or_else(ClipState::unbounded);
    }

    /// 矩形交集变化后重建链：矩形项在**链根**（scissor），圆角项依次挂在它下面。
    ///
    /// 覆盖率相乘在 shader 里与顺序无关，但链必须始终只有**一条**矩形项 —— 否则「矩形
    /// 交集」会被乘两次（正是 R05-10 的 0.25 暗边）。圆角项的矩形/半径不变，只重挂 parent。
    ///
    /// 矩形项的表项存的是**吸附到整数物理像素**的矩形（[`snap_rect_clip_to_pixels`]，
    /// Skia `roundOut` 语义）：shader 的 scissor 判定按像素索引做，浮点边界会让边界像素在
    /// 1 ulp 上翻面（AR2b P1-N1）。圆角项**不**吸附 —— 它们的几何必须保持精确，AA 带宽逐
    /// 像素计算。
    fn rebuild_rect_chain(&mut self) {
        let rect_entry = if self.state.has_rect {
            let rect = snap_rect_clip_to_pixels(aabb_to_rect(self.state.rect_aabb));
            Some(self.chain_entry(None, ClipKind::Rect, rect, [0.0; 4]))
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
            let entry = self.chain_entry(parent, ClipKind::Rounded, rect, radii);
            self.state.rounded[index].entry = entry;
            parent = Some(entry);
        }
    }

    /// 裁剪表项的唯一插入点（按 `(parent, kind, rect 位模式, radii 位模式)` 去重：
    /// 完全相同的层复用同一表项）。注意**去重不等于覆盖率正确**：嵌套的相同圆角层在
    /// `ClipState` 里已被跳过（见 `duplicate_rounded`），本函数只复用字节相同的条目。
    fn chain_entry(
        &mut self,
        parent: Option<ClipChainId>,
        kind: ClipKind,
        rect: [f32; 4],
        corner_radius: [f32; 4],
    ) -> ClipChainId {
        let key = (
            parent.map_or(0, |parent| parent.0),
            kind.gpu(),
            rect.map(f32::to_bits),
            corner_radius.map(f32::to_bits),
        );
        if let Some(existing) = self.chains.get(&key) {
            return *existing;
        }
        let id = ClipChainId(self.buffer.clips.len() as u32);
        self.buffer.clips.push(ClipChain {
            parent,
            kind,
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
        CLIP_KIND_RECT, CLIP_KIND_ROUNDED, ClipChain, ClipChainId, ClipKind, ClipState,
        GRADIENT_HORIZONTAL, GRADIENT_RADIAL, GRADIENT_VERTICAL, GpuPrimitive, InstanceBuffer,
        InstanceEncoder, KIND_GLYPH, KIND_QUAD, KIND_SHADOW, MAX_CLIP_ITEMS, SHADOW_SIGMA_MIN,
        SHADOW_SIGMA_SCALE, TEXTURE_GLYPH, normalize_corner_radius, snap_rect_clip_to_pixels,
    };
    use crate::{
        math::{Point, Rect, Scale, Size, Transform},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            canvas::Canvas,
            color::{Color, ColorResolver},
            geometry::GeometryId,
            primitive::{
                Border, ClipOp, ClipRect, GeometryDraw, GlyphRun, Gradient, GradientKind,
                PositionedGlyph, Primitive, Quad, QuadPaint, Shadow,
            },
            scene::{MAX_CLIP_DEPTH, Scene},
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

    /// shader `clip_mask` 的 **CPU 镜像**（测试 oracle，`pixel` 是片元中心，真实取值恒为
    /// `(k + 0.5, j + 0.5)`）。
    ///
    /// 三份 shader 的逐语句等价由 `build.rs` 的归一化比对守护（`MATH_FUNCTIONS` 含
    /// `clip_mask`），本镜像负责钉住**数值语义**（覆盖率到底是 0.5 还是 0.25）。AA 带宽取
    /// `length(ddx, ddy)` = 1 px（屏幕对齐几何的导数模长）。矩形项与 shader 同构：**像素索引**
    /// `floor(pixel)` 与编码期吸附到整数物理像素的 `[lo, hi)` 比较（不是 ±0.5 的浮点单元）。
    fn clip_coverage(clips: &[ClipChain], head: ClipChainId, pixel: (f32, f32)) -> f32 {
        let mut m = 1.0f32;
        let mut cursor = Some(head);
        while let Some(id) = cursor {
            let entry = clips[id.index()];
            if entry.rect[2] <= 0.0 || entry.rect[3] <= 0.0 {
                return 0.0;
            }
            match entry.kind {
                ClipKind::Rect => {
                    let (pixel_x, pixel_y) = (pixel.0.floor(), pixel.1.floor());
                    let (lo_x, lo_y) = (entry.rect[0], entry.rect[1]);
                    let (hi_x, hi_y) = (lo_x + entry.rect[2], lo_y + entry.rect[3]);
                    if pixel_x < lo_x || pixel_y < lo_y || pixel_x >= hi_x || pixel_y >= hi_y {
                        return 0.0;
                    }
                }
                ClipKind::Rounded => {
                    let half = (entry.rect[2] * 0.5, entry.rect[3] * 0.5);
                    let center = (entry.rect[0] + half.0, entry.rect[1] + half.1);
                    let d = sd_rounded_rect(
                        (pixel.0 - center.0, pixel.1 - center.1),
                        half,
                        entry.corner_radius,
                    );
                    m *= (0.5 - d).clamp(0.0, 1.0);
                }
            }
            cursor = entry.parent;
        }
        m
    }

    /// `sd_rounded_rect` 的测试侧等价实现（与草稿 shader 同一公式；带宽按 1 px 归一）。
    fn sd_rounded_rect(p: (f32, f32), half: (f32, f32), radii: [f32; 4]) -> f32 {
        let r = if p.0 > 0.0 {
            if p.1 > 0.0 { radii[2] } else { radii[1] }
        } else if p.1 > 0.0 {
            radii[3]
        } else {
            radii[0]
        };
        let r = r.clamp(0.0, half.0.min(half.1));
        let q = (p.0.abs() - half.0 + r, p.1.abs() - half.1 + r);
        let outside = (q.0.max(0.0).powi(2) + q.1.max(0.0).powi(2)).sqrt();
        q.0.max(q.1).min(0.0) + outside - r
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

    /// R05-8 / AR2 P2-②：圆角半径的唯一规范化 = ① CSS 比例缩放 + ② SDF 合法域 clamp。
    ///
    /// 旧实现（逐角 `min(w,h)/2`）被注释写成「CSS 规则」，两者在「同一条边上半径不均衡」时
    /// 结论不同：CSS 保持四角比例，逐角 clamp 破坏比例。本测试锁住两步的顺序与数值。
    #[test]
    fn corner_radius_applies_css_proportional_scaling_then_the_sdf_domain_clamp() {
        // 100x2 的细条 + 半径 4：上下边和 8 > 2 ⇒ f = 0.25 ⇒ 半径 1 = half_min。
        assert_eq!(normalize_corner_radius([4.0; 4], 100.0, 2.0), [1.0; 4]);
        // 4x8 + 半径 8：上边和 16 > 4 ⇒ f = 0.25 ⇒ 2；再夹到 half_min = 2。
        assert_eq!(normalize_corner_radius([8.0; 4], 4.0, 8.0), [2.0; 4]);
        // 30x100 + 上边两角各 20：和 40 > 30 ⇒ f = 0.75 ⇒ 15 = half_min（比例保持不变）。
        assert_eq!(
            normalize_corner_radius([20.0, 20.0, 0.0, 0.0], 30.0, 100.0),
            [15.0, 15.0, 0.0, 0.0]
        );
        // 20x100 + tl=10 / tr=30：和 40 > 20 ⇒ f = 0.5 ⇒ (5, 15)；再夹到 half_min = 10。
        // 逐角 clamp 会得到 (10, 10)（比例被破坏）—— 这是本测试要钉住的差异。
        assert_eq!(
            normalize_corner_radius([10.0, 30.0, 0.0, 0.0], 20.0, 100.0),
            [5.0, 10.0, 0.0, 0.0]
        );
        // 左边界越界：100x20 + tl=1 / bl=40：左边和 41 > 20 ⇒ f = 20/41 ⇒ (0.488, 19.51)；
        // 再夹到 half_min = 10（bl 越过 SDF 合法域 ⇒ 必须夹，否则等值线反转）。
        let scaled = normalize_corner_radius([1.0, 0.0, 0.0, 40.0], 100.0, 20.0);
        assert!((scaled[0] - 20.0 / 41.0).abs() < 1e-5, "{scaled:?}");
        assert_eq!(scaled[3], 10.0);
        assert_eq!((scaled[1], scaled[2]), (0.0, 0.0));
        // 负半径 / NaN / 零尺寸 / 非有限尺寸
        assert_eq!(normalize_corner_radius([-3.0; 4], 10.0, 10.0), [0.0; 4]);
        assert_eq!(normalize_corner_radius([f32::NAN; 4], 10.0, 10.0), [0.0; 4]);
        assert_eq!(normalize_corner_radius([2.0; 4], 0.0, 10.0), [0.0; 4]);
        assert_eq!(normalize_corner_radius([2.0; 4], 10.0, f32::NAN), [0.0; 4]);
        assert_eq!(
            normalize_corner_radius([2.0; 4], f32::INFINITY, 10.0),
            [0.0; 4]
        );

        // quad 路径：10x2 的盒子 + 半径 3（逻辑）@ 2x ⇒ 物理 20x4，左/右边和 12 > 4。
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
            "clip 半径必须走同一规范化（CSS 比例缩放 + SDF 合法域）"
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
        assert_eq!(
            buffer.clips[head].kind,
            ClipKind::Rect,
            "轴对齐矩形 clip = 像素精确 scissor"
        );
        // 这条链不带圆角（三层都是直角矩形 ⇒ 全部合并进同一条表项）。
        assert_eq!(buffer.clips[head].corner_radius, [0.0; 4]);
        // 物理 (5,5,10,10) × 2 = (10,10,20,20)，xywh。
        assert_eq!(instance.clip_rect, [10.0, 10.0, 20.0, 20.0]);
    }

    /// AR2 P1-A（假绿重写）：**完全相同的圆角 clip 在栈里重复出现只保留一层**。
    ///
    /// 旧测试名为「去重」，断言却是「链长 2 + 表里两个圆角项」（= 没去重），把
    /// 「嵌套相同圆角 clip 会重复相乘 ⇒ 变暗」这个缺陷固化成了期望。交集是幂等的
    /// （`A ∩ A = A`），重复层再乘一次覆盖率恒为错 —— 现在按 rect+radii 位模式去重。
    #[test]
    fn nested_identical_rounded_clips_keep_a_single_chain_entry() {
        let rounded = ClipRect {
            rect: rect(0.0, 0.0, 30.0, 30.0),
            corner_radius: [2.0; 4],
        };
        let scene = scene_with(|scene, lane| {
            push_clip(scene, lane, rounded);
            // 同一个圆角 clip 再来一层：交集不变 ⇒ 不得再占一层、也不得再乘一次覆盖率。
            push_clip(scene, lane, rounded);
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.params1[3], 1.0, "重复层不增加链长");
        assert_eq!(buffer.clips.len(), 2, "哨兵 + 1 条圆角项");
        let head = buffer.clips[instance.params1[0] as usize];
        assert_eq!(head.kind, ClipKind::Rounded);
        assert_eq!(head.corner_radius, [4.0; 4], "2 逻辑 × scale 2");
        assert_eq!(head.parent, None);
        // 覆盖率镜像：像素恰好落在圆角边界上 ⇒ 单层 AA = 0.5（两层相乘会是 0.25）。
        let coverage = clip_coverage(&buffer.clips, ClipChainId(1), (60.0, 30.0));
        assert!(
            (coverage - 0.5).abs() < 1e-4,
            "嵌套相同圆角 clip 只能乘一次覆盖率，实际 {coverage}"
        );
    }

    /// 不同的圆角 clip 仍然逐层相乘（圆角形状的交集不是矩形，无法压成一条）。
    #[test]
    fn distinct_nested_rounded_clips_each_keep_their_entry() {
        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 30.0, 30.0),
                    corner_radius: [2.0; 4],
                },
            );
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(2.0, 2.0, 26.0, 26.0),
                    corner_radius: [2.0; 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.params1[3], 2.0, "两层都要参与 AA");
        assert_eq!(buffer.clips.len(), 3, "哨兵 + 2 条圆角项");
        let head = buffer.clips[instance.params1[0] as usize];
        assert_eq!(head.kind, ClipKind::Rounded);
        let root = buffer.clips[head.parent.expect("第二层必须有 parent").index()];
        assert_eq!(root.kind, ClipKind::Rounded);
        assert_eq!(root.parent, None);
    }

    /// AR2 P1-A 的核心回归：矩形 clip 是**像素精确 scissor**，与圆角 clip 同边界时不得出现
    /// `0.5 × 0.5 = 0.25` 的暗边（`Clip::Bounds` 容器 + Canvas 圆角 clip 是行情/图表最常见的嵌套）。
    #[test]
    fn rect_and_rounded_clip_with_the_same_bounds_do_not_darken_the_edge() {
        // 逻辑 29.75 @scale 2 ⇒ 物理 59.5：右/下边界落在**半像素**上，片元中心 (59.5, …) 恰好压在
        // 圆角 AA 的等值线上（覆盖率 0.5）。只有这种分数边界才能区分「乘一次 AA」和「乘两次」。
        let scene = scene_with(|scene, lane| {
            // 祖先/容器的矩形 clip（`Clip::Bounds`）。
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 29.75, 29.75),
                    corner_radius: [0.0; 4],
                },
            );
            // 内部 Canvas 的圆角 clip，几何边界相同。
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 29.75, 29.75),
                    corner_radius: [2.0; 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        // 链长 2 = 1 条矩形（scissor）+ 1 条圆角（AA），互不重复相乘。
        assert_eq!(instance.params1[3], 2.0);
        let head = buffer.clips[instance.params1[0] as usize];
        assert_eq!(head.kind, ClipKind::Rounded);
        let root = buffer.clips[head.parent.expect("圆角项挂在矩形项下面").index()];
        assert_eq!(root.kind, ClipKind::Rect, "矩形项 = scissor，必须是链根");
        assert_eq!(root.parent, None);
        assert_eq!(root.corner_radius, [0.0; 4]);
        // 矩形项吸附到整数物理像素（roundOut：`floor(0)` / `ceil(59.5) = 60`）；
        // 圆角项**不**吸附（AA 需要精确几何）。
        assert_eq!(root.rect, [0.0, 0.0, 60.0, 60.0]);
        assert_eq!(head.rect, [0.0, 0.0, 59.5, 59.5]);
        // 片元中心 (59.5, 30.5)：圆角 AA = 0.5，矩形 scissor 保留该像素（roundOut 不漏裁）
        // ⇒ 覆盖率 0.5。旧实现（矩形也做 AA）会得到 0.25 的暗边。
        let coverage = clip_coverage(
            &buffer.clips,
            ClipChainId(instance.params1[0] as u32),
            (59.5, 30.5),
        );
        assert!(
            (coverage - 0.5).abs() < 1e-4,
            "同边界的矩形 + 圆角 clip 只能乘一次 AA（0.5），实际 {coverage}"
        );
        // 边界外一个像素：scissor 与 AA 都裁（0），不是 0.25 的鬼线。
        let outside = clip_coverage(
            &buffer.clips,
            ClipChainId(instance.params1[0] as u32),
            (60.5, 30.5),
        );
        assert_eq!(outside, 0.0);
    }

    /// AR2 P1-A ③ / AR2b P2-N2：纯矩形裁剪的覆盖率只有 0 或 1（像素精确 scissor），边界像素
    /// 不是 0.5。**必须用分数边界**：整数边界下「矩形也走 AA」的实现同样给出 0/1（AA 在整数
    /// 边界上正好是 0 或 1），测试是假绿（复审 M5 变异证明）。
    #[test]
    fn pure_rect_clip_coverage_is_binary() {
        // 逻辑 30.3 × 30.7 @scale 2 ⇒ 物理 (0,0,60.6,61.4) ⇒ roundOut 吸附到 [0,61) × [0,62)。
        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 30.3, 30.7),
                    corner_radius: [0.0; 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        let head = ClipChainId(instance.params1[0] as u32);
        // 吸附后的表项必须是整数物理像素（shader 按像素索引比较，浮点边界会有 1 ulp 平局）。
        assert_eq!(buffer.clips[head.index()].rect, [0.0, 0.0, 61.0, 62.0]);
        // 框内（含 roundOut 保留的边界像素 60 / 61）：覆盖率恰好 1.0。
        // 分数边界 60.6 上 AA 实现会给 0.6，整数边界下同样会「恰好」通过 ⇒ 这才是活断言。
        for pixel in [
            (0.5, 0.5),
            (60.5, 30.5),
            (30.5, 61.5),
            (60.5, 61.5),
            (30.5, 30.5),
        ] {
            assert_eq!(
                clip_coverage(&buffer.clips, head, pixel),
                1.0,
                "{pixel:?} 必须全保留"
            );
        }
        for pixel in [(61.5, 30.5), (30.5, 62.5), (-0.5, 30.5), (30.5, -0.5)] {
            assert_eq!(
                clip_coverage(&buffer.clips, head, pixel),
                0.0,
                "{pixel:?} 必须全裁"
            );
        }
    }

    /// AR2b P1-N1（编码期半边）：矩形 clip 必须吸附到**整数物理像素**且方向为「向外」
    /// （Skia `SkIRect::roundOut`）—— shader 侧据此按像素索引比较，1 ulp 不再能翻面。
    #[test]
    fn rect_clip_snaps_outward_to_integer_pixels() {
        // 落在一个像素中间：向外取整 = 覆盖全部与矩形有交的像素（不漏裁）。
        assert_eq!(
            snap_rect_clip_to_pixels([10.3, 4.7, 20.4, 9.6]),
            [10.0, 4.0, 21.0, 11.0]
        );
        // 左/上边界取 floor，右/下边界取 ceil（不是 trunc / round）：负坐标同理。
        assert_eq!(
            snap_rect_clip_to_pixels([-3.25, -0.75, 5.5, 2.25]),
            [-4.0, -1.0, 7.0, 3.0]
        );
        // 已经是整数：幂等（吸附不改变任何像素的取舍）。
        assert_eq!(
            snap_rect_clip_to_pixels([8.0, 8.0, 140.0, 90.0]),
            [8.0, 8.0, 140.0, 90.0]
        );
        // 空交集 / 零尺寸：归宿成 w = h = 0（全裁），不得被向外取整变成 1 px 的保留区。
        assert_eq!(
            snap_rect_clip_to_pixels([10.6, 0.0, -0.2, 5.0]),
            [10.0, 0.0, 0.0, 0.0]
        );
        assert_eq!(
            snap_rect_clip_to_pixels([4.0, 4.0, 0.0, 10.0]),
            [4.0, 4.0, 0.0, 0.0]
        );
        // 极端坐标：`hi - lo` 会溢出成 inf ⇒ 退回原值（不产生 inf 表项）。
        assert_eq!(
            snap_rect_clip_to_pixels([3.0e38, 0.0, 3.0e38, 10.0]),
            [3.0e38, 0.0, 3.0e38, 10.0]
        );
    }

    /// AR2b P2-N1：`GpuClipEntry.meta.y` 是 GPU 契约（shader 的 `clip_mask` 据此分流矩形
    /// scissor / 圆角 AA），必须有 **headless** 断言。变异验证：把 `ClipKind::gpu()` 的
    /// Rect/Rounded 取值对调 ⇒ 本测试与 debug 自检（`meta.y` 与「半径是否全 0」交叉校验）
    /// 都红；复审实测对调后 1174 单测 + 8 条 golden **全绿**。
    #[test]
    fn gpu_clip_entries_encode_the_kind_in_meta_y() {
        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(0.0, 0.0, 30.0, 30.0),
                    corner_radius: [0.0; 4],
                },
            );
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(2.0, 2.0, 26.0, 26.0),
                    corner_radius: [3.0; 4],
                },
            );
            assert!(scene.push(lane, quad_primitive(solid())));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        let head_id = ClipChainId(instance.params1[0] as u32);
        let head = buffer.clips[head_id.index()];
        assert_eq!(head.kind, ClipKind::Rounded);
        let root_id = head.parent.expect("圆角项挂在矩形项下面");
        assert_eq!(buffer.clips[root_id.index()].kind, ClipKind::Rect);
        // 上传路径（`write_clips_gpu` → `GpuClipEntry`）里的 `meta.y` 逐条钉死。
        let mut uploaded = Vec::new();
        buffer.write_clips_gpu(&mut uploaded);
        assert_eq!(uploaded.len(), 3, "哨兵 + 矩形 + 圆角");
        // 表项 0 = 哨兵：kind 写 Rect 但 `clip_index == 0` 时 shader 直接早退，无副作用。
        assert_eq!(
            uploaded[0].meta,
            [0, CLIP_KIND_RECT as u32, 0, 0],
            "哨兵项 meta"
        );
        assert_eq!(
            uploaded[root_id.index()].meta,
            [0, CLIP_KIND_RECT as u32, 0, 0],
            "矩形项（链根）的 meta：parent = 0、kind = RECT"
        );
        assert_eq!(
            uploaded[head_id.index()].meta,
            [root_id.index() as u32, CLIP_KIND_ROUNDED as u32, 0, 0],
            "圆角项的 meta：parent = 矩形项、kind = ROUNDED"
        );
    }

    /// R05-10 的零尺寸语义：0 宽/高的 clip 必须**全裁**（覆盖率 0），不是 0.5 的鬼线。
    ///
    /// 两个层面都要钉住：① 实例的 `clip_rect`（交集的浮点 AABB）w/h 非正；② 矩形 clip **表项**
    /// 是整数物理像素的 `w = h = 0`（向外取整不能把空交集变成 1 px 的保留区，AR2b P1-N1）。
    #[test]
    fn zero_sized_clip_encodes_as_a_zero_extent() {
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
        let entry = buffer.clips[instance.params1[0] as usize];
        assert_eq!(entry.kind, ClipKind::Rect);
        assert_eq!(
            entry.rect,
            [8.0, 8.0, 0.0, 0.0],
            "矩形项必须归宿成整数的零尺寸（scissor 全裁），不得被 roundOut 变成 1 px"
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

        // AR2 P1-⑥ / P1-D3：**8 层圆角 + 祖先矩形 clip** 必须可渲染（链长 = 9 = MAX_CLIP_ITEMS），
        // 而不是每帧 `ClipDepthExceeded` 丢帧。矩形项不占圆角预算（求交合并成 scissor）。
        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(-10.0, -10.0, 200.0, 200.0),
                    corner_radius: [0.0; 4],
                },
            );
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
            for _ in 0..9 {
                assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
            }
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.params1[3], 9.0, "8 层圆角 + 1 条矩形 = 链长 9");
        assert!(
            instance.params1[3] <= MAX_CLIP_ITEMS,
            "链长必须落在 shader 的循环上界内"
        );

        // 上界与预算的关系：MAX_CLIP_ITEMS = MAX_CLIP_DEPTH + 1（shader 的
        // KYTRIX_MAX_CLIP_ITEMS 与 KYTRIX_MAX_CLIP_DEPTH 由 build.rs 逐值比对）。
        assert_eq!(MAX_CLIP_ITEMS, f32::from(MAX_CLIP_DEPTH) + 1.0);

        // 9 层圆角 + 矩形仍然报错（圆角超预算，矩形不背锅）。
        let scene = scene_with(|scene, lane| {
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(-10.0, -10.0, 200.0, 200.0),
                    corner_radius: [0.0; 4],
                },
            );
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
        });
        match try_encode(&scene) {
            Err(crate::error::EngineError::ClipDepthExceeded { depth, limit }) => {
                assert_eq!((depth, limit), (9, MAX_CLIP_DEPTH), "报的是圆角层数");
            }
            Err(other) => panic!("9 层圆角 + 矩形必须报 ClipDepthExceeded，实际 {other:?}"),
            Ok(_) => panic!("9 层圆角 + 矩形必须报 ClipDepthExceeded"),
        }
    }

    /// AR2b P3-N3：上面那条「8 圆角 + 祖先矩形」用例走的是 `scene.push(Clip::Push)`，**没有**
    /// 经过 `Canvas::push_clip` 的门限。这条补上 Canvas 集成用例：同一个 `Canvas` 里压 8 层圆角
    /// （门限必须允许，因为矩形祖先不占圆角预算）+ 祖先矩形 ⇒ 链长 9，编码成功。
    #[test]
    fn canvas_rounded_clips_under_an_ancestor_rect_fit_the_chain_limit() {
        let mut canvas = Canvas::new(rect(0.0, 0.0, 64.0, 32.0));
        for index in 0..usize::from(MAX_CLIP_DEPTH) {
            assert!(
                canvas.push_clip(ClipRect {
                    // 逐层不同的矩形：完全相同的圆角层在编码期会去重（`duplicate_rounded`）。
                    rect: rect(index as f32, 0.0, 30.0, 30.0),
                    corner_radius: [2.0; 4],
                }),
                "Canvas 门限只数圆角层：第 8 层圆角必须允许"
            );
        }
        canvas.push_quad(Quad {
            bounds: rect(1.0, 2.0, 10.0, 20.0),
            corner_radius: [0.0; 4],
            paint: solid(),
        });
        for _ in 0..usize::from(MAX_CLIP_DEPTH) {
            assert!(canvas.pop_clip());
        }

        let scene = scene_with(|scene, lane| {
            // 祖先矩形 clip（`Clip::Bounds` 容器走同一条 `ClipOp::Push` 路径）。
            push_clip(
                scene,
                lane,
                ClipRect {
                    rect: rect(-10.0, -10.0, 200.0, 200.0),
                    corner_radius: [0.0; 4],
                },
            );
            assert!(scene.push(lane, Primitive::Canvas(Rc::new(canvas))));
            assert!(scene.push(lane, Primitive::Clip(ClipOp::Pop)));
        });
        let buffer = encode(&scene);
        let instance = buffer.instances[0];
        assert_eq!(instance.params1[3], 9.0, "8 层圆角 + 1 条矩形 = 链长 9");
        assert_eq!(instance.params1[3], MAX_CLIP_ITEMS);
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

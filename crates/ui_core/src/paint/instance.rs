//! `GpuPrimitive` 契约（128 B / 16 B 对齐）与 CPU 侧实例编码。
//!
//! 本文是 `render/shaders/contract.md` 的**权威来源**（I2/I8）：字段顺序、offset、`kind`/`params`
//! 取值表的任何改动都必须同时改 Rust、三套 shader、`contract.md`，并触发像素快照回归。
//!
//! 范围（`ui_core-架构决策-B3B6.md` §2）：
//! - 8 类 `Primitive` → `GpuPrimitive`（`GlyphRun` 是唯一的 1→N）；
//! - `Path`/`Geometry` 0 实例 + `FrameOp::Geometry`；
//! - clip 链去重（`ClipChainId`，**跨帧稳定索引**）+ **轴对齐矩形求交合并**（Skia `SkClipStack`
//!   语义）+ 矩形/圆角两种覆盖率语义（矩形 = 像素精确 scissor、圆角 = 解析 AA；见 [`ClipKind`]）；
//! - **按 lane 分段 + 版本复用**的增量编码：`Scene` 的每条 lane 有内容版本，版本未变 ⇒ 整段
//!   复用（不重编码、不重传）；版本变化才重编码那一条 lane 并只上传它的实例段。实例环 =
//!   「lane 段 + 空洞」，容量按历史峰值保留（`compaction` 在无脏帧压实）；
//! - 绘制序 op 按**连续 lane 段**合并（一条 draw call 画多条 lane），每条 op 带真实包围盒，
//!   damage 的 op 级跳过因此真正生效。
//!
//! **P07 只做上传/批次**：本模块不碰 GPU、不持设备。

use smallvec::SmallVec;

use crate::{
    error::EngineError,
    math::{Point, Rect, Scale, Transform},
    paint::{
        atlas::MAX_IMAGE_PAGES,
        color::{ColorResolver, LinearPremulRgba},
        primitive::{ClipOp, GradientKind, Primitive, QuadPaint},
        scene::MAX_CLIP_DEPTH,
    },
};

/// 编码器的**帧源**：绘制序 + 每条 lane 的内容版本与图元（B6-2）。
///
/// 两个实现：`Scene`（UI 线程的 inline 合成路径）与 `LaneStore`（合成线程的 delta 应用结果）。
/// 编码器只依赖这个窄接口，因此「编码发生在哪个线程」对编码逻辑完全透明 —— 全量与增量的产物
/// 逐字节一致的可验证性就建立在这上面。
pub(crate) trait LaneSource {
    /// 一条 lane 的内容版本 + 图元切片（`None` = 句柄陈旧/已释放）。
    fn lane_state(&self, id: crate::paint::scene::LaneId) -> Option<(u32, &[Primitive])>;

    /// 本帧的 lane 绘制序。
    fn order(&self) -> &[crate::paint::scene::LaneId];

    /// 该 lane 属于哪个滚动槽（`0` = 不随滚动偏移；见 `render/delta.rs` 的坐标系约定）。
    ///
    /// 合成线程只按「槽位 → 偏移表」在绘制期平移，因此**滚动不改变 lane 内容**；这里返回的槽位
    /// 参与段的复用判据（槽位变了必须重编码，因为 op 的归属变了）。
    fn lane_scroll(&self, _id: crate::paint::scene::LaneId) -> u32 {
        0
    }

    /// 该 lane 的几何块绘制范围（None = 非几何块 lane / 整块几何绘制；X5）。
    ///
    /// 编码器据此把这条 lane 编码成「只画第 chunk 块」的几何 op（几何节点的块 lane），
    /// 因此一次数据更新只重编码受影响的那一条 lane。
    fn lane_geometry_chunk(
        &self,
        _id: crate::paint::scene::LaneId,
    ) -> Option<(crate::paint::geometry::GeometryId, u32)> {
        None
    }

    /// 本帧**已确认变化**的 lane 集合（编码器增量快路径的输入）。
    ///
    /// `None` = 帧源不知道本帧的变化集（`Scene`：UI 侧的脏表已经被 `DeltaBuilder` 消费），
    /// 编码器必须全量扫描绘制序。
    ///
    /// **契约**：返回 `Some` 的帧源必须保证「除这些 lane 的内容之外，绘制序、lane 集合、
    /// 段表布局自上次编码以来都没有变」。`LaneStore::apply` 用内部 `fast` 标志守这条
    /// （绘制序变化 / lane 移除 / 代际复用 / 全量重置都会让它返回 `None`）。
    fn changed_lanes(&self) -> Option<&[crate::paint::scene::LaneId]> {
        None
    }
}

/// 编码期的几何快照查表（`GeometryStore` 在 UI 线程；合成线程用自己的 `GeometryCatalog`）。
pub(crate) trait GeometryLookup {
    fn snapshot_arc(
        &self,
        id: crate::paint::geometry::GeometryId,
    ) -> Option<std::sync::Arc<crate::paint::geometry::GeometrySnapshot>>;
}

impl GeometryLookup for crate::paint::geometry_store::GeometryStore {
    fn snapshot_arc(
        &self,
        id: crate::paint::geometry::GeometryId,
    ) -> Option<std::sync::Arc<crate::paint::geometry::GeometrySnapshot>> {
        crate::paint::geometry_store::GeometryStore::snapshot_arc(self, id)
    }
}

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
    /// `clip_index, atlas_page, kind, clip_depth`（整数编码进 f32）。
    ///
    /// `atlas_page` 是**图集 texture array 的层号**（图集页索引）：字形图集恒 1 层 ⇒ 恒 `0`；
    /// 图片图集 `0..page_count`（`AtlasTile.page`；`MAX_IMAGE_PAGES` 只是页号上限）。采样哪张图集由 `.z`（`kind`）决定，
    /// `.y` 只是该图集的层号（采样坐标，不是 sampler 索引，见 contract §2/§7）。
    /// `clip_depth` 是 GPU 裁剪链的**真实链长** = 至多一条矩形项 + k 条圆角项
    /// （k ≤ `MAX_CLIP_DEPTH`），上界 [`MAX_CLIP_ITEMS`]；shader 用它收口 clip 循环。
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
pub(crate) fn normalize_corner_radius(radii: [f32; 4], width: f32, height: f32) -> [f32; 4] {
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

/// `ClipChain` 在裁剪表里的索引（**跨帧稳定**：表只追加 / 按内容去重，`0` 是「无裁剪」哨兵）。
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
        ///
        /// **烘焙坐标空间**：bounds 是编码时实例数据里的坐标，不含滚动偏移；合成端偏移
        /// （`scroll`）在绘制期由 `RootUniform::with_scroll` 加到顶点上，`bounds` 因此对
        /// damage / op 跳过是保守的（滚动只改变 op 落在屏幕上的位置，不改变它的大小）。
        bounds: PhysicalBounds,
        /// 滚动槽（`0` = 不随滚动偏移）。
        ///
        /// 同一条 op 里的实例**必须属于同一个滚动槽**：`append_segment_ops` 在槽位变化时切断
        /// 合并（否则一个 draw call 会把两组不同的偏移加到同一批顶点上）。
        scroll: u32,
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

    /// 本 op 的滚动槽（`0` = 不随滚动偏移；`Geometry` 没有槽，恒 0）。
    ///
    /// 消费者：三个后端的 op 级跳过 —— 它们要把 `bounds`（烘焙坐标）平移到屏幕坐标再与
    /// damage / scissor 求交。
    pub(crate) fn scroll(self) -> u32 {
        match self {
            Self::Instances { scroll, .. } => scroll,
            Self::Geometry { .. } => 0,
        }
    }
}

/// op 的**绘制期平移量**（物理像素；槽位越界 / 无槽 ⇒ `[0, 0]`）。
///
/// 表就是 `FramePayload::scroll`（槽位 → 平移量，`ScrollTable::physical_offset` 的产物）。
/// 三个后端的 op 级跳过都用它把 `FrameOp::bounds`（烘焙坐标）平移到屏幕坐标。
pub(crate) fn scroll_offset(scroll: &[[f32; 2]], op: FrameOp) -> [f32; 2] {
    scroll
        .get(op.scroll() as usize)
        .copied()
        .unwrap_or([0.0, 0.0])
}

/// 把帧 scissor 与本 op 所属滚动槽的**屏幕裁剪区**求交（`None` = 不收窄）。
///
/// 滚动容器的视口 clip 不进实例的裁剪链（它随滚动在内容坐标里移动，见 `paint/pass.rs` 的
/// `paint_node`）—— 它改由这个每 op scissor 承担，精度与烘焙 clip 相同（都是轴对齐矩形）。
/// 槽位 0（不随滚动偏移）与越界槽位都返回 `None`。
pub(crate) fn scrolled_scissor(
    scroll_clip: &[[i32; 4]],
    slot: u32,
    scissor: [i32; 4],
) -> Option<[i32; 4]> {
    if slot == 0 {
        return None;
    }
    let clip = scroll_clip.get(slot as usize)?;
    Some([
        scissor[0].max(clip[0]),
        scissor[1].max(clip[1]),
        scissor[2].min(clip[2]),
        scissor[3].min(clip[3]),
    ])
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

    /// 平移到屏幕坐标：op 的 `bounds` 与实例字节同处**烘焙坐标空间**，而 damage / scissor 是
    /// 屏幕坐标。局部刷新的 op 跳过判据必须先把 bounds 平移（`offset` = `FramePayload::scroll`
    /// 里该槽的绘制期平移量，物理像素），否则滚动帧会把本该绘制的 op 提前剔掉 —— 横向滚动时
    /// 尤其明显（内容烘焙在 x≈1000、视口在 0..800，相交判据直接失败）。
    ///
    /// 空区间（`EMPTY`）平移后仍是空区间：用饱和加减保住这个性质。
    pub(crate) fn translated(self, offset: [f32; 2]) -> Self {
        let dx = offset[0].round() as i32;
        let dy = offset[1].round() as i32;
        Self {
            x0: self.x0.saturating_add(dx),
            y0: self.y0.saturating_add(dy),
            x1: self.x1.saturating_add(dx),
            y1: self.y1.saturating_add(dy),
        }
    }

    /// 并入另一个可见范围（`build_ops` 的 lane 段合并）。
    fn union(self, other: Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
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

/// 绘制序里一条 op 最多合并多少条**实例环连续**的 lane。
///
/// 为什么不「一条 lane 一条 op」：10k lane 的整帧会产生 10k 次 draw call（三个后端的单次提交
/// 是微秒级），整帧绘制反而更慢。合并后一条 op 覆盖一段连续的 lane 段，`bounds` 是这些段的
/// 实例包围盒并集 —— damage 的 op 级跳过照旧生效（代价只是同一组内的邻居多画一遍，仍被
/// scissor 夹住）。
const MAX_LANES_PER_OP: u32 = 64;

/// 裁剪表条数上限：超过就**整表重建**（重置 ⇒ 下一帧全量重编码）。
///
/// 裁剪表跨帧持久（复用实例段里内嵌的 `clip_index` 必须继续有效），因此只增不减；动画 /
/// 滚动每帧产生的旧表项是垃圾。上限把「每帧整表上传」的字节数钉住（2048 × 48 B ≈ 96 KiB），
/// 重置的代价是一帧全量编码 —— 只有长动画才会触发。
const CLIP_TABLE_LIMIT: usize = 2048;

/// 变化区间的合并间隙（实例数）：距离小于该值的两个区间并成一块上传。
///
/// 间隙里的字节是垃圾（没有任何 op 引用它），多搬几个实例（每条 128 B）比多一次 chunk
/// 记账便宜。取 8 = 1 KiB 一次 `memcpy` 的零头。
const CHANGE_MERGE_GAP: u32 = 8;

/// 一条 lane 的实例段（跨帧保留：版本一致时整段复用，不重编码）。
struct LaneSegment {
    /// 编码时的 lane 代际（arena 槽位被新 lane 复用时旧段作废）。
    generation: u32,
    /// 编码时的 lane 版本；与 `LaneSource::lane_state` 不等 ⇒ 重编码。
    version: u32,
    /// 编码时的滚动槽（与 `LaneSource::lane_scroll` 不等 ⇒ 重编码）。
    scroll: u32,
    /// 实例环下标区间 `[start, end)`（`end - start` = 本 lane 的实例数）。
    start: u32,
    end: u32,
    /// 段内绘制项（实例 run 与几何绘制按绘制序交错）。
    parts: SmallVec<[LanePart; 2]>,
}

impl LaneSegment {
    fn range(&self) -> std::ops::Range<u32> {
        self.start..self.end
    }

    fn len(&self) -> u32 {
        self.end - self.start
    }
}

/// lane 内的一个绘制项。
///
/// 几何项**装箱**：`GeometryDrawGpu` 带 24 B 变换 + `Arc`，内联会把 `LanePart` 撑到 ~100 B
/// （每 lane 一个段，10k lane 就是 MB 级常驻）。装箱后每个绘制项 24 B。
enum LanePart {
    /// 段内连续 `len` 个实例。
    Instances { len: u32, bounds: PhysicalBounds },
    /// 一次几何绘制；每帧生成 op 表时把 `draw` 复制进 `InstanceBuffer::geometry`。
    Geometry(Box<LaneGeometry>),
}

/// 几何绘制项（`LanePart::Geometry` 的载荷）。
struct LaneGeometry {
    draw: GeometryDrawGpu,
    clip_index: u32,
    clip_rect: [u32; 4],
    bounds: PhysicalBounds,
}

/// 自上次取走以来内容变化的实例区间（**增量载荷**的唯一来源）。
#[derive(Default)]
struct ChangeSet {
    ranges: Vec<std::ops::Range<u32>>,
    /// `ranges` 是否已按 `start` 升序（按绘制序写入时天然有序，只有段搬移才乱序）。
    sorted: bool,
}

impl ChangeSet {
    fn clear(&mut self) {
        self.ranges.clear();
        self.sorted = true;
    }

    fn push(&mut self, range: std::ops::Range<u32>) {
        if range.is_empty() {
            return;
        }
        if self
            .ranges
            .last()
            .is_some_and(|last| last.start > range.start)
        {
            self.sorted = false;
        }
        self.ranges.push(range);
    }

    /// 排序 + 合并（间隙 < `CHANGE_MERGE_GAP` 的并成一块），返回合并后的区间。
    fn merged(&mut self) -> &[std::ops::Range<u32>] {
        if !self.sorted {
            self.ranges.sort_unstable_by_key(|range| range.start);
            self.sorted = true;
        }
        let mut out = 0usize;
        for read in 0..self.ranges.len() {
            let range = self.ranges[read].clone();
            if out > 0 {
                let last = &mut self.ranges[out - 1];
                if range.start <= last.end.saturating_add(CHANGE_MERGE_GAP) {
                    last.end = last.end.max(range.end);
                    continue;
                }
            }
            self.ranges[out] = range;
            out += 1;
        }
        self.ranges.truncate(out);
        &self.ranges
    }
}

/// 一帧的编码产物：实例环 + 裁剪表（两者**跨帧持久**）与每帧重建的绘制序 / 几何表。
///
/// 实例环是「lane 段 + 空洞」的拼接（按长度精确打包、首次适配复用底层空闲块）。**不留对齐
/// 填充**：相邻 lane 的段必须首尾相接，绘制序才能把连续 lane 合并成一条 op（对齐填充会
/// 让 10k lane 变成 10k 次 draw call）。空洞里的字节是垃圾，没有任何 op 引用它；压缩只在
/// 无脏帧执行。裁剪表跨帧持久的原因见 [`InstanceEncoder`]。
pub(crate) struct InstanceBuffer {
    instances: Vec<GpuPrimitive>,
    /// 空闲实例块（升序、不重叠、不相邻）。
    free: Vec<std::ops::Range<u32>>,
    /// 实例环已分配到的末尾（= 已分配块的长度之和）。
    used: u32,
    clips: Vec<ClipChain>,
    geometry: Vec<GeometryDrawGpu>,
    ops: Vec<FrameOp>,
    /// lane arena 槽位下标 → 实例段（陈旧代际的项在下一次命中的槽位时回收）。
    segments: Vec<Option<Box<LaneSegment>>>,
    /// 自上次取走以来内容变化的区间。
    changed: ChangeSet,
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
    /// 本 op 画几何的第几块（块在索引缓冲里连续；绘制范围来自 `snapshot.chunks()[chunk]`）。
    ///
    /// 整块几何绘制（`Canvas::push_geometry` / `Path`）由编码器展开成每块一个 op；
    /// 几何节点的多 lane 分块让每块 lane 只编码自己的 op（X5）。
    pub chunk: u32,
    pub transform: Transform,
    /// linear premultiplied RGBA（`draw.color0`）。
    pub color: [f32; 4],
    /// 几何数据的不可变快照（页 + 块元数据）。
    ///
    /// 随帧过线是**有意**的：`GeometryStore` 在 UI 线程，GPU 线程不能直接读它；把快照附在
    /// 绘制项上，GPU 线程按 `(index, generation)` 缓存自己的缓冲，按页 `ptr_eq` 增量上传，
    /// 既不需要额外的跨线程注册通道，也让「注册后第一帧」自然完成上传。
    pub snapshot: std::sync::Arc<crate::paint::geometry::GeometrySnapshot>,
}

impl InstanceBuffer {
    fn new() -> Self {
        let mut buffer = Self {
            instances: Vec::with_capacity(256),
            free: Vec::new(),
            used: 0,
            clips: Vec::with_capacity(8),
            geometry: Vec::with_capacity(8),
            ops: Vec::with_capacity(16),
            segments: Vec::new(),
            changed: ChangeSet {
                ranges: Vec::new(),
                sorted: true,
            },
        };
        buffer.clips.push(CLIP_SENTINEL);
        buffer
    }

    /// 全量重建：清空实例环 / 段表 / 裁剪表（保留容量）。调用方负责随后重编码全部 lane。
    fn reset(&mut self) {
        self.instances.clear();
        self.free.clear();
        self.used = 0;
        self.clips.clear();
        self.clips.push(CLIP_SENTINEL);
        self.geometry.clear();
        self.ops.clear();
        self.segments.clear();
        self.changed.clear();
    }

    /// 本帧存活实例数（**不含空洞**；绘制统计与 `FrameDump` 的实例计数）。
    pub(crate) fn len(&self) -> usize {
        self.segments
            .iter()
            .flatten()
            .map(|segment| segment.len() as usize)
            .sum()
    }

    /// 实例环总实例数（含空洞；GPU 侧缓冲容量 / SRV 尺寸的判据）。
    pub(crate) fn instance_count(&self) -> u32 {
        self.used
    }

    /// 仅测试 / 诊断：本帧**存活**实例里图片图元引用的 texture array 层号（升序，含重复）。
    ///
    /// 消费者：fail-closed 回归测试（RBT-01 不变量 1 —— 「扩层未建立时不得有任何 Image draw
    /// 引用新层，旧层照画」）。空洞与已释放段不参与（实例环里它们的字节是垃圾）。
    ///
    /// 该方法的唯一消费者是 `app::resources_tests` 里读 `Window::instance_buffer()` 的用例，
    /// 而后者本身由 `feature = "test-support"` 门控（`InstanceBuffer` 只在测试面暴露）；
    /// 因此方法也带同一 feature 守卫，默认特性（无测试消费者）下不会变成 dead code。
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn image_layers(&self) -> Vec<u16> {
        let mut layers = Vec::new();
        for segment in self.segments.iter().flatten() {
            for instance in &self.instances[segment.start as usize..segment.end as usize] {
                if instance.params1[2] == KIND_IMAGE {
                    layers.push(instance.params1[1] as u16);
                }
            }
        }
        layers.sort_unstable();
        layers
    }

    /// `GpuPrimitive` 数组的字节视图（P02 §8.1 的 128 B 布局；全量载荷的上传源）。
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

    /// 自上次取走以来变化的实例区间（升序、已合并；**增量载荷的来源**）。
    pub(crate) fn changes(&mut self) -> &[std::ops::Range<u32>] {
        self.changed.merged()
    }

    /// 变化区间已被载荷取走：本帧之后的变化从这个基线重新累计。
    pub(crate) fn clear_changes(&mut self) {
        self.changed.clear();
    }

    /// 存活实例切片（按实例环下标升序；诊断导出用，非热路径）。
    #[cfg(feature = "test-support")]
    pub(crate) fn live_instances(&self) -> impl Iterator<Item = &[GpuPrimitive]> + '_ {
        let mut ranges: Vec<std::ops::Range<u32>> = self
            .segments
            .iter()
            .flatten()
            .filter(|segment| !segment.range().is_empty())
            .map(|segment| segment.range())
            .collect();
        ranges.sort_unstable_by_key(|range| range.start);
        ranges
            .into_iter()
            .map(move |range| &self.instances[range.start as usize..range.end as usize])
    }

    /// 分配 `len` 个实例；返回实际下标区间。
    ///
    /// 首次适配：优先复用空闲块（按长度精确切分，多出来的部分留在空闲表里），否则在末尾
    /// 顺延增长。**不做对齐填充** —— 精确打包是「相邻 lane 段连续」的前提，绘制序的 op
    /// 合并（一条 draw call 画多条 lane）依赖它。
    fn allocate(&mut self, len: u32) -> std::ops::Range<u32> {
        if len == 0 {
            return self.used..self.used;
        }
        for index in 0..self.free.len() {
            let range = self.free[index].clone();
            if range.end - range.start < len {
                continue;
            }
            let start = range.start;
            if range.end - start > len {
                self.free[index] = start + len..range.end;
            } else {
                self.free.remove(index);
            }
            return start..start + len;
        }
        let start = self.used;
        self.used += len;
        if self.instances.len() < self.used as usize {
            self.instances
                .resize(self.used as usize, bytemuck::Zeroable::zeroed());
        }
        start..start + len
    }

    /// 归还一段实例（与相邻空闲块合并）。
    fn release(&mut self, range: std::ops::Range<u32>) {
        if range.is_empty() {
            return;
        }
        let mut index = 0;
        while index < self.free.len() && self.free[index].start < range.start {
            index += 1;
        }
        self.free.insert(index, range);
        if index + 1 < self.free.len() && self.free[index].end == self.free[index + 1].start {
            let next = self.free.remove(index + 1);
            self.free[index].end = next.end;
        }
        if index > 0 && self.free[index - 1].end == self.free[index].start {
            let current = self.free.remove(index);
            self.free[index - 1].end = current.end;
        }
    }

    /// 写入一段实例（`range` 必须来自 [`InstanceBuffer::allocate`]，长度相等）。
    fn write_instances(&mut self, range: std::ops::Range<u32>, source: &[GpuPrimitive]) {
        debug_assert_eq!(range.len(), source.len());
        self.instances[range.start as usize..range.end as usize].copy_from_slice(source);
        self.changed.push(range);
    }

    /// 实例环是否值得压实：空闲块合计 ≥ 25% 且至少 64 个实例的碎片。
    ///
    /// 门槛取「25% + 64 实例」：低于它的空洞不值得一次整环搬运（compaction 只在无脏帧执行，
    /// 但仍有 ~1.28 MB 级的内存带宽成本）。
    fn needs_compaction(&self) -> bool {
        let free: u32 = self.free.iter().map(|range| range.end - range.start).sum();
        free >= 64 && free * 4 >= self.used
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
/// `InstanceBuffer::new` / `reset` 都会先写入它：`FramePayload` 的 dump / 健康检查可以假定「clip 表恒有
/// 哨兵」，不必等第一帧编码。
const CLIP_SENTINEL: ClipChain = ClipChain {
    parent: None,
    kind: ClipKind::Rect,
    rect: [0.0; 4],
    corner_radius: [0.0; 4],
};

/// `Primitive → GpuPrimitive` 的编码器（`Window` 持有，缓冲跨帧复用）。
///
/// # 增量模型（B6 §2「按层保留 + 只重编码脏层」）
///
/// 编码产物是**按 lane 分段**的：`Scene` 每条 lane 有内容版本（`Scene::lane_state`），编码器
/// 记住每条 lane 上次编码的版本与实例段；版本一致 ⇒ 整段复用（连实例字节都不碰），版本变化才
/// 重编码那一条 lane，并把新段记进「变化区间」交给 `FramePayload` 做增量上传。
///
/// 三个跨帧持久的状态必须同时成立：
/// 1. **实例环**（`InstanceBuffer::instances`）—— 段地址稳定，未变的段内容不动；
/// 2. **裁剪表**（`InstanceBuffer::clips`）—— 实例里内嵌的 `clip_index` 指向它，段复用时索引
///    必须继续有效，所以表**不按帧清空**，只按内容去重追加（超过 [`CLIP_TABLE_LIMIT`] 整表
///    重建 ⇒ 全量重编码）；
/// 3. **段表**（`InstanceBuffer::segments`）—— lane arena 槽位 → 段。
///
/// 全量与增量的产物**逐字节一致**：两条路径用同一个 [`LaneEncoder`] 与同一套裁剪表去重逻辑，
/// 差别只是「哪些 lane 被重新编码」。全量路径的触发条件见 [`InstanceEncoder::encode`]。
pub(crate) struct InstanceEncoder {
    buffer: InstanceBuffer,
    state: ClipState,
    /// clip 栈回溯（每层 push 前保存一次完整状态；嵌套深度由调用方决定，不设硬上限）。
    saved: SmallVec<[ClipState; 8]>,
    chains: ahash::HashMap<(u32, u32, [u32; 4], [u32; 4]), ClipChainId>,
    /// 颜色解析缓存（跨帧保留，每帧 clear）。
    colors: crate::paint::color::ResolveCache,
    /// 上次编码的 DPI（变化 ⇒ 物理坐标全变 ⇒ 全量重编码）。
    scale: Option<Scale>,
    /// 单 lane 编码的实例工作区（容量跨 lane 复用；编码完成后整段搬进实例环）。
    scratch: Vec<GpuPrimitive>,
    /// 单 lane 编码的绘制项工作区。
    scratch_parts: SmallVec<[LanePart; 2]>,
    /// 本帧重编码的 lane 数（`FrameRecord.encoded_lanes`）。
    encoded_lanes: usize,
    /// 本帧重编码的几何块 op 数（`FrameRecord.geometry_encoded_chunks`；增量几何的断言面）。
    encoded_geometry_chunks: usize,
    /// 本帧重编码写进实例环的字节数（`FrameRecord.uploaded_bytes` 的编码器侧值）。
    ///
    /// 生产路径也保留：合成线程的 `ComposeStats`（HUD / 帧记录）在 release 构建里读它。
    written_bytes: usize,
    /// 本帧整段复用的 lane 段数（`FrameRecord.reused_segments`）。
    reused_segments: usize,
    /// 上一次编码是「全量扫描」并且成功了 —— 段表与 op 表因此覆盖整个绘制序。
    ///
    /// 只有它为 `true` 时才能走增量快路径（见 [`InstanceEncoder::encode_changed`]）：快路径
    /// 假定「除变化 lane 之外，段表与 op 表与上一帧一致」。
    primed: bool,
    /// 走增量快路径的帧数（仅测试：守住「快路径没被悄悄关掉」这条性能不变量）。
    #[cfg(test)]
    fast_encodes: u32,
    /// 本帧图片图集**可画层数上限**（fail-closed，RBT-01 不变量 1）：`Image` 图元引用的
    /// texture array 层号 `>=` 它时**不产出实例**（不画占位、不画错层）。
    ///
    /// 生产者是 `App::upload_image_page` 的逐窗口记账（`Window::image_drawable_layers`），
    /// 经 `SceneDelta` 送到合成端；没有 App 图集记账的驱动（快照 / `Window::frame`）保持
    /// `u32::MAX` = 不过滤（它们自己整份上传，层数上限仍由 `MAX_IMAGE_PAGES` 兜底）。
    image_layers: u32,
}

impl InstanceEncoder {
    /// 消费者：`Window::new`。
    pub(crate) fn new() -> Self {
        Self {
            buffer: InstanceBuffer::new(),
            state: ClipState::unbounded(),
            saved: SmallVec::new(),
            chains: ahash::HashMap::default(),
            colors: crate::paint::color::ResolveCache::new(),
            scale: None,
            scratch: Vec::with_capacity(64),
            scratch_parts: SmallVec::new(),
            encoded_lanes: 0,
            encoded_geometry_chunks: 0,
            reused_segments: 0,
            written_bytes: 0,
            primed: false,
            image_layers: u32::MAX,
            #[cfg(test)]
            fast_encodes: 0,
        }
    }

    /// 设置本帧图片图集可画层数上限（fail-closed，RBT-01 不变量 1）。
    ///
    /// 消费者：`render::compose::WindowComposer::compose`（每帧从 `SceneDelta` 取 App 的逐窗口
    /// 记账）。上限变小/变大都会让受影响 lane 的实例内容变化 —— 合成端在**上限变化的那一帧**
    /// 强制整帧重编码 + 全量载荷（见 `compose` 的 `limit_changed`），所以这里不需要自己通知。
    pub(crate) fn set_image_layers(&mut self, layers: u32) {
        self.image_layers = layers;
    }

    /// 编码整帧（增量：只重编码版本变化的 lane）。消费者：`Window::paint_frame`。
    ///
    /// **回退全量的条件**（不确定 ⇒ 宁可全量，不可画错）：
    /// - DPI 变化（`scale` 不同）：物理坐标、缩放后的圆角与 clip 全部变了；
    /// - 裁剪表超过 [`CLIP_TABLE_LIMIT`]：需要重置表 ⇒ 已缓存段里的 `clip_index` 全部失效。
    ///
    /// 其余「看起来可能不确定」的情形不需要这里判断：窗口 resize / 滚动 / 文案变化都会让
    /// paint pass 重建相关 lane（版本 +1），增量路径自然覆盖；图集换代同理会让文本节点重建
    /// lane（`App::repaint_text_if_atlas_changed`）。
    ///
    /// # Errors
    /// [`EngineError::ClipDepthExceeded`]：GPU 裁剪链超过 `MAX_CLIP_DEPTH`。轴对齐矩形 clip
    /// 已在编码期求交合并（只占一项），因此普通嵌套容器不会触顶；只有圆角 clip 逐层计数。
    /// 超限时**必须失败**：shader 的循环是定长，静默丢最外层 clip 会让子内容越界可见。
    pub(crate) fn encode<S: LaneSource>(
        &mut self,
        source: &S,
        scale: Scale,
        resolver: ColorResolver,
        geometry: &dyn GeometryLookup,
    ) -> Result<&InstanceBuffer, EngineError> {
        self.encoded_lanes = 0;
        self.encoded_geometry_chunks = 0;
        self.reused_segments = 0;
        self.written_bytes = 0;
        self.colors.clear();
        let reset = self.scale != Some(scale) || self.buffer.clips.len() > CLIP_TABLE_LIMIT;
        if reset {
            self.buffer.reset();
            self.chains.clear();
            self.scale = Some(scale);
            self.primed = false;
        }
        let order = source.order();
        // 快路径的门禁：**上一次**是全量扫描（段表 / op 表覆盖整个绘制序）、本帧没有整表重建、
        // 且帧源能给出可信变化集。三者缺一就退回全量扫描（行为与旧实现逐字节一致）。
        if !reset
            && self.primed
            && let Some(changed) = source.changed_lanes()
        {
            #[cfg(test)]
            {
                self.fast_encodes += 1;
            }
            self.encode_changed(source, changed, order, scale, resolver, geometry)?;
        } else {
            self.encode_scanned(source, order, scale, resolver, geometry)?;
            self.primed = true;
        }
        // 自检只在 debug 构建里跑：循环本身（遍历全部 op / clip / instance）不是
        // `debug_assert!` 的宏体，release 下也会白跑 —— 10k 图元 ≈ 40 µs/帧。
        #[cfg(debug_assertions)]
        self.self_check(order);
        Ok(&self.buffer)
    }

    /// 全量扫描编码：遍历整条绘制序（首帧 / 整表重建 / 帧源不提供变化集时的唯一路径）。
    fn encode_scanned<S: LaneSource>(
        &mut self,
        source: &S,
        order: &[crate::paint::scene::LaneId],
        scale: Scale,
        resolver: ColorResolver,
        geometry: &dyn GeometryLookup,
    ) -> Result<(), EngineError> {
        let Self {
            buffer,
            state,
            saved,
            chains,
            colors,
            scratch,
            scratch_parts,
            image_layers,
            encoded_lanes,
            encoded_geometry_chunks,
            reused_segments,
            written_bytes,
            ..
        } = self;
        // 绘制序在**同一次 lane 遍历**里生成（消掉 10k lane 的第二次 O(n) 扫描）：
        // op / 几何表先借出，循环结束时再放回（compaction 之后才需要重建）。
        let mut ops = std::mem::take(&mut buffer.ops);
        ops.clear();
        let mut geometry_ops = std::mem::take(&mut buffer.geometry);
        geometry_ops.clear();
        let mut group = OpGroup::new();
        for lane_id in order {
            let slot = lane_id.slot() as usize;
            // arena 槽位被**新** lane 复用（代际不同）：旧段属于已释放的 lane，回收。
            let stale = buffer.segments.get(slot).is_some_and(|entry| {
                entry
                    .as_ref()
                    .is_some_and(|segment| segment.generation != lane_id.generation())
            });
            if stale && let Some(old) = buffer.segments[slot].take() {
                buffer.release(old.range());
            }
            let Some((version, primitives)) = source.lane_state(*lane_id) else {
                continue;
            };
            // 滚动槽：段复用时**段本身**的滚动槽必须与本次一致，否则要重编码（段落归属变了）。
            let scroll_slot = source.lane_scroll(*lane_id);
            let geometry_chunk = source.lane_geometry_chunk(*lane_id);
            // 复用判据：段存在 + 版本一致 + 滚动槽一致 ⇒ 整段不重编码（连实例字节都不碰）。
            let reusable = buffer
                .segments
                .get(slot)
                .and_then(Option::as_ref)
                .is_some_and(|segment| segment.version == version && segment.scroll == scroll_slot);
            if reusable {
                *reused_segments += 1;
            } else {
                scratch.clear();
                scratch_parts.clear();
                {
                    let mut lane = LaneEncoder {
                        instances: scratch,
                        parts: scratch_parts,
                        clips: &mut buffer.clips,
                        chains,
                        state,
                        saved,
                        scale,
                        resolver,
                        colors,
                        geometry,
                        chunk_scope: geometry_chunk,
                        emitted_geometry_chunks: 0,
                        run_start: 0,
                        image_layers: *image_layers,
                    };
                    lane.encode_lane(primitives, geometry_chunk)?;
                    *encoded_geometry_chunks += lane.emitted_geometry_chunks;
                }
                let len = scratch.len() as u32;
                let previous = buffer.segments.get_mut(slot).and_then(Option::take);
                // 段框（`Box`）跨帧复用：整帧重编码时 10k 条 lane 每帧 new/drop 一次 Box 是
                // 六位数的分配开销（`frame_full_10k` 实测 ~15%）。只有首次为某条 lane 建段
                // 才真正分配；`parts` 也用 `swap` 复用（SmallVec 的内联容量直接换手）。
                let (mut segment, range) = match previous {
                    // 实例数不变 ⇒ 原地重写：段地址不动，op 分组的连续性也不受影响。
                    Some(old) if old.len() == len => {
                        let range = old.range();
                        buffer.write_instances(range.clone(), scratch);
                        (old, range)
                    }
                    // 实例数变了：旧段归还（空洞留给下次分配 / compaction），新段重新分配。
                    Some(old) => {
                        buffer.release(old.range());
                        let range = buffer.allocate(len);
                        buffer.write_instances(range.clone(), scratch);
                        (old, range)
                    }
                    None => {
                        let range = buffer.allocate(len);
                        buffer.write_instances(range.clone(), scratch);
                        (
                            Box::new(LaneSegment {
                                generation: lane_id.generation(),
                                version,
                                scroll: scroll_slot,
                                start: 0,
                                end: 0,
                                parts: SmallVec::new(),
                            }),
                            range,
                        )
                    }
                };
                segment.generation = lane_id.generation();
                segment.version = version;
                segment.scroll = scroll_slot;
                segment.start = range.start;
                segment.end = range.start + len;
                segment.parts.clear();
                std::mem::swap(&mut segment.parts, scratch_parts);
                if buffer.segments.len() <= slot {
                    buffer.segments.resize_with(slot + 1, || None);
                }
                buffer.segments[slot] = Some(segment);
                *encoded_lanes += 1;
                *written_bytes += len as usize * 128;
            }
            // 本 lane 的 op 追加进绘制序（复用与新编码走同一条路径）。
            if let Some(segment) = buffer.segments.get(slot).and_then(Option::as_deref)
                && segment.generation == lane_id.generation()
            {
                append_segment_ops(&mut ops, &mut geometry_ops, &mut group, segment);
            }
        }
        flush_group(&mut ops, &mut group);
        buffer.ops = ops;
        buffer.geometry = geometry_ops;
        // compaction 只在**没有任何 lane 被重编码**的帧执行：它要重排整张实例环
        // （一次 1.28 MB 级搬运），绝不抢增量帧的预算。压实会改段起点 ⇒ 重建 op 表。
        if *encoded_lanes == 0 && *reused_segments > 0 && buffer.needs_compaction() {
            compact(buffer, order);
            build_ops(buffer, order);
        }
        Ok(())
    }

    /// 增量快路径：只处理 `changed` 里的 lane；**段布局没变时沿用上一帧的 op 表**。
    ///
    /// 与全量扫描的差别只有三处：
    /// 1. 迭代集合是帧源给出的变化集（稳态通常 1 条），不是整条绘制序（10k）；
    /// 2. 不在同一次遍历里重建 op / 几何表 —— 所有重编码 lane 的**段区间**与几何绘制都没变时，
    ///    上一帧的 op 表仍然有效（实例字节变了不影响 op 的段区间）；
    /// 3. `reused_segments` 由「绘制序长度 − 重编码条数」导出：快路径的前提是绘制序里全是
    ///    活 lane（`LaneStore::apply` 在释放 lane 时同步从绘制序摘除，并让变化集失效）。
    ///
    /// 任何一种「段布局可能变了」的情况（段长度变化 / 新 lane / 段回收 / 几何绘制出现）都会
    /// 触发一次 [`build_ops`]（O(lanes) 的 op 表重建），行为与全量扫描完全一致。
    fn encode_changed<S: LaneSource>(
        &mut self,
        source: &S,
        changed: &[crate::paint::scene::LaneId],
        order: &[crate::paint::scene::LaneId],
        scale: Scale,
        resolver: ColorResolver,
        geometry: &dyn GeometryLookup,
    ) -> Result<(), EngineError> {
        // 上一帧的 op 表能否沿用：段区间、绘制项（实例包围盒烘焙在 op 里）或滚动槽任一变化
        // 都会让旧 op 失效。
        let mut ops_stale = false;
        {
            let Self {
                buffer,
                state,
                saved,
                chains,
                colors,
                scratch,
                scratch_parts,
                image_layers,
                encoded_lanes,
                encoded_geometry_chunks,
                written_bytes,
                ..
            } = self;
            for lane_id in changed {
                let slot = lane_id.slot() as usize;
                let stale = buffer.segments.get(slot).is_some_and(|entry| {
                    entry
                        .as_ref()
                        .is_some_and(|segment| segment.generation != lane_id.generation())
                });
                if stale && let Some(old) = buffer.segments[slot].take() {
                    buffer.release(old.range());
                    ops_stale = true;
                }
                let Some((version, primitives)) = source.lane_state(*lane_id) else {
                    continue;
                };
                let scroll_slot = source.lane_scroll(*lane_id);
                let geometry_chunk = source.lane_geometry_chunk(*lane_id);
                let reusable = buffer
                    .segments
                    .get(slot)
                    .and_then(Option::as_ref)
                    .is_some_and(|segment| {
                        segment.version == version && segment.scroll == scroll_slot
                    });
                if reusable {
                    continue;
                }
                let previous_range = buffer
                    .segments
                    .get(slot)
                    .and_then(Option::as_ref)
                    .map(|segment| segment.range());
                let previous_scroll = buffer
                    .segments
                    .get(slot)
                    .and_then(Option::as_ref)
                    .map(|segment| segment.scroll);
                scratch.clear();
                scratch_parts.clear();
                {
                    let mut lane = LaneEncoder {
                        instances: scratch,
                        parts: scratch_parts,
                        clips: &mut buffer.clips,
                        chains,
                        state,
                        saved,
                        scale,
                        resolver,
                        colors,
                        geometry,
                        chunk_scope: geometry_chunk,
                        emitted_geometry_chunks: 0,
                        run_start: 0,
                        image_layers: *image_layers,
                    };
                    lane.encode_lane(primitives, geometry_chunk)?;
                    *encoded_geometry_chunks += lane.emitted_geometry_chunks;
                }
                // 绘制项必须逐项相同（实例包围盒烘焙在 op 里、几何表项按 op 顺序编号）：
                // 有一项不同就重建 op 表。实例字节变了不影响绘制项，因此原地重写仍可复用 op。
                let parts_unchanged = buffer
                    .segments
                    .get(slot)
                    .and_then(Option::as_ref)
                    .is_some_and(|segment| parts_equal(&segment.parts, scratch_parts));
                ops_stale |= !parts_unchanged;
                // 滚动槽烘焙在 op 里（绘制期按它取偏移），槽位变了同样要重建。
                ops_stale |= previous_scroll != Some(scroll_slot);
                let len = scratch.len() as u32;
                let previous = buffer.segments.get_mut(slot).and_then(Option::take);
                let (mut segment, range) = match previous {
                    Some(old) if old.len() == len => {
                        let range = old.range();
                        buffer.write_instances(range.clone(), scratch);
                        (old, range)
                    }
                    Some(old) => {
                        buffer.release(old.range());
                        let range = buffer.allocate(len);
                        buffer.write_instances(range.clone(), scratch);
                        (old, range)
                    }
                    None => {
                        let range = buffer.allocate(len);
                        buffer.write_instances(range.clone(), scratch);
                        (
                            Box::new(LaneSegment {
                                generation: lane_id.generation(),
                                version,
                                scroll: scroll_slot,
                                start: 0,
                                end: 0,
                                parts: SmallVec::new(),
                            }),
                            range,
                        )
                    }
                };
                segment.generation = lane_id.generation();
                segment.version = version;
                segment.scroll = scroll_slot;
                segment.start = range.start;
                segment.end = range.start + len;
                segment.parts.clear();
                std::mem::swap(&mut segment.parts, scratch_parts);
                if buffer.segments.len() <= slot {
                    buffer.segments.resize_with(slot + 1, || None);
                }
                buffer.segments[slot] = Some(segment);
                *encoded_lanes += 1;
                *written_bytes += len as usize * 128;
                ops_stale |= previous_range.as_ref() != Some(&range);
            }
        }
        if ops_stale {
            build_ops(&mut self.buffer, order);
        }
        // compaction：与全量扫描同一门禁（只在**没有任何 lane 被重编码**的帧执行；它要重排整张
        // 实例环，压实会改段起点 ⇒ 重建 op 表）。
        if self.encoded_lanes == 0 && !order.is_empty() && self.buffer.needs_compaction() {
            compact(&mut self.buffer, order);
            build_ops(&mut self.buffer, order);
        }
        self.reused_segments = order.len().saturating_sub(self.encoded_lanes);
        Ok(())
    }

    /// 引擎侧释放一条 lane（节点不再持有它）：立刻回收实例段（否则要等 arena 槽位被复用）。
    ///
    /// 消费者：`Window::paint_frame` 的 `drain_removed_lanes`。
    pub(crate) fn release_lane(&mut self, lane: crate::paint::scene::LaneId) {
        let slot = lane.slot() as usize;
        let matches = self.buffer.segments.get(slot).is_some_and(|entry| {
            entry
                .as_ref()
                .is_some_and(|segment| segment.generation == lane.generation())
        });
        if matches && let Some(segment) = self.buffer.segments[slot].take() {
            self.buffer.release(segment.range());
        }
    }

    /// 走增量快路径的帧数（仅测试；见 [`InstanceEncoder::encode_changed`]）。
    #[cfg(test)]
    pub(crate) fn fast_encodes(&self) -> u32 {
        self.fast_encodes
    }

    /// 本帧重编码的 lane 数（`FrameRecord.encoded_lanes` / `ComposeStats.encoded_lanes`）。
    pub(crate) fn encoded_lanes(&self) -> usize {
        self.encoded_lanes
    }

    /// 本帧重编码的几何块 op 数（增量几何的断言面：追加 K 线只应重编码尾块）。
    pub(crate) fn encoded_geometry_chunks(&self) -> usize {
        self.encoded_geometry_chunks
    }

    /// 本帧增量上传的实例字节数（编码器侧值；渲染器路径会用载荷的实际字节数覆盖它）。
    pub(crate) fn written_bytes(&self) -> usize {
        self.written_bytes
    }

    /// 本帧整段复用的 lane 段数（`FrameRecord.reused_segments`）。
    pub(crate) fn reused_segments(&self) -> usize {
        self.reused_segments
    }

    /// 下一次 `encode` 是否会整帧重建（DPI 变化 / 裁剪表超限）。
    ///
    /// 合成线程用它判断「没有 lane 变化但必须重编码」的帧（否则会漏掉一次全量重建）。
    pub(crate) fn needs_reset(&self, scale: Scale) -> bool {
        self.scale != Some(scale) || self.buffer.clips.len() > CLIP_TABLE_LIMIT
    }

    /// 丢弃全部跨帧持久状态（整帧重建：首帧 / DPI 变化 / 合成侧 lane 表重置）。
    ///
    /// 与 `encode` 内部的 `reset` 同一套清理；差别是调用方**显式**要求（delta 的 `full`），不靠
    /// 版本探测。
    pub(crate) fn invalidate(&mut self) {
        self.buffer.reset();
        self.chains.clear();
        self.state = ClipState::unbounded();
        self.saved.clear();
        self.scale = None;
        self.encoded_lanes = 0;
        self.encoded_geometry_chunks = 0;
        self.reused_segments = 0;
        self.written_bytes = 0;
        self.primed = false;
    }

    /// 本帧的编码结果（消费者：`Window::instance_bytes`，P07 上传）。
    pub(crate) fn buffer(&self) -> &InstanceBuffer {
        &self.buffer
    }

    /// 可变借出（消费者：`FramePayload` 的增量载荷构建 —— 要取走并清空变化区间）。
    pub(crate) fn buffer_mut(&mut self) -> &mut InstanceBuffer {
        &mut self.buffer
    }

    /// 编码器自检（P02 §8.8 的不变量）。
    ///
    /// 只在 debug 构建里编译：release 下这些循环没有任何副作用，却是 O(op + clip + instance)
    /// 的纯开销。
    #[cfg(debug_assertions)]
    fn self_check(&self, order: &[crate::paint::scene::LaneId]) {
        let buffer = &self.buffer;
        let mut covered = 0usize;
        for op in &buffer.ops {
            match *op {
                FrameOp::Instances { start, len, .. } => {
                    covered += len as usize;
                    debug_assert!(len > 0, "FrameOp::Instances must not be empty");
                    debug_assert!(
                        (start as usize) + (len as usize) <= buffer.instances.len(),
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
                        (index as usize) < buffer.geometry.len(),
                        "FrameOp::Geometry out of range"
                    );
                    debug_assert!(
                        (clip_index as usize) < buffer.clips.len(),
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
        // 段表不变量：范围在环内、绘制项与段长一致、段之间不重叠。
        //
        // 注意：段表里可以有**不在本帧绘制序**的段（lane 被 `Scene::release_lane` 释放、
        // 但 arena 槽位还没被复用的孤儿）。它们不参与 op、由 compaction 丢弃，因此覆盖判据
        // 只统计绘制序引用到的段。
        let mut live = 0usize;
        let mut ranges: Vec<std::ops::Range<u32>> = Vec::new();
        for (slot, entry) in buffer.segments.iter().enumerate() {
            let Some(segment) = entry.as_deref() else {
                continue;
            };
            debug_assert_eq!(
                segment.end,
                segment.start + segment.len(),
                "segment {slot} range mismatch"
            );
            debug_assert!(
                (segment.end as usize) <= buffer.instances.len(),
                "segment {slot} escapes the instance arena"
            );
            let parts_len: u32 = segment
                .parts
                .iter()
                .map(|part| match part {
                    LanePart::Instances { len, .. } => *len,
                    LanePart::Geometry(_) => 0,
                })
                .sum();
            debug_assert_eq!(
                parts_len,
                segment.len(),
                "segment {slot} parts disagree with its length"
            );
            if !segment.range().is_empty() {
                ranges.push(segment.range());
            }
        }
        for lane_id in order {
            let slot = lane_id.slot() as usize;
            if let Some(segment) = buffer.segments.get(slot).and_then(Option::as_deref)
                && segment.generation == lane_id.generation()
            {
                live += segment.len() as usize;
            }
        }
        debug_assert_eq!(covered, live, "FrameOp must cover every live instance");
        ranges.sort_unstable_by_key(|range| range.start);
        for pair in ranges.windows(2) {
            debug_assert!(
                pair[0].end <= pair[1].start,
                "instance segments must not overlap"
            );
        }
        for (index, chain) in buffer.clips.iter().enumerate() {
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
        // 每个**存活**实例的 `clip_depth`（params1.w）必须等于它那条链的真实链长：shader 用这个
        // 值收口循环，写错就是静默少裁（R05-5 / R05-11）。链长还必须有界（shader 的循环上界是
        // `KYTRIX_MAX_CLIP_ITEMS`），且链上至多一条矩形项、矩形项只能是链根。
        for segment in buffer.segments.iter().flatten() {
            for primitive in &buffer.instances[segment.start as usize..segment.end as usize] {
                let clip_index = primitive.params1[0] as usize;
                debug_assert!(
                    clip_index < buffer.clips.len(),
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
                    let entry = buffer.clips[cursor];
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
}

/// 正在累积的实例 op（[`append_segment_ops`] / [`build_ops`] 的内部状态）。
struct OpGroup {
    active: bool,
    start: u32,
    len: u32,
    /// 已并入的 lane 条数（合并上限 `MAX_LANES_PER_OP`）。
    lanes: u32,
    bounds: PhysicalBounds,
    /// 本组统一的滚动槽（不同槽必须切断合并）。
    scroll: u32,
}

impl OpGroup {
    fn new() -> Self {
        Self {
            active: false,
            start: 0,
            len: 0,
            lanes: 0,
            bounds: PhysicalBounds::EMPTY,
            scroll: 0,
        }
    }
}

/// 把一条 lane 的绘制项追加进绘制序。
///
/// **合并规则**：实例环里**连续**的相邻 lane 段合并成一条 [`FrameOp::Instances`]（上限
/// [`MAX_LANES_PER_OP`] 条 lane），`bounds` 是这些段的实例包围盒并集 —— 于是 damage 的
/// op 级跳过真正生效（旧实现整个 UI 只有一条 `bounds == 整屏` 的 op，跳过永远失效）。
/// 几何绘制切断合并并插在它自己的绘制位置上（形状/几何 op 的先后关系不变）。
fn append_segment_ops(
    ops: &mut Vec<FrameOp>,
    geometry: &mut Vec<GeometryDrawGpu>,
    group: &mut OpGroup,
    segment: &LaneSegment,
) {
    let mut cursor = segment.start;
    let mut lane_counted = false;
    for part in &segment.parts {
        match part {
            LanePart::Instances { len, bounds } => {
                if *len == 0 {
                    continue;
                }
                let contiguous = group.active
                    && group.start + group.len == cursor
                    && group.lanes < MAX_LANES_PER_OP
                    && group.scroll == segment.scroll;
                if group.active && !contiguous {
                    flush_group(ops, group);
                }
                if !group.active {
                    *group = OpGroup {
                        active: true,
                        start: cursor,
                        scroll: segment.scroll,
                        ..OpGroup::new()
                    };
                }
                if !lane_counted {
                    group.lanes += 1;
                    lane_counted = true;
                }
                group.len += *len;
                group.bounds = group.bounds.union(*bounds);
                cursor += *len;
            }
            LanePart::Geometry(part) => {
                flush_group(ops, group);
                let index = geometry.len() as u32;
                geometry.push(part.draw.clone());
                ops.push(FrameOp::Geometry {
                    index,
                    clip_index: part.clip_index,
                    clip_rect: part.clip_rect,
                    bounds: part.bounds,
                });
            }
        }
    }
}

/// 从段表重建整份绘制序（compaction 改变段起点之后调用；正常帧在 `encode` 的 lane 遍历里
/// 增量追加，不走这里）。
fn build_ops(buffer: &mut InstanceBuffer, order: &[crate::paint::scene::LaneId]) {
    // 先把两张每帧重建的表借出（容量跨帧保留，稳态零分配）：循环里只读段表，不再与
    // `&mut buffer` 冲突。
    let mut ops = std::mem::take(&mut buffer.ops);
    ops.clear();
    let mut geometry = std::mem::take(&mut buffer.geometry);
    geometry.clear();
    let mut group = OpGroup::new();
    for lane_id in order {
        let slot = lane_id.slot() as usize;
        let Some(segment) = buffer.segments.get(slot).and_then(Option::as_deref) else {
            continue;
        };
        if segment.generation != lane_id.generation() {
            continue;
        }
        append_segment_ops(&mut ops, &mut geometry, &mut group, segment);
    }
    flush_group(&mut ops, &mut group);
    buffer.ops = ops;
    buffer.geometry = geometry;
}

/// 两段绘制项是否逐项相同（决定编码器能否沿用上一帧的 op 表）。
///
/// op 里烘焙了**实例包围盒**（damage 的 op 级跳过用它）与几何表下标，因此绘制项只要有一项
/// 不同（长度 / 包围盒 / 几何绘制）就必须重建 op 表。几何绘制一律视为「不同」：几何表项按
/// op 顺序编号，宁可重建也不冒险复用旧下标。
fn parts_equal(previous: &[LanePart], current: &[LanePart]) -> bool {
    previous.len() == current.len()
        && previous
            .iter()
            .zip(current)
            .all(|(old, new)| match (old, new) {
                (
                    LanePart::Instances {
                        len: old_len,
                        bounds: old_bounds,
                    },
                    LanePart::Instances {
                        len: new_len,
                        bounds: new_bounds,
                    },
                ) => old_len == new_len && old_bounds == new_bounds,
                _ => false,
            })
}

fn flush_group(ops: &mut Vec<FrameOp>, group: &mut OpGroup) {
    if group.active && group.len > 0 {
        ops.push(FrameOp::Instances {
            start: group.start,
            len: group.len,
            bounds: group.bounds,
            scroll: group.scroll,
        });
    }
    group.active = false;
    group.len = 0;
    group.lanes = 0;
}

/// 压实实例环：按绘制序把段搬到前部（消除空洞），段起点随之更新。
///
/// 只在**无脏帧**执行（`InstanceEncoder::encode` 的门禁）：字节搬运量 = 存活实例数，
/// 10k 图元 ≈ 1.28 MB。搬完之后整张环都是「变化区间」（下一次载荷必然包含全部存活段）。
///
/// 不在绘制序里的段（已释放、但 arena 槽位还没被复用的老 lane）在这里丢弃：它的内容不再被
/// 任何 op 引用，下次进绘制序时会因「无段」而重编码。
fn compact(buffer: &mut InstanceBuffer, order: &[crate::paint::scene::LaneId]) {
    let mut packed: Vec<GpuPrimitive> = Vec::with_capacity(buffer.instances.len());
    let mut starts: Vec<(usize, u32)> = Vec::with_capacity(order.len());
    let mut seen: Vec<bool> = vec![false; buffer.segments.len()];
    let mut cursor = 0u32;
    for lane_id in order {
        let slot = lane_id.slot() as usize;
        let Some(segment) = buffer.segments.get(slot).and_then(Option::as_deref) else {
            continue;
        };
        if segment.generation != lane_id.generation() {
            continue;
        }
        let range = segment.range();
        packed.extend_from_slice(&buffer.instances[range.start as usize..range.end as usize]);
        starts.push((slot, cursor));
        seen[slot] = true;
        cursor += segment.len();
    }
    for (live, entry) in seen.iter().zip(buffer.segments.iter_mut()) {
        if !*live {
            *entry = None;
        }
    }
    buffer.instances.clear();
    buffer.instances.extend_from_slice(&packed);
    buffer.used = cursor;
    buffer
        .instances
        .resize(buffer.used as usize, bytemuck::Zeroable::zeroed());
    buffer.free.clear();
    for (slot, start) in starts {
        if let Some(segment) = buffer.segments[slot].as_mut() {
            let len = segment.len();
            segment.start = start;
            segment.end = start + len;
        }
    }
    buffer.changed.clear();
    buffer.changed.push(0..cursor);
}

/// 单 lane 编码的借用包（把 `&mut` 字段拆开，避免整结构可变借用冲突）。
///
/// 每条 lane 独立编码（lane 在 `Canvas` 契约里是 clip 平衡的，C1），实例先写进
/// `instances` 工作区，编码完成后由调用方整段搬进实例环。裁剪表项**直接追加到跨帧持久的
/// 裁剪表**（`clips`）：复用的段里内嵌的 `clip_index` 因此继续有效。
struct LaneEncoder<'a> {
    instances: &'a mut Vec<GpuPrimitive>,
    parts: &'a mut SmallVec<[LanePart; 2]>,
    clips: &'a mut Vec<ClipChain>,
    state: &'a mut ClipState,
    saved: &'a mut SmallVec<[ClipState; 8]>,
    chains: &'a mut ahash::HashMap<(u32, u32, [u32; 4], [u32; 4]), ClipChainId>,
    scale: Scale,
    resolver: ColorResolver,
    /// 颜色解析缓存（P1-2.1）：一帧内同色高度重复，宽色域解析要跑 24 步二分。
    colors: &'a mut crate::paint::color::ResolveCache,
    /// 几何描述查表（UI 线程是 `GeometryStore`，合成线程是 `GeometryCatalog`）。
    geometry: &'a dyn GeometryLookup,
    /// 本 lane 的几何块绘制范围（`None` = 非几何块 lane / 整块几何绘制；X5）。
    chunk_scope: Option<(crate::paint::geometry::GeometryId, u32)>,
    /// 本 lane 编码出的几何块 op 数（编码完成后由调用方累进 `InstanceEncoder`）。
    emitted_geometry_chunks: usize,
    run_start: usize,
    /// 本帧图片图集可画层数上限（fail-closed，RBT-01 不变量 1；来自 `InstanceEncoder`）。
    image_layers: u32,
}

impl LaneEncoder<'_> {
    /// 编码一条 lane 的全部图元（clip 状态从「无裁剪」开始，结束时必须回到 0 层）。
    fn encode_lane(
        &mut self,
        primitives: &[Primitive],
        chunk_scope: Option<(crate::paint::geometry::GeometryId, u32)>,
    ) -> Result<(), EngineError> {
        self.chunk_scope = chunk_scope;
        *self.state = ClipState::unbounded();
        self.saved.clear();
        for primitive in primitives {
            self.encode_primitive(primitive, 0)?;
        }
        debug_assert_eq!(self.state.depth(), 0, "lane must be clip-balanced (C1)");
        self.flush_run();
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
                    let color0 = self.colors.resolve(&self.resolver, run.color);
                    for glyph in &run.glyphs {
                        self.emit_glyph(color0, glyph);
                    }
                }
            }
            Primitive::Path(path) => {
                self.flush_run();
                let color = self.colors.resolve(&self.resolver, path.fill).to_array();
                // Path 的顶点在逻辑窗口空间、无 per-primitive 变换（`Path` 没有 transform
                // 字段）；damage 用它自己的 bounds 而不是 clip AABB（R05-19）。
                self.push_geometry_op(path.geometry, Transform::IDENTITY, color, Some(path.bounds));
            }
            Primitive::Geometry(draw) => {
                self.flush_run();
                let color = self.colors.resolve(&self.resolver, draw.color).to_array();
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
        let id = ClipChainId(self.clips.len() as u32);
        self.clips.push(ClipChain {
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
        let len = self.instances.len() - self.run_start;
        if len > 0 {
            let bounds = run_bounds(&self.instances[self.run_start..]);
            self.parts.push(LanePart::Instances {
                len: len as u32,
                bounds,
            });
            self.run_start = self.instances.len();
        }
    }

    /// 取几何快照（`Arc` 克隆，不复制页字节）。未注册的句柄给**空快照**：渲染层会以
    /// `GeometryRegister` 错误拒绝，而不是画错东西。
    fn geometry_snapshot(
        &self,
        id: crate::paint::geometry::GeometryId,
    ) -> Option<std::sync::Arc<crate::paint::geometry::GeometrySnapshot>> {
        self.geometry.snapshot_arc(id)
    }

    /// 追加一次几何绘制（每帧生成 op 表时按绘制序复制进 `InstanceBuffer::geometry`）。
    ///
    /// **分块展开**：几何数据按固定块大小切分（`GEOMETRY_CHUNK_INDICES`），这里每块发一个
    /// `LanePart::Geometry` / `FrameOp::Geometry`（绘制范围 = 块的索引区间）。块在索引缓冲里
    /// 连续，因此逐块绘制与整段绘制的像素结果完全一致；好处是失效/上传可以按块/页进行
    /// （几何节点的多 lane 分块在此基础上让「追加只重建尾块」成立）。
    ///
    /// `op_bounds`：给定则用它（`Path` 自带逻辑 AABB），否则用**块 AABB**（`snapshot.chunks`，
    /// 由引擎按顶点位置现算）；块的 AABB 为空（退化几何：全零顶点）时退回整块几何的
    /// `snapshot.bounds()`，保持旧实现的可见范围（不因块切分而改变像素路径）。
    ///
    /// R05-12 / R05-19：旧实现对无 clip 的几何给出「全屏」包围盒，局部刷新时把所有几何 draw
    /// 都重发；给了 transform 却仍用未变换的 bounds 会在平移后留残影。
    fn push_geometry_op(
        &mut self,
        id: crate::paint::geometry::GeometryId,
        transform: Transform,
        color: [f32; 4],
        op_bounds: Option<Rect>,
    ) {
        let registered = self.geometry_snapshot(id);
        let clip_aabb = self.clip_aabb();
        let clip_index = self.clip_index() as u32;
        let clip_rect = clip_aabb.map(f32::to_bits);
        // 块 lane（几何节点的一条块 lane）只发它自己那一块；整块几何绘制（Canvas/Path）展开全部块。
        let scope = self.chunk_scope.filter(|(scoped, _)| *scoped == id);
        let mut emitted = 0usize;
        let mut push_part =
            |emitted: &mut usize, chunk: u32, bounds: Rect, snapshot: std::sync::Arc<_>| {
                let visible =
                    visible_aabb(transformed_aabb(transform, bounds), self.scale, clip_aabb);
                self.parts.push(LanePart::Geometry(Box::new(LaneGeometry {
                    draw: GeometryDrawGpu {
                        id,
                        chunk,
                        transform,
                        color,
                        snapshot,
                    },
                    clip_index,
                    clip_rect,
                    bounds: bounds_of_aabb(visible),
                })));
                *emitted += 1;
            };
        match (&registered, scope) {
            // 块 lane：只编码本块（块 AABB 为空时退回整块几何 bounds，保持旧可见范围）。
            (Some(snapshot), Some((_, chunk))) => {
                if let Some(meta) = snapshot.chunk(chunk) {
                    let local = match (op_bounds, meta.bounds.is_empty()) {
                        (Some(bounds), _) => bounds,
                        (None, false) => meta.bounds,
                        (None, true) => snapshot.bounds(),
                    };
                    push_part(&mut emitted, chunk, local, std::sync::Arc::clone(snapshot));
                }
            }
            (Some(snapshot), None) => {
                // 快照的块表是绘制序的权威来源；空块表只可能来自未注册兜底（不会走到这里）。
                for (chunk, meta) in snapshot.chunks().iter().enumerate() {
                    let local = match (op_bounds, meta.bounds.is_empty()) {
                        (Some(bounds), _) => bounds,
                        (None, false) => meta.bounds,
                        (None, true) => snapshot.bounds(),
                    };
                    push_part(
                        &mut emitted,
                        chunk as u32,
                        local,
                        std::sync::Arc::clone(snapshot),
                    );
                }
            }
            // 未注册：拿不到真实范围，保守地用 clip 上界（GPU 侧会报 GeometryRegister）。
            (None, _) => push_part(
                &mut emitted,
                0,
                op_bounds.unwrap_or_else(|| {
                    Rect::new(
                        clip_aabb[0],
                        clip_aabb[1],
                        (clip_aabb[2] - clip_aabb[0]).max(0.0),
                        (clip_aabb[3] - clip_aabb[1]).max(0.0),
                    )
                }),
                crate::paint::geometry_store::missing_snapshot(),
            ),
        }
        self.emitted_geometry_chunks += emitted;
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
                // R04-4：边框宽度**吸附到整数物理像素且至少 1 px**（Skia hairline / WebRender
                // 的边框吸附）。取整与布局提交值共用 `math::snap_physical`（见
                // `math::snap_stroke_width`）：125% 下 1 px 边框是 1 物理像素的硬边，不再是
                // 1.25 px 的 AA 带；宽度定了之后带以已吸附的 `quad.bounds` 外边界为锚点向内
                // 展开，内边界随之为整数物理像素（位置不需要二次取整）。
                let width = border.map_or(0.0, |border| {
                    crate::math::snap_stroke_width(border.width, factor)
                });
                (
                    self.colors.resolve(&self.resolver, *fill),
                    border.map_or(LinearPremulRgba::TRANSPARENT, |border| {
                        self.colors.resolve(&self.resolver, border.color)
                    }),
                    // 夹到 SDF 合法域（半尺寸）是**第二道**防线：极小盒子上「整数带宽」可能超过
                    // 半尺寸，此时以正确性（等值线不反转）优先，允许退化回分数宽度。
                    width.clamp(0.0, half_min),
                    GRADIENT_NONE,
                )
            }
            QuadPaint::Gradient(gradient) => (
                self.colors.resolve(&self.resolver, gradient.from),
                self.colors.resolve(&self.resolver, gradient.to),
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
        self.instances.push(instance);
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
            color0: self.colors.resolve(&self.resolver, shadow.color).to_array(),
            color1: [0.0; 4],
            params0: [0.0, blur, GRADIENT_NONE, pad],
            params1: [self.clip_index(), 0.0, KIND_SHADOW, self.clip_depth()],
            uv: [0.0; 4],
            clip_rect: self.clip_rect(),
        };
        self.instances.push(instance);
    }

    fn emit_image(&mut self, image: &crate::paint::primitive::Image) {
        // RD-8：`page` 是 texture array 的层号，越界采样在三端都是未定义行为（Vulkan 可能
        // 设备丢失）。契约破坏在 debug 直接断言；release 版本 clamp 到最后一层（安全但内容错，
        // 绝不把越界层号写进 GPU 实例）。
        debug_assert!(
            image.tile.page < MAX_IMAGE_PAGES,
            "image tile page {} out of range (MAX_IMAGE_PAGES={MAX_IMAGE_PAGES})",
            image.tile.page
        );
        // fail-closed（RBT-01 不变量 1）：本帧数组里**还没有确定内容**的层不画 —— 不画占位、
        // 不画错层。上限来自 App 的逐窗口图集记账（`Window::image_drawable_layers`），
        // 与「所有窗口都整组入队才推进数组」是同一条不变量；这里不是常量判据。
        if u32::from(image.tile.page) >= self.image_layers {
            return;
        }
        let page = image.tile.page.min(MAX_IMAGE_PAGES - 1);
        let physical = image.bounds.to_physical(self.scale);
        let color0 = self.colors.resolve(&self.resolver, image.modulation);
        let bounds = physical_to_bounds(physical);
        let instance = GpuPrimitive {
            bounds,
            corner_radius: [0.0; 4],
            color0: color0.to_array(),
            color1: [0.0; 4],
            params0: [0.0; 4],
            params1: [
                self.clip_index(),
                f32::from(page),
                KIND_IMAGE,
                self.clip_depth(),
            ],
            uv: image.tile.uv(),
            clip_rect: self.clip_rect(),
        };
        self.instances.push(instance);
    }

    fn emit_glyph(
        &mut self,
        color0: LinearPremulRgba,
        glyph: &crate::paint::primitive::PositionedGlyph,
    ) {
        // RD-8：glyph 图集只有 1 层（layer 恒 0），越界 tile 是契约破坏。
        debug_assert_eq!(
            glyph.tile.page, 0,
            "glyph atlas tile must live in texture array layer 0"
        );
        let physical = Rect::from_origin_size(glyph.position, glyph.size).to_physical(self.scale);
        let bounds = physical_to_bounds(physical);
        let instance = GpuPrimitive {
            bounds,
            corner_radius: [0.0; 4],
            color0: color0.to_array(),
            color1: [0.0; 4],
            params0: [0.0; 4],
            params1: [self.clip_index(), 0.0, KIND_GLYPH, self.clip_depth()],
            uv: glyph.tile.uv(),
            clip_rect: self.clip_rect(),
        };
        self.instances.push(instance);
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
    use std::sync::Arc;

    use super::{
        CLIP_KIND_RECT, CLIP_KIND_ROUNDED, ClipChain, ClipChainId, ClipKind, ClipState,
        GRADIENT_HORIZONTAL, GRADIENT_RADIAL, GRADIENT_VERTICAL, GpuPrimitive, InstanceEncoder,
        KIND_GLYPH, KIND_QUAD, KIND_SHADOW, MAX_CLIP_ITEMS, SHADOW_SIGMA_MIN, SHADOW_SIGMA_SCALE,
        normalize_corner_radius, snap_rect_clip_to_pixels,
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

    /// 滚动槽的每 op scissor：槽位 0（不随滚动偏移）不收窄；真实槽位取与帧 scissor 的交集；
    /// 完全被裁掉时给出空区间（调用方据此跳过整个 op）。
    #[test]
    fn the_scrolled_scissor_narrows_only_real_scroll_slots() {
        // 表按下标索引：0 是哨兵（零面积），1 是真实滚动槽。
        let clips = [[0, 0, 0, 0], [10, 20, 110, 220]];
        assert_eq!(
            super::scrolled_scissor(&clips, 0, [0, 0, 800, 600]),
            None,
            "槽位 0 是「不随滚动偏移」的哨兵，不得收窄"
        );
        assert_eq!(
            super::scrolled_scissor(&clips, 1, [50, 0, 800, 600]),
            Some([50, 20, 110, 220]),
            "帧 scissor 与槽位屏幕裁剪区求交"
        );
        assert_eq!(
            super::scrolled_scissor(&clips, 1, [0, 300, 800, 600]),
            Some([10, 300, 110, 220]),
            "完全不相交 ⇒ 空区间（调用方跳过整个 op）"
        );
        assert_eq!(
            super::scrolled_scissor(&clips, 9, [0, 0, 800, 600]),
            None,
            "越界槽位按不收窄处理（保守：与不滚动等价）"
        );
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

    /// 编码结果的只读快照（测试专用：编码器的 `InstanceBuffer` 跨帧持久，不能整体克隆）。
    struct Encoded {
        instances: Vec<GpuPrimitive>,
        ops: Vec<super::FrameOp>,
        clips: Vec<ClipChain>,
    }

    impl Encoded {
        /// 裁剪表的 GPU 布局（与生产路径 `InstanceBuffer::write_clips_gpu` 同一转换）。
        fn write_clips_gpu(&self, out: &mut Vec<super::GpuClipEntry>) {
            out.clear();
            out.extend(self.clips.iter().copied().map(ClipChain::gpu_entry));
        }
    }

    fn encode(scene: &Scene) -> Encoded {
        try_encode(scene).expect("encode")
    }

    /// 指定 DPI 的编码（R04-4 的宽度吸附必须跨 scale 断言）。
    fn encode_at(scene: &Scene, factor: f32) -> Encoded {
        let mut encoder = InstanceEncoder::new();
        let scale = Scale::new(factor).expect("合法 scale");
        let buffer = encoder
            .encode(
                scene,
                scale,
                resolver(),
                &crate::paint::geometry_store::GeometryStore::new(),
            )
            .expect("encode");
        Encoded {
            instances: buffer.instances.clone(),
            clips: buffer.clips.clone(),
            ops: buffer.ops.clone(),
        }
    }

    fn try_encode(scene: &Scene) -> Result<Encoded, crate::error::EngineError> {
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(
            scene,
            scale(),
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )?;
        Ok(Encoded {
            instances: buffer.instances.clone(),
            clips: buffer.clips.clone(),
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
            assert!(scene.push(lane, Primitive::Canvas(Arc::new(canvas))));
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
            assert!(scene.push(lane, Primitive::Canvas(Arc::new(canvas))));
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
            assert!(scene.push(lane, Primitive::Canvas(Arc::new(canvas))));
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

    /// R04-4：`params0.x`（边框带宽，物理像素）必须是**整数且 ≥1**，与节点的亚像素相位无关。
    ///
    /// 旧实现是 `border.width * factor`：125% 下 1 px 边框 = 1.25 px 的 AA 带（一行 80% 亮 +
    /// 一行 25% 亮），且明暗随节点 y 坐标变化。新规则来自 `math::snap_stroke_width`
    /// （Skia/WebRender 的边框设备像素吸附）：1 dp → 1/2/2 物理像素（125/150/175%）。
    #[test]
    fn border_width_snaps_to_integer_device_pixels() {
        for (factor, expected) in [
            (1.0, 1.0),
            (1.25, 1.0),
            (1.5, 2.0),
            (1.75, 2.0),
            (2.0, 2.0),
            (3.0, 3.0),
        ] {
            for offset in [0.0f32, 0.1, 0.25, 0.4, 0.49, 0.5, 0.75, 0.9] {
                let scene = scene_with(|scene, lane| {
                    assert!(scene.push(
                        lane,
                        Primitive::Quad(Quad {
                            bounds: rect(offset, offset, 20.0, 10.0).snap_to_device(factor),
                            corner_radius: [0.0; 4],
                            paint: QuadPaint::Solid {
                                fill: Color::WHITE,
                                border: Some(Border {
                                    width: 1.0,
                                    color: Color::BLACK,
                                }),
                            },
                        }),
                    ));
                });
                let buffer = encode_at(&scene, factor);
                let instance = buffer.instances[0];
                assert_eq!(
                    instance.params0[0], expected,
                    "1 dp 边框在 {factor}× / offset {offset} 下必须是 {expected} 物理像素"
                );
                assert_eq!(
                    instance.params0[0].fract(),
                    0.0,
                    "边框带宽必须是整数物理像素"
                );
            }
        }
    }

    /// hairline：亚像素宽度也至少 1 物理像素（细线不消失），但受 SDF 合法域（半尺寸）夹紧。
    #[test]
    fn hairline_border_never_disappears() {
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: rect(10.0, 10.0, 20.0, 10.0),
                    corner_radius: [0.0; 4],
                    paint: QuadPaint::Solid {
                        fill: Color::TRANSPARENT,
                        border: Some(Border {
                            width: 0.25,
                            color: Color::BLACK,
                        }),
                    },
                }),
            ));
        });
        let buffer = encode_at(&scene, 1.25);
        assert_eq!(
            buffer.instances[0].params0[0], 1.0,
            "0.25 dp 边框在 125% 下也必须画出 1 物理像素"
        );
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
            page: 0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            generation: 0,
            slot: 0,
            slot_epoch: 0,
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
            page: 0,
            u0,
            v0: 0.0,
            u1: u0 + 0.1,
            v1: 0.1,
            generation: 0,
            slot: 0,
            slot_epoch: 0,
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
            buffer.instances.iter().all(|i| i.params1[1] == 0.0),
            "字形图集恒 1 层：层号必须是 0"
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
        // 真实顶点位置 (0,0)/(10,0)/(0,10)：块的 AABB 由引擎按顶点现算 = (0,0,10,10)。
        let points = [[0.0f32, 0.0, -1.0], [10.0, 0.0, -1.0], [0.0, 10.0, -1.0]];
        let vertex_bytes: Vec<u8> = points
            .iter()
            .flat_map(|point| point.iter().flat_map(|value| value.to_ne_bytes()))
            .collect();
        let desc = crate::paint::geometry::GeometryDesc {
            vertex_bytes: bytes::Bytes::from(vertex_bytes),
            index_bytes: bytes::Bytes::from(vec![0u8, 0, 1, 0, 2, 0]),
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
            .encode(&scene, scale(), resolver(), &store)
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
            .encode(&scene, scale(), resolver(), &store)
            .expect("encode");
        let draw = buffer.geometry.first().expect("必须有一条几何绘制");
        assert_eq!(
            draw.snapshot.vertex_len() as usize,
            stride * 3,
            "编码器必须从传入的 store 取到真实顶点（而不是空 MISSING 描述）"
        );
        assert_eq!(draw.snapshot.index_len(), 6);
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
            page: 0,
            u0: 0.1,
            v0: 0.2,
            u1: 0.3,
            v1: 0.4,
            generation: 0,
            slot: 0,
            slot_epoch: 0,
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
        assert_eq!(
            instance.params1[1],
            tile.page_f32(),
            "params1.y 必须是图集页（texture array 层号），不是旧的 texture_index"
        );
        assert_eq!(instance.params1[2], super::KIND_IMAGE);
        assert_eq!(instance.uv, [0.1, 0.2, 0.3, 0.4]);
        assert_eq!(instance.params1[3], 0.0, "无裁剪 ⇒ clip_depth 0");
    }

    /// RD-8：编码期对越界图片层号硬校验（debug 断言）；release 走 clamp，绝不把越界层号
    /// 写进 GPU 实例（texture array 越界采样是三端未定义行为）。
    #[test]
    #[should_panic(expected = "image tile page")]
    fn image_page_out_of_range_is_rejected_at_encode_time() {
        let tile = AtlasTile {
            atlas: AtlasKind::Image,
            page: crate::paint::atlas::MAX_IMAGE_PAGES,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            generation: 0,
            slot: 0,
            slot_epoch: 0,
        };
        let scene = scene_with(|scene, lane| {
            assert!(scene.push(
                lane,
                Primitive::Image(crate::paint::primitive::Image {
                    bounds: rect(0.0, 0.0, 4.0, 4.0),
                    tile,
                    modulation: Color::WHITE,
                }),
            ));
        });
        let _ = encode(&scene);
    }

    /// D-9-c：图片的 `params1.y` 逐页编码（页 1 写 1.0，不是旧的 `TEXTURE_IMAGE = 1` 巧合值）。
    #[test]
    fn image_encodes_the_atlas_page_as_the_texture_array_layer() {
        for page in [0u16, 1, 2, 3] {
            let tile = AtlasTile {
                atlas: AtlasKind::Image,
                page,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                generation: 1,
                slot: 0,
                slot_epoch: 0,
            };
            let scene = scene_with(|scene, lane| {
                assert!(scene.push(
                    lane,
                    Primitive::Image(crate::paint::primitive::Image {
                        bounds: rect(0.0, 0.0, 1.0, 1.0),
                        tile,
                        modulation: Color::WHITE,
                    }),
                ));
            });
            let buffer = encode(&scene);
            assert_eq!(
                buffer.instances[0].params1[1],
                f32::from(page),
                "页号必须整数编码进 params1.y"
            );
            assert_eq!(buffer.instances[0].params1[1].fract(), 0.0);
        }
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

    // ── B6：帧间增量实例编码 ───────────────────────────────────────────────

    fn empty_store() -> crate::paint::geometry_store::GeometryStore {
        crate::paint::geometry_store::GeometryStore::new()
    }

    /// `count` 条 lane 的网格场景（每 lane 一个 10×20 的 quad，位置随下标错开）。
    fn grid_scene(count: usize) -> (Scene, Vec<crate::paint::scene::LaneId>) {
        let mut scene = Scene::empty();
        let mut lanes = Vec::with_capacity(count);
        for index in 0..count {
            let lane = scene.new_lane();
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: Rect::new(
                        (index % 8) as f32 * 16.0,
                        (index / 8) as f32 * 24.0,
                        10.0,
                        20.0
                    ),
                    corner_radius: [0.0; 4],
                    paint: solid(),
                })
            ));
            lanes.push(lane);
        }
        scene.set_order(&lanes);
        (scene, lanes)
    }

    /// 一条 lane 里所有实例的字节（按实例环区间取；空洞不参与）。
    fn lane_instance_bytes(
        encoder: &InstanceEncoder,
        lane: crate::paint::scene::LaneId,
    ) -> Vec<u8> {
        let segment = encoder
            .buffer()
            .segments
            .get(lane.slot() as usize)
            .and_then(Option::as_deref)
            .expect("lane 必须有实例段");
        assert_eq!(segment.generation, lane.generation());
        encoder.buffer().as_bytes()[segment.start as usize * 128..segment.end as usize * 128]
            .to_vec()
    }

    /// 没有任何 lane 变化的帧：所有段整段复用，实例字节与绘制序逐字节不变。
    #[test]
    fn an_unchanged_frame_reuses_every_segment() {
        let (scene, lanes) = grid_scene(8);
        let mut encoder = InstanceEncoder::new();
        encoder
            .encode(&scene, scale(), resolver(), &empty_store())
            .expect("初帧");
        assert_eq!(encoder.encoded_lanes(), 8);
        assert_eq!(encoder.reused_segments(), 0);
        let instances = encoder.buffer().as_bytes().to_vec();
        let ops = encoder.buffer().ops().to_vec();
        assert_eq!(encoder.buffer().instance_count(), 8);
        // 初帧的载荷已经交付（取走变化区间），之后的帧才能断言「没有新增变化」。
        encoder.buffer_mut().clear_changes();

        encoder
            .encode(&scene, scale(), resolver(), &empty_store())
            .expect("空闲帧");
        assert_eq!(encoder.encoded_lanes(), 0, "无变化帧不得重编码任何 lane");
        assert_eq!(encoder.reused_segments(), lanes.len());
        assert_eq!(encoder.buffer().as_bytes(), instances.as_slice());
        assert_eq!(encoder.buffer().ops(), ops.as_slice());
        assert!(
            encoder.buffer_mut().changes().is_empty(),
            "无变化帧不得产生增量载荷"
        );
    }

    /// 只改一条 lane：只有那一条被重编码，变化区间就是它的段。
    #[test]
    fn a_dirty_lane_only_reencodes_itself() {
        let (mut scene, lanes) = grid_scene(8);
        let mut encoder = InstanceEncoder::new();
        encoder
            .encode(&scene, scale(), resolver(), &empty_store())
            .expect("初帧");
        encoder.buffer_mut().clear_changes();

        let lane = lanes[3];
        let segment = encoder.buffer().segments[lane.slot() as usize]
            .as_deref()
            .expect("段存在")
            .range();
        assert!(scene.clear_lane(lane));
        assert!(scene.push(
            lane,
            Primitive::Quad(Quad {
                bounds: Rect::new(1.0, 2.0, 10.0, 20.0),
                corner_radius: [0.0; 4],
                paint: solid(),
            })
        ));
        encoder
            .encode(&scene, scale(), resolver(), &empty_store())
            .expect("增量帧");

        assert_eq!(encoder.encoded_lanes(), 1, "只有脏 lane 被重编码");
        assert_eq!(
            encoder.written_bytes(),
            128,
            "只上传那一条 lane 的实例字节（1 个实例 = 128 B）"
        );
        assert_eq!(encoder.reused_segments(), lanes.len() - 1);
        assert_eq!(
            encoder.buffer_mut().changes(),
            &[segment],
            "变化区间 = 那条 lane 的段（实例数不变 ⇒ 原地重写）"
        );
    }

    /// **全量 / 增量等价**：同一场景，「多帧增量」与「一次全量」的实例字节逐字节一致。
    #[test]
    fn incremental_and_full_encodes_agree_byte_for_byte() {
        let (mut scene, lanes) = grid_scene(6);
        let mut incremental = InstanceEncoder::new();
        let store = empty_store();
        incremental
            .encode(&scene, scale(), resolver(), &store)
            .expect("初帧");
        // 两帧增量：每帧只改一条 lane（形状/位置变、实例数不变 ⇒ 原地重写）。
        for (round, lane) in [lanes[2], lanes[4]].into_iter().enumerate() {
            assert!(scene.clear_lane(lane));
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: Rect::new(1.0 + round as f32, 2.0, 10.0, 20.0),
                    corner_radius: [1.0; 4],
                    paint: solid(),
                })
            ));
            incremental
                .encode(&scene, scale(), resolver(), &store)
                .expect("增量帧");
        }

        let mut full = InstanceEncoder::new();
        full.encode(&scene, scale(), resolver(), &store)
            .expect("全量帧");

        assert_eq!(
            incremental.buffer().as_bytes(),
            full.buffer().as_bytes(),
            "全量与增量的实例环必须逐字节一致"
        );
        assert_eq!(incremental.buffer().ops(), full.buffer().ops());
        assert_eq!(
            incremental.buffer().instance_count(),
            full.buffer().instance_count()
        );
        for lane in &lanes {
            assert_eq!(
                lane_instance_bytes(&incremental, *lane),
                lane_instance_bytes(&full, *lane),
                "lane {lane:?} 的实例字节必须一致"
            );
        }
    }

    /// 实例数变化（段搬移）之后：每条 lane 的内容仍与全量编码一致。
    #[test]
    fn moved_lanes_keep_their_content() {
        let (mut scene, lanes) = grid_scene(4);
        let mut incremental = InstanceEncoder::new();
        let store = empty_store();
        incremental
            .encode(&scene, scale(), resolver(), &store)
            .expect("初帧");
        // 把一条 lane 从一个实例改成三个（段长度变化 ⇒ 搬段）。
        let lane = lanes[1];
        assert!(scene.clear_lane(lane));
        for index in 0..3 {
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: Rect::new(index as f32, 0.0, 4.0, 4.0),
                    corner_radius: [0.0; 4],
                    paint: solid(),
                })
            ));
        }
        incremental
            .encode(&scene, scale(), resolver(), &store)
            .expect("增量帧");
        assert_eq!(incremental.encoded_lanes(), 1);

        let mut full = InstanceEncoder::new();
        full.encode(&scene, scale(), resolver(), &store)
            .expect("全量帧");
        for lane in &lanes {
            assert_eq!(
                lane_instance_bytes(&incremental, *lane),
                lane_instance_bytes(&full, *lane),
                "lane {lane:?} 的内容必须与全量编码一致"
            );
        }
    }

    /// 裁剪表超上限 ⇒ 整表重建 + 全量重编码（缓存段的 `clip_index` 全部失效）。
    #[test]
    fn overflowing_the_clip_table_forces_a_full_reencode() {
        let (mut scene, lanes) = grid_scene(3);
        let mut encoder = InstanceEncoder::new();
        let store = empty_store();
        encoder
            .encode(&scene, scale(), resolver(), &store)
            .expect("初帧");
        // 直接灌表项把表撑到上限之上（等价于长动画每帧追加新 clip 的累积效应）。
        let filler = encoder.buffer().clips[0];
        while encoder.buffer().clips.len() <= super::CLIP_TABLE_LIMIT {
            encoder.buffer_mut().clips.push(filler);
        }
        // 再来一条脏 lane：不为它重编码，但表超限会让整帧回退全量。
        let lane = lanes[0];
        assert!(scene.clear_lane(lane));
        assert!(scene.push(lane, quad_primitive(solid())));
        encoder
            .encode(&scene, scale(), resolver(), &store)
            .expect("重置帧");
        assert_eq!(
            encoder.encoded_lanes(),
            lanes.len(),
            "裁剪表重置必须让所有 lane 重编码"
        );
        assert_eq!(
            encoder.buffer().clips.len(),
            1,
            "重置后裁剪表只剩哨兵（本场景无 clip）"
        );
    }

    /// DPI 变化 ⇒ 物理坐标全变 ⇒ 全量重编码。
    #[test]
    fn a_dpi_change_forces_a_full_reencode() {
        let (mut scene, lanes) = grid_scene(4);
        let mut encoder = InstanceEncoder::new();
        let store = empty_store();
        encoder
            .encode(&scene, Scale::ONE, resolver(), &store)
            .expect("1× 帧");
        // 改一条 lane 之后切 2×：即使只有一条脏 lane，也必须全部重编码。
        let lane = lanes[2];
        assert!(scene.clear_lane(lane));
        assert!(scene.push(lane, quad_primitive(solid())));
        encoder
            .encode(&scene, scale(), resolver(), &store)
            .expect("2× 帧");
        assert_eq!(encoder.encoded_lanes(), lanes.len());
        // 物理化的 bounds：1× 下是 10×20，2× 下是 20×40。
        let bytes = lane_instance_bytes(&encoder, lanes[0]);
        let instance: &[super::GpuPrimitive] = bytemuck::cast_slice(&bytes);
        assert_eq!(instance[0].bounds, [0.0, 0.0, 20.0, 40.0]);
    }

    /// 压实：碎片超过门槛的**无脏帧**会把段前移、空洞消失，内容与绘制序不变。
    #[test]
    fn compaction_packs_the_arena_on_a_clean_frame() {
        // 100 条 lane、每条 2 个实例（实例环 = 200）；随后每条减到 1 个实例：
        // 每条搬走一个 2 实例的旧段、在空闲块里切出 1 个 ⇒ 实例环膨胀到 ~300、碎片 ~100。
        let (mut scene, lanes) = grid_scene(100);
        let mut encoder = InstanceEncoder::new();
        let store = empty_store();
        for lane in &lanes {
            assert!(scene.push(
                *lane,
                Primitive::Quad(Quad {
                    bounds: Rect::new(8.0, 0.0, 4.0, 4.0),
                    corner_radius: [0.0; 4],
                    paint: solid(),
                })
            ));
        }
        encoder
            .encode(&scene, scale(), resolver(), &store)
            .expect("初帧");
        for lane in &lanes {
            assert!(scene.clear_lane(*lane));
            assert!(scene.push(
                *lane,
                Primitive::Quad(Quad {
                    bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
                    corner_radius: [0.0; 4],
                    paint: solid(),
                })
            ));
        }
        encoder
            .encode(&scene, scale(), resolver(), &store)
            .expect("搬段帧");
        let used_before = encoder.buffer().instance_count();
        let free_before: u32 = encoder
            .buffer()
            .free
            .iter()
            .map(|range| range.end - range.start)
            .sum();
        assert!(free_before >= 64, "100 条 lane 搬段必须留下足够碎片");

        // 无脏帧 ⇒ compaction。
        encoder
            .encode(&scene, scale(), resolver(), &store)
            .expect("压实帧");
        assert_eq!(encoder.encoded_lanes(), 0);
        assert!(encoder.buffer().free.is_empty(), "压实之后不得留下空闲块");
        let live: u32 = 100;
        assert_eq!(encoder.buffer().instance_count(), live);
        assert!(used_before > live, "压实前必须有空洞");
        assert_eq!(encoder.buffer().len() as u32, live);

        // 内容与绘制序必须与全量编码一致。
        let mut full = InstanceEncoder::new();
        full.encode(&scene, scale(), resolver(), &store)
            .expect("全量帧");
        assert_eq!(encoder.buffer().as_bytes(), full.buffer().as_bytes());
        assert_eq!(encoder.buffer().ops(), full.buffer().ops());
    }

    /// op 级跳过：op 的 bounds 是真实局部范围（不是整屏），且连续 lane 合并后 op 数远小于 lane 数。
    #[test]
    fn op_bounds_are_local_and_ops_are_coalesced() {
        let (scene, _lanes) = grid_scene(200);
        let buffer = encode(&scene);
        let instances_ops: Vec<super::PhysicalBounds> = buffer
            .ops
            .iter()
            .filter_map(|op| match op {
                super::FrameOp::Instances { bounds, .. } => Some(*bounds),
                super::FrameOp::Geometry { .. } => None,
            })
            .collect();
        assert_eq!(
            instances_ops.len(),
            200usize.div_ceil(super::MAX_LANES_PER_OP as usize),
            "连续 lane 必须合并成 op（不是一条 lane 一条 op）"
        );
        // 第一条 op 只覆盖它的 64 条 lane（8 列 × 8 行，物理像素 = 逻辑 × 2），不是整屏。
        let first = instances_ops[0];
        assert_eq!(
            (first.x0, first.y0),
            (-2, -2),
            "包围盒含 AA 外扩 1 px + 取整余量 1 px"
        );
        assert!(
            first.x1 <= (8 * 16 + 10) * 2 + 2,
            "op bounds 不得扩散到整屏：{first:?}"
        );
        // damage 只在最后一组附近：前面的 op 必须判定为不相交（真正跳过）。
        let bottom = instances_ops.last().expect("至少一条 op");
        let damage = [bottom.x0, bottom.y0, bottom.x1, bottom.y1];
        assert!(
            !first.intersects(damage),
            "顶部 op 与底部 damage 不相交（op 级跳过生效）"
        );
    }
}

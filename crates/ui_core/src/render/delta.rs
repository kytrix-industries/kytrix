//! `SceneDelta`：UI 线程 → 合成线程的唯一传输契约（架构决策 B3B6 §2 / B6-2）。
//!
//! ## 为什么不另造一层增量协议
//!
//! 第一期的增量编码已经具备全部要素：`Lane` 有内容版本（`Scene::lane_state`）、
//! `InstanceEncoder` 按 lane 段复用、`FramePayload` 只带「自上次载荷以来的变化实例段」。缺的只是
//! **把编码搬出 UI 线程**所需的两样东西：
//!
//! 1. 一条 `Send` 的 lane 快照（`Arc<[Primitive]>`，`Arc` 让复用 lane 零拷贝）；
//! 2. 一份「这一帧变了什么」的描述（变化 lane / 移除 lane / 绘制序 / 滚动偏移 / 图集无关的
//!    damage）。
//!
//! 因此 [`SceneDelta`] 只描述**变化**，语义与 `FramePayload` 的增量载荷一一对应：lane 版本没变
//! ⇒ 合成线程不重编码 ⇒ 载荷里没有对应实例段。
//!
//! ## 队列语义（有界 + 合并）
//!
//! 每窗口一个 [`DeltaSlot`]：**只保留最新**（和 `LatestFrame` 同构），被覆盖的 delta 按
//! 「应用顺序 = 旧 → 新」合并进新 delta（lane 后写胜出、damage 取并集、滚动偏移后写胜出）。
//! 于是「丢帧不丢变化」对 lane 数据同样成立：GPU 侧 `InstanceMirror` 的欠账机制因此不需要改。
//!
//! ## 应用顺序
//!
//! `LaneStore` 只按 delta 的到达顺序应用；UI 侧的帧序号（[`SceneDelta::index`]）单调，合成线程
//! 用它丢弃过期 delta（`index` 不大于已应用值时整帧丢弃）。

use std::{sync::Arc, time::Instant};

#[cfg(target_os = "linux")]
use crate::math::Size;
use crate::{
    math::{PhysicalRect, Point, Scale},
    paint::{
        geometry::GeometryId,
        geometry_store::GeometryStore,
        instance::{GeometryLookup, LaneSource},
        primitive::Primitive,
        scene::{LaneId, Scene},
    },
    render::compositor::{DamageRects, Plan},
    window::{FrameIndex, WindowId},
};

/// 一条变化 lane 的快照（`primitives` 是**完整**内容，不是 diff）。
///
/// `Arc<[Primitive]>` 而不是 `Vec`：同一份 lane 内容在「UI 侧的 snapshot 缓存 / 总线上的 delta /
/// 合成侧的 lane 表」之间三处共享，零拷贝；只有内容真的变化（版本 +1）时才重建一次。
#[derive(Clone)]
pub(crate) struct LaneDelta {
    pub lane: LaneId,
    pub version: u32,
    pub primitives: Arc<[Primitive]>,
}

/// 一个滚动槽的偏移变化（逻辑像素；正数 = 内容向负方向移动，与 `NodeTree` 的 `scroll_offset` 同义）。
///
/// `viewport` 是该滚动容器的可见区（**物理像素**）：滚动帧的 damage 直接取它 —— 合成端偏移下
/// 「移动」不是 blit 而是按新偏移重画整个视口，因此视口内部全部像素都可能变，视口外部不变。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScrollDelta {
    pub slot: u32,
    pub offset: Point,
    pub viewport: PhysicalRect,
}

/// 一帧的「头部」（原 `FrameRequest` 的非载荷字段）：
///
/// 合成线程用它构造 `FrameRequest`，因此这些值必须与 delta **同帧**过线 —— `surface` / `scale`
/// 各自跨帧会让几何通道画在错误尺度上（`FrameRequest::scale` 的注释）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameHeader {
    pub window: WindowId,
    /// UI 侧提交序号（单调；合成线程用它丢弃过期 delta）。
    pub index: FrameIndex,
    /// UI 侧的 damage 计划（合成线程会并入滚动暴露区）。
    pub plan: Plan,
    /// 目标物理像素。
    pub surface: (u32, u32),
    /// 窗口 DPI（几何通道的逻辑→物理换算）。
    pub scale: Scale,
    /// 窗口逻辑尺寸（Wayland 的 buffer 尺寸来源）。
    #[cfg(target_os = "linux")]
    pub surface_local: Size,
    /// 整数回退的 `wl_surface` buffer scale（fractional 模式恒 1）。
    #[cfg(target_os = "linux")]
    pub buffer_scale: i32,
    /// UI 侧的发布时刻（HUD 的 publish→present 延迟起点）。
    pub published_at: Instant,
}

/// UI → 合成线程的一帧描述（**只含变化**）。
pub(crate) struct SceneDelta {
    pub header: FrameHeader,
    /// 变化 lane（新建 / 内容更新）。版本与合成侧相同时会被跳过（幂等）。
    pub lanes: Vec<LaneDelta>,
    /// 已释放的 lane（回收实例段）。
    pub removed: Vec<LaneId>,
    /// 绘制序：**仅当顺序变化时**携带（滚动 / 重排）；`None` = 沿用上一次。
    pub order: Option<Arc<[LaneId]>>,
    /// 滚动偏移变化（O(槽数)，不触碰任何图元）。
    pub scroll: Vec<ScrollDelta>,
    /// 本帧引用的几何描述（`Arc` 克隆；合成线程按 id 缓存）。
    pub geometries: Vec<(GeometryId, Arc<crate::paint::geometry::GeometryDesc>)>,
    /// 是否整帧重建（首帧 / DPI 变化 / 裁剪表重建）：合成线程会重置 lane 表与编码器。
    pub full: bool,
}

impl SceneDelta {
    /// 空 delta（只有头部）：没有任何 lane 变化时仍可能因滚动 / damage 出帧。
    pub(crate) fn empty(header: FrameHeader) -> Self {
        Self {
            header,
            lanes: Vec::with_capacity(0),
            removed: Vec::with_capacity(0),
            order: None,
            scroll: Vec::with_capacity(0),
            geometries: Vec::with_capacity(0),
            full: false,
        }
    }
}

/// 把**被覆盖**的旧 delta 合并进新 delta（应用顺序 = 旧 → 新）。
///
/// 合并规则与 `FramePayload::absorb` 同构：
/// - lane：同 id 以**新**为准；旧 delta 里新 delta 没有的 lane 必须保留（否则那段变化永远丢）；
/// - removed：并集减去「新 delta 里重新出现」的 lane；
/// - order：新 delta 有就用新的，否则保留旧的（顺序变化不能丢）；
/// - scroll：同槽以新为准；
/// - damage：`Plan::merge`（已有实现，含容量降级）；
/// - `full`：任一为真即为真（整帧重建不能被覆盖成增量）。
pub(crate) fn absorb(old: SceneDelta, new: &mut SceneDelta) {
    let SceneDelta {
        header,
        lanes,
        removed,
        order,
        scroll,
        geometries,
        full,
    } = old;
    // 旧帧的 damage 必须并进新帧（被覆盖的帧不产生 GPU 工作，但它的像素变化要补画）。
    new.header.plan = header.plan.merge(new.header.plan);
    new.full |= full;
    if new.order.is_none() {
        new.order = order;
    }
    // 旧 lane：新 delta 里同 id 的版本更新 ⇒ 直接用新的。
    if !lanes.is_empty() {
        let mut kept: Vec<LaneDelta> = Vec::with_capacity(lanes.len());
        for old in lanes {
            if !new.lanes.iter().any(|lane| lane.lane == old.lane) {
                kept.push(old);
            }
        }
        new.lanes.reserve(kept.len());
        new.lanes.extend(kept);
    }
    if !removed.is_empty() {
        for lane in removed {
            if !new.removed.contains(&lane) && !new.lanes.iter().any(|fresh| fresh.lane == lane) {
                new.removed.push(lane);
            }
        }
    }
    if !scroll.is_empty() {
        new.scroll
            .retain(|fresh| !scroll.iter().any(|old| old.slot == fresh.slot));
        new.scroll.extend(scroll);
    }
    if !geometries.is_empty() {
        let mut kept: Vec<(GeometryId, Arc<crate::paint::geometry::GeometryDesc>)> =
            Vec::with_capacity(geometries.len());
        for entry in geometries {
            if !new.geometries.iter().any(|(fresh, _)| *fresh == entry.0) {
                kept.push(entry);
            }
        }
        new.geometries.reserve(kept.len());
        new.geometries.extend(kept);
    }
}

/// UI 侧的变化收集器：把 `Scene` 的版本变化、绘制序、滚动偏移、几何描述收成一条 delta。
///
/// 复用判据与 [`crate::paint::instance::InstanceEncoder`] 完全一致（版本相等 ⇒ 不重编码），因此
/// 「UI 侧认为没变」与「合成侧不需要重编码」是同一个判据，不存在两套口径。
pub(crate) struct DeltaBuilder {
    /// 上次发布出去的版本（按 lane arena 槽位索引；代际变化视为「新 lane」）。
    versions: Vec<(u32, u32)>,
    /// 上次发布出去的绘制序（指针 + 长度判等：`Scene::set_order` 在顺序不变时不重写）。
    order: Arc<[LaneId]>,
    /// 上次发布出去的滚动偏移（按槽位）。
    scroll: Vec<Point>,
    /// 上次发布出去的几何描述（按 id）。
    geometries: ahash::HashMap<GeometryId, Arc<crate::paint::geometry::GeometryDesc>>,
    /// 上次发布时的 DPI（变化 ⇒ 物理坐标全变 ⇒ 整帧重建）。
    scale: Option<Scale>,
}

impl DeltaBuilder {
    pub(crate) fn new() -> Self {
        Self {
            versions: Vec::new(),
            order: Arc::from(Vec::new()),
            scroll: Vec::new(),
            geometries: ahash::HashMap::default(),
            scale: None,
        }
    }

    /// 本帧是否必须整帧重建（DPI 变化：物理坐标、缩放后的圆角与 clip 全部变了）。
    ///
    /// 只在 `true` 时清缓存；第一帧也返回 `true`（合成侧还没有任何 lane）。
    pub(crate) fn full_for(&mut self, scale: Scale) -> bool {
        let full = self.scale != Some(scale);
        self.scale = Some(scale);
        full
    }

    /// 采集本帧的 lane 变化 + 绘制序 + 几何描述。
    ///
    /// `scroll` 是**本帧全量**的滚动表（槽位 → 逻辑偏移），只有变化的槽进入 delta；
    /// `viewports` 是槽位 → 物理可见区（滚动 damage 用）。
    pub(crate) fn collect(
        &mut self,
        scene: &Scene,
        store: &GeometryStore,
        scroll: &[(Point, PhysicalRect)],
        full: bool,
        delta: &mut SceneDelta,
    ) {
        if full {
            self.versions.clear();
            self.order = Arc::from(Vec::new());
            self.scroll.clear();
            self.geometries.clear();
        }
        // 绘制序：`set_order` 在顺序不变时不会重写，因此指针/长度相等即可跳过。
        let order = scene.order();
        if full || !same_order(&self.order, order) {
            delta.order = Some(Arc::from(order));
            self.order = delta.order.clone().expect("刚刚写入的绘制序必须存在");
        }
        if self.versions.len() < scene.lane_count() {
            self.versions.resize(scene.lane_count(), (0, 0));
        }
        for lane in order.iter().copied() {
            let slot = lane.slot() as usize;
            let Some((version, primitives)) = scene.lane_state(lane) else {
                continue;
            };
            if slot < self.versions.len()
                && self.versions[slot] == (lane.generation(), version)
                && !full
            {
                continue;
            }
            // 内容变了：重建快照（`Arc<[Primitive]>` 只在**变化**的 lane 上分配一次；
            // 未变化的 lane 根本不进 delta，因此稳态零分配、零拷贝）。
            self.collect_geometries(primitives, store, delta);
            delta.lanes.push(LaneDelta {
                lane,
                version,
                primitives: Arc::from(primitives.to_vec()),
            });
            if slot >= self.versions.len() {
                self.versions.resize(slot + 1, (0, 0));
            }
            self.versions[slot] = (lane.generation(), version);
        }
        // 滚动：只有变化的槽进入 delta（滚动帧因此不触碰任何 lane）。
        if self.scroll.len() < scroll.len() {
            self.scroll
                .resize(scroll.len(), Point::new(f32::NAN, f32::NAN));
        }
        for (slot, (offset, viewport)) in scroll.iter().enumerate() {
            let previous = self.scroll[slot];
            if full || previous != *offset {
                delta.scroll.push(ScrollDelta {
                    slot: slot as u32,
                    offset: *offset,
                    viewport: *viewport,
                });
                self.scroll[slot] = *offset;
            }
        }
    }

    /// 一条 lane 引用的几何描述（只有变化 lane 会重新收集；未变化的 lane 的描述仍在缓存里）。
    fn collect_geometries(
        &mut self,
        primitives: &[Primitive],
        store: &GeometryStore,
        delta: &mut SceneDelta,
    ) {
        for primitive in primitives {
            let id = match primitive {
                Primitive::Geometry(draw) => draw.id,
                Primitive::Path(path) => path.geometry,
                _ => continue,
            };
            let Some(desc) = store.desc_arc(id) else {
                continue;
            };
            let known = self
                .geometries
                .get(&id)
                .is_some_and(|cached| Arc::ptr_eq(cached, &desc));
            if known {
                continue;
            }
            self.geometries.insert(id, Arc::clone(&desc));
            delta.geometries.push((id, desc));
        }
    }

    /// lane 被释放：清掉它的版本与快照，并记进 delta 的移除表（回收实例段）。
    pub(crate) fn note_removed(&mut self, lane: LaneId, delta: &mut SceneDelta) {
        let slot = lane.slot() as usize;
        if slot < self.versions.len() {
            self.versions[slot] = (0, 0);
        }
        delta.removed.push(lane);
    }
}

/// 绘制序是否与上次相同（长度 + 逐项）。
fn same_order(previous: &[LaneId], current: &[LaneId]) -> bool {
    previous.len() == current.len() && previous.iter().zip(current).all(|(a, b)| a == b)
}

/// 合成侧的 lane 表：delta 应用之后的「当前场景」。
///
/// 它只保存**内容版本 + 快照**（不需要 `Scene` 的 arena 语义：lane 的 slot/generation 已经在
/// `LaneId` 里）。绘制序单独一份，`None` 的 delta 沿用上一次。
#[derive(Default)]
pub(crate) struct LaneStore {
    lanes: Vec<Option<StoredLane>>,
    order: Vec<LaneId>,
}

struct StoredLane {
    generation: u32,
    version: u32,
    primitives: Arc<[Primitive]>,
}

impl LaneStore {
    pub(crate) fn new() -> Self {
        Self {
            lanes: Vec::new(),
            order: Vec::new(),
        }
    }

    /// 应用一条 delta（顺序 = 旧 → 新由调用方保证）。
    ///
    /// 返回 `true` = lane 表真的变了（调用方据此判断是否需要重编码）。版本相同的重复 delta
    /// （覆盖帧合并/重发）不会改变表，因此是幂等的。
    pub(crate) fn apply(&mut self, delta: &SceneDelta) -> bool {
        let mut changed = delta.full;
        if delta.full {
            self.lanes.clear();
            self.order.clear();
        }
        if let Some(order) = &delta.order
            && !same_order(&self.order, order)
        {
            self.order.clear();
            self.order.extend_from_slice(order);
            changed = true;
        }
        for removed in &delta.removed {
            let slot = removed.slot() as usize;
            if self
                .lanes
                .get_mut(slot)
                .and_then(Option::take)
                .is_some_and(|lane| lane.generation == removed.generation())
            {
                changed = true;
            }
        }
        for lane in &delta.lanes {
            let slot = lane.lane.slot() as usize;
            if self.lanes.len() <= slot {
                self.lanes.resize_with(slot + 1, || None);
            }
            let stale = self.lanes[slot].as_ref().is_some_and(|stored| {
                stored.generation == lane.lane.generation() && stored.version == lane.version
            });
            if stale {
                continue;
            }
            self.lanes[slot] = Some(StoredLane {
                generation: lane.lane.generation(),
                version: lane.version,
                primitives: Arc::clone(&lane.primitives),
            });
            changed = true;
        }
        changed
    }

    /// 已缓存的 lane 快照数（= 本帧可整段复用的段数）。
    pub(crate) fn len(&self) -> usize {
        self.lanes.iter().flatten().count()
    }
}

impl LaneSource for LaneStore {
    fn lane_state(&self, id: LaneId) -> Option<(u32, &[Primitive])> {
        let stored = self.lanes.get(id.slot() as usize)?.as_ref()?;
        (stored.generation == id.generation()).then_some((stored.version, &stored.primitives))
    }

    fn order(&self) -> &[LaneId] {
        &self.order
    }
}

impl LaneSource for Scene {
    fn lane_state(&self, id: LaneId) -> Option<(u32, &[Primitive])> {
        Scene::lane_state(self, id)
    }

    fn order(&self) -> &[LaneId] {
        Scene::order(self)
    }
}

/// 合成侧的几何描述缓存（`GeometryStore` 在 UI 线程，合成线程不能直接读它）。
#[derive(Default)]
pub(crate) struct GeometryCatalog {
    entries: ahash::HashMap<GeometryId, Arc<crate::paint::geometry::GeometryDesc>>,
}

impl GeometryCatalog {
    pub(crate) fn new() -> Self {
        Self {
            entries: ahash::HashMap::default(),
        }
    }

    /// 并入 delta 带来的描述（`full` 时先清空：整帧重建之后旧描述可能都不再被引用）。
    pub(crate) fn apply(&mut self, delta: &SceneDelta) {
        if delta.full {
            self.entries.clear();
        }
        for (id, desc) in &delta.geometries {
            self.entries.insert(*id, Arc::clone(desc));
        }
    }
}

impl GeometryLookup for GeometryCatalog {
    fn desc_arc(&self, id: GeometryId) -> Option<Arc<crate::paint::geometry::GeometryDesc>> {
        self.entries.get(&id).map(Arc::clone)
    }
}

/// 滚动槽的偏移表（物理像素，按槽位索引；合成线程唯一消费点）。
///
/// **坐标系约定**：`offset` 是逻辑像素的滚动位移，`physical = offset × scale`；GPU 侧把它加在
/// 顶点/裁剪的世界坐标上（`RootUniform::with_scroll`），因此实例数据里烘焙的坐标始终是
/// **滚动容器内容空间**（不随滚动变化）—— 滚动帧不重编码的前提。
#[derive(Default)]
pub(crate) struct ScrollTable {
    offsets: Vec<Point>,
    changed: Vec<u32>,
}

impl ScrollTable {
    pub(crate) fn new() -> Self {
        Self {
            offsets: Vec::new(),
            changed: Vec::new(),
        }
    }

    /// 应用 delta 的滚动变化，返回本帧变化的槽位（damage 需要按视口补画）。
    pub(crate) fn apply(&mut self, deltas: &[ScrollDelta], scale: Scale) -> &[u32] {
        self.changed.clear();
        for delta in deltas {
            let slot = delta.slot as usize;
            if self.offsets.len() <= slot {
                self.offsets.resize(slot + 1, Point::new(0.0, 0.0));
            }
            if self.offsets[slot] != delta.offset {
                self.offsets[slot] = delta.offset;
                self.changed.push(delta.slot);
            }
            let _ = scale;
        }
        &self.changed
    }

    /// 槽位的物理偏移（槽位不存在 ⇒ 0）。
    pub(crate) fn physical_offset(&self, slot: u32, scale: Scale) -> [f32; 2] {
        let offset = self
            .offsets
            .get(slot as usize)
            .copied()
            .unwrap_or_else(|| Point::new(0.0, 0.0));
        let factor = scale.factor();
        [offset.x * factor, offset.y * factor]
    }

    /// GPU 侧的偏移表（`FramePayload` 的载荷之一）。
    pub(crate) fn to_payload(&self, scale: Scale, out: &mut Vec<[f32; 2]>) {
        out.clear();
        out.extend((0..self.offsets.len() as u32).map(|slot| self.physical_offset(slot, scale)));
    }
}

/// 滚动帧的 damage：**整个视口**。
///
/// 合成端偏移下滚动不是 blit（那需要先搬像素），而是按新偏移重画整条滚动内容的 op —— 视口内的
/// 全部像素都可能变，视口外不变。因此 damage 就是视口本身，比「旧/新可见区差集」更保守但**必然
/// 正确**（差集优化留给需要它的后端，见架构决策 §4 的 Qt blit-scroll 注记）。
pub(crate) fn scroll_damage(viewport: PhysicalRect) -> Option<PhysicalRect> {
    let width = viewport.x1.saturating_sub(viewport.x0);
    let height = viewport.y1.saturating_sub(viewport.y0);
    (width > 0 && height > 0).then_some(viewport)
}

/// 把滚动暴露区并入 damage 计划（不改变 `Full` / `Idle` 的语义）。
pub(crate) fn push_scroll_damage(plan: Plan, viewports: &[PhysicalRect]) -> Plan {
    if viewports.is_empty() {
        return plan;
    }
    let base = match plan {
        Plan::Full => return Plan::Full,
        Plan::Partial(rects) => rects,
        Plan::Idle => DamageRects::new(),
    };
    let mut rects = base;
    for viewport in viewports {
        if let Some(damage) = scroll_damage(*viewport)
            && !rects.push(damage)
        {
            // 容量耗尽：整帧重画（`Present1` / scissor 的固定容量不能溢出）。
            return Plan::Full;
        }
    }
    Plan::Partial(rects)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        DeltaBuilder, FrameHeader, GeometryCatalog, LaneStore, SceneDelta, ScrollDelta,
        ScrollTable, absorb, push_scroll_damage, scroll_damage,
    };
    #[cfg(target_os = "linux")]
    use crate::math::Size;
    use crate::{
        math::{PhysicalRect, Point, Rect, Scale},
        paint::{
            color::Color,
            instance::GeometryLookup,
            primitive::{Primitive, Quad, QuadPaint},
            scene::Scene,
        },
        render::compositor::{DamageRects, Plan},
        window::{FrameIndex, WindowId},
    };

    fn header(index: u32, plan: Plan) -> FrameHeader {
        FrameHeader {
            window: WindowId::from_raw(1),
            index: FrameIndex::from_raw(index),
            plan,
            surface: (64, 64),
            scale: Scale::ONE,
            #[cfg(target_os = "linux")]
            surface_local: Size::new(64.0, 64.0),
            #[cfg(target_os = "linux")]
            buffer_scale: 1,
            published_at: std::time::Instant::now(),
        }
    }

    fn quad() -> Primitive {
        Primitive::Quad(Quad {
            bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: Color::WHITE,
                border: None,
            },
        })
    }

    fn lane(lane: crate::paint::scene::LaneId, version: u32) -> super::LaneDelta {
        super::LaneDelta {
            lane,
            version,
            primitives: Arc::from(vec![quad()]),
        }
    }

    fn store() -> crate::paint::geometry_store::GeometryStore {
        crate::paint::geometry_store::GeometryStore::new()
    }

    /// 版本未变的 lane 不进入 delta（增量编码的 UI 侧判据）。
    #[test]
    fn only_changed_lanes_enter_the_delta() {
        let mut scene = Scene::empty();
        let first = scene.new_lane();
        let second = scene.new_lane();
        assert!(scene.push(first, quad()));
        assert!(scene.push(second, quad()));
        scene.set_order(&[first, second]);
        let geometry = store();
        let mut builder = DeltaBuilder::new();
        let mut delta = SceneDelta::empty(header(1, Plan::Full));
        builder.collect(&scene, &geometry, &[], true, &mut delta);
        assert_eq!(delta.lanes.len(), 2, "全量帧必须带全部 lane");

        // 只改一条 lane：delta 里只有它。
        assert!(scene.push(second, quad()));
        let mut delta = SceneDelta::empty(header(2, Plan::Partial(DamageRects::new())));
        builder.collect(&scene, &geometry, &[], false, &mut delta);
        assert_eq!(delta.lanes.len(), 1);
        assert_eq!(delta.lanes[0].lane, second);
        assert!(delta.order.is_none(), "绘制序没变 ⇒ 不携带");
    }

    /// 滚动帧：只有偏移变化时 delta 里 **没有任何 lane**（`encoded_lanes == 0` 的 UI 侧前提）。
    #[test]
    fn a_scroll_only_delta_carries_no_lane() {
        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(lane, quad()));
        scene.set_order(&[lane]);
        let geometry = store();
        let mut builder = DeltaBuilder::new();
        let viewport = PhysicalRect::new(0, 0, 64, 64);
        let mut first = SceneDelta::empty(header(1, Plan::Full));
        builder.collect(
            &scene,
            &geometry,
            &[(Point::new(0.0, 0.0), viewport)],
            true,
            &mut first,
        );
        let mut scrolled = SceneDelta::empty(header(2, Plan::Idle));
        builder.collect(
            &scene,
            &geometry,
            &[(Point::new(0.0, -12.0), viewport)],
            false,
            &mut scrolled,
        );
        assert!(scrolled.lanes.is_empty(), "滚动帧不得重编码任何 lane");
        assert!(scrolled.order.is_none());
        assert_eq!(scrolled.scroll.len(), 1);
        assert_eq!(scrolled.scroll[0].offset.y, -12.0);
        assert!(scrolled.lanes.is_empty() && scrolled.order.is_none());
    }

    /// `DeltaBuilder` 的采集成本只随**变化**的 lane 增长：10k 条 lane 里只改 1 条时，delta 里
    /// 只有 1 个快照（否则全量帧之外每帧都会重建 10k 份 `Arc<[Primitive]>`）。
    #[test]
    fn a_one_lane_change_does_not_snapshot_the_whole_scene() {
        const LANES: usize = 10_000;
        let mut scene = Scene::empty();
        let mut order = Vec::with_capacity(LANES);
        for _ in 0..LANES {
            let lane = scene.new_lane();
            assert!(scene.push(lane, quad()));
            order.push(lane);
        }
        scene.set_order(&order);
        let geometry = store();
        let mut builder = DeltaBuilder::new();
        let mut first = SceneDelta::empty(header(1, Plan::Full));
        builder.collect(&scene, &geometry, &[], true, &mut first);
        assert_eq!(first.lanes.len(), LANES, "全量帧带全部 lane 快照");

        // 只改最后一条 lane：delta 里只有它，且绘制序不携带。
        assert!(scene.push(order[LANES - 1], quad()));
        let mut delta = SceneDelta::empty(header(2, Plan::Partial(DamageRects::new())));
        builder.collect(&scene, &geometry, &[], false, &mut delta);
        assert_eq!(delta.lanes.len(), 1, "只有变化的 lane 才重建快照");
        assert_eq!(delta.lanes[0].lane, order[LANES - 1]);
        assert_eq!(
            Arc::strong_count(&delta.lanes[0].primitives),
            1,
            "快照只被 delta 持有（没有额外复制）"
        );
        assert!(delta.order.is_none(), "绘制序没变 ⇒ 不携带");
    }

    /// 被覆盖的 delta 必须并进新 delta：lane 后写胜出、旧 lane 不丢、damage 取并集。
    #[test]
    fn absorbing_a_covered_delta_keeps_every_lane() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        let b = scene.new_lane();
        let mut older = SceneDelta::empty(header(
            1,
            Plan::Partial({
                let mut rects = DamageRects::new();
                assert!(rects.push(PhysicalRect::new(0, 0, 8, 8)));
                rects
            }),
        ));
        older.lanes.push(lane(a, 1));
        let mut newer = SceneDelta::empty(header(
            2,
            Plan::Partial({
                let mut rects = DamageRects::new();
                assert!(rects.push(PhysicalRect::new(8, 8, 16, 16)));
                rects
            }),
        ));
        newer.lanes.push(lane(b, 1));
        absorb(older, &mut newer);
        assert_eq!(newer.lanes.len(), 2, "旧帧的 lane 变化不能丢");
        assert!(matches!(newer.header.plan, Plan::Partial(_)));
        let Plan::Partial(rects) = newer.header.plan else {
            panic!("应保持 Partial");
        };
        assert_eq!(rects.len(), 2, "damage 取并集");
    }

    /// 同一 lane 在新旧 delta 里都出现：新版本胜出，不重复。
    #[test]
    fn a_newer_lane_supersedes_the_covered_one() {
        let mut scene = Scene::empty();
        let lane_id = scene.new_lane();
        let mut older = SceneDelta::empty(header(1, Plan::Idle));
        older.lanes.push(lane(lane_id, 3));
        let mut newer = SceneDelta::empty(header(2, Plan::Idle));
        newer.lanes.push(lane(lane_id, 4));
        absorb(older, &mut newer);
        assert_eq!(newer.lanes.len(), 1);
        assert_eq!(newer.lanes[0].version, 4);
    }

    /// `LaneStore` 的应用是幂等的：重复应用同一 delta 不改变表。
    #[test]
    fn applying_the_same_delta_twice_is_idempotent() {
        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(lane, quad()));
        scene.set_order(&[lane]);
        let geometry = store();
        let mut builder = DeltaBuilder::new();
        let mut delta = SceneDelta::empty(header(1, Plan::Full));
        builder.collect(&scene, &geometry, &[], true, &mut delta);
        let mut applied = SceneDelta::empty(header(2, Plan::Idle));
        applied.lanes = delta.lanes.clone();
        applied.order = delta.order.clone();
        applied.full = false;
        let mut store = LaneStore::new();
        assert!(store.apply(&delta));
        assert!(!store.apply(&applied), "版本一致 ⇒ 不改变 lane 表");
        assert_eq!(store.len(), 1);
    }

    /// 滚动表按 scale 换算成物理偏移，并原样进载荷。
    #[test]
    fn the_scroll_table_converts_to_physical_offsets() {
        let mut table = ScrollTable::new();
        let scale = Scale::new(2.0).expect("scale");
        let deltas = [ScrollDelta {
            slot: 0,
            offset: Point::new(0.0, -10.0),
            viewport: PhysicalRect::new(0, 0, 100, 100),
        }];
        assert_eq!(table.apply(&deltas, scale), &[0]);
        assert_eq!(table.physical_offset(0, scale), [0.0, -20.0]);
        let mut out = Vec::new();
        table.to_payload(scale, &mut out);
        assert_eq!(out, vec![[0.0, -20.0]]);
        // 重复应用同样的偏移：不算「变化」（滚动帧的 damage 因此不重复补）。
        assert!(table.apply(&deltas, scale).is_empty());
    }

    /// 滚动 damage = 视口；零面积视口不产生 damage。
    #[test]
    fn scroll_damage_is_the_viewport() {
        let viewport = PhysicalRect::new(4, 5, 60, 65);
        assert_eq!(scroll_damage(viewport), Some(viewport));
        assert_eq!(scroll_damage(PhysicalRect::new(4, 5, 4, 65)), None);
    }

    /// 滚动暴露区并进 damage 计划；`Full` 保持 `Full`，容量耗尽降级 `Full`。
    #[test]
    fn scroll_damage_is_merged_into_the_plan() {
        let viewport = PhysicalRect::new(0, 0, 32, 32);
        assert_eq!(
            push_scroll_damage(Plan::Idle, &[viewport]),
            Plan::Partial({
                let mut rects = DamageRects::new();
                assert!(rects.push(viewport));
                rects
            })
        );
        assert_eq!(push_scroll_damage(Plan::Full, &[viewport]), Plan::Full);
        let many = vec![viewport; crate::render::damage::MAX_DAMAGE_RECTS + 1];
        assert_eq!(push_scroll_damage(Plan::Idle, &many), Plan::Full);
    }

    /// 几何描述：只有本帧引用的才进 delta，且 `Arc` 复用（不重复携带）。
    #[test]
    fn geometry_descriptions_are_carried_once() {
        use crate::{
            math::Transform,
            paint::{
                geometry::{GeometryDesc, GeometryPipeline, IndexType, Topology},
                primitive::GeometryDraw,
            },
        };
        let mut scene = Scene::empty();
        let mut geometry = store();
        let id = geometry
            .insert(GeometryDesc {
                vertex_bytes: bytes::Bytes::from_static(&[0u8; 12]),
                index_bytes: bytes::Bytes::from_static(&[0u8; 6]),
                vertex_stride: 12,
                index_type: IndexType::U16,
                topology: Topology::TriangleList,
                bounds: Rect::new(0.0, 0.0, 8.0, 8.0),
                pipeline: GeometryPipeline::Ui,
            })
            .expect("注册几何");
        let lane = scene.new_lane();
        assert!(scene.push(
            lane,
            Primitive::Geometry(GeometryDraw {
                id,
                transform: Transform::IDENTITY,
                color: Color::WHITE,
            })
        ));
        scene.set_order(&[lane]);
        let mut builder = DeltaBuilder::new();
        let mut delta = SceneDelta::empty(header(1, Plan::Full));
        builder.collect(&scene, &geometry, &[], true, &mut delta);
        assert_eq!(delta.geometries.len(), 1);
        assert_eq!(delta.geometries[0].0, id);
        // 同一 lane 再变一次（内容相同但版本 +1）：描述已经在缓存里，不重复携带。
        assert!(scene.push(lane, quad()));
        let mut again = SceneDelta::empty(header(2, Plan::Full));
        builder.collect(&scene, &geometry, &[], false, &mut again);
        assert!(again.geometries.is_empty(), "描述已缓存 ⇒ 不重复过线");
        let mut catalog = GeometryCatalog::new();
        catalog.apply(&delta);
        assert!(catalog.desc_arc(id).is_some());
    }
}

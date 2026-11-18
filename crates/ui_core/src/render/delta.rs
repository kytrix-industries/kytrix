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
    /// 图元烘焙在哪个滚动槽的内容空间（`0` = 已是屏幕坐标；见 [`ScrollTable`] 的坐标系约定）。
    pub scroll: u32,
    pub primitives: Arc<[Primitive]>,
}

/// 一个滚动槽的偏移变化（逻辑像素；正数 = 内容向负方向移动，与 `NodeTree` 的 `scroll_offset` 同义）。
///
/// `viewport` 是该滚动槽的**屏幕裁剪区**（物理像素）：容器 padding box 与本帧全部祖先 clip 的交集。
/// 它有两个消费者：
/// 1. 滚动帧的 damage —— 合成端偏移下「移动」不是 blit 而是按新偏移重画整条滚动内容的 op，
///    因此视口内部全部像素都可能变，视口外部不变；
/// 2. GPU 侧的**每 op scissor** —— 滚动容器自己的视口 clip 不进实例的裁剪链（它随滚动变化，
///    烘焙进去就要求每帧重编码），改由这个矩形在绘制期裁剪（见 `FramePayload::scroll_clip`）。
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
/// 合并规则（**每一条都是「后写胜出」**，与 `FramePayload::absorb` 同构）：
/// - lane：同 id 以**新**为准；旧 delta 里新 delta 没有的 lane 必须保留（否则那段变化永远丢），
///   但**被新 delta 释放的 lane 不得复活**（旧内容 + 新移除 = 复活出一个不在绘制序里的 lane）；
/// - removed：并集减去「新 delta 里重新出现」的 lane；旧的移除不能被新的内容覆盖掉；
/// - order：新 delta 有就用新的，否则保留旧的（顺序变化不能丢）；
/// - scroll：同槽以**新**为准（旧 delta 的同槽偏移必须丢弃 —— 否则连续覆盖时偏移冻结在最旧值）；
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
    // 旧 lane：新 delta 里同 id 的版本更新 ⇒ 直接用新的；被新 delta 移除的 ⇒ 不保留。
    if !lanes.is_empty() {
        let mut kept: Vec<LaneDelta> = Vec::with_capacity(lanes.len());
        for old in lanes {
            if !new.lanes.iter().any(|lane| lane.lane == old.lane)
                && !new.removed.contains(&old.lane)
            {
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
    // 滚动：同槽**以新为准**。旧的同槽条目必须丢掉，否则它会被追加到新条目之后，把偏移
    // 冻结在旧值上（旧实现正是如此：`new.scroll.retain(..) + extend(old)`）。
    if !scroll.is_empty() {
        let mut kept: Vec<ScrollDelta> = Vec::with_capacity(scroll.len());
        for old in scroll {
            if !new.scroll.iter().any(|fresh| fresh.slot == old.slot) {
                kept.push(old);
            }
        }
        new.scroll.reserve(kept.len());
        new.scroll.extend(kept);
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
    /// 上次发布出去的滚动槽屏幕裁剪区（按槽位；视口变化也必须过线，否则 GPU 侧 scissor 会陈旧）。
    scroll_viewports: Vec<PhysicalRect>,
    /// 上次发布出去的几何描述（按 id）。
    geometries: ahash::HashMap<GeometryId, Arc<crate::paint::geometry::GeometryDesc>>,
    /// 上次发布时的 DPI（变化 ⇒ 物理坐标全变 ⇒ 整帧重建）。
    scale: Option<Scale>,
    /// 下一帧必须整帧发布（**消费端重置**：渲染器重建后 GPU 侧的实例镜像已经清空）。
    ///
    /// 由 `Window` 在观察到 `FrameSink::reset_epoch` 变化（设备恢复 / 新渲染器挂上）时置位，
    /// 由 [`DeltaBuilder::full_for`] 消费一次。没有这个握手，恢复后的第一帧只会发「变化 lane」，
    /// 镜像里其余段是 0 ⇒ 退化实例。
    force_full: bool,
    /// `Scene` 脏表的复用缓冲（跨帧保容量 ⇒ 增量帧的采集零分配）。
    dirty: Vec<LaneId>,
    /// 回退全量扫描的帧数（全量帧 / 脏表不完整）。**回归防线**：稳态增量帧必须保持 0 增长。
    #[cfg(test)]
    full_scans: u32,
    /// 本帧真正查过版本的 lane 数（`full_scans` 的姊妹指标：脏 1 条 ⇒ 只查 1 条）。
    #[cfg(test)]
    visited_lanes: usize,
}

impl DeltaBuilder {
    pub(crate) fn new() -> Self {
        Self {
            versions: Vec::new(),
            order: Arc::from(Vec::new()),
            scroll: Vec::new(),
            scroll_viewports: Vec::new(),
            geometries: ahash::HashMap::default(),
            scale: None,
            force_full: false,
            dirty: Vec::new(),
            #[cfg(test)]
            full_scans: 0,
            #[cfg(test)]
            visited_lanes: 0,
        }
    }

    /// 回退全量扫描的累计帧数（见 [`DeltaBuilder::collect`]）。
    ///
    /// 只有三种情况会增长：全量帧（首帧 / DPI 变化）、`Scene` 脏表不完整（`clear_frame`）、
    /// 以及显式要求整帧发布的帧。稳态增量帧必须**不再增长** —— 超过预期就说明 UI 侧的
    /// 「只发布变化」退化成了「每帧扫 10k 条 lane」。
    #[cfg(test)]
    pub(crate) fn full_scans(&self) -> u32 {
        self.full_scans
    }

    /// 上一次 `collect` 查过版本的 lane 数（脏 1 条时必须是 1，而不是 lane 总数）。
    #[cfg(test)]
    pub(crate) fn visited_lanes(&self) -> usize {
        self.visited_lanes
    }

    /// 本帧是否必须整帧重建（DPI 变化：物理坐标、缩放后的圆角与 clip 全部变了）。
    ///
    /// 只在 `true` 时清缓存；第一帧也返回 `true`（合成侧还没有任何 lane）。
    /// **消费端重置**（[`DeltaBuilder::force_full`]）也在这里被取走一次。
    pub(crate) fn full_for(&mut self, scale: Scale) -> bool {
        let forced = std::mem::take(&mut self.force_full);
        let full = forced || self.scale != Some(scale);
        self.scale = Some(scale);
        full
    }

    /// 要求**下一帧整帧发布**（消费端（GPU 侧实例镜像）已经重建/清空）。
    ///
    /// 触发源是 [`crate::render::frame::FrameSink::reset_epoch`] 的变化（设备恢复等）：增量载荷只会
    /// 填满「变化」的那些 lane 段，而镜像里其余段已经被重置成 0 —— 不整帧重发就会画成退化实例。
    /// 这一帧同时必须带全量 damage（`Window` 会把 `full_damage_pending` 置位）。
    pub(crate) fn force_full(&mut self) {
        self.force_full = true;
    }

    /// 下一帧是否已被要求整帧发布（仅测试：验证信号被置位/消费一次）。
    #[cfg(test)]
    pub(crate) fn forced_full(&self) -> bool {
        self.force_full
    }

    /// 采集本帧的 lane 变化 + 绘制序 + 几何描述。
    ///
    /// **成本模型**：稳态增量帧只遍历 [`Scene::take_dirty_lanes`] 的脏表（paint pass 这一帧真正
    /// 重建/清空过的 lane，通常是 1 条），而不是绘制序里的全部 lane（10k）。
    ///
    /// **回退**：`full`（全量帧必须发布全部 lane 内容）或 `Scene` 的脏表不完整（`clear_frame`
    /// 这类全表复位）时扫全部 lane，并记一次 [`DeltaBuilder::full_scans`]。
    ///
    /// `scroll` 是**本帧全量**的滚动表（槽位 → 逻辑偏移），只有变化的槽进入 delta；
    /// `viewports` 是槽位 → 物理可见区（滚动 damage 用）。
    pub(crate) fn collect(
        &mut self,
        scene: &mut Scene,
        store: &GeometryStore,
        scroll: &[(Point, PhysicalRect)],
        full: bool,
        delta: &mut SceneDelta,
    ) {
        // 整帧重建的**唯一**置真点：`full` 同时意味着「本 delta 带全部 lane 内容」，因此消费端
        // （`LaneStore` / 编码器 / `InstanceMirror`）可以放心重置。缺这一行 ⇒ `delta.full` 是死旗，
        // 设备恢复后镜像永远缺内容（复审 AR4 P1-1 / P2-2）。
        delta.full = full;
        if full {
            self.versions.clear();
            self.order = Arc::from(Vec::new());
            self.scroll.clear();
            self.scroll_viewports.clear();
            self.geometries.clear();
        }
        // 脏表在任何分支之前取走：`full` 帧也要清空它，否则条目会留到下一帧。
        let mut dirty = std::mem::take(&mut self.dirty);
        let complete = scene.take_dirty_lanes(&mut dirty);
        // 绘制序：`set_order` 在顺序不变时不会重写，因此逐项判等是唯一判据（O(lanes) 比较，
        // 但不构造/不拷贝；顺序真的变化时才分配一次）。
        let order = scene.order();
        if full || !same_order(&self.order, order) {
            delta.order = Some(Arc::from(order));
            self.order = delta.order.clone().expect("刚刚写入的绘制序必须存在");
        }
        if self.versions.len() < scene.lane_count() {
            self.versions.resize(scene.lane_count(), (0, 0));
        }
        #[cfg(test)]
        {
            self.visited_lanes = 0;
        }
        if full || !complete {
            // 回退全量扫描：全量帧必须发布每条 lane 的完整内容；脏表不可信时必须自己找变化。
            #[cfg(test)]
            {
                self.full_scans += 1;
            }
            for lane in order.iter().copied() {
                self.collect_lane(scene, store, delta, lane, full);
            }
        } else if !dirty.is_empty() {
            for lane in dirty.iter().copied() {
                self.collect_lane(scene, store, delta, lane, false);
            }
        }
        // 脏表缓冲交给 builder 继续持有（下一帧复用同一块容量）。
        self.dirty = dirty;
        // 滚动：只有变化的槽进入 delta（滚动帧因此不触碰任何 lane）。
        if self.scroll.len() < scroll.len() {
            self.scroll
                .resize(scroll.len(), Point::new(f32::NAN, f32::NAN));
            self.scroll_viewports.resize(
                scroll.len(),
                PhysicalRect::new(i32::MIN, i32::MIN, i32::MIN, i32::MIN),
            );
        }
        for (slot, (offset, viewport)) in scroll.iter().enumerate() {
            // 偏移**或屏幕裁剪区**变化都要过线：后者决定 GPU 侧该槽全部 op 的 scissor，
            // 漏发会让窗口 resize / 祖先 clip 变化之后滚动内容被裁在旧矩形里。
            if full || self.scroll[slot] != *offset || self.scroll_viewports[slot] != *viewport {
                delta.scroll.push(ScrollDelta {
                    slot: slot as u32,
                    offset: *offset,
                    viewport: *viewport,
                });
                self.scroll[slot] = *offset;
                self.scroll_viewports[slot] = *viewport;
            }
        }
    }

    /// 单条 lane 的采集（脏表与全量扫描共用同一个判据）。
    fn collect_lane(
        &mut self,
        scene: &Scene,
        store: &GeometryStore,
        delta: &mut SceneDelta,
        lane: LaneId,
        full: bool,
    ) {
        let slot = lane.slot() as usize;
        #[cfg(test)]
        {
            self.visited_lanes += 1;
        }
        let Some((version, primitives)) = scene.lane_state(lane) else {
            return;
        };
        if !full
            && slot < self.versions.len()
            && self.versions[slot] == (lane.generation(), version)
        {
            return;
        }
        // 内容变了：重建快照（`Arc<[Primitive]>` 只在**变化**的 lane 上分配一次；
        // 未变化的 lane 根本不进 delta，因此稳态零分配、零拷贝）。
        self.collect_geometries(primitives, store, delta);
        delta.lanes.push(LaneDelta {
            lane,
            version,
            scroll: scene.lane_scroll(lane),
            primitives: Arc::from(primitives.to_vec()),
        });
        if slot >= self.versions.len() {
            self.versions.resize(slot + 1, (0, 0));
        }
        self.versions[slot] = (lane.generation(), version);
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
    /// 上次 [`LaneStore::apply`] 里内容**真的变了**的 lane（编码器快路径的输入）。
    changed: Vec<LaneId>,
    /// 上次 `apply` 期间**除 lane 内容之外**的一切都没变（绘制序 / lane 集合 / 全量重置）。
    ///
    /// 只有 `true` 时 `changed` 才是可信的「本帧全部变化」：编码器据此跳过 10k 条 lane 的
    /// 扫描并沿用上一帧的 op 表。绘制序或 lane 集合一变就置 `false`（那些变化会让 op 表失效，
    /// 必须回退全量扫描）。
    fast: bool,
}

struct StoredLane {
    generation: u32,
    version: u32,
    /// 该 lane 的图元烘焙在哪个滚动槽的内容空间（见 [`ScrollTable`]）。
    scroll: u32,
    primitives: Arc<[Primitive]>,
}

impl LaneStore {
    pub(crate) fn new() -> Self {
        Self {
            lanes: Vec::new(),
            order: Vec::new(),
            changed: Vec::new(),
            fast: false,
        }
    }

    /// 应用一条 delta（顺序 = 旧 → 新由调用方保证）。
    ///
    /// 返回 `true` = lane 表真的变了（调用方据此判断是否需要重编码）。版本相同的重复 delta
    /// （覆盖帧合并/重发）不会改变表，因此是幂等的。
    pub(crate) fn apply(&mut self, delta: &SceneDelta) -> bool {
        self.changed.clear();
        self.fast = !delta.full;
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
            // 绘制序变了：op 表（每条 op 的段区间与顺序）必须重建。
            self.fast = false;
        }
        for removed in &delta.removed {
            let slot = removed.slot() as usize;
            if let Some(stored) = self.lanes.get_mut(slot).and_then(Option::take)
                && stored.generation == removed.generation()
            {
                changed = true;
                // 段会被编码器回收 ⇒ 段布局变化 ⇒ op 表必须重建。
                self.fast = false;
                // 同时把已释放的 lane 从绘制序里摘掉：编码器的快路径依赖「绘制序里全是活 lane」
                // 这一不变量（否则它算出的复用数会偏）。下一帧 UI 侧会带来新绘制序。
                self.order.retain(|lane| *lane != *removed);
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
            // arena 槽位被新 lane 复用（代际变化）⇒ 旧段作废 ⇒ 段布局会变。
            if self.lanes[slot]
                .as_ref()
                .is_some_and(|stored| stored.generation != lane.lane.generation())
            {
                self.fast = false;
            }
            self.lanes[slot] = Some(StoredLane {
                generation: lane.lane.generation(),
                version: lane.version,
                scroll: lane.scroll,
                primitives: Arc::clone(&lane.primitives),
            });
            self.changed.push(lane.lane);
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

    fn lane_scroll(&self, id: LaneId) -> u32 {
        self.lanes
            .get(id.slot() as usize)
            .and_then(Option::as_ref)
            .filter(|stored| stored.generation == id.generation())
            .map_or(0, |stored| stored.scroll)
    }

    fn order(&self) -> &[LaneId] {
        &self.order
    }

    fn changed_lanes(&self) -> Option<&[LaneId]> {
        self.fast.then_some(self.changed.as_slice())
    }
}

impl LaneSource for Scene {
    fn lane_state(&self, id: LaneId) -> Option<(u32, &[Primitive])> {
        Scene::lane_state(self, id)
    }

    fn lane_scroll(&self, id: LaneId) -> u32 {
        Scene::lane_scroll(self, id)
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

/// 滚动槽的偏移表（逻辑像素，按槽位索引；合成线程唯一消费点）。
///
/// **坐标系约定**：`offset` 是滚动容器的**逻辑滚动位移**（`NodeInteraction::ScrollSlot::offset`
/// 的同一个值：正数 = 内容向负方向移动）。图元烘焙在「容器内容空间」时**不含**这个位移，
/// 因此绘制期要加的量是 `−offset × scale`（`physical_offset` 的返回值，物理像素）：
///
/// ```text
/// 屏幕坐标 = 烘焙坐标 + physical_offset(slot)        （GPU：world_px + frame.params.yz）
/// 烘焙坐标 = 布局坐标 − 祖先滚动位移                  （paint pass：不含本槽自己的位移）
/// ```
///
/// 于是滚动本槽**不改变任何烘焙字节**（不重编码），而裁剪链与图元同处烘焙空间 ⇒ 两者
/// 天然一致（视口未被平移，内容按位移滑动）。
#[derive(Default)]
pub(crate) struct ScrollTable {
    offsets: Vec<Point>,
    /// 每个槽的**屏幕裁剪区**（物理像素；GPU 侧每 op scissor 的来源）。
    viewports: Vec<PhysicalRect>,
    changed: Vec<u32>,
}

impl ScrollTable {
    pub(crate) fn new() -> Self {
        Self {
            offsets: Vec::new(),
            viewports: Vec::new(),
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
                self.viewports
                    .resize(slot + 1, PhysicalRect::new(0, 0, 0, 0));
            }
            if self.offsets[slot] != delta.offset {
                self.offsets[slot] = delta.offset;
                self.changed.push(delta.slot);
            }
            // 视口即使没变便宜地覆盖写（它在 payload 里每帧全量过线）。
            self.viewports[slot] = delta.viewport;
            let _ = scale;
        }
        &self.changed
    }

    /// 槽位的**绘制期平移量**（物理像素）= `−offset × scale`（槽位不存在 ⇒ 0）。
    ///
    /// 符号不能反：`scroll_to(0, +1)` 表示「内容上移 1 逻辑像素」，GPU 必须把顶点往**上**
    /// 平移（`−scale` 物理像素）。旧实现直接返回 `+offset × scale`（复审 AR4 P1-3 的潜伏 bug：
    /// 当时 `scroll_slots` 恒空、槽位从未被消费，因此从未暴露）。
    pub(crate) fn physical_offset(&self, slot: u32, scale: Scale) -> [f32; 2] {
        let offset = self
            .offsets
            .get(slot as usize)
            .copied()
            .unwrap_or_else(|| Point::new(0.0, 0.0));
        let factor = scale.factor();
        [-offset.x * factor, -offset.y * factor]
    }

    /// GPU 侧的偏移表（`FramePayload` 的载荷之一）。
    pub(crate) fn to_payload(&self, scale: Scale, out: &mut Vec<[f32; 2]>) {
        out.clear();
        out.extend((0..self.offsets.len() as u32).map(|slot| self.physical_offset(slot, scale)));
    }

    /// GPU 侧的**每槽屏幕裁剪区**（`[x0, y0, x1, y1]`，物理像素；槽位越界 ⇒ 空表项不追加）。
    ///
    /// 消费者：三个后端的 `draw_ops` —— 滚动槽的 op 在绘制前把 scissor 收窄到这个矩形。
    pub(crate) fn viewports_to_payload(&self, out: &mut Vec<[i32; 4]>) {
        out.clear();
        out.extend(
            self.viewports
                .iter()
                .map(|viewport| [viewport.x0, viewport.y0, viewport.x1, viewport.y1]),
        );
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
            scroll: 0,
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
        builder.collect(&mut scene, &geometry, &[], true, &mut delta);
        assert_eq!(delta.lanes.len(), 2, "全量帧必须带全部 lane");

        // 只改一条 lane：delta 里只有它。
        assert!(scene.push(second, quad()));
        let mut delta = SceneDelta::empty(header(2, Plan::Partial(DamageRects::new())));
        builder.collect(&mut scene, &geometry, &[], false, &mut delta);
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
            &mut scene,
            &geometry,
            &[(Point::new(0.0, 0.0), viewport)],
            true,
            &mut first,
        );
        let mut scrolled = SceneDelta::empty(header(2, Plan::Idle));
        builder.collect(
            &mut scene,
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

    /// **回归防线（UI 线程不扫全表）**：增量帧的采集只遍历 `Scene` 的脏表（paint pass 这一帧
    /// 真正改过的 lane），而不是绘制序里的全部 10k 条；回退全量扫描必须可观测且只在
    /// 「必须发布全部 lane / 脏表不可信」时发生。
    #[test]
    fn a_one_lane_change_visits_only_the_dirty_lane() {
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
        builder.collect(&mut scene, &geometry, &[], true, &mut first);
        assert_eq!(
            builder.full_scans(),
            1,
            "首帧必须回退全量扫描（合成侧还没有任何 lane）"
        );
        assert_eq!(builder.visited_lanes(), LANES);

        // 只改一条 lane：本帧只该看它一条。
        assert!(scene.push(order[LANES / 2], quad()));
        let mut delta = SceneDelta::empty(header(2, Plan::Partial(DamageRects::new())));
        builder.collect(&mut scene, &geometry, &[], false, &mut delta);
        assert_eq!(delta.lanes.len(), 1);
        assert_eq!(
            builder.visited_lanes(),
            1,
            "脏 1 条 lane ⇒ 只查 1 条（不得扫全部 {LANES} 条）"
        );
        assert_eq!(builder.full_scans(), 1, "稳态增量帧不得回退全量扫描");
        assert!(delta.order.is_none(), "绘制序没变 ⇒ 不携带");

        // `clear_frame` 之后脏表不可信 ⇒ 回退一次全量扫描（并记数）。
        scene.clear_frame();
        let mut after_reset = SceneDelta::empty(header(3, Plan::Partial(DamageRects::new())));
        builder.collect(&mut scene, &geometry, &[], false, &mut after_reset);
        assert_eq!(builder.full_scans(), 2, "脏表不完整必须回退全量扫描");
        assert_eq!(builder.visited_lanes(), LANES);
        assert_eq!(
            after_reset.lanes.len(),
            LANES,
            "全表复位后每条 lane 的版本都变了，必须全部重新发布"
        );
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
        builder.collect(&mut scene, &geometry, &[], true, &mut first);
        assert_eq!(first.lanes.len(), LANES, "全量帧带全部 lane 快照");

        // 只改最后一条 lane：delta 里只有它，且绘制序不携带。
        assert!(scene.push(order[LANES - 1], quad()));
        let mut delta = SceneDelta::empty(header(2, Plan::Partial(DamageRects::new())));
        builder.collect(&mut scene, &geometry, &[], false, &mut delta);
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

    // ── `absorb` 的**方向**契约（复审 AR4 P1-2：旧实现让旧值胜出）──────────────

    fn scroll_delta(slot: u32, y: f32) -> ScrollDelta {
        ScrollDelta {
            slot,
            offset: Point::new(0.0, y),
            viewport: PhysicalRect::new(0, 0, 64, 64),
        }
    }

    /// **回归防线（AR4 P1-2 / 变异 M9）**：被覆盖 delta 里的滚动偏移必须被新 delta 的同槽值
    /// **覆盖**。旧实现用 `retain(新) + extend(旧)` ⇒ 合并应用得到 `−10`（最旧值），
    /// 连续快速 publish 时偏移会冻结在最旧值。
    #[test]
    fn a_covered_scroll_offset_is_superseded_by_the_newer_one() {
        let mut older = SceneDelta::empty(header(1, Plan::Idle));
        older.scroll.push(scroll_delta(0, -10.0));
        let mut newer = SceneDelta::empty(header(2, Plan::Idle));
        newer.scroll.push(scroll_delta(0, -20.0));
        absorb(older, &mut newer);
        assert_eq!(newer.scroll.len(), 1, "同槽只能留一条");
        assert_eq!(newer.scroll[0].offset.y, -20.0, "同槽以新为准");
    }

    /// **等价性**（复审 §8 验收口径）：被覆盖 delta 含 scroll 时，「合并后应用」与
    /// 「逐帧顺序应用」必须得到同一张偏移表。
    #[test]
    fn absorbing_a_covered_scroll_delta_matches_sequential_application() {
        let scale = Scale::ONE;
        let viewport = PhysicalRect::new(0, 0, 64, 64);
        // 顺序应用：−10 → −20 → −30（连续三帧快速发布，中间两帧被覆盖）。
        let mut sequential = ScrollTable::new();
        for y in [-10.0f32, -20.0, -30.0] {
            let _ = sequential.apply(
                &[ScrollDelta {
                    slot: 0,
                    offset: Point::new(0.0, y),
                    viewport,
                }],
                scale,
            );
        }
        // 合并应用：旧帧（−10）被（−20）覆盖，再被（−30）覆盖 —— 每一步都是 `absorb`。
        let mut previous = SceneDelta::empty(header(3, Plan::Idle));
        previous.scroll.push(scroll_delta(0, -10.0));
        let mut middle = SceneDelta::empty(header(4, Plan::Idle));
        middle.scroll.push(scroll_delta(0, -20.0));
        absorb(previous, &mut middle);
        let mut latest = SceneDelta::empty(header(5, Plan::Idle));
        latest.scroll.push(scroll_delta(0, -30.0));
        absorb(middle, &mut latest);

        let mut table = ScrollTable::new();
        let _ = table.apply(&latest.scroll, scale);
        assert_eq!(
            table.physical_offset(0, scale),
            sequential.physical_offset(0, scale),
            "合并路径的最终偏移必须是**最新值**（顺序应用 = −30 ⇒ 平移 +30）"
        );
        assert_eq!(table.physical_offset(0, scale), [0.0, 30.0]);
    }

    /// 旧 delta 的**不同槽**偏移必须保留（丢一帧不能丢另一个容器的滚动）。
    #[test]
    fn absorbing_keeps_covered_scroll_offsets_of_other_slots() {
        let mut older = SceneDelta::empty(header(1, Plan::Idle));
        older.scroll.push(scroll_delta(0, -10.0));
        older.scroll.push(scroll_delta(3, -7.0));
        let mut newer = SceneDelta::empty(header(2, Plan::Idle));
        newer.scroll.push(scroll_delta(0, -20.0));
        absorb(older, &mut newer);
        let slots: Vec<u32> = newer.scroll.iter().map(|scroll| scroll.slot).collect();
        assert_eq!(slots, vec![0, 3], "新槽在前、旧的不同槽在后");
        assert_eq!(newer.scroll[0].offset.y, -20.0);
        assert_eq!(newer.scroll[1].offset.y, -7.0);
    }

    /// **回归防线**：旧的「移除」不能被新 delta 的**旧内容**复活。
    ///
    /// 触发路径：帧 N 改了 lane X 的内容 → 帧 N+1 释放了 X（`note_removed`）。合并后如果
    /// 保留旧内容，`LaneStore::apply` 会先 take 再 push ⇒ 复活出一个不在绘制序里的 lane。
    #[test]
    fn a_released_lane_is_not_resurrected_by_the_covered_delta() {
        let mut scene = Scene::empty();
        let released = scene.new_lane();
        let mut older = SceneDelta::empty(header(1, Plan::Idle));
        older.lanes.push(lane(released, 2));
        let mut newer = SceneDelta::empty(header(2, Plan::Idle));
        newer.removed.push(released);
        absorb(older, &mut newer);
        assert!(
            newer.lanes.is_empty(),
            "被新帧释放的 lane 不得把旧内容并回来：{:?}",
            newer.lanes.len()
        );
        assert_eq!(newer.removed, vec![released]);
    }

    /// `order`：新 delta 有就用新的；没有则保留旧的（顺序变化不能随被覆盖帧丢掉）。
    #[test]
    fn order_follows_the_newer_delta_and_survives_without_one() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        let b = scene.new_lane();
        // 新的有 order ⇒ 用新的。
        let mut older = SceneDelta::empty(header(1, Plan::Idle));
        older.order = Some(Arc::from(vec![a, b]));
        let mut newer = SceneDelta::empty(header(2, Plan::Idle));
        newer.order = Some(Arc::from(vec![b, a]));
        absorb(older, &mut newer);
        assert_eq!(
            newer.order.as_deref(),
            Some([b, a].as_slice()),
            "新绘制序胜出"
        );
        // 新的没有 ⇒ 保留旧的。
        let mut older = SceneDelta::empty(header(3, Plan::Idle));
        older.order = Some(Arc::from(vec![a, b]));
        let mut newer = SceneDelta::empty(header(4, Plan::Idle));
        absorb(older, &mut newer);
        assert_eq!(
            newer.order.as_deref(),
            Some([a, b].as_slice()),
            "顺序变化不能被覆盖丢"
        );
    }

    /// `full`：任一为真即为真（整帧重建不能被覆盖成增量，反之亦然）。
    #[test]
    fn full_is_sticky_in_both_directions() {
        let older = {
            let mut delta = SceneDelta::empty(header(1, Plan::Idle));
            delta.full = true;
            delta
        };
        let mut newer = SceneDelta::empty(header(2, Plan::Idle));
        absorb(older, &mut newer);
        assert!(newer.full, "旧帧要求整帧重建 ⇒ 合并结果仍是 Full");
        let mut fresh = SceneDelta::empty(header(3, Plan::Idle));
        fresh.full = true;
        absorb(SceneDelta::empty(header(4, Plan::Idle)), &mut fresh);
        assert!(fresh.full, "新帧自己就是 Full 时不能被旧帧降级");
    }

    /// 几何描述：同 id 以新为准，旧的不同 id 保留。
    #[test]
    fn geometries_follow_the_newer_delta() {
        use crate::paint::geometry::{GeometryDesc, GeometryPipeline, IndexType, Topology};

        let desc = |seed: u8| GeometryDesc {
            vertex_bytes: bytes::Bytes::from(vec![seed; 12]),
            index_bytes: bytes::Bytes::from_static(&[0u8; 6]),
            vertex_stride: 12,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: Rect::new(0.0, 0.0, 8.0, 8.0),
            pipeline: GeometryPipeline::Ui,
        };
        let mut store = store();
        let first = store.insert(desc(1)).expect("几何 1");
        let second = store.insert(desc(2)).expect("几何 2");
        let shared = store.insert(desc(3)).expect("几何 3");
        // 同一个 id 的两个**不同** Arc（模拟「描述被重新注册」）：absorb 之后必须只剩新的那个。
        let older_shared = store.desc_arc(shared).expect("描述");
        let newer_shared = Arc::new((*older_shared).clone());
        let mut older = SceneDelta::empty(header(1, Plan::Idle));
        older
            .geometries
            .push((first, store.desc_arc(first).expect("描述")));
        older.geometries.push((shared, older_shared));
        let mut newer = SceneDelta::empty(header(2, Plan::Idle));
        newer
            .geometries
            .push((second, store.desc_arc(second).expect("描述")));
        newer.geometries.push((shared, Arc::clone(&newer_shared)));
        absorb(older, &mut newer);
        assert_eq!(
            newer
                .geometries
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![second, shared, first],
            "新 delta 的描述在前，旧的未覆盖项追加在后"
        );
        assert!(
            Arc::ptr_eq(&newer.geometries[1].1, &newer_shared),
            "同 id 的描述以新为准（旧的被丢弃）"
        );
    }

    /// **回归防线（消费端重置 ⇒ 生产端整帧重发）**：`force_full` 必须被下一帧消费一次，
    /// 且那一帧的 delta 真的带 `full = true`（否则 `FramePayload::full` 生产不可达）。
    #[test]
    fn a_forced_full_frame_is_published_once_with_the_full_flag() {
        let mut scene = Scene::empty();
        let first = scene.new_lane();
        let second = scene.new_lane();
        assert!(scene.push(first, quad()));
        assert!(scene.push(second, quad()));
        scene.set_order(&[first, second]);
        let mut builder = DeltaBuilder::new();
        let geometry = store();
        // 首帧本来就 full（合成侧还没有 lane）。
        let mut delta = SceneDelta::empty(header(1, Plan::Full));
        let full = builder.full_for(Scale::ONE);
        builder.collect(&mut scene, &geometry, &[], full, &mut delta);
        assert!(delta.full, "首帧必须 full");
        assert_eq!(delta.lanes.len(), 2);

        // 稳态增量帧：不是 full。
        assert!(scene.push(first, quad()));
        let mut delta = SceneDelta::empty(header(2, Plan::Idle));
        let full = builder.full_for(Scale::ONE);
        builder.collect(&mut scene, &geometry, &[], full, &mut delta);
        assert!(!delta.full);
        assert_eq!(delta.lanes.len(), 1, "只有变化的 lane");

        // 消费端重置 ⇒ 强制整帧重发（一次）。
        builder.force_full();
        assert!(builder.forced_full());
        let mut delta = SceneDelta::empty(header(3, Plan::Full));
        let full = builder.full_for(Scale::ONE);
        assert!(full, "force_full 必须让 full_for 返回 true");
        builder.collect(&mut scene, &geometry, &[], full, &mut delta);
        assert!(delta.full, "重置后的第一帧必须是 full");
        assert_eq!(delta.lanes.len(), 2, "full 帧必须带全部 lane 内容");
        // 只消费一次：下一帧回到增量。
        let mut delta = SceneDelta::empty(header(4, Plan::Idle));
        let full = builder.full_for(Scale::ONE);
        builder.collect(&mut scene, &geometry, &[], full, &mut delta);
        assert!(!delta.full);
        assert!(delta.lanes.is_empty(), "无变化帧不带 lane");
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
        builder.collect(&mut scene, &geometry, &[], true, &mut delta);
        let mut applied = SceneDelta::empty(header(2, Plan::Idle));
        applied.lanes = delta.lanes.clone();
        applied.order = delta.order.clone();
        applied.full = false;
        let mut store = LaneStore::new();
        assert!(store.apply(&delta));
        assert!(!store.apply(&applied), "版本一致 ⇒ 不改变 lane 表");
        assert_eq!(store.len(), 1);
    }

    /// 滚动表按 scale 换算成**绘制期平移量**（`−offset × scale`），并原样进载荷。
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
        assert_eq!(
            table.physical_offset(0, scale),
            [0.0, 20.0],
            "offset 是「内容位移」，绘制期要加的是它的相反数（−(−10)×2 = +20）"
        );
        let mut out = Vec::new();
        table.to_payload(scale, &mut out);
        assert_eq!(out, vec![[0.0, 20.0]]);
        // 重复应用同样的偏移：不算「变化」（滚动帧的 damage 因此不重复补）。
        assert!(table.apply(&deltas, scale).is_empty());
    }

    /// 滚动槽的**屏幕裁剪区**必须随载荷过线（GPU 侧每 op scissor 的唯一来源）。
    #[test]
    fn the_scroll_table_carries_the_screen_clip_of_every_slot() {
        let mut table = ScrollTable::new();
        let viewport = PhysicalRect::new(10, 20, 110, 220);
        let _ = table.apply(
            &[ScrollDelta {
                slot: 1,
                offset: Point::new(0.0, 5.0),
                viewport,
            }],
            Scale::ONE,
        );
        let mut out = Vec::new();
        table.viewports_to_payload(&mut out);
        assert_eq!(
            out,
            vec![[0, 0, 0, 0], [10, 20, 110, 220]],
            "槽位 0 是哨兵（零面积），槽位 1 = 容器视口"
        );
    }

    /// **视口变化本身也要过线**：窗口 resize / 祖先 clip 变化时偏移可能没变，但 GPU 侧
    /// scissor 必须更新（否则滚动内容被裁在旧矩形里）。
    #[test]
    fn a_viewport_change_alone_is_published() {
        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(lane, quad()));
        scene.set_order(&[lane]);
        let geometry = store();
        let mut builder = DeltaBuilder::new();
        let first_viewport = PhysicalRect::new(0, 0, 64, 64);
        let mut delta = SceneDelta::empty(header(1, Plan::Full));
        builder.collect(
            &mut scene,
            &geometry,
            &[(Point::new(0.0, 0.0), first_viewport)],
            true,
            &mut delta,
        );
        let resized = PhysicalRect::new(0, 0, 64, 40);
        let mut delta = SceneDelta::empty(header(2, Plan::Idle));
        builder.collect(
            &mut scene,
            &geometry,
            &[(Point::new(0.0, 0.0), resized)],
            false,
            &mut delta,
        );
        assert_eq!(delta.scroll.len(), 1, "只有视口变了，偏移没变");
        assert_eq!(delta.scroll[0].viewport, resized);
    }

    /// **方向语义**（复审 AR4 P1-3 的潜伏 bug）：`scroll_to(0, +1)`（内容上移 1 逻辑像素）
    /// ⇒ 绘制期平移量是 `−scale`，不是 `+scale`。
    #[test]
    fn scrolling_down_moves_baked_content_up() {
        let mut table = ScrollTable::new();
        let scale = Scale::new(1.5).expect("scale");
        let _ = table.apply(
            &[ScrollDelta {
                slot: 1,
                offset: Point::new(0.0, 1.0),
                viewport: PhysicalRect::new(0, 0, 10, 10),
            }],
            scale,
        );
        assert_eq!(table.physical_offset(1, scale), [0.0, -1.5]);
        assert_eq!(table.physical_offset(0, scale), [0.0, 0.0], "槽 0 = 不平移");
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
        builder.collect(&mut scene, &geometry, &[], true, &mut delta);
        assert_eq!(delta.geometries.len(), 1);
        assert_eq!(delta.geometries[0].0, id);
        // 同一 lane 再变一次（内容相同但版本 +1）：描述已经在缓存里，不重复携带。
        assert!(scene.push(lane, quad()));
        let mut again = SceneDelta::empty(header(2, Plan::Full));
        builder.collect(&mut scene, &geometry, &[], false, &mut again);
        assert!(again.geometries.is_empty(), "描述已缓存 ⇒ 不重复过线");
        let mut catalog = GeometryCatalog::new();
        catalog.apply(&delta);
        assert!(catalog.desc_arc(id).is_some());
    }
}

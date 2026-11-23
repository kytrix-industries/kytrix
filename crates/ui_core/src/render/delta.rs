use std::{sync::Arc, time::Instant};

#[cfg(target_os = "linux")]
use crate::math::LogicalSize;
use crate::{
    math::{DevicePixelRatio, DeviceRect, LogicalPoint, dp, px},
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

#[derive(Clone)]
pub(crate) struct LaneDelta {
    pub lane: LaneId,
    pub version: u32,
    pub scroll: u32,
    pub geometry_chunk: Option<(GeometryId, u32)>,
    pub primitives: Arc<[Primitive]>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScrollDelta {
    pub slot: u32,
    pub offset: LogicalPoint,
    pub viewport: DeviceRect,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameHeader {
    pub window: WindowId,
    pub index: FrameIndex,
    pub plan: Plan,
    pub surface: (u32, u32),
    pub scale: DevicePixelRatio,
    #[cfg(target_os = "linux")]
    pub surface_local: LogicalSize,
    #[cfg(target_os = "linux")]
    pub buffer_scale: i32,
    pub published_at: Instant,
}

pub(crate) struct SceneDelta {
    pub header: FrameHeader,
    pub lanes: Vec<LaneDelta>,
    pub removed: Vec<LaneId>,
    pub order: Option<Arc<[LaneId]>>,
    pub scroll: Vec<ScrollDelta>,
    pub geometries: Vec<(GeometryId, Arc<crate::paint::geometry::GeometrySnapshot>)>,
    pub full: bool,
    pub image_layers: u32,
}

impl SceneDelta {
    pub(crate) fn empty(header: FrameHeader) -> Self {
        Self {
            header,
            lanes: Vec::with_capacity(0),
            removed: Vec::with_capacity(0),
            order: None,
            scroll: Vec::with_capacity(0),
            geometries: Vec::with_capacity(0),
            full: false,
            image_layers: u32::MAX,
        }
    }
}

pub(crate) fn absorb(old: SceneDelta, new: &mut SceneDelta) {
    let SceneDelta {
        header,
        lanes,
        removed,
        order,
        scroll,
        geometries,
        full,
        ..
    } = old;
    new.header.plan = header.plan.merge(new.header.plan);
    new.full |= full;
    if new.order.is_none() {
        new.order = order;
    }
    if !lanes.is_empty() {
        let mut kept: Vec<LaneDelta> = Vec::with_capacity(lanes.len());
        for old in lanes {
            let slot = old.lane.slot();
            if !new.lanes.iter().any(|lane| lane.lane.slot() == slot)
                && !new.removed.iter().any(|removed| removed.slot() == slot)
            {
                kept.push(old);
            }
        }
        new.lanes.reserve(kept.len());
        new.lanes.extend(kept);
    }
    if !removed.is_empty() {
        for lane in removed {
            let slot = lane.slot();
            if !new.removed.iter().any(|removed| removed.slot() == slot)
                && !new.lanes.iter().any(|fresh| fresh.lane.slot() == slot)
            {
                new.removed.push(lane);
            }
        }
    }
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
        let mut kept: Vec<(GeometryId, Arc<crate::paint::geometry::GeometrySnapshot>)> =
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

pub(crate) struct DeltaBuilder {
    versions: Vec<(u32, u32)>,
    order: Arc<[LaneId]>,
    scroll: Vec<LogicalPoint>,
    scroll_viewports: Vec<DeviceRect>,
    geometries: ahash::HashMap<GeometryId, Arc<crate::paint::geometry::GeometrySnapshot>>,
    scale: Option<DevicePixelRatio>,
    force_full: bool,
    dirty: Vec<LaneId>,
    #[cfg(test)]
    full_scans: u32,
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

    #[cfg(test)]
    pub(crate) fn full_scans(&self) -> u32 {
        self.full_scans
    }

    #[cfg(test)]
    pub(crate) fn visited_lanes(&self) -> usize {
        self.visited_lanes
    }

    pub(crate) fn full_for(&mut self, scale: DevicePixelRatio) -> bool {
        let forced = std::mem::take(&mut self.force_full);
        let full = forced || self.scale != Some(scale);
        self.scale = Some(scale);
        full
    }

    pub(crate) fn force_full(&mut self) {
        self.force_full = true;
    }

    #[cfg(test)]
    pub(crate) fn forced_full(&self) -> bool {
        self.force_full
    }

    pub(crate) fn collect(
        &mut self,
        scene: &mut Scene,
        store: &GeometryStore,
        scroll: &[(LogicalPoint, DeviceRect)],
        full: bool,
        delta: &mut SceneDelta,
    ) {
        delta.full = full;
        if full {
            self.versions.clear();
            self.order = Arc::from(Vec::new());
            self.scroll.clear();
            self.scroll_viewports.clear();
            self.geometries.clear();
        }
        let mut dirty = std::mem::take(&mut self.dirty);
        let complete = scene.take_dirty_lanes(&mut dirty);
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
        self.dirty = dirty;
        if self.scroll.len() < scroll.len() {
            self.scroll
                .resize(scroll.len(), LogicalPoint::new(px(f32::NAN), px(f32::NAN)));
            self.scroll_viewports.resize(
                scroll.len(),
                DeviceRect::new(dp(i32::MIN), dp(i32::MIN), dp(i32::MIN), dp(i32::MIN)),
            );
        }
        for (slot, (offset, viewport)) in scroll.iter().enumerate() {
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
        self.collect_geometries(primitives, store, delta);
        delta.lanes.push(LaneDelta {
            lane,
            version,
            scroll: scene.lane_scroll(lane),
            geometry_chunk: scene.lane_geometry_chunk(lane),
            primitives: Arc::from(primitives.to_vec()),
        });
        if slot >= self.versions.len() {
            self.versions.resize(slot + 1, (0, 0));
        }
        self.versions[slot] = (lane.generation(), version);
    }

    fn collect_geometries(
        &mut self,
        primitives: &[Primitive],
        store: &GeometryStore,
        delta: &mut SceneDelta,
    ) {
        for primitive in primitives {
            match primitive {
                Primitive::Geometry(draw) => self.note_geometry(draw.id, store, delta),
                Primitive::Path(path) => self.note_geometry(path.geometry, store, delta),
                Primitive::Canvas(canvas) => {
                    for inner in canvas.primitives() {
                        match inner {
                            Primitive::Geometry(draw) => {
                                self.note_geometry(draw.id, store, delta);
                            }
                            Primitive::Path(path) => {
                                self.note_geometry(path.geometry, store, delta);
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn note_geometry(&mut self, id: GeometryId, store: &GeometryStore, delta: &mut SceneDelta) {
        let Some(snapshot) = store.snapshot_arc(id) else {
            return;
        };
        let known = self
            .geometries
            .get(&id)
            .is_some_and(|cached| Arc::ptr_eq(cached, &snapshot));
        if known {
            return;
        }
        self.geometries.insert(id, Arc::clone(&snapshot));
        delta.geometries.push((id, snapshot));
    }

    pub(crate) fn note_removed(&mut self, lane: LaneId, delta: &mut SceneDelta) {
        let slot = lane.slot() as usize;
        if slot < self.versions.len() {
            self.versions[slot] = (0, 0);
        }
        delta.removed.push(lane);
    }
}

fn same_order(previous: &[LaneId], current: &[LaneId]) -> bool {
    previous.len() == current.len() && previous.iter().zip(current).all(|(a, b)| a == b)
}

#[derive(Default)]
pub(crate) struct LaneStore {
    lanes: Vec<Option<StoredLane>>,
    order: Vec<LaneId>,
    changed: Vec<LaneId>,
    fast: bool,
}

struct StoredLane {
    generation: u32,
    version: u32,
    scroll: u32,
    geometry_chunk: Option<(GeometryId, u32)>,
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
            self.fast = false;
        }
        for removed in &delta.removed {
            let slot = removed.slot() as usize;
            if let Some(stored) = self.lanes.get_mut(slot).and_then(Option::take)
                && stored.generation == removed.generation()
            {
                changed = true;
                self.fast = false;
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
                geometry_chunk: lane.geometry_chunk,
                primitives: Arc::clone(&lane.primitives),
            });
            self.changed.push(lane.lane);
            changed = true;
        }
        changed
    }

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

    fn lane_geometry_chunk(&self, id: LaneId) -> Option<(GeometryId, u32)> {
        self.lanes
            .get(id.slot() as usize)
            .and_then(Option::as_ref)
            .filter(|stored| stored.generation == id.generation())
            .and_then(|stored| stored.geometry_chunk)
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

    fn lane_geometry_chunk(&self, id: LaneId) -> Option<(GeometryId, u32)> {
        Scene::lane_geometry_chunk(self, id)
    }

    fn order(&self) -> &[LaneId] {
        Scene::order(self)
    }
}

#[derive(Default)]
pub(crate) struct GeometryCatalog {
    entries: ahash::HashMap<GeometryId, Arc<crate::paint::geometry::GeometrySnapshot>>,
}

impl GeometryCatalog {
    pub(crate) fn new() -> Self {
        Self {
            entries: ahash::HashMap::default(),
        }
    }

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
    fn snapshot_arc(
        &self,
        id: GeometryId,
    ) -> Option<Arc<crate::paint::geometry::GeometrySnapshot>> {
        self.entries.get(&id).map(Arc::clone)
    }
}

#[derive(Default)]
pub(crate) struct ScrollTable {
    offsets: Vec<LogicalPoint>,
    viewports: Vec<DeviceRect>,
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

    pub(crate) fn apply(&mut self, deltas: &[ScrollDelta], scale: DevicePixelRatio) -> &[u32] {
        self.changed.clear();
        for delta in deltas {
            let slot = delta.slot as usize;
            if self.offsets.len() <= slot {
                self.offsets
                    .resize(slot + 1, LogicalPoint::new(px(0.0), px(0.0)));
                self.viewports
                    .resize(slot + 1, DeviceRect::new(dp(0), dp(0), dp(0), dp(0)));
            }
            if self.offsets[slot] != delta.offset {
                self.offsets[slot] = delta.offset;
                self.changed.push(delta.slot);
            }
            self.viewports[slot] = delta.viewport;
            let _ = scale;
        }
        &self.changed
    }

    pub(crate) fn physical_offset(&self, slot: u32, scale: DevicePixelRatio) -> [f32; 2] {
        let offset = self
            .offsets
            .get(slot as usize)
            .copied()
            .unwrap_or_else(|| LogicalPoint::new(px(0.0), px(0.0)));
        let factor = scale.factor();
        [(-offset.x * factor).value(), (-offset.y * factor).value()]
    }

    pub(crate) fn to_payload(&self, scale: DevicePixelRatio, out: &mut Vec<[f32; 2]>) {
        out.clear();
        out.extend((0..self.offsets.len() as u32).map(|slot| self.physical_offset(slot, scale)));
    }

    pub(crate) fn viewports_to_payload(&self, out: &mut Vec<[i32; 4]>) {
        out.clear();
        out.extend(self.viewports.iter().map(|viewport| {
            [
                viewport.x0.value(),
                viewport.y0.value(),
                viewport.x1.value(),
                viewport.y1.value(),
            ]
        }));
    }
}

pub(crate) fn scroll_damage(viewport: DeviceRect) -> Option<DeviceRect> {
    let width = viewport.x1 - viewport.x0;
    let height = viewport.y1 - viewport.y0;
    (width > 0 && height > 0).then_some(viewport)
}

pub(crate) fn push_scroll_damage(plan: Plan, viewports: &[DeviceRect]) -> Plan {
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
    use crate::math::LogicalSize;
    use crate::{
        math::{DevicePixelRatio, DeviceRect, LogicalPoint, LogicalRect, dp, px},
        paint::{
            color::Color,
            instance::{GeometryLookup, LaneSource},
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
            scale: DevicePixelRatio::ONE,
            #[cfg(target_os = "linux")]
            surface_local: LogicalSize::new(px(64.0), px(64.0)),
            #[cfg(target_os = "linux")]
            buffer_scale: 1,
            published_at: std::time::Instant::now(),
        }
    }

    fn quad() -> Primitive {
        Primitive::Quad(Quad {
            bounds: LogicalRect::new(px(0.0), px(0.0), px(4.0), px(4.0)),
            corner_radius: [px(0.0); 4],
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
            geometry_chunk: None,
            primitives: Arc::from(vec![quad()]),
        }
    }

    fn store() -> crate::paint::geometry_store::GeometryStore {
        crate::paint::geometry_store::GeometryStore::new()
    }

    fn register(
        store: &mut crate::paint::geometry_store::GeometryStore,
    ) -> crate::paint::geometry::GeometryId {
        use crate::paint::geometry::{GeometryDesc, GeometryPipeline, IndexType, Topology};
        store
            .insert(GeometryDesc {
                vertex_bytes: bytes::Bytes::from_static(&[0u8; 12]),
                index_bytes: bytes::Bytes::from_static(&[0u8; 6]),
                vertex_stride: 12,
                index_type: IndexType::U16,
                topology: Topology::TriangleList,
                bounds: LogicalRect::new(px(0.0), px(0.0), px(8.0), px(8.0)),
                pipeline: GeometryPipeline::Ui,
            })
            .expect("注册几何")
    }

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

        assert!(scene.push(second, quad()));
        let mut delta = SceneDelta::empty(header(2, Plan::Partial(DamageRects::new())));
        builder.collect(&mut scene, &geometry, &[], false, &mut delta);
        assert_eq!(delta.lanes.len(), 1);
        assert_eq!(delta.lanes[0].lane, second);
        assert!(delta.order.is_none(), "绘制序没变 ⇒ 不携带");
    }

    #[test]
    fn a_scroll_only_delta_carries_no_lane() {
        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(lane, quad()));
        scene.set_order(&[lane]);
        let geometry = store();
        let mut builder = DeltaBuilder::new();
        let viewport = DeviceRect::new(dp(0), dp(0), dp(64), dp(64));
        let mut first = SceneDelta::empty(header(1, Plan::Full));
        builder.collect(
            &mut scene,
            &geometry,
            &[(LogicalPoint::new(px(0.0), px(0.0)), viewport)],
            true,
            &mut first,
        );
        let mut scrolled = SceneDelta::empty(header(2, Plan::Idle));
        builder.collect(
            &mut scene,
            &geometry,
            &[(LogicalPoint::new(px(0.0), px(-12.0)), viewport)],
            false,
            &mut scrolled,
        );
        assert!(scrolled.lanes.is_empty(), "滚动帧不得重编码任何 lane");
        assert!(scrolled.order.is_none());
        assert_eq!(scrolled.scroll.len(), 1);
        assert_eq!(scrolled.scroll[0].offset.y, px(-12.0));
        assert!(scrolled.lanes.is_empty() && scrolled.order.is_none());
    }

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

    #[test]
    fn absorbing_a_covered_delta_keeps_every_lane() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        let b = scene.new_lane();
        let mut older = SceneDelta::empty(header(
            1,
            Plan::Partial({
                let mut rects = DamageRects::new();
                assert!(rects.push(DeviceRect::new(dp(0), dp(0), dp(8), dp(8))));
                rects
            }),
        ));
        older.lanes.push(lane(a, 1));
        let mut newer = SceneDelta::empty(header(
            2,
            Plan::Partial({
                let mut rects = DamageRects::new();
                assert!(rects.push(DeviceRect::new(dp(8), dp(8), dp(16), dp(16))));
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

    #[test]
    fn absorb_drops_an_old_generation_lane_when_the_slot_is_reused() {
        let mut scene = Scene::empty();
        let old = scene.new_lane();
        scene.release_lane(old);
        let fresh = scene.new_lane();
        assert_eq!(old.slot(), fresh.slot());
        assert_ne!(old.generation(), fresh.generation());

        let mut older = SceneDelta::empty(header(1, Plan::Idle));
        older.lanes.push(lane(old, 1));
        let mut newer = SceneDelta::empty(header(2, Plan::Idle));
        newer.lanes.push(lane(fresh, 7));
        absorb(older, &mut newer);
        assert_eq!(newer.lanes.len(), 1, "同槽只保留新代际 lane");
        assert_eq!(newer.lanes[0].lane, fresh);

        let mut store = LaneStore::new();
        assert!(store.apply(&newer));
        assert!(
            store.lane_state(fresh).is_some(),
            "新代际 lane 必须能在 LaneStore 中查到，不能被旧代际覆盖"
        );
    }

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

    fn scroll_delta(slot: u32, y: f32) -> ScrollDelta {
        ScrollDelta {
            slot,
            offset: LogicalPoint::new(px(0.0), px(y)),
            viewport: DeviceRect::new(dp(0), dp(0), dp(64), dp(64)),
        }
    }

    #[test]
    fn a_covered_scroll_offset_is_superseded_by_the_newer_one() {
        let mut older = SceneDelta::empty(header(1, Plan::Idle));
        older.scroll.push(scroll_delta(0, -10.0));
        let mut newer = SceneDelta::empty(header(2, Plan::Idle));
        newer.scroll.push(scroll_delta(0, -20.0));
        absorb(older, &mut newer);
        assert_eq!(newer.scroll.len(), 1, "同槽只能留一条");
        assert_eq!(newer.scroll[0].offset.y, px(-20.0), "同槽以新为准");
    }

    #[test]
    fn absorbing_a_covered_scroll_delta_matches_sequential_application() {
        let scale = DevicePixelRatio::ONE;
        let viewport = DeviceRect::new(dp(0), dp(0), dp(64), dp(64));
        let mut sequential = ScrollTable::new();
        for y in [-10.0f32, -20.0, -30.0] {
            let _ = sequential.apply(
                &[ScrollDelta {
                    slot: 0,
                    offset: LogicalPoint::new(px(0.0), px(y)),
                    viewport,
                }],
                scale,
            );
        }
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
        assert_eq!(newer.scroll[0].offset.y, px(-20.0));
        assert_eq!(newer.scroll[1].offset.y, px(-7.0));
    }

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

    #[test]
    fn order_follows_the_newer_delta_and_survives_without_one() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        let b = scene.new_lane();
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

    #[test]
    fn geometries_follow_the_newer_delta() {
        use crate::paint::geometry::{GeometryDesc, GeometryPipeline, IndexType, Topology};

        let desc = |seed: u8| GeometryDesc {
            vertex_bytes: bytes::Bytes::from(vec![seed; 12]),
            index_bytes: bytes::Bytes::from_static(&[0u8; 6]),
            vertex_stride: 12,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: LogicalRect::new(px(0.0), px(0.0), px(8.0), px(8.0)),
            pipeline: GeometryPipeline::Ui,
        };
        let mut store = store();
        let first = store.insert(desc(1)).expect("几何 1");
        let second = store.insert(desc(2)).expect("几何 2");
        let shared = store.insert(desc(3)).expect("几何 3");
        let older_shared = store.snapshot_arc(shared).expect("描述");
        store
            .update_vertices(shared, 0..12, &[9u8; 12])
            .expect("几何 3 的数据更新");
        let newer_shared = store.snapshot_arc(shared).expect("新描述");
        assert!(
            !Arc::ptr_eq(&older_shared, &newer_shared),
            "更新必须产出新的快照 Arc"
        );
        let mut older = SceneDelta::empty(header(1, Plan::Idle));
        older
            .geometries
            .push((first, store.snapshot_arc(first).expect("描述")));
        older.geometries.push((shared, older_shared));
        let mut newer = SceneDelta::empty(header(2, Plan::Idle));
        newer
            .geometries
            .push((second, store.snapshot_arc(second).expect("描述")));
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
        let mut delta = SceneDelta::empty(header(1, Plan::Full));
        let full = builder.full_for(DevicePixelRatio::ONE);
        builder.collect(&mut scene, &geometry, &[], full, &mut delta);
        assert!(delta.full, "首帧必须 full");
        assert_eq!(delta.lanes.len(), 2);

        assert!(scene.push(first, quad()));
        let mut delta = SceneDelta::empty(header(2, Plan::Idle));
        let full = builder.full_for(DevicePixelRatio::ONE);
        builder.collect(&mut scene, &geometry, &[], full, &mut delta);
        assert!(!delta.full);
        assert_eq!(delta.lanes.len(), 1, "只有变化的 lane");

        builder.force_full();
        assert!(builder.forced_full());
        let mut delta = SceneDelta::empty(header(3, Plan::Full));
        let full = builder.full_for(DevicePixelRatio::ONE);
        assert!(full, "force_full 必须让 full_for 返回 true");
        builder.collect(&mut scene, &geometry, &[], full, &mut delta);
        assert!(delta.full, "重置后的第一帧必须是 full");
        assert_eq!(delta.lanes.len(), 2, "full 帧必须带全部 lane 内容");
        let mut delta = SceneDelta::empty(header(4, Plan::Idle));
        let full = builder.full_for(DevicePixelRatio::ONE);
        builder.collect(&mut scene, &geometry, &[], full, &mut delta);
        assert!(!delta.full);
        assert!(delta.lanes.is_empty(), "无变化帧不带 lane");
    }

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

    #[test]
    fn the_scroll_table_converts_to_physical_offsets() {
        let mut table = ScrollTable::new();
        let scale = DevicePixelRatio::new(2.0).expect("scale");
        let deltas = [ScrollDelta {
            slot: 0,
            offset: LogicalPoint::new(px(0.0), px(-10.0)),
            viewport: DeviceRect::new(dp(0), dp(0), dp(100), dp(100)),
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
        assert!(table.apply(&deltas, scale).is_empty());
    }

    #[test]
    fn the_scroll_table_carries_the_screen_clip_of_every_slot() {
        let mut table = ScrollTable::new();
        let viewport = DeviceRect::new(dp(10), dp(20), dp(110), dp(220));
        let _ = table.apply(
            &[ScrollDelta {
                slot: 1,
                offset: LogicalPoint::new(px(0.0), px(5.0)),
                viewport,
            }],
            DevicePixelRatio::ONE,
        );
        let mut out = Vec::new();
        table.viewports_to_payload(&mut out);
        assert_eq!(
            out,
            vec![[0, 0, 0, 0], [10, 20, 110, 220]],
            "槽位 0 是哨兵（零面积），槽位 1 = 容器视口"
        );
    }

    #[test]
    fn a_viewport_change_alone_is_published() {
        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(lane, quad()));
        scene.set_order(&[lane]);
        let geometry = store();
        let mut builder = DeltaBuilder::new();
        let first_viewport = DeviceRect::new(dp(0), dp(0), dp(64), dp(64));
        let mut delta = SceneDelta::empty(header(1, Plan::Full));
        builder.collect(
            &mut scene,
            &geometry,
            &[(LogicalPoint::new(px(0.0), px(0.0)), first_viewport)],
            true,
            &mut delta,
        );
        let resized = DeviceRect::new(dp(0), dp(0), dp(64), dp(40));
        let mut delta = SceneDelta::empty(header(2, Plan::Idle));
        builder.collect(
            &mut scene,
            &geometry,
            &[(LogicalPoint::new(px(0.0), px(0.0)), resized)],
            false,
            &mut delta,
        );
        assert_eq!(delta.scroll.len(), 1, "只有视口变了，偏移没变");
        assert_eq!(delta.scroll[0].viewport, resized);
    }

    #[test]
    fn scrolling_down_moves_baked_content_up() {
        let mut table = ScrollTable::new();
        let scale = DevicePixelRatio::new(1.5).expect("scale");
        let _ = table.apply(
            &[ScrollDelta {
                slot: 1,
                offset: LogicalPoint::new(px(0.0), px(1.0)),
                viewport: DeviceRect::new(dp(0), dp(0), dp(10), dp(10)),
            }],
            scale,
        );
        assert_eq!(table.physical_offset(1, scale), [0.0, -1.5]);
        assert_eq!(table.physical_offset(0, scale), [0.0, 0.0], "槽 0 = 不平移");
    }

    #[test]
    fn scroll_damage_is_the_viewport() {
        let viewport = DeviceRect::new(dp(4), dp(5), dp(60), dp(65));
        assert_eq!(scroll_damage(viewport), Some(viewport));
        assert_eq!(
            scroll_damage(DeviceRect::new(dp(4), dp(5), dp(4), dp(65))),
            None
        );
    }

    #[test]
    fn scroll_damage_is_merged_into_the_plan() {
        let viewport = DeviceRect::new(dp(0), dp(0), dp(32), dp(32));
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
                bounds: LogicalRect::new(px(0.0), px(0.0), px(8.0), px(8.0)),
                pipeline: GeometryPipeline::Ui,
            })
            .expect("注册几何");
        let lane = scene.new_lane();
        assert!(scene.push(
            lane,
            Primitive::Geometry(GeometryDraw::full(id, Transform::IDENTITY, Color::WHITE))
        ));
        scene.set_order(&[lane]);
        let mut builder = DeltaBuilder::new();
        let mut delta = SceneDelta::empty(header(1, Plan::Full));
        builder.collect(&mut scene, &geometry, &[], true, &mut delta);
        assert_eq!(delta.geometries.len(), 1);
        assert_eq!(delta.geometries[0].0, id);
        assert!(scene.push(lane, quad()));
        let mut again = SceneDelta::empty(header(2, Plan::Full));
        builder.collect(&mut scene, &geometry, &[], false, &mut again);
        assert!(again.geometries.is_empty(), "描述已缓存 ⇒ 不重复过线");
        let mut catalog = GeometryCatalog::new();
        catalog.apply(&delta);
        assert!(catalog.snapshot_arc(id).is_some());
    }

    #[test]
    fn canvas_contents_contribute_their_geometry_descriptions() {
        use crate::{
            math::Transform,
            paint::{
                canvas::Canvas,
                geometry::GeometryId,
                primitive::{GeometryDraw, Path},
            },
        };
        let mut scene = Scene::empty();
        let mut geometry = store();
        let through_geometry = register(&mut geometry);
        let through_path = register(&mut geometry);
        let mut canvas = Canvas::new(LogicalRect::new(px(0.0), px(0.0), px(8.0), px(8.0)));
        canvas.push_geometry(GeometryDraw::full(
            through_geometry,
            Transform::IDENTITY,
            Color::WHITE,
        ));
        canvas.push_path(Path {
            bounds: LogicalRect::new(px(0.0), px(0.0), px(8.0), px(8.0)),
            geometry: through_path,
            fill: Color::WHITE,
        });
        let lane = scene.new_lane();
        assert!(scene.push(lane, Primitive::Canvas(Arc::new(canvas))));
        scene.set_order(&[lane]);

        let mut builder = DeltaBuilder::new();
        let mut delta = SceneDelta::empty(header(1, Plan::Full));
        builder.collect(&mut scene, &geometry, &[], true, &mut delta);
        let ids: Vec<GeometryId> = delta.geometries.iter().map(|(id, _)| *id).collect();
        assert!(
            ids.contains(&through_geometry),
            "Canvas 内 `GeometryDraw` 引用的几何必须进 delta.geometries：{ids:?}"
        );
        assert!(
            ids.contains(&through_path),
            "Canvas 内 `Path` 引用的几何走同一条收集路径（也必须进 delta.geometries）：{ids:?}"
        );
        assert_eq!(delta.geometries.len(), 2, "两个不同 id 各携带一次");

        assert!(scene.push(lane, quad()));
        let mut again = SceneDelta::empty(header(2, Plan::Partial(DamageRects::new())));
        builder.collect(&mut scene, &geometry, &[], false, &mut again);
        assert_eq!(again.lanes.len(), 1, "lane 内容确实变了");
        assert!(again.geometries.is_empty(), "描述已缓存 ⇒ 不重复过线");
    }

    #[test]
    fn top_level_paths_are_already_collected() {
        use crate::paint::primitive::Path;
        let mut scene = Scene::empty();
        let mut geometry = store();
        let id = register(&mut geometry);
        let lane = scene.new_lane();
        assert!(scene.push(
            lane,
            Primitive::Path(Path {
                bounds: LogicalRect::new(px(0.0), px(0.0), px(8.0), px(8.0)),
                geometry: id,
                fill: Color::WHITE,
            })
        ));
        scene.set_order(&[lane]);
        let mut builder = DeltaBuilder::new();
        let mut delta = SceneDelta::empty(header(1, Plan::Full));
        builder.collect(&mut scene, &geometry, &[], true, &mut delta);
        assert_eq!(
            delta
                .geometries
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![id]
        );
    }

    #[test]
    fn replacing_canvas_contents_stops_carrying_the_old_geometries() {
        use crate::{
            math::Transform,
            paint::{canvas::Canvas, primitive::GeometryDraw},
        };
        let mut scene = Scene::empty();
        let mut geometry = store();
        let first = register(&mut geometry);
        let second = register(&mut geometry);
        let mut canvas = Canvas::new(LogicalRect::new(px(0.0), px(0.0), px(8.0), px(8.0)));
        canvas.push_geometry(GeometryDraw::full(first, Transform::IDENTITY, Color::WHITE));
        canvas.push_geometry(GeometryDraw::full(
            second,
            Transform::IDENTITY,
            Color::WHITE,
        ));
        let lane = scene.new_lane();
        assert!(scene.push(lane, Primitive::Canvas(Arc::new(canvas))));
        scene.set_order(&[lane]);
        let mut builder = DeltaBuilder::new();
        let mut first_delta = SceneDelta::empty(header(1, Plan::Full));
        builder.collect(&mut scene, &geometry, &[], true, &mut first_delta);
        assert_eq!(first_delta.geometries.len(), 2);

        assert!(scene.clear_lane(lane));
        assert!(scene.push(
            lane,
            Primitive::Canvas(Arc::new(Canvas::new(LogicalRect::new(
                px(0.0),
                px(0.0),
                px(8.0),
                px(8.0)
            ))))
        ));
        let mut changed = SceneDelta::empty(header(2, Plan::Partial(DamageRects::new())));
        builder.collect(&mut scene, &geometry, &[], false, &mut changed);
        assert_eq!(changed.lanes.len(), 1, "lane 版本必须变化");
        assert!(
            changed.geometries.is_empty(),
            "新内容不引用几何 ⇒ 变化帧不得携带旧描述：{:?}",
            changed.geometries
        );

        let mut incremental = GeometryCatalog::new();
        incremental.apply(&first_delta);
        incremental.apply(&changed);
        for id in [first, second] {
            assert!(
                incremental.snapshot_arc(id).is_some(),
                "非 full 的 catalog 不驱逐旧描述（现有语义）"
            );
        }
        let mut full_delta = SceneDelta::empty(header(3, Plan::Full));
        builder.collect(&mut scene, &geometry, &[], true, &mut full_delta);
        assert!(full_delta.geometries.is_empty());
        let mut full = GeometryCatalog::new();
        full.apply(&first_delta);
        full.apply(&full_delta);
        for id in [first, second] {
            assert!(
                full.snapshot_arc(id).is_none(),
                "full 帧清空后旧描述不再被引用 ⇒ catalog 查不到"
            );
        }
    }
}

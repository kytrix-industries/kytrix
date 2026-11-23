use crate::{
    node::id::{Arena, ArenaKey},
    paint::primitive::{ClipOp, Primitive},
};

pub(crate) const MAX_CLIP_DEPTH: u8 = 8;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Lane {
    primitives: Vec<Primitive>,
    clip_depth: u8,
    scroll: u16,
    version: u32,
}

impl Lane {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn version(&self) -> u32 {
        self.version
    }

    pub(crate) fn push(&mut self, primitive: Primitive) {
        match &primitive {
            Primitive::Clip(ClipOp::Push(_)) => {
                self.clip_depth = self.clip_depth.saturating_add(1);
            }
            Primitive::Clip(ClipOp::Pop) => {
                debug_assert!(usize::from(self.clip_depth()) > 0);
                self.clip_depth = self.clip_depth.saturating_sub(1);
            }
            _ => {}
        }
        self.version = self.version.wrapping_add(1);
        self.primitives.push(primitive);
    }

    pub(crate) fn clear(&mut self) {
        self.primitives.clear();
        self.clip_depth = 0;
        self.version = self.version.wrapping_add(1);
    }

    pub(crate) fn clip_depth(&self) -> u8 {
        self.clip_depth
    }

    pub(crate) fn scroll(&self) -> u32 {
        u32::from(self.scroll)
    }

    pub(crate) fn set_scroll(&mut self, slot: u32) {
        let slot = u16::try_from(slot).unwrap_or(u16::MAX);
        if self.scroll != slot {
            self.scroll = slot;
            self.version = self.version.wrapping_add(1);
        }
    }

    pub(crate) fn rounded_clip_depth(&self) -> usize {
        let mut open = 0usize;
        let mut rounded = 0usize;
        for primitive in self.primitives.iter().rev() {
            match primitive {
                Primitive::Clip(ClipOp::Pop) => open += 1,
                Primitive::Clip(ClipOp::Push(clip)) => {
                    if open > 0 {
                        open -= 1;
                    } else if clip.corner_radius.iter().any(|radius| *radius > 0.0) {
                        rounded += 1;
                    }
                }
                _ => {}
            }
        }
        rounded
    }

    pub(crate) fn primitives(&self) -> &[Primitive] {
        &self.primitives
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.primitives.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LaneId {
    index: u32,
    generation: u32,
}

impl ArenaKey for LaneId {
    fn from_parts(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }

    fn parts(self) -> (u32, u32) {
        (self.index, self.generation)
    }
}

impl LaneId {
    pub(crate) const fn slot(self) -> u32 {
        self.index
    }

    pub(crate) const fn generation(self) -> u32 {
        self.generation
    }

    #[cfg(test)]
    pub(crate) const fn index(self) -> u32 {
        self.index
    }

    #[cfg(test)]
    pub(crate) const fn for_test(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }
}

#[derive(Debug)]
pub struct Scene {
    lanes: Arena<Lane, LaneId>,
    order: Vec<LaneId>,
    dirty: Vec<LaneId>,
    dirty_complete: bool,
    geometry_chunks: ahash::HashMap<LaneId, (crate::paint::geometry::GeometryId, u32)>,
}

impl Scene {
    pub(crate) fn empty() -> Self {
        Self {
            lanes: Arena::<Lane, LaneId>::new(),
            order: Vec::new(),
            dirty: Vec::new(),
            dirty_complete: true,
            geometry_chunks: ahash::HashMap::default(),
        }
    }

    pub fn new_lane(&mut self) -> LaneId {
        self.lanes.insert_with(|_| Lane::new())
    }

    pub fn release_lane(&mut self, id: LaneId) {
        let _ = self.lanes.remove(id);
        self.geometry_chunks.remove(&id);
    }

    pub(crate) fn set_lane_geometry_chunk(
        &mut self,
        lane: LaneId,
        chunk: Option<(crate::paint::geometry::GeometryId, u32)>,
    ) {
        match chunk {
            Some(chunk) => {
                self.geometry_chunks.insert(lane, chunk);
            }
            None => {
                self.geometry_chunks.remove(&lane);
            }
        }
    }

    #[must_use]
    pub(crate) fn lane_geometry_chunk(
        &self,
        lane: LaneId,
    ) -> Option<(crate::paint::geometry::GeometryId, u32)> {
        self.geometry_chunks.get(&lane).copied()
    }

    #[must_use]
    pub fn push(&mut self, lane: LaneId, primitive: Primitive) -> bool {
        match self.lanes.get_mut(lane) {
            Some(target) => {
                target.push(primitive);
                self.dirty.push(lane);
                true
            }
            None => false,
        }
    }

    pub fn clear_frame(&mut self) {
        self.geometry_chunks.clear();
        for lane in self.lanes.values_mut() {
            lane.clear();
        }
        self.dirty_complete = false;
    }

    #[must_use]
    pub fn clear_lane(&mut self, id: LaneId) -> bool {
        match self.lanes.get_mut(id) {
            Some(lane) => {
                lane.clear();
                self.dirty.push(id);
                true
            }
            None => false,
        }
    }

    pub(crate) fn take_dirty_lanes(&mut self, into: &mut Vec<LaneId>) -> bool {
        std::mem::swap(into, &mut self.dirty);
        self.dirty.clear();
        let complete = self.dirty_complete;
        self.dirty_complete = true;
        complete
    }

    pub fn set_order(&mut self, order: &[LaneId]) {
        if self.order == order {
            return;
        }
        self.order.clear();
        self.order.extend_from_slice(order);
    }

    pub fn clear_order(&mut self) {
        self.order.clear();
    }

    #[must_use]
    pub fn lane_primitives(&self, id: LaneId) -> Option<&[Primitive]> {
        self.lanes.get(id).map(Lane::primitives)
    }

    #[must_use]
    pub(crate) fn lane_state(&self, id: LaneId) -> Option<(u32, &[Primitive])> {
        self.lanes
            .get(id)
            .map(|lane| (lane.version(), lane.primitives()))
    }

    #[must_use]
    pub fn lane_count(&self) -> usize {
        self.lanes.len()
    }

    #[must_use]
    pub(crate) fn lane_scroll(&self, id: LaneId) -> u32 {
        self.lanes.get(id).map_or(0, Lane::scroll)
    }

    #[must_use]
    pub(crate) fn set_lane_scroll(&mut self, id: LaneId, slot: u32) -> bool {
        match self.lanes.get_mut(id) {
            Some(lane) => {
                let before = lane.version();
                lane.set_scroll(slot);
                if lane.version() != before {
                    self.dirty.push(id);
                }
                true
            }
            None => false,
        }
    }

    #[must_use]
    pub fn order(&self) -> &[LaneId] {
        &self.order
    }

    pub fn iter(&self) -> impl Iterator<Item = &Primitive> + Clone + '_ {
        self.order
            .iter()
            .filter_map(|id| self.lane_primitives(*id))
            .flatten()
    }

    #[must_use]
    pub fn primitive_count(&self) -> usize {
        self.iter().count()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.primitive_count() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::{Lane, LaneId, MAX_CLIP_DEPTH, Scene};
    use crate::{
        math::{Point, Rect},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            color::Color,
            primitive::{ClipOp, ClipRect, Image, Primitive, Quad, QuadPaint, Shadow},
        },
    };

    fn clip() -> ClipRect {
        ClipRect {
            rect: Rect::new(0.0, 0.0, 8.0, 8.0),
            corner_radius: [0.0; 4],
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

    fn image() -> Primitive {
        Primitive::Image(Image {
            bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
            tile: AtlasTile {
                atlas: AtlasKind::Image,
                page: 0,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                generation: 0,
                slot: 0,
                slot_epoch: 0,
            },
            modulation: Color::WHITE,
        })
    }

    fn shadow() -> Primitive {
        Primitive::Shadow(Shadow {
            rect: Rect::new(0.0, 0.0, 4.0, 4.0),
            offset: Point::new(0.0, 1.0),
            blur: 2.0,
            color: Color::BLACK,
            corner_radius: [0.0; 4],
        })
    }

    fn kinds(scene: &Scene, id: LaneId) -> Vec<&'static str> {
        scene
            .lane_primitives(id)
            .expect("lane 存在")
            .iter()
            .map(|primitive| match primitive {
                Primitive::Quad(_) => "quad",
                Primitive::Image(_) => "image",
                Primitive::Shadow(_) => "shadow",
                Primitive::Clip(_) => "clip",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn new_lane_is_empty() {
        let lane = Lane::new();
        assert!(lane.is_empty());
        assert!(lane.primitives().is_empty());
        assert_eq!(lane.clip_depth(), 0);
    }

    #[test]
    fn push_preserves_order() {
        let mut lane = Lane::new();
        lane.push(quad());
        lane.push(image());
        lane.push(shadow());
        assert_eq!(lane.primitives().len(), 3);
        assert!(!lane.is_empty());
    }

    #[test]
    fn push_tracks_clip_depth() {
        let mut lane = Lane::new();
        lane.push(quad());
        assert_eq!(lane.clip_depth(), 0);
        lane.push(Primitive::Clip(ClipOp::Push(clip())));
        assert_eq!(lane.clip_depth(), 1);
        lane.push(Primitive::Clip(ClipOp::Pop));
        assert_eq!(lane.clip_depth(), 0);
        assert_eq!(lane.primitives().len(), 3);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "assertion failed")]
    fn pop_underflow_is_debug_assert() {
        let mut lane = Lane::new();
        lane.push(Primitive::Clip(ClipOp::Pop));
    }

    #[test]
    fn depth_cap_is_not_enforced_on_the_lane() {
        let mut lane = Lane::new();
        for _ in 0..MAX_CLIP_DEPTH {
            lane.push(Primitive::Clip(ClipOp::Push(clip())));
        }
        assert_eq!(lane.clip_depth(), MAX_CLIP_DEPTH);
        lane.push(Primitive::Clip(ClipOp::Push(clip())));
        assert_eq!(lane.clip_depth(), MAX_CLIP_DEPTH + 1);
        assert_eq!(lane.primitives().len(), usize::from(MAX_CLIP_DEPTH) + 1);
    }

    #[test]
    fn max_clip_depth_is_u8_8() {
        let depth: u8 = MAX_CLIP_DEPTH;
        assert_eq!(depth, 8);
    }

    #[test]
    fn scene_empty_is_empty() {
        let scene = Scene::empty();

        assert_eq!(scene.lane_count(), 0);
        assert!(scene.order().is_empty());
        assert!(scene.is_empty());
        assert_eq!(scene.primitive_count(), 0);
    }

    #[test]
    fn new_lane_ids_are_distinct_and_dense() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        let b = scene.new_lane();
        let c = scene.new_lane();

        assert_eq!(
            (a.index(), b.index(), c.index()),
            (0, 1, 2),
            "lane 槽位必须稠密"
        );
        assert_eq!((a.generation(), b.generation()), (1, 1));
        assert_eq!(scene.lane_count(), 3);
    }

    #[test]
    fn release_then_reuse_bumps_generation() {
        let mut scene = Scene::empty();
        let old = scene.new_lane();
        scene.release_lane(old);
        assert!(scene.lane_primitives(old).is_none());
        assert!(!scene.push(old, quad()), "陈旧句柄必须拒绝写入");

        let new = scene.new_lane();
        assert_eq!(new.index(), old.index());
        assert_eq!(new.generation(), old.generation() + 1);
        assert!(scene.push(new, quad()));
        assert_eq!(scene.lane_primitives(new).map(<[Primitive]>::len), Some(1));
    }

    #[test]
    fn push_appends_in_order_to_one_lane() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        let b = scene.new_lane();

        assert!(scene.push(a, quad()));
        assert!(scene.push(a, image()));
        assert!(scene.push(a, shadow()));
        assert!(scene.push(b, quad()));

        assert_eq!(kinds(&scene, a), ["quad", "image", "shadow"]);
        assert_eq!(scene.lane_primitives(b).map(<[Primitive]>::len), Some(1));
    }

    #[test]
    fn push_tracks_clip_depth_via_lane() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        assert!(scene.push(a, quad()));
        assert!(scene.push(a, Primitive::Clip(ClipOp::Push(clip()))));
        assert!(scene.push(a, Primitive::Clip(ClipOp::Pop)));

        assert_eq!(kinds(&scene, a), ["quad", "clip", "clip"]);
        scene.set_order(&[a]);
        assert_eq!(scene.primitive_count(), 3);
    }

    #[test]
    fn set_order_controls_iter() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        let b = scene.new_lane();
        assert!(scene.push(a, quad()));
        assert!(scene.push(a, image()));
        assert!(scene.push(b, shadow()));
        assert!(scene.push(b, quad()));

        scene.set_order(&[b, a]);
        assert_eq!(scene.order(), &[b, a]);
        assert_eq!(scene.primitive_count(), 4);
        let first = scene.iter().next().expect("至少一个图元");
        assert!(
            matches!(first, Primitive::Shadow(_)),
            "b 的 lane 必须排在前"
        );

        scene.clear_order();
        assert!(scene.iter().next().is_none());
        assert_eq!(scene.primitive_count(), 0);
    }

    #[test]
    fn iter_skips_stale_lane_ids() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        assert!(scene.push(a, quad()));
        scene.set_order(&[a, a]);
        scene.release_lane(a);

        assert_eq!(
            scene.primitive_count(),
            0,
            "陈旧 lane id 必须被静默跳过而不是 panic"
        );
        assert!(scene.iter().next().is_none());
    }

    #[test]
    fn clear_frame_keeps_lanes_and_capacity() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        assert!(scene.push(a, quad()));
        assert!(scene.push(a, image()));
        assert!(scene.push(a, shadow()));

        scene.clear_frame();
        assert!(scene.is_empty());
        assert_eq!(scene.lane_count(), 1, "分配必须保留");
        assert_eq!(scene.lane_primitives(a).map(<[Primitive]>::len), Some(0));
        assert!(scene.push(a, quad()), "清空后同一 lane 必须可再次写入");
    }

    #[test]
    fn clear_frame_resets_clip_depth() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        for _ in 0..MAX_CLIP_DEPTH {
            assert!(scene.push(a, Primitive::Clip(ClipOp::Push(clip()))));
        }
        scene.clear_frame();
        for _ in 0..MAX_CLIP_DEPTH {
            assert!(scene.push(a, Primitive::Clip(ClipOp::Push(clip()))));
        }
        assert_eq!(scene.lane_primitives(a).map(<[Primitive]>::len), Some(8));
    }

    #[test]
    fn clear_lane_keeps_capacity_and_resets_clip_depth() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        for _ in 0..MAX_CLIP_DEPTH {
            assert!(scene.push(a, Primitive::Clip(ClipOp::Push(clip()))));
        }
        assert!(scene.clear_lane(a));
        assert_eq!(scene.lane_primitives(a).map(<[Primitive]>::len), Some(0));
        for _ in 0..MAX_CLIP_DEPTH {
            assert!(scene.push(a, Primitive::Clip(ClipOp::Push(clip()))));
        }
    }

    #[test]
    fn clear_lane_stale_is_false() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        scene.release_lane(a);
        assert!(!scene.clear_lane(a), "陈旧句柄必须返回 false 而不是 panic");
    }

    #[test]
    fn order_keeps_empty_lanes() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        let b = scene.new_lane();
        assert!(scene.push(b, quad()));
        scene.set_order(&[a, b]);
        assert_eq!(scene.order(), &[a, b], "空 lane 保留在绘制序里");
        assert_eq!(scene.primitive_count(), 1, "空 lane 不贡献图元");
        assert!(!scene.is_empty());
    }

    #[test]
    fn lane_version_advances_on_every_content_mutation() {
        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        let version = |scene: &Scene| scene.lane_state(lane).map(|(version, _)| version);
        let initial = version(&scene).expect("lane 存在");

        assert!(scene.push(lane, quad()));
        let after_push = version(&scene).expect("lane 存在");
        assert_ne!(after_push, initial, "push 必须推进版本");

        assert!(scene.clear_lane(lane));
        let after_clear = version(&scene).expect("lane 存在");
        assert_ne!(after_clear, after_push, "clear_lane 必须推进版本");

        assert!(scene.push(lane, quad()));
        scene.clear_frame();
        assert_ne!(
            version(&scene).expect("lane 存在"),
            after_clear,
            "clear_frame 必须推进版本"
        );

        scene.release_lane(lane);
        assert!(
            version(&scene).is_none(),
            "释放之后版本查询给 None（陈旧句柄不得命中复用判据）"
        );
    }
}

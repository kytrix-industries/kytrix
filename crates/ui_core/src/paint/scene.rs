//! 绘制桶（`Lane`）、clip 深度上限、以及一帧的绘制场景（`Scene`/`LaneId`）。
//!
//! **`Scene` / `LaneId` / lane 分配与绘制序在 P03 落地**（`design/phase-02-paint-foundation.md` §7.2、§19；
//! `design/phase-03-node-state.md` §13）。`Lane` 与 `MAX_CLIP_DEPTH` 保持 crate 私有：应用层通过
//! [`Canvas`] 与 [`Scene`] 的只读方法间接观测。
//!
//! [`Canvas`]: crate::paint::canvas::Canvas

use crate::{
    node::id::{Arena, ArenaKey},
    paint::primitive::{ClipOp, Primitive},
};

/// clip 栈深度上限：**类型是 `u8`**，值 8。
///
/// 三处必须一致：本模块 `Lane::push` 的 debug 断言、`Canvas::push_clip` 的 `bool` 门限、
/// P10 三套 shader 的固定数组长度。
pub(crate) const MAX_CLIP_DEPTH: u8 = 8;

/// 一个绘制输出桶。内容按绘制序排列；`clip_depth` 是 C2/C3 的**增量校验状态**。
///
/// 只在 crate 内可见：应用层通过 `Canvas` 构造与读取（`design/phase-02-paint-foundation.md` §11）。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Lane {
    primitives: Vec<Primitive>,
    clip_depth: u8,
}

impl Lane {
    /// 空 lane。消费者：`Canvas::new`。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 唯一的压入点：更新 `clip_depth` 并做 C2/C3 的 debug 断言。消费者：`Canvas` 的 `push_*`、`Scene::push`。
    pub(crate) fn push(&mut self, primitive: Primitive) {
        match &primitive {
            Primitive::Clip(ClipOp::Push(_)) => {
                debug_assert!(usize::from(self.clip_depth()) < usize::from(MAX_CLIP_DEPTH));
                self.clip_depth = self.clip_depth.saturating_add(1);
            }
            Primitive::Clip(ClipOp::Pop) => {
                debug_assert!(usize::from(self.clip_depth()) > 0);
                self.clip_depth = self.clip_depth.saturating_sub(1);
            }
            _ => {}
        }
        self.primitives.push(primitive);
    }

    /// 清空内容并复位 clip 深度，但**保留 `Vec` 容量**（消费者：`Scene::clear_frame`）。
    pub(crate) fn clear(&mut self) {
        self.primitives.clear();
        self.clip_depth = 0;
    }

    /// 当前 clip 嵌套深度。消费者：`Lane::push`（内部）与 `Canvas` 的 clip 门限。
    pub(crate) fn clip_depth(&self) -> u8 {
        self.clip_depth
    }

    /// 只读切片。消费者：`Canvas::primitives`、`Scene::lane_primitives`。
    pub(crate) fn primitives(&self) -> &[Primitive] {
        &self.primitives
    }

    /// 消费者：`Canvas::is_empty`。
    pub(crate) fn is_empty(&self) -> bool {
        self.primitives.is_empty()
    }
}

/// lane 句柄（P02 §7.2 冻结形状：私有字段 + 代际）。8 B，`Copy + Eq + Hash`。
///
/// 释放后代际 +1，旧句柄立即失效（否则会往**别人的** lane 里写图元）。
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
    /// arena 槽位下标（**仅测试**：公开面不暴露内部下标，避免应用层依赖槽位布局）。
    #[cfg(test)]
    pub(crate) const fn index(self) -> u32 {
        self.index
    }

    /// 代际（**仅测试**：同上）。
    #[cfg(test)]
    pub(crate) const fn generation(self) -> u32 {
        self.generation
    }

    /// 仅测试：直接构造句柄（layout 侧表与 `remove` 的 lane 归还用例需要）。
    #[cfg(test)]
    pub(crate) const fn for_test(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }
}

/// 一帧的绘制指令（I2）。**持久**：lane id 跨帧稳定（`Node.lanes` 持有）。
///
/// **派生约束**：只 `derive(Debug)`。不 `Clone`（lane 复用是搬不是拷，P02 S-15）、不 `Default`
/// （`new_without_default` 用私有构造器规避），且**无公开构造器**：应用层只能读 `Window::scene()`。
#[derive(Debug)]
pub struct Scene {
    lanes: Arena<Lane, LaneId>,
    /// 本帧的 lane 绘制序（P06 的 paint 阶段用 `set_order` 写入）。
    order: Vec<LaneId>,
}

impl Scene {
    /// **唯一的构造点**（`pub(crate)`）：消费者 `Window::new`。应用层不可构造。
    pub(crate) fn empty() -> Self {
        Self {
            lanes: Arena::<Lane, LaneId>::new(),
            order: Vec::new(),
        }
    }

    /// engine-internal, 应用层不得调用 —— 分配一条空 lane。
    pub fn new_lane(&mut self) -> LaneId {
        self.lanes.insert_with(|_| Lane::new())
    }

    /// engine-internal, 应用层不得调用 —— 释放 lane（代际 +1，旧句柄立即失效）；陈旧句柄 → 无操作。
    pub fn release_lane(&mut self, id: LaneId) {
        let _ = self.lanes.remove(id);
    }

    /// engine-internal, 应用层不得调用 —— 向 lane 追加一条图元（**唯一**写入点）。
    ///
    /// 返回 `false` = lane 句柄陈旧。内部转调 `Lane::push`（更新 `clip_depth` 并做 C2/C3 的 debug 断言）。
    #[must_use]
    pub fn push(&mut self, lane: LaneId, primitive: Primitive) -> bool {
        match self.lanes.get_mut(lane) {
            Some(target) => {
                target.push(primitive);
                true
            }
            None => false,
        }
    }

    /// engine-internal, 应用层不得调用 —— 清空**所有** lane 的内容但保留分配与容量（每帧 paint 的起点）。
    pub fn clear_frame(&mut self) {
        for lane in self.lanes.values_mut() {
            lane.clear();
        }
    }

    /// engine-internal, 应用层不得调用 —— 清空**一条** lane 的内容并复位 `clip_depth`（保留容量）。
    ///
    /// 返回 `false` = lane 句柄陈旧。消费者：P06 的 paint pass（按节点重建 lane 的起点）。
    #[must_use]
    pub fn clear_lane(&mut self, id: LaneId) -> bool {
        match self.lanes.get_mut(id) {
            Some(lane) => {
                lane.clear();
                true
            }
            None => false,
        }
    }

    /// engine-internal, 应用层不得调用 —— 写入本帧绘制序。
    ///
    /// **契约**：`order` 由调用方（P06 的 paint 阶段）保证**无重复**；本方法不查重（查重是 O(n)）。
    pub fn set_order(&mut self, order: &[LaneId]) {
        self.order.clear();
        self.order.extend_from_slice(order);
    }

    /// engine-internal, 应用层不得调用 —— 撤销绘制序。
    pub fn clear_order(&mut self) {
        self.order.clear();
    }

    /// 一条 lane 的图元切片。**不**返回 `&Lane`（`Lane` 是 crate 私有，见 F-38）。
    #[must_use]
    pub fn lane_primitives(&self, id: LaneId) -> Option<&[Primitive]> {
        self.lanes.get(id).map(Lane::primitives)
    }

    /// 已分配的 lane 数。
    #[must_use]
    pub fn lane_count(&self) -> usize {
        self.lanes.len()
    }

    /// 本帧绘制序。
    #[must_use]
    pub fn order(&self) -> &[LaneId] {
        &self.order
    }

    /// 按 `order` 展平（陈旧 `LaneId` 静默跳过，不 panic）。
    pub fn iter(&self) -> impl Iterator<Item = &Primitive> + Clone + '_ {
        self.order
            .iter()
            .filter_map(|id| self.lane_primitives(*id))
            .flatten()
    }

    /// 图元总数（**按绘制序统计**；未进 `order` 的 lane 不计，因此它回答的是「这一帧要画多少」）。
    #[must_use]
    pub fn primitive_count(&self) -> usize {
        self.iter().count()
    }

    /// 是否没有任何图元（**按绘制序统计**：未进 `order` 的 lane 不计）。
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
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                generation: 0,
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
    #[cfg(debug_assertions)]
    #[should_panic(expected = "assertion failed")]
    fn depth_cap_is_debug_assert() {
        let mut lane = Lane::new();
        for _ in 0..MAX_CLIP_DEPTH {
            lane.push(Primitive::Clip(ClipOp::Push(clip())));
        }
        assert_eq!(lane.clip_depth(), MAX_CLIP_DEPTH);
        lane.push(Primitive::Clip(ClipOp::Push(clip())));
    }

    #[test]
    fn max_clip_depth_is_u8_8() {
        let depth: u8 = MAX_CLIP_DEPTH;
        assert_eq!(depth, 8);
    }

    // ── P03：Scene / LaneId ──

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
        // 若 clear 没有把 clip_depth 复位为 0，下一次 push(Push) 会立刻触发深度上限断言。
        for _ in 0..MAX_CLIP_DEPTH {
            assert!(scene.push(a, Primitive::Clip(ClipOp::Push(clip()))));
        }
        assert_eq!(scene.lane_primitives(a).map(<[Primitive]>::len), Some(8));
    }

    // ── P06：clear_lane ──

    #[test]
    fn clear_lane_keeps_capacity_and_resets_clip_depth() {
        let mut scene = Scene::empty();
        let a = scene.new_lane();
        for _ in 0..MAX_CLIP_DEPTH {
            assert!(scene.push(a, Primitive::Clip(ClipOp::Push(clip()))));
        }
        assert!(scene.clear_lane(a));
        assert_eq!(scene.lane_primitives(a).map(<[Primitive]>::len), Some(0));
        // clip_depth 已复位：可以再压满 8 层而不触发 C3 断言。
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
}

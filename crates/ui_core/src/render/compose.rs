use crate::{
    error::EngineError,
    paint::{color::ColorResolver, instance::InstanceEncoder},
    render::{
        compositor::Plan,
        delta::{
            GeometryCatalog, LaneStore, SceneDelta, ScrollTable, push_scroll_damage, scroll_damage,
        },
        frame::FramePayload,
        geometry_upload::{GeometryUploadPlan, GeometryUploadTracker},
    },
};

pub(crate) struct ComposedFrame {
    pub plan: Plan,
    pub payload: FramePayload,
    pub stats: ComposeStats,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ComposeStats {
    pub encoded_lanes: usize,
    pub reused_segments: usize,
    pub written_bytes: usize,
    pub geometry_encoded_chunks: usize,
    pub geometry_uploaded_bytes: usize,
    pub instance_count: u32,
    pub drawn_instances: u32,
    pub scroll_slots: usize,
}

pub(crate) struct WindowComposer {
    encoder: InstanceEncoder,
    lanes: LaneStore,
    geometry: GeometryCatalog,
    geometry_uploads: GeometryUploadTracker,
    scroll: ScrollTable,
    stats: ComposeStats,
    image_layers: u32,
}

impl WindowComposer {
    pub(crate) fn new() -> Self {
        Self {
            encoder: InstanceEncoder::new(),
            lanes: LaneStore::new(),
            geometry: GeometryCatalog::new(),
            geometry_uploads: GeometryUploadTracker::new(),
            scroll: ScrollTable::new(),
            stats: ComposeStats::default(),
            image_layers: u32::MAX,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn buffer(&self) -> &crate::paint::instance::InstanceBuffer {
        self.encoder.buffer()
    }

    pub(crate) fn compose(
        &mut self,
        delta: &SceneDelta,
        resolver: ColorResolver,
    ) -> Result<ComposedFrame, EngineError> {
        let scale = delta.header.scale;
        for lane in &delta.removed {
            self.encoder.release_lane(*lane);
        }
        let lanes_changed = self.lanes.apply(delta);
        self.geometry.apply(delta);
        let scroll_slots = self.scroll.apply(&delta.scroll, scale).len();
        let viewports: Vec<_> = delta
            .scroll
            .iter()
            .filter_map(|scroll| scroll_damage(scroll.viewport))
            .collect();
        let plan = push_scroll_damage(delta.header.plan, &viewports);
        let limit_changed = delta.image_layers != self.image_layers;
        if limit_changed {
            self.image_layers = delta.image_layers;
        }
        self.encoder.set_image_layers(self.image_layers);
        let full = delta.full || limit_changed;
        if full {
            self.encoder.invalidate();
        }
        if lanes_changed || full || self.encoder.needs_reset(scale) {
            self.encoder
                .encode(&self.lanes, scale, resolver, &self.geometry)?;
            let buffer = self.encoder.buffer();
            self.stats = ComposeStats {
                encoded_lanes: self.encoder.encoded_lanes(),
                reused_segments: self.encoder.reused_segments(),
                written_bytes: self.encoder.written_bytes(),
                geometry_encoded_chunks: self.encoder.encoded_geometry_chunks(),
                geometry_uploaded_bytes: 0,
                instance_count: buffer.instance_count(),
                drawn_instances: buffer.len() as u32,
                scroll_slots,
            };
        } else {
            self.stats.encoded_lanes = 0;
            self.stats.reused_segments = self.lanes.len();
            self.stats.written_bytes = 0;
            self.stats.geometry_encoded_chunks = 0;
            self.stats.geometry_uploaded_bytes = 0;
            self.stats.scroll_slots = scroll_slots;
            let buffer = self.encoder.buffer();
            self.stats.instance_count = buffer.instance_count();
            self.stats.drawn_instances = buffer.len() as u32;
        }
        let mut scroll_payload = Vec::new();
        self.scroll.to_payload(scale, &mut scroll_payload);
        let mut scroll_clip = Vec::new();
        self.scroll.viewports_to_payload(&mut scroll_clip);
        let payload = if full {
            let payload = FramePayload::full(self.encoder.buffer(), scroll_payload, scroll_clip);
            self.encoder.buffer_mut().clear_changes();
            payload
        } else {
            FramePayload::from_buffer_delta(self.encoder.buffer_mut(), scroll_payload, scroll_clip)
        };
        if delta.full {
            self.geometry_uploads.clear();
        }
        self.stats.geometry_uploaded_bytes = self
            .geometry_uploads
            .plan(payload.geometry())
            .iter()
            .map(GeometryUploadPlan::uploaded_bytes)
            .sum();
        Ok(ComposedFrame {
            plan,
            payload,
            stats: self.stats,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::WindowComposer;
    #[cfg(target_os = "linux")]
    use crate::math::LogicalSize;
    use crate::{
        math::{DevicePixelRatio, DeviceRect, LogicalPoint, LogicalRect, dp, px},
        paint::{
            color::{Color, ColorResolver},
            primitive::{Primitive, Quad, QuadPaint},
            scene::Scene,
        },
        render::{
            compositor::{DamageRects, Plan},
            delta::{DeltaBuilder, FrameHeader, SceneDelta},
            frame::{FrameRequest, InstanceMirror, LatestFrame, MAX_FRAMES_IN_FLIGHT},
        },
        window::{FrameIndex, WindowId},
    };

    fn header(index: u32, plan: Plan, scale: DevicePixelRatio) -> FrameHeader {
        FrameHeader {
            window: WindowId::from_raw(1),
            index: FrameIndex::from_raw(index),
            plan,
            surface: (64, 64),
            scale,
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

    struct Fixture {
        scene: Scene,
        lane: crate::paint::scene::LaneId,
        store: crate::paint::geometry_store::GeometryStore,
        builder: DeltaBuilder,
        composer: WindowComposer,
        next: u32,
    }

    impl Fixture {
        fn new(lanes: usize) -> Self {
            let mut scene = Scene::empty();
            let mut order = Vec::with_capacity(lanes);
            for _ in 0..lanes {
                let lane = scene.new_lane();
                assert!(scene.push(lane, quad()));
                order.push(lane);
            }
            scene.set_order(&order);
            Self {
                lane: order[0],
                scene,
                store: crate::paint::geometry_store::GeometryStore::new(),
                builder: DeltaBuilder::new(),
                composer: WindowComposer::new(),
                next: 1,
            }
        }

        fn frame(
            &mut self,
            plan: Plan,
            full: bool,
            scroll: &[(LogicalPoint, DeviceRect)],
        ) -> SceneDelta {
            let index = self.next;
            self.next += 1;
            let mut delta = SceneDelta::empty(header(index, plan, DevicePixelRatio::ONE));
            self.builder
                .collect(&mut self.scene, &self.store, scroll, full, &mut delta);
            delta
        }

        fn frame_with_signals(&mut self, plan: Plan) -> SceneDelta {
            let index = self.next;
            self.next += 1;
            let mut delta = SceneDelta::empty(header(index, plan, DevicePixelRatio::ONE));
            let full = self.builder.full_for(DevicePixelRatio::ONE);
            self.builder
                .collect(&mut self.scene, &self.store, &[], full, &mut delta);
            delta
        }

        fn compose(&mut self, delta: &SceneDelta) -> super::ComposedFrame {
            self.composer
                .compose(delta, ColorResolver)
                .expect("compose")
        }
    }

    #[test]
    fn the_first_frame_encodes_every_lane() {
        let mut fixture = Fixture::new(4);
        let delta = fixture.frame(Plan::Full, true, &[]);
        let composed = fixture.compose(&delta);
        assert_eq!(composed.stats.encoded_lanes, 4);
        assert_eq!(composed.stats.drawn_instances, 4);
        assert_eq!(composed.payload.instance_count(), 4);
        assert_eq!(
            composed.payload.chunks().len(),
            1,
            "首帧只有一块覆盖全环的变化区间"
        );
    }

    #[test]
    fn an_incremental_frame_only_encodes_the_changed_lane() {
        let mut fixture = Fixture::new(4);
        let delta = fixture.frame(Plan::Full, true, &[]);
        let _ = fixture.compose(&delta);
        assert!(fixture.scene.push(fixture.lane, quad()));
        let delta = fixture.frame(Plan::Partial(DamageRects::new()), false, &[]);
        let composed = fixture.compose(&delta);
        assert_eq!(composed.stats.encoded_lanes, 1);
        assert_eq!(composed.stats.reused_segments, 3);
        assert_eq!(composed.payload.chunks().len(), 1);
    }

    #[test]
    fn a_scroll_frame_does_not_encode_anything() {
        let mut fixture = Fixture::new(4);
        let viewport = DeviceRect::new(dp(0), dp(0), dp(64), dp(64));
        let delta = fixture.frame(
            Plan::Full,
            true,
            &[(LogicalPoint::new(px(0.0), px(0.0)), viewport)],
        );
        let first = fixture.compose(&delta);
        assert_eq!(first.stats.encoded_lanes, 4);
        assert!(first.payload.scroll().is_empty() || first.payload.scroll()[0] == [0.0, 0.0]);
        assert_eq!(
            fixture.composer.encoder.fast_encodes(),
            0,
            "首帧走全量扫描，不计入增量快路径"
        );

        let delta = fixture.frame(
            Plan::Idle,
            false,
            &[(LogicalPoint::new(px(0.0), px(-20.0)), viewport)],
        );
        assert!(
            delta.lanes.is_empty() && delta.order.is_none(),
            "滚动 delta 不得带 lane"
        );
        let scrolled = fixture.compose(&delta);
        assert_eq!(scrolled.stats.encoded_lanes, 0, "滚动帧不得重编码任何 lane");
        assert_eq!(
            fixture.composer.encoder.fast_encodes(),
            0,
            "滚动帧不得进入编码器（门禁为假时 encode 根本不该被调用）"
        );
        assert_eq!(scrolled.stats.reused_segments, 4);
        assert!(
            scrolled.payload.chunks().is_empty(),
            "滚动帧的载荷不得带实例块"
        );
        assert_eq!(
            scrolled.payload.scroll()[0],
            [0.0, 20.0],
            "偏移进载荷：绘制期平移量 = −位移（逻辑滚动 -20 ⇒ +20 物理像素）"
        );
        let Plan::Partial(rects) = scrolled.plan else {
            panic!("滚动帧的 damage 必须是 Partial，实际 {:?}", scrolled.plan);
        };
        assert_eq!(rects.as_slice(), &[viewport]);
    }

    #[test]
    fn scrolling_never_touches_instance_bytes() {
        let mut moving = Fixture::new(3);
        let viewport = DeviceRect::new(dp(0), dp(0), dp(64), dp(64));
        let delta = moving.frame(
            Plan::Full,
            true,
            &[(LogicalPoint::new(px(0.0), px(0.0)), viewport)],
        );
        let before = moving.compose(&delta).stats;
        let delta = moving.frame(Plan::Partial(DamageRects::new()), false, &[]);
        let baseline = moving.compose(&delta);
        assert_eq!(baseline.stats.encoded_lanes, 0);
        assert!(baseline.payload.chunks().is_empty());
        assert_eq!(before.drawn_instances, baseline.stats.drawn_instances);
        let delta = moving.frame(
            Plan::Idle,
            false,
            &[(LogicalPoint::new(px(0.0), px(-30.0)), viewport)],
        );
        let scrolled = moving.compose(&delta);
        assert!(scrolled.payload.chunks().is_empty());
        assert_eq!(scrolled.stats.encoded_lanes, 0);
    }

    #[test]
    fn a_scale_change_reencodes_every_lane() {
        let mut fixture = Fixture::new(3);
        let delta = fixture.frame(Plan::Full, true, &[]);
        let _ = fixture.compose(&delta);
        let mut delta = SceneDelta::empty(header(
            9,
            Plan::Full,
            DevicePixelRatio::new(2.0).expect("scale"),
        ));
        let full = fixture
            .builder
            .full_for(DevicePixelRatio::new(2.0).expect("scale"));
        assert!(full, "DPI 变化必须整帧重建");
        fixture
            .builder
            .collect(&mut fixture.scene, &fixture.store, &[], full, &mut delta);
        delta.full = true;
        let composed = fixture.compose(&delta);
        assert_eq!(composed.stats.encoded_lanes, 3);
    }

    #[test]
    fn unchanged_lanes_reuse_their_snapshot() {
        let mut fixture = Fixture::new(2);
        let delta = fixture.frame(Plan::Full, true, &[]);
        let first = delta.lanes[0].primitives.clone();
        let _ = fixture.compose(&delta);
        let delta = fixture.frame(Plan::Partial(DamageRects::new()), false, &[]);
        assert!(delta.lanes.is_empty(), "无变化帧不得带 lane");
        assert!(Arc::strong_count(&first) >= 1);
    }

    #[test]
    fn moving_a_lane_with_the_same_instance_count_rebuilds_the_op_bounds() {
        let mut fixture = Fixture::new(2);
        let delta = fixture.frame(Plan::Full, true, &[]);
        let _ = fixture.compose(&delta);
        assert!(fixture.scene.clear_lane(fixture.lane));
        assert!(fixture.scene.push(
            fixture.lane,
            Primitive::Quad(Quad {
                bounds: LogicalRect::new(px(400.0), px(0.0), px(4.0), px(4.0)),
                corner_radius: [px(0.0); 4],
                paint: QuadPaint::Solid {
                    fill: Color::WHITE,
                    border: None,
                },
            })
        ));
        let delta = fixture.frame(Plan::Partial(DamageRects::new()), false, &[]);
        let composed = fixture.compose(&delta);
        assert_eq!(
            composed.stats.encoded_lanes, 1,
            "实例数不变也要重编码实例字节"
        );
        let rightmost = composed
            .payload
            .ops()
            .iter()
            .filter_map(|op| match op {
                crate::paint::instance::FrameOp::Instances { bounds, .. } => Some(bounds.x1),
                crate::paint::instance::FrameOp::Geometry { .. } => None,
            })
            .max()
            .expect("至少有实例 op");
        assert!(
            rightmost >= 390,
            "op 包围盒必须包含移动后的位置（实际 x1={}）",
            rightmost.value()
        );
    }

    #[test]
    fn incremental_frames_take_the_encoder_fast_path() {
        let mut fixture = Fixture::new(4);
        let delta = fixture.frame(Plan::Full, true, &[]);
        let _ = fixture.compose(&delta);
        assert_eq!(
            fixture.composer.encoder.fast_encodes(),
            0,
            "首帧必须走全量扫描（合成侧还没有任何段）"
        );

        assert!(fixture.scene.push(fixture.lane, quad()));
        let delta = fixture.frame(Plan::Partial(DamageRects::new()), false, &[]);
        let composed = fixture.compose(&delta);
        assert_eq!(
            fixture.composer.encoder.fast_encodes(),
            1,
            "增量帧必须走快路径（帧源给出了可信变化集）"
        );
        assert_eq!(composed.stats.encoded_lanes, 1);
        assert_eq!(composed.stats.reused_segments, 3);
    }

    #[test]
    fn one_dirty_lane_never_reencodes_the_whole_scene() {
        const LANES: usize = 10_000;
        let mut fixture = Fixture::new(LANES);
        let delta = fixture.frame(Plan::Full, true, &[]);
        let first = fixture.compose(&delta);
        assert_eq!(first.stats.encoded_lanes, LANES, "首帧必须全量编码");

        assert!(fixture.scene.clear_lane(fixture.lane));
        assert!(fixture.scene.push(fixture.lane, quad()));
        let delta = fixture.frame(Plan::Partial(DamageRects::new()), false, &[]);
        assert_eq!(delta.lanes.len(), 1, "只有脏 lane 进入 delta");
        let composed = fixture.compose(&delta);
        assert_eq!(
            composed.stats.encoded_lanes, 1,
            "只有 1 条 lane 变脏 ⇒ 只允许重编码 1 条（不是全量）"
        );
        assert_eq!(
            composed.stats.reused_segments,
            LANES - 1,
            "其余 {} 条 lane 的实例段必须整段复用",
            LANES - 1
        );
        assert_eq!(
            composed.stats.written_bytes, 128,
            "只上传脏 lane 的 1 个实例（128 B 实例布局）"
        );
        assert_eq!(composed.payload.chunks().len(), 1, "载荷只带 1 个实例块");
    }

    #[test]
    fn encoding_the_same_scene_from_a_delta_is_byte_identical() {
        use crate::paint::instance::InstanceEncoder;

        let mut fixture = Fixture::new(6);
        assert!(fixture.scene.push(fixture.lane, quad()));
        let delta = fixture.frame(Plan::Full, true, &[]);
        let composed = fixture.compose(&delta);
        drop(composed);

        let mut direct = InstanceEncoder::new();
        direct
            .encode(
                &fixture.scene,
                DevicePixelRatio::ONE,
                ColorResolver,
                &fixture.store,
            )
            .expect("direct encode");
        let composed_buffer = fixture.composer.buffer();
        assert_eq!(
            direct.buffer().as_bytes(),
            composed_buffer.as_bytes(),
            "两条路径的实例字节必须逐字节一致"
        );
        assert_eq!(direct.buffer().ops(), composed_buffer.ops());
        assert_eq!(
            direct.buffer().instance_count(),
            composed_buffer.instance_count()
        );
        assert!(fixture.scene.push(fixture.lane, quad()));
        let delta = fixture.frame(Plan::Partial(DamageRects::new()), false, &[]);
        let composed = fixture.compose(&delta);
        direct
            .encode(
                &fixture.scene,
                DevicePixelRatio::ONE,
                ColorResolver,
                &fixture.store,
            )
            .expect("direct encode");
        assert_eq!(
            direct.buffer().as_bytes(),
            fixture.composer.buffer().as_bytes()
        );
        assert_eq!(composed.stats.encoded_lanes, 1);
    }

    #[test]
    fn the_first_frame_after_a_rebuild_refills_the_mirror_byte_identically() {
        const LANES: usize = 4;
        let mut fixture = Fixture::new(LANES);
        let mut mirror = InstanceMirror::new(MAX_FRAMES_IN_FLIGHT);

        let first = fixture.frame_with_signals(Plan::Full);
        assert!(first.full, "首帧必须整帧发布");
        let composed = fixture.compose(&first);
        assert!(
            mirror.prepare(0, &composed.payload),
            "从未写过的槽必须整块写"
        );
        mirror.finish(0);
        assert_eq!(mirror.bytes().len(), LANES * 128, "镜像 = 整张实例环");

        assert!(fixture.scene.clear_lane(fixture.lane));
        assert!(fixture.scene.push(fixture.lane, quad()));
        let incremental = fixture.frame_with_signals(Plan::Partial(DamageRects::new()));
        assert!(!incremental.full, "同一 DPI 的普通帧不得整帧发布");
        assert_eq!(incremental.lanes.len(), 1, "只有那条脏 lane 进入 delta");
        let composed = fixture.compose(&incremental);
        assert_eq!(composed.payload.chunks().len(), 1, "增量帧只带变化的段");
        assert!(
            !mirror.prepare(0, &composed.payload),
            "增量帧不得整块重写镜像"
        );
        mirror.finish(0);
        let before = mirror.bytes().to_vec();

        let mut rebuilt = InstanceMirror::new(MAX_FRAMES_IN_FLIGHT);
        assert!(rebuilt.bytes().is_empty(), "重建后的镜像没有任何内容");
        fixture.builder.force_full();
        assert!(fixture.builder.forced_full(), "信号必须先置位");

        let resync = fixture.frame_with_signals(Plan::Full);
        assert!(
            !fixture.builder.forced_full(),
            "信号必须被 `full_for` 取走（只强制一帧）"
        );
        assert!(resync.full, "重建后的第一帧必须整帧发布（delta.full）");
        assert_eq!(resync.lanes.len(), LANES, "整帧必须带全部 lane 内容");
        let composed = fixture.compose(&resync);
        assert_eq!(composed.payload.chunks().len(), 1, "全量载荷只有一块");
        let whole = 0..LANES as u32;
        assert_eq!(
            composed.payload.chunks()[0].range(),
            whole,
            "整块必须覆盖 [0, instance_count)"
        );
        assert_eq!(
            (whole.end - whole.start) as usize * 128,
            LANES * 128,
            "整块的字节数 = 实例数 × 128（块区间 × 固定实例布局）"
        );
        assert!(
            rebuilt.prepare(0, &composed.payload),
            "重建后的槽必须整块写"
        );
        rebuilt.finish(0);
        assert_eq!(
            rebuilt.bytes(),
            before.as_slice(),
            "恢复后的镜像内容必须与重建前逐字节一致"
        );

        assert!(fixture.scene.clear_lane(fixture.lane));
        assert!(fixture.scene.push(fixture.lane, quad()));
        let incremental = fixture.frame_with_signals(Plan::Partial(DamageRects::new()));
        assert!(!incremental.full, "没有消费端重置信号 ⇒ 不得整帧发布");
        let composed = fixture.compose(&incremental);
        assert_eq!(
            composed.payload.chunks().len(),
            1,
            "增量载荷只带 1 段（覆盖不了整张环）"
        );
        let covered = composed.payload.chunks()[0].range();
        assert!(
            covered.end - covered.start < LANES as u32,
            "增量块不得覆盖整张实例环（实际 {covered:?}）"
        );
        let mut rebuilt_without_signal = InstanceMirror::new(MAX_FRAMES_IN_FLIGHT);
        assert!(
            rebuilt_without_signal.prepare(0, &composed.payload),
            "空镜像总是整块写它自己的缓冲（与载荷是否全量无关）"
        );
        rebuilt_without_signal.finish(0);
        assert_ne!(
            rebuilt_without_signal.bytes(),
            before.as_slice(),
            "增量载荷喂给重建后的空镜像 ⇒ 内容与重建前不一致（所以 full 信号不可省）"
        );
    }

    #[test]
    fn releasing_a_lane_rebuilds_the_ops_without_the_freed_range() {
        let mut fixture = Fixture::new(3);
        let delta = fixture.frame(Plan::Full, true, &[]);
        let first = fixture.compose(&delta);
        assert_eq!(first.stats.drawn_instances, 3);

        let removed = fixture.scene.order()[1];
        let remaining: Vec<_> = fixture
            .scene
            .order()
            .iter()
            .copied()
            .filter(|lane| *lane != removed)
            .collect();
        fixture.scene.release_lane(removed);
        fixture.scene.set_order(&remaining);
        let mut delta = fixture.frame(Plan::Partial(DamageRects::new()), false, &[]);
        fixture.builder.note_removed(removed, &mut delta);
        let composed = fixture.compose(&delta);

        assert_eq!(
            composed.stats.drawn_instances, 2,
            "被释放 lane 的实例必须从 ops 里消失"
        );
        assert_eq!(
            fixture.composer.encoder.fast_encodes(),
            0,
            "释放 lane 必须走全量 ops 重建，而不是增量快路径"
        );
        let ops: Vec<[u32; 2]> = fixture
            .composer
            .buffer()
            .ops()
            .iter()
            .map(|op| match *op {
                crate::paint::instance::FrameOp::Instances { start, len, .. } => [start, len],
                crate::paint::instance::FrameOp::Geometry { .. } => panic!("没有几何 op"),
            })
            .collect();
        let total: u32 = ops.iter().map(|entry| entry[1]).sum();
        assert_eq!(total, 2, "ops 只能覆盖存活 lane 的实例区间：{ops:?}");
    }

    #[test]
    fn a_removal_note_with_a_stale_generation_keeps_the_live_lane() {
        let mut fixture = Fixture::new(1);
        let delta = fixture.frame(Plan::Full, true, &[]);
        let _ = fixture.compose(&delta);
        let live = fixture.scene.order()[0];
        assert_eq!(fixture.composer.lanes.len(), 1);

        let stale = crate::paint::scene::LaneId::for_test(live.slot(), live.generation() + 1);
        let mut delta = fixture.frame(Plan::Partial(DamageRects::new()), false, &[]);
        fixture.builder.note_removed(stale, &mut delta);
        let _ = fixture.compose(&delta);
        assert_eq!(
            fixture.composer.lanes.len(),
            1,
            "generation 不匹配的移除通知不得把存活 lane 从 LaneStore 摘掉"
        );

        let added = fixture.scene.new_lane();
        assert!(fixture.scene.push(added, quad()));
        let order: Vec<_> = fixture
            .scene
            .order()
            .iter()
            .copied()
            .chain([added])
            .collect();
        fixture.scene.set_order(&order);
        let delta = fixture.frame(Plan::Partial(DamageRects::new()), false, &[]);
        assert!(delta.order.is_some(), "新增 lane 必须重发绘制序");
        let composed = fixture.compose(&delta);
        assert_eq!(
            composed.stats.encoded_lanes, 1,
            "order 变化必须全量扫描：只有新增 lane 需要重编码"
        );
        assert_eq!(
            composed.stats.reused_segments, 1,
            "存活 lane 的实例段必须被复用"
        );
        let total: u32 = fixture
            .composer
            .buffer()
            .ops()
            .iter()
            .map(|op| match *op {
                crate::paint::instance::FrameOp::Instances { len, .. } => len,
                crate::paint::instance::FrameOp::Geometry { .. } => panic!("没有几何 op"),
            })
            .sum();
        assert_eq!(
            total, 2,
            "存活 lane 与新增 lane 的实例区间都必须出现在 ops 中（静默丢段会只剩 1）"
        );
    }

    #[test]
    fn reordering_lanes_without_removal_rebuilds_the_ops() {
        let mut scene = Scene::empty();
        let mut order = Vec::with_capacity(3);
        for index in 0..3u32 {
            let lane = scene.new_lane();
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: LogicalRect::new(px(index as f32 * 10.0), px(0.0), px(4.0), px(4.0)),
                    corner_radius: [px(0.0); 4],
                    paint: QuadPaint::Solid {
                        fill: Color::WHITE,
                        border: None,
                    },
                })
            ));
            order.push(lane);
        }
        scene.set_order(&order);
        let store = crate::paint::geometry_store::GeometryStore::new();
        let mut builder = DeltaBuilder::new();
        let mut composer = WindowComposer::new();
        let mut delta = SceneDelta::empty(header(1, Plan::Full, DevicePixelRatio::ONE));
        builder.collect(&mut scene, &store, &[], true, &mut delta);
        let _ = composer.compose(&delta, ColorResolver).expect("首帧");

        scene.set_order(&[order[2], order[0], order[1]]);
        let mut delta = SceneDelta::empty(header(
            2,
            Plan::Partial(DamageRects::new()),
            DevicePixelRatio::ONE,
        ));
        builder.collect(&mut scene, &store, &[], false, &mut delta);
        assert!(delta.order.is_some(), "仅顺序变化必须发布新的绘制序");
        assert!(delta.lanes.is_empty(), "仅顺序变化不得带 lane 内容");
        assert!(delta.removed.is_empty(), "仅顺序变化不得带移除通知");
        let composed = composer.compose(&delta, ColorResolver).expect("重排帧");
        assert_eq!(
            composer.encoder.fast_encodes(),
            0,
            "仅顺序变化必须走全量 ops 重建，而不是增量快路径"
        );
        assert_eq!(composed.stats.drawn_instances, 3);
        assert_eq!(
            composed.stats.reused_segments, 3,
            "仅顺序变化只重排 ops：三条 lane 的实例段都应复用"
        );
        let orders: Vec<(i32, i32)> = composer
            .buffer()
            .ops()
            .iter()
            .map(|op| match *op {
                crate::paint::instance::FrameOp::Instances { bounds, .. } => {
                    (bounds.x0.value(), bounds.x1.value())
                }
                crate::paint::instance::FrameOp::Geometry { .. } => panic!("没有几何 op"),
            })
            .collect();
        assert_eq!(
            orders,
            vec![(18, 26), (-2, 16)],
            "ops 必须按新的绘制序分组：lane2 独占一组，lane0+lane1 连续成第二组（旧序只会是一组）：{orders:?}"
        );
    }

    #[test]
    fn a_shrinking_relayout_survives_two_full_frames_being_coalesced() {
        const LANES: usize = 4;
        let mut fixture = Fixture::new(LANES);
        let first = fixture.frame(Plan::Full, true, &[]);
        assert!(first.full);
        let older = fixture.compose(&first).payload;
        assert_eq!(older.instance_count(), LANES as u32);

        assert!(fixture.scene.clear_lane(fixture.lane));
        let shrunk = fixture.frame(Plan::Full, true, &[]);
        assert!(shrunk.full, "重排后的帧必须整帧发布");
        let newer = fixture.compose(&shrunk).payload;
        assert_eq!(
            newer.instance_count(),
            LANES as u32 - 1,
            "清空一条 lane ⇒ 实例环缩小"
        );

        let mut slot = LatestFrame::new();
        slot.publish(FrameRequest::from_header(
            header(1, Plan::Full, DevicePixelRatio::ONE),
            older,
        ));
        slot.publish(FrameRequest::from_header(
            header(2, Plan::Full, DevicePixelRatio::ONE),
            newer,
        ));
        let merged = slot.take().expect("latest frame").payload;
        let mut mirror = InstanceMirror::new(MAX_FRAMES_IN_FLIGHT);
        assert!(mirror.prepare(0, &merged), "合并后的载荷必须能写进实例环");
        assert_eq!(
            mirror.bytes().len(),
            (LANES - 1) * 128,
            "镜像只覆盖新的实例环"
        );
    }
}

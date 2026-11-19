//! 每窗口的**合成器**（`WindowComposer`）：把一条 `SceneDelta` 变成一份 `FramePayload`。
//!
//! 这是 B6-2 的落位点：**编码不再发生在 UI 线程**。本类型是线程无关的纯组件 ——
//!
//! - **inline 模式**（headless / 离屏 / 单测）：`Window` 直接持有它，`paint_frame` 里同步合成；
//! - **threaded 模式**（真实窗口）：它被移交给合成线程（`render/compositor_thread.rs`），
//!   UI 线程只发布 delta。
//!
//! 两种模式共用同一份代码与同一份产物：全量与增量的实例字节逐字节一致
//! （`InstanceEncoder` 的两条路径同一个 `LaneEncoder`）。
//!
//! ## 为什么合成线程只做「编码之外」的合成工作
//!
//! 像素生成（lane 重建）仍在 UI 线程 —— lane 内容是 `Scene` 的唯一真相，搬到别的线程只会引入
//! 双写。合成线程拿到的是**内容快照**（`Arc<[Primitive]>`），它做的是「把快照变成 GPU 字节」：
//! 版本比对 → 只重编码变化 lane → 构造增量载荷 → 分发。这与 Chrome 的
//! `LayerTreeHost`（UI）→ `LayerTreeHostImpl`（impl 线程）分工一致。

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

/// 一帧合成之后的产物：damage 计划（合成线程可能并入滚动暴露区）+ 载荷。
pub(crate) struct ComposedFrame {
    pub plan: Plan,
    pub payload: FramePayload,
    pub stats: ComposeStats,
}

/// 本帧合成的可观测计数（HUD / `FrameRecord` / 测试）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ComposeStats {
    /// 本帧重编码的 lane 数（`encoded_lanes`；滚动帧必须是 0）。
    pub encoded_lanes: usize,
    /// 本帧整段复用的 lane 段数。
    pub reused_segments: usize,
    /// 本帧写进实例环的字节数。
    pub written_bytes: usize,
    /// 本帧重编码的几何块 op 数（X5：增量几何的断言面）。
    pub geometry_encoded_chunks: usize,
    /// 本帧 GPU 侧实际上传的几何字节数（X5；渲染器路径回填，headless 恒 0）。
    pub geometry_uploaded_bytes: usize,
    /// 实例环总实例数（含空洞）。
    pub instance_count: u32,
    /// 本帧绘制序真正引用的实例数（不含空洞）。
    pub drawn_instances: u32,
    /// 本帧滚动偏移变化的槽数（`FrameStats.scroll_offset_frames` 的来源）。
    pub scroll_slots: usize,
}

/// 每窗口的合成器状态（`InstanceEncoder` 的唯一持有者）。
pub(crate) struct WindowComposer {
    encoder: InstanceEncoder,
    lanes: LaneStore,
    geometry: GeometryCatalog,
    /// 几何快照的页级上传计划器（X5）：只用于 **CPU 侧计数**（headless 绝对可靠；真实窗口的
    /// GPU 线程各持一份自己的 tracker 做实际上传）。
    geometry_uploads: GeometryUploadTracker,
    scroll: ScrollTable,
    stats: ComposeStats,
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
        }
    }

    /// 最近一帧的编码产物（HUD / 诊断 / 测试：`Window::instance_buffer` 的唯一来源）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn buffer(&self) -> &crate::paint::instance::InstanceBuffer {
        self.encoder.buffer()
    }

    /// 合成一帧：应用 delta → （必要时）增量编码 → 构造载荷。
    ///
    /// **没有 lane 级变化时不重编码**（滚动帧 / 纯 damage 帧）：实例环的段与 op 表整份沿用上一
    /// 帧，载荷里 `chunks` 为空、只有滚动偏移表与 damage —— 这正是「滚动帧 `encoded_lanes == 0`」
    /// 的实现点。
    ///
    /// # Errors
    /// [`EngineError::ClipDepthExceeded`]：剪裁链超过 `MAX_CLIP_DEPTH`（内容错误，必须冒泡给
    /// 应用；静默丢 clip 会让子内容越界可见）。
    pub(crate) fn compose(
        &mut self,
        delta: &SceneDelta,
        resolver: ColorResolver,
    ) -> Result<ComposedFrame, EngineError> {
        let scale = delta.header.scale;
        // 已释放的 lane：立刻回收实例段（否则要等 arena 槽位被复用）。
        for lane in &delta.removed {
            self.encoder.release_lane(*lane);
        }
        let lanes_changed = self.lanes.apply(delta);
        self.geometry.apply(delta);
        let scroll_slots = self.scroll.apply(&delta.scroll, scale).len();
        // 滚动暴露区必须补画：合成端偏移改变了 op 落在屏幕上的位置，旧像素作废。
        let viewports: Vec<_> = delta
            .scroll
            .iter()
            .filter_map(|scroll| scroll_damage(scroll.viewport))
            .collect();
        let plan = push_scroll_damage(delta.header.plan, &viewports);
        if delta.full {
            // 整帧重建（首帧 / DPI 变化 / 裁剪表重建）：编码器必须丢弃全部段与裁剪表。
            self.encoder.invalidate();
        }
        if lanes_changed || delta.full || self.encoder.needs_reset(scale) {
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
            // 无 lane 变化：只刷新「本帧复用了多少段」的计数（编码器仍按帧统计）。
            self.stats.encoded_lanes = 0;
            // 没有任何 lane 变化 ⇒ 全部 lane 段整段复用（滚动帧的核心不变量）。
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
        // 整帧重建（首帧 / DPI 变化 / 新渲染器挂上）必须发**整块**实例缓冲：GPU 侧的实例镜像
        // 只会被载荷填满，不发全量它就永远缺内容（`InstanceMirror` 的 `needs_full` 只是「整块
        // 写镜像」，镜像本身仍要靠载荷喂）。
        let payload = if delta.full {
            let payload = FramePayload::full(self.encoder.buffer(), scroll_payload, scroll_clip);
            // 全量块已经包含全部变化 ⇒ 变化区间清零（否则下一次增量会把它们再发一遍）。
            self.encoder.buffer_mut().clear_changes();
            payload
        } else {
            FramePayload::from_buffer_delta(self.encoder.buffer_mut(), scroll_payload, scroll_clip)
        };
        // X5：几何页级上传计划（与 GPU 后端同一 diff 算法）。整帧重建（首帧 / 设备恢复 /
        // 新渲染器挂上）意味着 GPU 侧缓冲是空的 ⇒ tracker 清空后本帧整份重传。
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
    use crate::math::Size;
    use crate::{
        math::{PhysicalRect, Point, Rect, Scale},
        paint::{
            color::{Color, ColorResolver},
            primitive::{Primitive, Quad, QuadPaint},
            scene::Scene,
        },
        render::{
            compositor::{DamageRects, Plan},
            delta::{DeltaBuilder, FrameHeader, SceneDelta},
            frame::{InstanceMirror, MAX_FRAMES_IN_FLIGHT},
        },
        window::{FrameIndex, WindowId},
    };

    fn header(index: u32, plan: Plan, scale: Scale) -> FrameHeader {
        FrameHeader {
            window: WindowId::from_raw(1),
            index: FrameIndex::from_raw(index),
            plan,
            surface: (64, 64),
            scale,
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
            scroll: &[(Point, PhysicalRect)],
        ) -> SceneDelta {
            let index = self.next;
            self.next += 1;
            let mut delta = SceneDelta::empty(header(index, plan, Scale::ONE));
            self.builder
                .collect(&mut self.scene, &self.store, scroll, full, &mut delta);
            delta
        }

        /// 走**生产路径**采一帧：`full` 由 builder 自己的信号推出（首帧 / DPI 变化 /
        /// [`DeltaBuilder::force_full`]），而不是由调用方直接指定。
        ///
        /// 全量协议用例必须走这条路径：只有它能让「把消费端重置信号去掉 ⇒ 这一帧退化成增量」
        /// 的变异真正反映到载荷上（[`Fixture::frame`] 的调用方直接给 `full`，测不出信号链）。
        fn frame_with_signals(&mut self, plan: Plan) -> SceneDelta {
            let index = self.next;
            self.next += 1;
            let mut delta = SceneDelta::empty(header(index, plan, Scale::ONE));
            let full = self.builder.full_for(Scale::ONE);
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

    /// 首帧：全部 lane 编码，载荷覆盖整张实例环。
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

    /// 增量帧：只有变化的 lane 被重编码，载荷只带它。
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

    /// **滚动帧不重编码**：只有偏移变化 ⇒ `encoded_lanes == 0` 且载荷里没有实例块。
    #[test]
    fn a_scroll_frame_does_not_encode_anything() {
        let mut fixture = Fixture::new(4);
        let viewport = PhysicalRect::new(0, 0, 64, 64);
        let delta = fixture.frame(Plan::Full, true, &[(Point::new(0.0, 0.0), viewport)]);
        let first = fixture.compose(&delta);
        assert_eq!(first.stats.encoded_lanes, 4);
        assert!(first.payload.scroll().is_empty() || first.payload.scroll()[0] == [0.0, 0.0]);
        assert_eq!(
            fixture.composer.encoder.fast_encodes(),
            0,
            "首帧走全量扫描，不计入增量快路径"
        );

        let delta = fixture.frame(Plan::Idle, false, &[(Point::new(0.0, -20.0), viewport)]);
        assert!(
            delta.lanes.is_empty() && delta.order.is_none(),
            "滚动 delta 不得带 lane"
        );
        let scrolled = fixture.compose(&delta);
        assert_eq!(scrolled.stats.encoded_lanes, 0, "滚动帧不得重编码任何 lane");
        // 门禁 `lanes_changed || delta.full || needs_reset` 为假 ⇒ 压根不进入 `encode()`。
        // `encode()` 一旦被调用，即使走增量快路径也会让 `fast_encodes` +1 —— 所以这条断言
        // 守的是门禁本身（复审 §7 的 M6/M11：把门禁改成 `if true` 时本用例必须失败）。
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
        // damage 必须覆盖视口（否则 GPUs 侧 scissor 会把整帧裁掉）。
        let Plan::Partial(rects) = scrolled.plan else {
            panic!("滚动帧的 damage 必须是 Partial，实际 {:?}", scrolled.plan);
        };
        assert_eq!(rects.as_slice(), &[viewport]);
    }

    /// 滚动帧的实例字节与「不滚动」时逐字节一致（偏移在绘制期才生效）。
    #[test]
    fn scrolling_never_touches_instance_bytes() {
        let mut moving = Fixture::new(3);
        let viewport = PhysicalRect::new(0, 0, 64, 64);
        let delta = moving.frame(Plan::Full, true, &[(Point::new(0.0, 0.0), viewport)]);
        let before = moving.compose(&delta).stats;
        // 重新采一帧（无变化）作为基线。
        let delta = moving.frame(Plan::Partial(DamageRects::new()), false, &[]);
        let baseline = moving.compose(&delta);
        assert_eq!(baseline.stats.encoded_lanes, 0);
        assert!(baseline.payload.chunks().is_empty());
        assert_eq!(before.drawn_instances, baseline.stats.drawn_instances);
        // 滚动 30 px：实例块仍然为空，说明实例数据没有被重写。
        let delta = moving.frame(Plan::Idle, false, &[(Point::new(0.0, -30.0), viewport)]);
        let scrolled = moving.compose(&delta);
        assert!(scrolled.payload.chunks().is_empty());
        assert_eq!(scrolled.stats.encoded_lanes, 0);
    }

    /// DPI 变化：整帧重编码（物理坐标全变）。
    #[test]
    fn a_scale_change_reencodes_every_lane() {
        let mut fixture = Fixture::new(3);
        let delta = fixture.frame(Plan::Full, true, &[]);
        let _ = fixture.compose(&delta);
        // 2× 的整帧 delta：`full_for` 判定 DPI 变化 ⇒ 采集全部 lane。
        let mut delta = SceneDelta::empty(header(9, Plan::Full, Scale::new(2.0).expect("scale")));
        let full = fixture.builder.full_for(Scale::new(2.0).expect("scale"));
        assert!(full, "DPI 变化必须整帧重建");
        fixture
            .builder
            .collect(&mut fixture.scene, &fixture.store, &[], full, &mut delta);
        delta.full = true;
        let composed = fixture.compose(&delta);
        assert_eq!(composed.stats.encoded_lanes, 3);
    }

    /// `Arc` 复用：未变化的 lane 不重建快照（滚动帧零分配的前提）。
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

    /// **回归防线（op 包围盒不能沿用旧的）**：实例数不变、但绘制项变了（位置移动）时，
    /// 编码器必须重建 op 表 —— op 的实例包围盒会参与 damage 的 op 级跳过，沿用旧值会漏画。
    #[test]
    fn moving_a_lane_with_the_same_instance_count_rebuilds_the_op_bounds() {
        let mut fixture = Fixture::new(2);
        let delta = fixture.frame(Plan::Full, true, &[]);
        let _ = fixture.compose(&delta);
        assert!(fixture.scene.clear_lane(fixture.lane));
        assert!(fixture.scene.push(
            fixture.lane,
            Primitive::Quad(Quad {
                bounds: Rect::new(400.0, 0.0, 4.0, 4.0),
                corner_radius: [0.0; 4],
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
            "op 包围盒必须包含移动后的位置（实际 x1={rightmost}）"
        );
    }

    /// **性能回归防线**：第二帧起必须走编码器的增量快路径（只处理变化 lane、沿用 op 表）。
    /// 快路径被悄悄关掉，就是每帧 10k 条 lane 的全量扫描 —— 那正是要守的回退。
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

    /// **回归防线（合成线程 / `SceneDelta` 落地后不许退化）**：10k lane 的场景里只有 1 条 lane
    /// 变脏的一帧，必须「只重编码 1 条 lane、其余每条 lane 整段复用、只上传 1 个实例」。
    ///
    /// 这条用例守护的是「增量编码被合成线程接线顺手改坏」的整类回归：一旦 `WindowComposer` 在
    /// 应用 delta 之后把版本判据弄丢（例如把 `LaneStore` 的版本当成 `Scene` 的版本、或每次
    /// 都走全量重建），`encoded_lanes` 会立刻变成 10k，而不是 1。
    #[test]
    fn one_dirty_lane_never_reencodes_the_whole_scene() {
        const LANES: usize = 10_000;
        let mut fixture = Fixture::new(LANES);
        let delta = fixture.frame(Plan::Full, true, &[]);
        let first = fixture.compose(&delta);
        assert_eq!(first.stats.encoded_lanes, LANES, "首帧必须全量编码");

        // 把那条 lane 改成一个实例（与 `frame_incremental_*` 的夹具一致：脏 lane 恰好 1 个实例）。
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

    /// **等价性**：同一份场景，UI 线程直接编码（`Scene` 作帧源）与合成线程从 `LaneStore` 编码
    /// （`SceneDelta` 应用之后）必须产出**逐字节一致**的实例环与同一份绘制序。
    ///
    /// 这是「把编码搬线程」的安全网：搬迁只改编码发生的位置，不改编码结果。
    #[test]
    fn encoding_the_same_scene_from_a_delta_is_byte_identical() {
        use crate::paint::instance::InstanceEncoder;

        let mut fixture = Fixture::new(6);
        // 让场景里出现多样的图元（圆角、渐变、clip），覆盖两条 clip 路径。
        assert!(fixture.scene.push(fixture.lane, quad()));
        let delta = fixture.frame(Plan::Full, true, &[]);
        let composed = fixture.compose(&delta);
        drop(composed);

        // UI 线程路径：同一个 `InstanceEncoder` 直接吃 `Scene`。
        let mut direct = InstanceEncoder::new();
        direct
            .encode(&fixture.scene, Scale::ONE, ColorResolver, &fixture.store)
            .expect("direct encode");
        // 合成线程路径：composer 的编码器吃 `LaneStore`（delta 应用之后）。
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
        // 再跑一帧增量：仍然一致。
        assert!(fixture.scene.push(fixture.lane, quad()));
        let delta = fixture.frame(Plan::Partial(DamageRects::new()), false, &[]);
        let composed = fixture.compose(&delta);
        direct
            .encode(&fixture.scene, Scale::ONE, ColorResolver, &fixture.store)
            .expect("direct encode");
        assert_eq!(
            direct.buffer().as_bytes(),
            fixture.composer.buffer().as_bytes()
        );
        assert_eq!(composed.stats.encoded_lanes, 1);
    }

    /// **AR4 P1-1 验收（复审 §8.1 原话）**：消费端实例镜像重建之后的第一帧必须是整帧，并且把
    /// 它写回重建后的镜像时，内容与重建前**逐字节一致**。
    ///
    /// 场景与生产同构：
    /// - `InstanceMirror::new(MAX_FRAMES_IN_FLIGHT)` 是 Windows `recover()` 里镜像重建的等价物
    ///   （`render/windows/compositor.rs`），重建后镜像里没有任何内容；
    /// - `DeltaBuilder::force_full()` 是 UI 线程在每帧开始前观察到 `FrameSink::reset_epoch`
    ///   变化（设备恢复 / 新渲染器挂上）时置位的信号，由 `full_for` 消费一次。
    ///
    /// 没有这条握手，恢复后的第一帧只发「变化 lane」，镜像里其余段是 0 ⇒ 退化实例。
    /// 本用例同时带变异守卫：不置信号的那一帧必须是增量，且把增量载荷喂给空镜像之后内容与
    /// 重建前**不一致** —— 去掉 full 信号，本用例必红。
    #[test]
    fn the_first_frame_after_a_rebuild_refills_the_mirror_byte_identically() {
        const LANES: usize = 4;
        let mut fixture = Fixture::new(LANES);
        let mut mirror = InstanceMirror::new(MAX_FRAMES_IN_FLIGHT);

        // 1. 首帧：`full_for` 天然为真（合成侧还没有任何 lane）⇒ 载荷是覆盖整张环的整块。
        let first = fixture.frame_with_signals(Plan::Full);
        assert!(first.full, "首帧必须整帧发布");
        let composed = fixture.compose(&first);
        assert!(
            mirror.prepare(0, &composed.payload),
            "从未写过的槽必须整块写"
        );
        mirror.finish(0);
        assert_eq!(mirror.bytes().len(), LANES * 128, "镜像 = 整张实例环");

        // 2. 改一条 lane 的增量帧：载荷只带那一段（逐帧增量协议真正生效）。
        //    clear + push 保持实例数不变 ⇒ 编码器原地复用该 lane 的段（镜像布局不重排）。
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
        // 「重建前的内容」：镜像此刻与场景逐字节一致。
        let before = mirror.bytes().to_vec();

        // 3. 消费端重置：GPU 侧镜像被重建（`recover()` 的等价物）+ UI 侧整帧信号。
        let mut rebuilt = InstanceMirror::new(MAX_FRAMES_IN_FLIGHT);
        assert!(rebuilt.bytes().is_empty(), "重建后的镜像没有任何内容");
        fixture.builder.force_full();
        assert!(fixture.builder.forced_full(), "信号必须先置位");

        // 4. 重建后的第一帧：信号被 `full_for` 取走 ⇒ 整帧发布 ⇒ 整块载荷。
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

        // 5. 变异守卫：同一场景里**不**置 `force_full`，那一帧只能是增量 —— 而且把这份增量
        //    载荷喂给空镜像之后，内容与重建前不一致。这就是「把 full 信号去掉 ⇒ 本用例必红」
        //    的落点：它把「增量载荷喂给空镜像 ⇒ 内容缺失」写死成事实。
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
}

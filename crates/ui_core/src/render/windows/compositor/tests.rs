//! `GpuRenderer` 的单元测试（拆分自 `compositor.rs`，测试代码未改动）。

use super::{
    GpuRenderer,
    persist::rtv_handle,
    plan::TargetState,
    record::{geometry_root, root_uniform},
};
use crate::{
    error::EngineError,
    math::{PhysicalRect, Point, Rect, Scale, Size, Transform},
    paint::{
        atlas::{AtlasKind, AtlasTile},
        color::{Color, ColorResolver},
        geometry::{GeometryId, GeometryPipeline, IndexType, Topology},
        instance::{FrameOp, GeometryDrawGpu, InstanceEncoder},
        primitive::{GeometryDraw, GlyphRun, Image, PositionedGlyph, Primitive, Quad, QuadPaint},
        scene::Scene,
    },
    render::{
        compositor::{FramePlan, Plan, plan},
        damage::DamageSummary,
        timer::FrameTimings,
    },
    window::FrameIndex,
};

fn lock() -> std::sync::MutexGuard<'static, ()> {
    crate::render::windows::device::gpu_test_lock()
}

/// WARP 设备不可用时**跳过并诊断**（设计允许；设备被移除过的进程里 WARP 创建可能失败）。
fn warp_renderer(size: (u32, u32)) -> Option<GpuRenderer> {
    match GpuRenderer::new_warp_offscreen(size) {
        Ok(renderer) => Some(renderer),
        Err(error) => {
            eprintln!("skip: WARP renderer unavailable: {error}");
            None
        }
    }
}

/// 关窗不泄漏：渲染器销毁后，它注册进共享设备句柄表的资源必须全部注销。
///
/// 进程级设备不会随窗口销毁，所以 persist / 上传缓冲 / staging / 几何缓冲的句柄必须在
/// `GpuRenderer::drop` 里主动交回（否则每关一个窗口就泄漏一份显存）。
#[test]
fn dropping_a_renderer_releases_its_shared_device_resources() {
    let _guard = lock();
    let shared = crate::render::windows::device::SharedGpu::new_warp().expect("WARP shared gpu");
    assert_eq!(shared.live_resources(), 0, "共享 GPU 创建时不注册资源");
    {
        let mut renderer =
            GpuRenderer::with_shared(std::sync::Arc::clone(&shared), (16, 16)).expect("renderer");
        let mut rows = crate::render::DirtyRows::default();
        rows.mark(0, u32::MAX);
        renderer.queue_atlas_upload(
            AtlasKind::Glyph,
            super::TEST_ATLAS_PAGE,
            vec![0u8; (super::TEST_ATLAS_PAGE * super::TEST_ATLAS_PAGE) as usize],
            rows,
        );
        let scene = Scene::empty();
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder
            .encode(
                &scene,
                Scale::ONE,
                resolver(),
                &crate::paint::geometry_store::GeometryStore::new(),
            )
            .expect("encode");
        let mut frame = full_plan((16, 16));
        let mut timings = FrameTimings::new();
        renderer
            .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
            .expect("submit");
        assert!(
            shared.live_resources() > 0,
            "渲染器必须持有 persist / upload / staging 资源"
        );
    }
    assert_eq!(
        shared.live_resources(),
        0,
        "关窗后渲染器注册的所有资源句柄必须注销"
    );
}

/// 多窗口共享契约：两个渲染器共享同一个进程级 `SharedGpu`，但在共享 SRV 堆里各有独立段。
#[test]
fn offscreen_renderers_share_the_process_gpu() {
    let _guard = lock();
    let Some(first) = warp_renderer((8, 8)) else {
        return;
    };
    let Some(second) = warp_renderer((8, 8)) else {
        return;
    };
    assert!(
        std::sync::Arc::ptr_eq(&first.shared, &second.shared),
        "两个窗口必须共享同一个 SharedGpu（device / queue / PSO / 图集纹理）"
    );
    assert_ne!(
        first.descriptors.base(),
        second.descriptors.base(),
        "每个窗口在共享 SRV 堆里必须有各自的段"
    );
}

/// 两个窗口并发录制 + 提交到同一条进程级队列：各自的 persist / readback 互不干扰。
///
/// 两张图集纹理是共享的，所以两个线程上传**同一页的相同内容**：共享纹理的最终内容因此与
/// 两个线程的提交顺序无关（跨窗口图集并发上传的正确性论证见 `windows/atlas.rs` 模块文档）。
#[test]
fn two_renderers_render_concurrently_on_one_shared_gpu() {
    let _guard = lock();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let page = super::TEST_ATLAS_PAGE;
    let mut threads = Vec::with_capacity(2);
    for _ in 0..2 {
        let barrier = std::sync::Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            let mut renderer =
                GpuRenderer::new_warp_offscreen((8, 8)).expect("shared WARP renderer");
            let mut pixels = vec![0u8; (page * page) as usize];
            for row in pixels.chunks_mut(page as usize) {
                row.fill(0x40);
            }
            let mut rows = crate::render::DirtyRows::default();
            rows.mark(0, u32::MAX);
            renderer.queue_atlas_upload(AtlasKind::Glyph, page, pixels, rows);

            let text_free = one_lane_scene(quad(
                Rect::new(2.0, 2.0, 2.0, 2.0),
                Color::srgb(1.0, 0.0, 0.0, 1.0),
            ));
            let mut encoder = InstanceEncoder::new();
            let buffer = encoder
                .encode(
                    &text_free,
                    Scale::ONE,
                    resolver(),
                    &crate::paint::geometry_store::GeometryStore::new(),
                )
                .expect("encode");
            // 尽量让两个线程的录制 / 提交重叠（真正的并发点）。
            barrier.wait();
            let mut timings = FrameTimings::new();
            for frame in 1..=6u32 {
                let mut plan = full_plan((8, 8));
                renderer
                    .submit_buffer(FrameIndex::from_raw(frame), buffer, &mut plan, &mut timings)
                    .expect("concurrent submit");
            }
            let image = renderer.readback().expect("readback");
            assert_eq!(pixel(&image, 8, 3, 3), [255, 0, 0, 255]);
            assert_eq!(pixel(&image, 8, 0, 0), [0, 0, 0, 0]);
        }));
    }
    for thread in threads {
        thread.join().expect("concurrent gpu thread must not panic");
    }
}
fn resolver() -> ColorResolver {
    ColorResolver
}

fn full_plan(size: (u32, u32)) -> FramePlan {
    FramePlan::new(
        plan(
            DamageSummary {
                full: true,
                rects: 0,
            },
            &[],
            PhysicalRect::new(0, 0, size.0 as i32, size.1 as i32),
        ),
        true,
    )
}

fn partial_plan(rects: &[PhysicalRect], surface: (u32, u32)) -> FramePlan {
    FramePlan::new(
        plan(
            DamageSummary {
                full: false,
                rects: rects.len(),
            },
            rects,
            PhysicalRect::new(0, 0, surface.0 as i32, surface.1 as i32),
        ),
        false,
    )
}

fn one_lane_scene(primitive: Primitive) -> Scene {
    let mut scene = Scene::empty();
    let lane = scene.new_lane();
    assert!(scene.push(lane, primitive));
    scene.set_order(&[lane]);
    scene
}

fn quad(bounds: Rect, fill: Color) -> Primitive {
    Primitive::Quad(Quad {
        bounds,
        corner_radius: [0.0; 4],
        paint: QuadPaint::Solid { fill, border: None },
    })
}

fn pixel(image: &super::ReadbackImage, width: u32, x: u32, y: u32) -> [u8; 4] {
    let index = ((y * width + x) * 4) as usize;
    [
        image.rgba8[index],
        image.rgba8[index + 1],
        image.rgba8[index + 2],
        image.rgba8[index + 3],
    ]
}

#[test]
fn root_uniform_matches_the_contract() {
    let uniform = root_uniform(800.0, 600.0, 1.25);
    let words = uniform.words();
    assert_eq!(words[0], 800.0);
    assert_eq!(words[1], 600.0);
    assert_eq!(words[2], 2.0 / 800.0);
    assert_eq!(words[3], 2.0 / 600.0);
    assert_eq!(words[4], 1.25, "frame.params.x 是 DPI scale");
    assert_eq!(words[8], 1.0);
    assert_eq!(words[11], 1.0);
    assert_eq!(words.len(), 28);
}

/// P0-1.1 回归：实例段起点只经 `draw.clip.y` 进入 shader，`DrawInstanced` 的
/// `StartInstanceLocation` 恒 0（D3D12 不会把它加到 `SV_InstanceID` 上）。
///
/// 这里扫描本文件源码做防回归：断言字面量用拼接构造，避免断言本身成为「禁止串」的来源。
#[test]
fn instance_segments_use_the_uniform_base_not_the_hardware_base() {
    let uniform = root_uniform(64.0, 64.0, 1.0).with_instance_base(7);
    assert_eq!(uniform.words()[25], 7.0, "draw.clip.y = instance_base");

    let source = include_str!("record.rs");
    let call = ["Draw", "Instanced(4, len, 0, "].concat();
    assert!(
        source.contains(&format!("{call}0)")),
        "硬件实例基址必须恒 0（D3D12 的 StartInstanceLocation 不加到 SV_InstanceID）"
    );
    assert!(
        !source.contains(&format!("{call}start)")),
        "回退到硬件基址会让第二段之后的实例全部错位"
    );
}

/// 测试用的最小几何描述（`draw_ops` 只读 `id`/`transform`/`color`，描述由 registry 提供）。
fn test_desc() -> crate::paint::geometry::GeometryDesc {
    crate::paint::geometry::GeometryDesc {
        vertex_bytes: bytes::Bytes::new(),
        index_bytes: bytes::Bytes::new(),
        vertex_stride: 12,
        index_type: IndexType::U16,
        topology: Topology::TriangleList,
        bounds: Rect::new(0.0, 0.0, 1.0, 1.0),
        pipeline: GeometryPipeline::Ui,
    }
}

/// 逐 dword 对照 `common.hlsl` 的 `GeometryDrawUniform`（B1）。
#[test]
fn geometry_root_matches_common_hlsl_layout() {
    let frame = root_uniform(800.0, 600.0, 1.0);
    let clip_rect = [4.0f32, 5.0, 100.0, 120.0].map(f32::to_bits);
    let draw = GeometryDrawGpu {
        id: GeometryId::for_test(0, 1),
        transform: Transform::translate(11.0, 13.0),
        color: resolver().resolve(Color::WHITE).to_array(),
        desc: std::sync::Arc::new(test_desc()),
    };
    let uniform = geometry_root(frame, draw, 3, clip_rect);
    let words = uniform.words();
    assert_eq!(
        &words[..8],
        &frame.words()[..8],
        "frame block is copied verbatim"
    );
    // draw.basis @8：translate 的线性部分是单位矩阵。
    assert_eq!(&words[8..12], &[1.0, 0.0, 0.0, 1.0]);
    // draw.translation @12
    assert_eq!(&words[12..14], &[11.0, 13.0]);
    // draw.color0 @16 = premultiplied white
    assert_eq!(&words[16..20], &[1.0, 1.0, 1.0, 1.0]);
    // draw.clip_rect @20 = (x, y, w, h)
    assert_eq!(&words[20..24], &[4.0, 5.0, 96.0, 115.0]);
    // draw.clip @24 = clip_index（数值编码，不是位重解释）
    assert_eq!(words[24], 3.0);
    // draw.clip.y @25 = instance_base（几何绘制恒 0）
    assert_eq!(words[25], 0.0);
    assert_eq!(words[26..].iter().copied().sum::<f32>(), 0.0);
}

/// P0-1.2 回归：几何绘制必须消费 `Path.fill` / `GeometryDraw.color` 与 `transform`。
#[test]
fn geometry_root_consumes_transform_and_color() {
    let frame = root_uniform(64.0, 32.0, 2.0);
    let draw = GeometryDrawGpu {
        id: GeometryId::for_test(0, 1),
        transform: Transform::scale(3.0, 4.0),
        color: resolver()
            .resolve(Color::srgb(1.0, 0.0, 0.0, 1.0))
            .to_array(),
        desc: std::sync::Arc::new(test_desc()),
    };
    let uniform = geometry_root(frame, draw, 0, [0; 4]);
    let words = uniform.words();
    assert_eq!(&words[8..12], &[3.0, 0.0, 0.0, 4.0], "transform 进 basis");
    assert!(
        words[16] > 0.0 && words[17] == 0.0 && words[18] == 0.0,
        "红色填充必须进 color0（linear premultiplied）"
    );
    assert_eq!(words[19], 1.0, "不透明 alpha");
    assert_eq!(words[4], 2.0, "DPI scale 进 frame.params.x");
}

#[test]
fn geometry_root_without_clip_uses_the_viewport() {
    let frame = root_uniform(64.0, 32.0, 1.0);
    let draw = GeometryDrawGpu {
        id: GeometryId::for_test(0, 1),
        transform: Transform::IDENTITY,
        color: resolver().resolve(Color::WHITE).to_array(),
        desc: std::sync::Arc::new(test_desc()),
    };
    let uniform = geometry_root(frame, draw, 0, [0; 4]);
    let words = uniform.words();
    assert_eq!(&words[20..24], &[0.0, 0.0, 64.0, 32.0]);
    assert_eq!(words[24], 0.0);
}

#[test]
fn instance_bytes_are_128_per_primitive() {
    let scene = one_lane_scene(quad(Rect::new(0.0, 0.0, 8.0, 8.0), Color::WHITE));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    assert_eq!(buffer.as_bytes().len(), 128);
    assert_eq!(buffer.ops().len(), 1);
    assert!(matches!(
        buffer.ops()[0],
        FrameOp::Instances {
            start: 0,
            len: 1,
            ..
        }
    ));
}

#[test]
fn clear_half_encodes_once_to_srgb() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((8, 8)) else {
        return;
    };
    renderer.set_background_color([0.5, 0.5, 0.5, 0.5]);
    let scene = Scene::empty();
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut frame = full_plan((8, 8));
    let mut timings = FrameTimings::new();
    renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
        .expect("submit");
    let image = renderer.readback().expect("readback");
    // probe：persist 资源 UNORM + RTV 视图 SRGB ⇒ clear(0.5) 的 RGB 只编码一次 = 188；
    // alpha 不参与 sRGB 变换（0.5 → 128）。
    assert_eq!(pixel(&image, 8, 4, 4), [188, 188, 188, 128]);
}

/// 背景色改动只标脏；离屏目标没有背景层，`refresh_background` 必须消费标记（不能永真）。
#[test]
fn a_background_change_marks_the_layer_dirty_until_the_refresh() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((16, 16)) else {
        return;
    };
    assert!(!renderer.background_dirty);
    renderer.set_background_color([0.1, 0.2, 0.3, 1.0]);
    assert!(renderer.background_dirty, "改色必须请求背景层重建");
    renderer
        .refresh_background()
        .expect("an offscreen renderer has no background layer to rebuild");
    assert!(
        !renderer.background_dirty,
        "没有背景层的目标也必须消费标记，否则每帧都会尝试重建"
    );
    // 同值 no-op：没有新改动时 refresh 不产生任何工作。
    renderer
        .refresh_background()
        .expect("a clean background layer is a no-op");
}

#[test]
fn quad_pixels_are_srgb_encoded_and_clipped_by_bounds() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    let scene = one_lane_scene(quad(
        Rect::new(16.0, 16.0, 16.0, 16.0),
        Color::srgb(1.0, 0.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut frame = full_plan((64, 64));
    let mut timings = FrameTimings::new();
    let stats = renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
        .expect("submit");
    assert_eq!(stats.draw_calls, 1, "one non-empty instance op = one draw");
    assert_eq!(stats.instances, 1);
    let image = renderer.readback().expect("readback");
    assert_eq!(pixel(&image, 64, 24, 24), [255, 0, 0, 255]);
    assert_eq!(pixel(&image, 64, 4, 4), [0, 0, 0, 0]);
}

/// 白底 + 红 damage：两帧都是非空场景，damage 之外的蓝块必须原样保留（B5）。
#[test]
fn draw_calls_follow_full_and_partial_scissor_passes() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    let scene = one_lane_scene(quad(
        Rect::new(0.0, 0.0, 64.0, 64.0),
        Color::srgb(1.0, 0.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut timings = FrameTimings::new();

    let mut first = full_plan((64, 64));
    let stats = renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut first, &mut timings)
        .expect("full frame");
    assert_eq!(stats.draw_calls, 1, "full frame has one scissor pass");

    let damage = [
        PhysicalRect::new(0, 0, 32, 64),
        PhysicalRect::new(32, 0, 64, 64),
    ];
    let mut second = partial_plan(&damage, (64, 64));
    let stats = renderer
        .submit_buffer(FrameIndex::from_raw(2), buffer, &mut second, &mut timings)
        .expect("partial frame");
    assert_eq!(
        stats.draw_calls, 2,
        "op 覆盖整个窗口 ⇒ 两个 damage 都与它相交，各画一次（裁剪不适用）"
    );
}

/// P1-2.1 回归：与 damage 不相交的 op 必须被跳过。
///
/// 旧实现对每个 damage 矩形重放**整份** op 列表 —— 这正是实测中
/// `frame_incremental_512`（288 µs）比 `frame_full_512`（222 µs）更慢的原因。
///
/// op 的边界是 **run**（连续实例段），不是单个图元：同 clip 链的相邻图元会合并进同一 op。
/// 这里用一个真实的几何绘制打断 run，让左右两个 quad 落在两个独立的 `Instances` op 里，
/// 才能观察到裁剪（否则两个 quad 共用一个 op，其并集包围盒覆盖整个窗口，无从裁起）。
#[test]
fn ops_outside_the_damage_are_skipped() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    let id = GeometryId::for_test(0, 0);
    #[rustfmt::skip]
    let vertices: [f32; 6] = [0.0, 0.0, 8.0, 0.0, 0.0, 8.0];
    let indices: [u16; 3] = [0, 1, 2];
    let vertex_bytes: Vec<u8> = vertices.iter().flat_map(|v| v.to_ne_bytes()).collect();
    let index_bytes: Vec<u8> = indices.iter().flat_map(|v| v.to_ne_bytes()).collect();
    renderer
        .register_geometry(
            id,
            &vertex_bytes,
            &index_bytes,
            GeometryPipeline::Ui,
            Topology::TriangleList,
            IndexType::U16,
        )
        .expect("register");

    let mut scene = Scene::empty();
    let lane = scene.new_lane();
    assert!(scene.push(
        lane,
        quad(
            Rect::new(0.0, 0.0, 32.0, 64.0),
            Color::srgb(1.0, 0.0, 0.0, 1.0),
        )
    ));
    // 几何 op 会 flush 当前实例段 ⇒ 后面那个 quad 落进独立的 op。
    assert!(scene.push(
        lane,
        Primitive::Geometry(GeometryDraw {
            id,
            transform: Transform::IDENTITY,
            color: Color::WHITE,
        })
    ));
    assert!(scene.push(
        lane,
        quad(
            // 与左 quad 留出 8 px 间隙：op 包围盒会外扩 1 px 吸收 AA，
            // 贴边的两个矩形仍会被判为相交（保守正确）。
            Rect::new(40.0, 0.0, 24.0, 64.0),
            Color::srgb(0.0, 0.0, 1.0, 1.0),
        )
    ));
    scene.set_order(&[lane]);
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut timings = FrameTimings::new();

    let mut first = full_plan((64, 64));
    let stats = renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut first, &mut timings)
        .expect("full frame");
    assert_eq!(stats.draw_calls, 3, "两个实例 op + 一个几何 op");

    // 只 damage 左半边 ⇒ 右边那个实例 op 与 damage 不相交，必须跳过。
    let before = crate::render::windows::diagnostics::snapshot();
    let mut second = partial_plan(&[PhysicalRect::new(0, 0, 32, 64)], (64, 64));
    let stats = renderer
        .submit_buffer(FrameIndex::from_raw(2), buffer, &mut second, &mut timings)
        .expect("partial frame");
    assert_eq!(
        stats.draw_calls, 2,
        "只有与 damage 相交的 op 才产生 draw call（旧的『整份重放』实现会是 3）"
    );
    // R08-13：局部帧实际遍历的 op 数必须少于「整份重放」的预算（3 op × 1 矩形）。
    let after = crate::render::windows::diagnostics::snapshot();
    assert_eq!(
        after.op_visits - before.op_visits,
        2,
        "只访问命中的 2 个 op"
    );
    assert_eq!(
        after.op_replay_budget - before.op_replay_budget,
        3,
        "整份重放的预算 = 3 op × 1 个 damage 矩形"
    );
}

/// R08-5：dev 构建开着调试层，渲染一帧之后 InfoQueue 里不得出现 CORRUPTION 级消息
/// （资源状态 / 描述符越界 / 命令列表误用都会在这一级报出来）。
///
/// ERROR 级不在这里断死：WARP / 驱动会在无关路径上打自己的错误（例如功能探测），把它们当
/// CI 失败源会让门禁随机红。计数仍进 `Diagnostics::debug_errors` 供现场判读。
#[test]
fn a_rendered_frame_produces_no_corruption_messages() {
    use crate::render::windows::debug;

    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    // 让这一帧真的走完整条路径：图集上传 + 全量帧 + 局部帧 + 读回。
    let page = super::TEST_ATLAS_PAGE;
    let mut pixels = vec![0u8; (page * page) as usize];
    for y in 1..11u32 {
        for x in 1..11u32 {
            pixels[(y * page + x) as usize] = 255;
        }
    }
    let mut rows = crate::render::DirtyRows::default();
    rows.mark(0, u32::MAX);
    renderer.queue_atlas_upload(AtlasKind::Glyph, page, pixels, rows);

    let scene = one_lane_scene(quad(
        Rect::new(0.0, 0.0, 32.0, 32.0),
        Color::srgb(1.0, 0.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut timings = FrameTimings::new();
    let mut full = full_plan((64, 64));
    renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut full, &mut timings)
        .expect("full frame");
    let mut partial = partial_plan(&[PhysicalRect::new(0, 0, 16, 16)], (64, 64));
    renderer
        .submit_buffer(FrameIndex::from_raw(2), buffer, &mut partial, &mut timings)
        .expect("partial frame");
    let _ = renderer.readback().expect("readback");

    // 先排空历史（其它测试的 WARP 消息），再看这一帧之后有没有 CORRUPTION。
    let _ = debug::drain_messages(renderer.shared.raw());
    let mut after = full_plan((64, 64));
    renderer
        .submit_buffer(FrameIndex::from_raw(3), buffer, &mut after, &mut timings)
        .expect("verify frame");
    let counts = debug::drain_messages(renderer.shared.raw());
    assert_eq!(
        counts.corruption, 0,
        "调试层报告了 CORRUPTION 级消息（资源状态 / 描述符 / 命令列表误用）"
    );
}

#[test]
fn damage_isolates_pixels_across_two_real_frames() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    renderer.set_background_color([1.0, 1.0, 1.0, 1.0]);
    let size = (64, 64);

    let blue = one_lane_scene(quad(
        Rect::new(32.0, 0.0, 32.0, 64.0),
        Color::srgb(0.0, 0.0, 1.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &blue,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut first = full_plan(size);
    let mut timings = FrameTimings::new();
    renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut first, &mut timings)
        .expect("first frame");

    let red = one_lane_scene(quad(
        Rect::new(0.0, 0.0, 32.0, 64.0),
        Color::srgb(1.0, 0.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &red,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let damage = [PhysicalRect::new(0, 0, 32, 64)];
    let mut second = partial_plan(&damage, size);
    assert!(matches!(second.plan(), Plan::Partial(_)));
    renderer
        .submit_buffer(FrameIndex::from_raw(2), buffer, &mut second, &mut timings)
        .expect("second frame");

    let image = renderer.readback().expect("readback");
    assert_eq!(
        pixel(&image, 64, 8, 32),
        [255, 0, 0, 255],
        "damage area repainted"
    );
    assert_eq!(
        pixel(&image, 64, 56, 32),
        [0, 0, 255, 255],
        "outside damage must keep the previous frame"
    );
}

/// geometry（`GeometryPipeline::Ui`）：白三角中心不透明、包围盒外保持 clear（B1）。
#[test]
fn geometry_ui_triangle_renders_white() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    let id = GeometryId::for_test(0, 0);
    // `Ui` 顶点 = position(f32×2) + 解析 AA 的覆盖率距离(f32)；-1 = 形状内部（R05-6）。
    #[rustfmt::skip]
    let vertices: [f32; 9] = [0.0, 0.0, -1.0, 32.0, 0.0, -1.0, 0.0, 32.0, -1.0];
    let indices: [u16; 3] = [0, 1, 2];
    let vertex_bytes: Vec<u8> = vertices.iter().flat_map(|v| v.to_ne_bytes()).collect();
    let index_bytes: Vec<u8> = indices.iter().flat_map(|v| v.to_ne_bytes()).collect();
    renderer
        .register_geometry(
            id,
            &vertex_bytes,
            &index_bytes,
            GeometryPipeline::Ui,
            Topology::TriangleList,
            IndexType::U16,
        )
        .expect("register");

    let scene = one_lane_scene(Primitive::Geometry(GeometryDraw {
        id,
        transform: Transform::IDENTITY,
        color: Color::WHITE,
    }));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut frame = full_plan((64, 64));
    let mut timings = FrameTimings::new();
    renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
        .expect("submit");
    let image = renderer.readback().expect("readback");
    assert_eq!(
        pixel(&image, 64, 6, 6),
        [255, 255, 255, 255],
        "inside the triangle"
    );
    assert_eq!(
        pixel(&image, 64, 48, 48),
        [0, 0, 0, 0],
        "outside the triangle"
    );
}

/// image（`GeometryPipeline::Textured` 的 quad 路径）：UV 采样命中上传的像素（B1/B5）。
#[test]
fn image_primitive_samples_the_image_atlas() {
    let _guard = lock();
    let page = super::TEST_ATLAS_PAGE;
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    let mut pixels = vec![0u8; (page * page * 4) as usize];
    // 整页纯蓝（BGRA，且图集 SRV 是 SRGB 视图）。
    for texel in pixels.as_chunks_mut::<4>().0 {
        texel[0] = 0xFF;
        texel[1] = 0x00;
        texel[2] = 0x00;
        texel[3] = 0xFF;
    }
    let mut rows = crate::render::atlas::DirtyRows::default();
    rows.mark(0, page);
    renderer.queue_atlas_upload(AtlasKind::Image, page, pixels, rows);

    let tile = AtlasTile {
        atlas: AtlasKind::Image,
        u0: 0.25,
        v0: 0.25,
        u1: 0.5,
        v1: 0.5,
        generation: 0,
    };
    let scene = one_lane_scene(Primitive::Image(Image {
        bounds: Rect::new(0.0, 0.0, 32.0, 32.0),
        tile,
        modulation: Color::WHITE,
    }));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut frame = full_plan((64, 64));
    let mut timings = FrameTimings::new();
    renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
        .expect("submit");
    let image = renderer.readback().expect("readback");
    assert_eq!(pixel(&image, 64, 16, 16), [0, 0, 255, 255], "blue texel");
    assert_eq!(
        pixel(&image, 64, 48, 16),
        [0, 0, 0, 0],
        "outside the image bounds"
    );
}

/// 在给定页尺寸上做一次「上传 → 画字形 → 读回」的可见性探测。
fn glyph_probe(page: u32) -> bool {
    let Some(mut renderer) = super::GpuRenderer::new_warp_offscreen((64, 64)).ok() else {
        return false;
    };
    let mut pixels = vec![0u8; (page * page) as usize];
    for y in 1..11u32 {
        for x in 1..11u32 {
            pixels[(y * page + x) as usize] = 255;
        }
    }
    let mut rows = crate::render::atlas::DirtyRows::default();
    rows.mark(0, page);
    renderer.queue_atlas_upload(AtlasKind::Glyph, page, pixels, rows);
    let tile = AtlasTile {
        atlas: AtlasKind::Glyph,
        u0: 1.0 / page as f32,
        v0: 1.0 / page as f32,
        u1: 11.0 / page as f32,
        v1: 11.0 / page as f32,
        generation: 0,
    };
    let scene = one_lane_scene(Primitive::GlyphRun(GlyphRun {
        color: Color::WHITE,
        glyphs: vec![PositionedGlyph {
            tile,
            position: Point::new(16.0, 16.0),
            size: Size::new(16.0, 16.0),
        }],
    }));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut frame = full_plan((64, 64));
    let mut timings = FrameTimings::new();
    if renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
        .is_err()
    {
        return false;
    }
    let Ok(image) = renderer.readback() else {
        return false;
    };
    pixel(&image, 64, 24, 24)[3] > 0
}

/// 字形像素级断言：把「tile 区域 255」的字形页上传，画一个只含该 tile 的 `GlyphRun`，
/// 断言覆盖度真的被采样到（V-64 的回归面；只看实例/UV 字段抓不到这类 bug）。
#[test]
fn glyph_run_samples_the_uploaded_glyph_page() {
    let _guard = lock();
    // 页尺寸扫描：64² 的测试页会掩盖分块/行距问题，产品页是 2048²。
    for page in [64u32, 256, 512, 1024, 2048] {
        let visible = glyph_probe(page);
        println!("page {page}: glyph visible = {visible}");
        assert!(visible, "page {page}: glyph coverage was not sampled");
    }
}
/// 奇数宽：读回的 row pitch 是 256 对齐，但输出必须逐像素紧凑（B5）。
#[test]
fn odd_surface_readback_has_no_row_padding() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((63, 17)) else {
        return;
    };
    let scene = Scene::empty();
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut frame = full_plan((63, 17));
    let mut timings = FrameTimings::new();
    renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
        .expect("submit");
    let image = renderer.readback().expect("readback");
    assert_eq!((image.width, image.height), (63, 17));
    assert_eq!(image.rgba8.len(), 63 * 17 * 4);
    assert_eq!(pixel(&image, 63, 62, 16), [0, 0, 0, 0]);
    assert_eq!(pixel(&image, 63, 0, 16), [0, 0, 0, 0]);
}

#[test]
fn resize_then_submit_keeps_the_capacity_contract() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((32, 32)) else {
        return;
    };
    assert_eq!(renderer.surface(), (32, 32));
    let scene = one_lane_scene(quad(
        Rect::new(0.0, 0.0, 16.0, 16.0),
        Color::srgb(0.0, 1.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut frame = full_plan((32, 32));
    let mut timings = FrameTimings::new();
    renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
        .expect("first submit");

    renderer.resize((96, 64)).expect("resize");
    assert_eq!(renderer.surface(), (96, 64));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut frame = full_plan((96, 64));
    renderer
        .submit_buffer(FrameIndex::from_raw(2), buffer, &mut frame, &mut timings)
        .expect("submit after resize");
    let image = renderer.readback().expect("readback");
    assert_eq!((image.width, image.height), (96, 64));
    assert_eq!(pixel(&image, 96, 8, 8), [0, 255, 0, 255]);
}

#[test]
fn render_snapshot_returns_the_surface_pixels() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((16, 8)) else {
        return;
    };
    let scene = Scene::empty();
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let image = renderer.render_snapshot(buffer).expect("snapshot");
    assert_eq!((image.width, image.height), (16, 8));
    assert_eq!(image.rgba8.len(), 16 * 8 * 4);
}

/// 全量帧的 copy 范围：`fresh`（交换链刚建好）整块 buffer，之后只拷内容子矩形。
/// 离屏目标没有合成树：持久目标恒等于 surface（没有 buffer 桶、也没有交换链）。
///
/// 桶（`buffer > content`）只服务**窗口路径**的「拖动中不重建交换链」；帧缓冲直读的离屏
/// 目标按内容精确分配，读回范围的唯一来源因此仍是 `surface`。
#[test]
fn an_offscreen_target_keeps_persist_equal_to_the_surface() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    assert_eq!(renderer.buffer(), (64, 64));
    renderer
        .set_content_size((32, 24))
        .expect("resize the content");
    assert_eq!(renderer.surface(), (32, 24));
    assert_eq!(renderer.buffer(), (32, 24), "离屏目标没有 buffer 桶");

    // 铺满整个（新）内容的红块：取样点在内容内部，避开解析 AA 的边缘像素。
    let scene = one_lane_scene(quad(
        Rect::new(0.0, 0.0, 64.0, 64.0),
        Color::srgb(1.0, 0.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut frame = full_plan((32, 24));
    let mut timings = FrameTimings::new();
    renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
        .expect("content frame");
    let image = renderer.readback().expect("readback");
    assert_eq!((image.width, image.height), (32, 24), "读回范围 = 内容尺寸");
    assert_eq!(pixel(&image, 32, 20, 16), [255, 0, 0, 255]);
}

/// 内容超过 16384 硬上限：`buffer_size` 的 `Swapchain` 错误必须先于任何 GPU 工作
/// （离屏目标也走同一条校验，错误面因此与窗口路径一致）。
#[test]
fn content_beyond_the_maximum_buffer_is_an_error() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((32, 32)) else {
        return;
    };
    match renderer.set_content_size((16_385, 32)) {
        Err(EngineError::Swapchain { detail }) => {
            assert!(detail.contains("16385"), "{detail}");
        }
        other => panic!("unexpected result: {other:?}"),
    }
}

/// R08-1 的尺寸序列：桶只在「内容 + 余量」跨过 256 对齐边界时变化 —— 这正是「拖动不再每
/// 越一像素就重建交换链」的依据（`ResizeBuffers` 次数 = 桶变化次数）。
///
/// WARP 上无法创建组合交换链（需要 DComp），因此这里验证的是**决策的纯函数部分**：
/// `buffer_size` 的序列 + 同一个桶内 `set_content_size` 只做 CPU 记账。真机上的
/// `ResizeBuffers` 调用次数与结果由 release demo 的运行时冒烟覆盖（见报告「真机验证需求」）。
#[test]
fn the_resize_bucket_sequence_only_changes_at_alignment_boundaries() {
    use crate::platform::windows::composition::buffer_size;

    // 一次 1000 → 1400 px 的连续拖动：只有跨过 1280（1024+64 → 对齐 1280）与 1536 时换桶。
    let sequence = [1000u32, 1100, 1200, 1280, 1281, 1400];
    let buckets: Vec<(u32, u32)> = sequence
        .iter()
        .map(|width| buffer_size((*width, 700)).expect("inside the limit"))
        .collect();
    let mut changes = 0;
    for pair in buckets.windows(2) {
        if pair[0] != pair[1] {
            changes += 1;
        }
    }
    assert!(changes <= 3, "拖动 400 px 至多换 3 次桶：{buckets:?}");
    assert_eq!(
        buffer_size((1000, 700)).expect("fits"),
        (1280, 768),
        "1000 + 64 对齐到 1280"
    );
    assert_eq!(buffer_size((1281, 700)).expect("fits"), (1536, 768));
    // 桶内的小幅变化不换桶（经典 1 px 抖动）。
    assert_eq!(
        buffer_size((1200, 640)).expect("fits"),
        buffer_size((1210, 648)).expect("fits")
    );
    // 0 尺寸（最小化）永远给 256×256 或更大，绝不请求 0 尺寸的 buffer。
    assert!(buffer_size((0, 0)).expect("fits").0 >= 256);
}

/// RTV 堆里只有一个描述符（持久目标）。back buffer 的 RTV 由**交换链自己**持有
/// （每条链 3 个）：内容层与 1×1 背景层各有一条链，互相不挤共享堆。
#[test]
fn the_rtv_heap_holds_only_the_persist_descriptor() {
    let _guard = lock();
    let Some(renderer) = warp_renderer((8, 8)) else {
        return;
    };
    let persist = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 0);
    assert_ne!(persist.ptr, 0, "持久目标必须有 RTV");
    // SAFETY: 堆句柄由渲染器持有；查询无副作用。
    let desc = unsafe { renderer.rtv_heap.GetDesc() };
    assert_eq!(desc.NumDescriptors, 1);
}

/// 背景层刷新只碰它自己的 1×1 交换链：主持久目标里的内容一个像素都不能变。
///
/// 离屏目标没有背景层（`background_swapchain = None`），`refresh_background` 必须消费那个
/// 「背景色已改」标记（不能永真），否则每帧都会尝试重建。
#[test]
fn a_background_refresh_never_touches_the_main_content_persist() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    let red = one_lane_scene(quad(
        Rect::new(0.0, 0.0, 64.0, 64.0),
        Color::srgb(1.0, 0.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &red,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut frame = full_plan((64, 64));
    let mut timings = FrameTimings::new();
    renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
        .expect("content frame");

    renderer.set_background_color([0.1, 0.2, 0.3, 1.0]);
    assert!(renderer.background_dirty);
    renderer
        .refresh_background()
        .expect("an offscreen renderer has no background layer to rebuild");
    assert!(!renderer.background_dirty, "标记必须被消费");
    assert!(
        renderer.background_swapchain.is_none(),
        "离屏目标不创建 1×1 背景交换链"
    );

    let image = renderer.readback().expect("readback");
    assert_eq!(
        pixel(&image, 64, 32, 32),
        [255, 0, 0, 255],
        "改背景色不得清掉主持久目标"
    );
}

/// R08-8 / R08-11：离屏目标的绘制模式与状态机一致 —— 首帧整帧画进 persist，之后局部帧才走
/// `Incremental`；一次整帧重画把状态推回 `PersistValid`。
#[test]
fn the_offscreen_target_alternates_between_full_and_incremental_frames() {
    use super::plan::{FrameDraw, frame_draw};

    let _guard = lock();
    let Some(mut renderer) = warp_renderer((16, 16)) else {
        return;
    };
    assert_eq!(renderer.target_state(), TargetState::Fresh);
    assert_eq!(
        frame_draw(renderer.target_state(), Plan::Full, false),
        FrameDraw::Full,
        "没有 back buffer 时全量帧只能画进 persist"
    );

    let scene = Scene::empty();
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut timings = FrameTimings::new();
    let mut first = full_plan((16, 16));
    renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut first, &mut timings)
        .expect("full frame");
    assert_eq!(
        renderer.target_state(),
        TargetState::PersistValid,
        "整帧画进 persist 之后它可以继续服务局部帧"
    );
    assert_eq!(
        frame_draw(
            renderer.target_state(),
            partial_plan(&[PhysicalRect::new(0, 0, 8, 8)], (16, 16)).plan(),
            false
        ),
        FrameDraw::Incremental
    );

    let mut second = partial_plan(&[PhysicalRect::new(0, 0, 8, 8)], (16, 16));
    renderer
        .submit_buffer(FrameIndex::from_raw(2), buffer, &mut second, &mut timings)
        .expect("partial frame");
    assert_eq!(renderer.target_state(), TargetState::PersistValid);
}
/// B6：**增量实例上传与全量上传的像素等价性**（同机 WARP 逐字节比较）。
///
/// 同一个渲染器先整帧渲染（建立持久实例缓冲的内容），再改一条 lane、只上传它的实例段；
/// 与「另起一个渲染器把最终场景一次全量渲染」的读回像素逐字节比较。
#[test]
fn incremental_instance_upload_matches_a_full_upload() {
    let _guard = lock();
    let Some(mut incremental) = warp_renderer((64, 64)) else {
        return;
    };
    let store = crate::paint::geometry_store::GeometryStore::new();

    // 两条 lane：红块 + 蓝块。
    let mut scene = Scene::empty();
    let red = scene.new_lane();
    assert!(scene.push(
        red,
        quad(
            Rect::new(8.0, 8.0, 16.0, 16.0),
            Color::srgb(1.0, 0.0, 0.0, 1.0)
        )
    ));
    let blue = scene.new_lane();
    assert!(scene.push(
        blue,
        quad(
            Rect::new(32.0, 8.0, 16.0, 16.0),
            Color::srgb(0.0, 0.0, 1.0, 1.0)
        )
    ));
    scene.set_order(&[red, blue]);

    let mut encoder = InstanceEncoder::new();
    encoder
        .encode(&scene, Scale::ONE, resolver(), &store)
        .expect("初帧");
    let mut timings = FrameTimings::new();
    let mut frame = full_plan((64, 64));
    // 首帧也走增量载荷（与窗口路径一致；`full()` 不会取走变化区间）。
    let first = crate::render::frame::FramePayload::from_buffer_delta(
        encoder.buffer_mut(),
        Vec::new(),
        Vec::new(),
    );
    let stats = incremental
        .submit_snapshot(FrameIndex::from_raw(1), &first, &mut frame, &mut timings)
        .expect("首帧");
    assert_eq!(stats.instances, 2);

    // 改蓝块（位置 + 颜色变、实例数不变 ⇒ 原地重写那一个段）→ 增量载荷只带 128 B。
    assert!(scene.clear_lane(blue));
    assert!(scene.push(
        blue,
        quad(
            Rect::new(32.0, 32.0, 16.0, 16.0),
            Color::srgb(0.0, 1.0, 0.0, 1.0)
        )
    ));
    encoder
        .encode(&scene, Scale::ONE, resolver(), &store)
        .expect("增量帧");
    let payload = crate::render::frame::FramePayload::from_buffer_delta(
        encoder.buffer_mut(),
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(payload.instance_count(), 2);
    assert_eq!(
        payload
            .chunks()
            .iter()
            .map(|chunk| chunk.bytes().len())
            .sum::<usize>(),
        128,
        "只改一条 lane 的增量载荷必须只有它的实例段"
    );
    let mut frame = full_plan((64, 64));
    incremental
        .submit_snapshot(FrameIndex::from_raw(2), &payload, &mut frame, &mut timings)
        .expect("增量帧");
    let incremental_image = incremental.readback().expect("readback(增量)");

    // 对照：全新渲染器把最终场景一次全量渲染。
    let Some(mut reference) = warp_renderer((64, 64)) else {
        return;
    };
    let mut frame = full_plan((64, 64));
    let mut timings = FrameTimings::new();
    let full = crate::render::frame::FramePayload::full(encoder.buffer(), Vec::new(), Vec::new());
    reference
        .submit_snapshot(FrameIndex::from_raw(1), &full, &mut frame, &mut timings)
        .expect("全量参考帧");
    let reference_image = reference.readback().expect("readback(全量)");

    assert_eq!(
        incremental_image.rgba8, reference_image.rgba8,
        "增量上传与全量上传的像素必须逐字节一致"
    );
    assert_eq!(
        pixel(&reference_image, 64, 40, 40),
        [0, 255, 0, 255],
        "绿块必须真的画出来了（否则比较没有意义）"
    );
}

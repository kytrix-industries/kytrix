//! `GpuRenderer` 的单元测试（拆分自 `compositor.rs`，测试代码未改动）。

use windows::Win32::Graphics::Direct3D12::D3D12_DESCRIPTOR_HEAP_TYPE_RTV;

use super::{
    GpuRenderer, Persist,
    persist::{grown_buffer, rtv_handle, shrink_target},
    plan::{copy_content, copy_extent, copy_plan},
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
                &resolver(),
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
                    &resolver(),
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

/// D3D12 的 copy 范围：`fresh` 时全量，`presented()` 之后恢复局部（等价覆盖原共享层单测）。
#[test]
fn a_fresh_swapchain_forces_a_full_copy_until_presented() {
    let rect = PhysicalRect::new(0, 0, 10, 10);
    let partial = partial_plan(&[rect], (100, 50)).plan();
    assert!(matches!(partial, Plan::Partial(_)));
    let mut frame = FramePlan::new(partial, true);
    assert_eq!(copy_plan(frame), Plan::Full, "fresh ⇒ 全量 copy");
    frame.presented();
    assert_eq!(copy_plan(frame), partial, "present 成功后恢复局部 copy");
    assert_eq!(copy_plan(FramePlan::new(Plan::Full, false)), Plan::Full);
    assert_eq!(copy_plan(FramePlan::new(Plan::Idle, false)), Plan::Idle);
    assert_eq!(
        copy_plan(FramePlan::new(Plan::Idle, true)),
        Plan::Full,
        "fresh 的空帧仍然要全量 copy"
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
            &resolver(),
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
            &resolver(),
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
            &resolver(),
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
            &resolver(),
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
            &resolver(),
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
    let mut second = partial_plan(&[PhysicalRect::new(0, 0, 32, 64)], (64, 64));
    let stats = renderer
        .submit_buffer(FrameIndex::from_raw(2), buffer, &mut second, &mut timings)
        .expect("partial frame");
    assert_eq!(
        stats.draw_calls, 2,
        "只有与 damage 相交的 op 才产生 draw call（旧的『整份重放』实现会是 3）"
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
            &resolver(),
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
            &resolver(),
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
            &resolver(),
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
            &resolver(),
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
            &resolver(),
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
            &resolver(),
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
            &resolver(),
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
            &resolver(),
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
            &resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let image = renderer.render_snapshot(buffer).expect("snapshot");
    assert_eq!((image.width, image.height), (16, 8));
    assert_eq!(image.rgba8.len(), 16 * 8 * 4);
}

/// 全量帧的 copy 范围：`fresh`（交换链刚建好）整块 buffer，之后只拷内容子矩形。
#[test]
fn a_full_frame_copies_the_content_unless_the_swapchain_is_fresh() {
    let buffer = (1000, 700);
    let content = (800, 600);
    assert_eq!(
        copy_extent(FramePlan::new(Plan::Full, true), buffer, content),
        buffer,
        "第一次 present 之前 back buffer 的内容未定义"
    );
    assert_eq!(
        copy_extent(FramePlan::new(Plan::Full, false), buffer, content),
        content,
        "之后的全量帧只写可见的内容子矩形"
    );
    let fresh_partial = FramePlan::new(
        partial_plan(&[PhysicalRect::new(0, 0, 8, 8)], content).plan(),
        true,
    );
    assert_eq!(copy_extent(fresh_partial, buffer, content), buffer);
}

/// 内容子矩形：绘制与读回都只按内容尺寸，高水位 buffer 不参与（`design/DEVIATIONS.md` §36 F-199）。
#[test]
fn the_content_size_is_independent_of_the_buffer() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    assert_eq!(renderer.buffer(), (64, 64));
    renderer
        .set_content_size((32, 24))
        .expect("shrink the content");
    assert_eq!(renderer.surface(), (32, 24));
    assert_eq!(renderer.buffer(), (64, 64), "buffer 不随内容变化");

    // 铺满整个 buffer 的红块：内容子矩形之外的像素必须不参与本帧。
    let scene = one_lane_scene(quad(
        Rect::new(0.0, 0.0, 64.0, 64.0),
        Color::srgb(1.0, 0.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            Scale::ONE,
            &resolver(),
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
    assert_eq!(pixel(&image, 32, 31, 23), [255, 0, 0, 255]);
}

/// 内容越过 grow-only 桶、但渲染器没有合成目标（离屏）：显式 `Composition` 错误，不静默
/// 裁剪。真实窗口路径的 grow 需要 DComp + HWND，由 release demo 的运行时冒烟覆盖。
#[test]
fn growing_beyond_the_buffer_without_a_composition_target_is_an_error() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((32, 32)) else {
        return;
    };
    match renderer.set_content_size((33, 32)) {
        Err(EngineError::Composition { detail }) => {
            assert!(detail.contains("composition target"), "{detail}");
        }
        other => panic!("unexpected result: {other:?}"),
    }
    assert!(
        renderer.set_content_size((32, 32)).is_ok(),
        "同值必须是 no-op"
    );
}

/// 内容超过 16384 硬上限：`buffer_size` 的 `Swapchain` 错误必须先于任何 GPU 工作。
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

/// P1-A：corner drag（一轴变大、另一轴变小）时，另一轴必须保留旧 buffer 的高水位。
///
/// 视觉切换那一帧的旧 clip 仍可能指向旧内容，旧内容 ≤ 旧 buffer；只有新 buffer 逐轴 ≥
/// 旧 buffer，中间帧才不可能采样到未定义像素。
#[test]
fn growth_keeps_the_other_axis_at_the_high_water_mark() {
    // buffer 2560×1792，新内容 2800×600：宽度增长，高度不得按内容缩回 768。
    let grown = grown_buffer((2560, 1792), (2800, 600)).expect("grows");
    assert_eq!(grown, (3072, 1792));
    assert!(
        grown.1 >= 1792,
        "高度必须保留旧 buffer（旧 clip 仍可能指向旧内容）: {grown:?}"
    );
    // 另一方向同理。
    assert_eq!(
        grown_buffer((2560, 1792), (1200, 2400)).expect("grows"),
        (2560, 2560)
    );
    // 当前 buffer 比按内容算出的桶大：新桶不得缩小。
    let covered = grown_buffer((3328, 2048), (2000, 600)).expect("fits");
    assert!(covered.0 >= 3328 && covered.1 >= 2048, "{covered:?}");
    // 就算内容完全落在当前 buffer 内，纯函数也不会把桶算小（防御性）。
    assert_eq!(
        grown_buffer((2560, 1792), (2000, 600)).expect("fits"),
        (2560, 1792)
    );
    // 上限检查在 max 之后仍然生效。
    assert!(matches!(
        grown_buffer((1536, 1280), (16_385, 100)),
        Err(EngineError::Swapchain { .. })
    ));
}

/// RTV 堆固定两个描述符：0 = 内容，1 = 1×1 背景（按 `GetDescriptorHandleIncrementSize`
/// 偏移）。背景层 prime 只写第二个，主持久目标一个像素都不碰。
#[test]
fn the_rtv_heap_has_a_second_descriptor_for_the_background_layer() {
    let _guard = lock();
    let Some(renderer) = warp_renderer((8, 8)) else {
        return;
    };
    let main = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 0);
    let background = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 1);
    // SAFETY: 设备 / 堆句柄由渲染器持有，调用期间有效；查询无副作用。
    let size = unsafe {
        renderer
            .shared
            .raw()
            .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV)
    };
    assert_eq!(
        background.ptr,
        main.ptr + size as usize,
        "背景 RTV 必须是堆里的第二个描述符"
    );
    assert!(background.ptr > main.ptr);
}

/// 缩容决策：只在内容桶**严格小于**当前高水位、且渲染器有合成目标时返回新桶。
#[test]
fn the_shrink_decision_only_returns_a_smaller_bucket() {
    // 1200×925 内容 ⇒ 1280×1024；历史高水位 2560×1792（1900×1300 时开出来的桶）。
    assert_eq!(
        shrink_target((2560, 1792), (1200, 925), true).expect("fits"),
        Some((1280, 1024)),
        "静止缩小时回到当前内容的桶"
    );
    // 单轴缩小也算：宽度 2000+64 的桶是 2304，高度从 1792 缩到 768。
    assert_eq!(
        shrink_target((2560, 1792), (2000, 600), true).expect("fits"),
        Some((2304, 768))
    );
    // 内容桶 == 当前 buffer：已经是最小桶，不缩（否则会每 2 s 重建一次）。
    assert_eq!(
        shrink_target((2304, 1280), (2000, 1200), true).expect("fits"),
        None
    );
    // 没有合成目标（离屏 / 快照）：永不缩容（离屏由 `resize` 直接重建）。
    assert_eq!(
        shrink_target((2560, 1792), (1200, 925), false).expect("fits"),
        None
    );
    // 防御：内容比 buffer 还大（grow-only 不变量被破坏）时不得「缩」成更大的桶。
    assert_eq!(
        shrink_target((512, 512), (2000, 900), true).expect("fits"),
        None
    );
    // 上限检查仍然生效（内容 > 16384 是显式错误，不静默裁剪）。
    assert!(matches!(
        shrink_target((16384, 16384), (16_385, 100), true),
        Err(EngineError::Swapchain { .. })
    ));
}

/// 离屏目标没有合成树：`maybe_shrink` 是 no-op，高水位保持不变。
#[test]
fn an_offscreen_renderer_never_shrinks_the_high_water_bucket() {
    let _guard = lock();
    // 1024² 的 buffer 与 32² 的内容桶（768²）才有「可缩」的候选。
    let Some(mut renderer) = warp_renderer((1024, 1024)) else {
        return;
    };
    renderer
        .set_content_size((32, 32))
        .expect("content stays inside the buffer");
    assert_eq!(renderer.buffer(), (1024, 1024));
    assert!(
        renderer.shrink_in().is_none(),
        "离屏目标没有缩容候选，空闲等待必须保持无限等"
    );
    renderer
        .maybe_shrink()
        .expect("an offscreen renderer has no composition target to switch");
    assert_eq!(
        renderer.buffer(),
        (1024, 1024),
        "没有合成目标时不得重建持久目标"
    );
}

/// 背景层刷新只碰它自己的 1×1 持久目标：主持久目标里的内容一个像素都不能变。
///
/// 旧实现借主 persist prime 1×1 背景层，每次改色都会把整块内容目标清成背景色；离屏目标
/// 没有背景持久目标（`background_persist = None`），这条断言同时守住那个不变量与「标记必须
/// 被消费、不能永真」的既有语义。
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
            &resolver(),
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
        renderer.background_persist.is_none(),
        "离屏目标不创建 1×1 背景持久目标"
    );

    let image = renderer.readback().expect("readback");
    assert_eq!(
        pixel(&image, 64, 32, 32),
        [255, 0, 0, 255],
        "改背景色不得清掉主持久目标"
    );
}

/// 缩容 prime 的内容保留：新目标先 clear，再把旧目标的 `(0, 0, content)` 拷到左上角；
/// 内容之外保持背景色。WARP 离屏没有合成目标，所以这里直接测录制片段（真实 DComp 窗口
/// 的整条 `maybe_shrink` 由 release demo 的运行时冒烟覆盖）。
#[test]
fn a_shrink_prime_copies_the_old_content_into_the_new_persist() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    // 旧内容：整块红。
    let red = one_lane_scene(quad(
        Rect::new(0.0, 0.0, 64.0, 64.0),
        Color::srgb(1.0, 0.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &red,
            Scale::ONE,
            &resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut frame = full_plan((64, 64));
    let mut timings = FrameTimings::new();
    renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
        .expect("old content");
    // 新目标 128×128：离屏路径不建背景层，第二个 RTV 描述符在这里空闲可用。
    let rtv = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 1);
    let destination = Persist::new(&renderer.shared, (128, 128), rtv).expect("new persist");
    let source = std::mem::replace(&mut renderer.persist, destination);
    {
        let destination = &renderer.persist;
        let source = &source;
        renderer
            .stream
            .submit_immediate(|list| {
                // 缩容 prime 的固定顺序：先整块 clear，再从旧目标拷内容。
                // SAFETY: 两个目标都处于 RENDER_TARGET；清整块不需要矩形。
                unsafe { list.ClearRenderTargetView(destination.rtv, &[1.0, 1.0, 1.0, 1.0], None) };
                copy_content(list, source, destination, (64, 64));
                Ok(())
            })
            .expect("record the shrink copy");
    }
    renderer.surface = (128, 128);
    let image = renderer.readback().expect("readback");
    assert_eq!((image.width, image.height), (128, 128));
    assert_eq!(
        pixel(&image, 128, 8, 8),
        [255, 0, 0, 255],
        "旧内容原样保留在新目标的左上角"
    );
    assert_eq!(
        pixel(&image, 128, 100, 100),
        [255, 255, 255, 255],
        "内容之外是 clear 出来的背景色"
    );
}

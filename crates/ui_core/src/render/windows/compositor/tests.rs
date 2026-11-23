use super::{
    GpuRenderer,
    persist::rtv_handle,
    plan::TargetState,
    record::{geometry_root, root_uniform},
};
use crate::{
    error::EngineError,
    math::{
        DevicePixelRatio, DeviceRect, LogicalPoint, LogicalRect, LogicalSize, Transform, dp, px,
    },
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
        frame::FramePayload,
        timer::FrameTimings,
    },
    window::FrameIndex,
};

fn lock() -> std::sync::MutexGuard<'static, ()> {
    crate::render::windows::device::gpu_test_lock()
}

fn spec(page: u32, layers: u32) -> crate::render::AtlasArraySpec {
    crate::render::AtlasArraySpec { page, layers }
}

fn warp_renderer(size: (u32, u32)) -> Option<GpuRenderer> {
    match GpuRenderer::new_warp_offscreen(size) {
        Ok(renderer) => Some(renderer),
        Err(error) => {
            eprintln!("skip: WARP renderer unavailable: {error}");
            None
        }
    }
}

#[test]
fn dropping_a_renderer_releases_its_shared_device_resources() {
    let _guard = lock();
    let shared = crate::render::windows::device::SharedGpu::new_warp().expect("WARP shared gpu");
    assert_eq!(shared.live_resources(), 0, "共享 GPU 创建时不注册资源");
    {
        let mut renderer =
            GpuRenderer::with_shared(std::sync::Arc::clone(&shared), (16, 16)).expect("renderer");
        let rects = crate::render::DirtyRects::full();
        renderer.queue_atlas_upload(
            AtlasKind::Glyph,
            0,
            spec(super::TEST_ATLAS_PAGE, 1),
            vec![0u8; (super::TEST_ATLAS_PAGE * super::TEST_ATLAS_PAGE) as usize],
            rects,
        );
        let scene = Scene::empty();
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder
            .encode(
                &scene,
                DevicePixelRatio::ONE,
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
            let rects = crate::render::DirtyRects::full();
            renderer.queue_atlas_upload(AtlasKind::Glyph, 0, spec(page, 1), pixels, rects);

            let text_free = one_lane_scene(quad(
                LogicalRect::new(px(2.0), px(2.0), px(2.0), px(2.0)),
                Color::srgb(1.0, 0.0, 0.0, 1.0),
            ));
            let mut encoder = InstanceEncoder::new();
            let buffer = encoder
                .encode(
                    &text_free,
                    DevicePixelRatio::ONE,
                    resolver(),
                    &crate::paint::geometry_store::GeometryStore::new(),
                )
                .expect("encode");
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
            DeviceRect::new(dp(0), dp(0), dp(size.0 as i32), dp(size.1 as i32)),
        ),
        true,
    )
}

fn partial_plan(rects: &[DeviceRect], surface: (u32, u32)) -> FramePlan {
    FramePlan::new(
        plan(
            DamageSummary {
                full: false,
                rects: rects.len(),
            },
            rects,
            DeviceRect::new(dp(0), dp(0), dp(surface.0 as i32), dp(surface.1 as i32)),
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

fn quad(bounds: LogicalRect, fill: Color) -> Primitive {
    Primitive::Quad(Quad {
        bounds,
        corner_radius: [px(0.0); 4],
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

fn test_snapshot() -> std::sync::Arc<crate::paint::geometry::GeometrySnapshot> {
    crate::paint::geometry_store::snapshot_for_test(crate::paint::geometry::GeometryDesc {
        vertex_bytes: bytes::Bytes::from_static(&[0u8; 36]),
        index_bytes: bytes::Bytes::from_static(&[0, 0, 1, 0, 2, 0]),
        vertex_stride: 12,
        index_type: IndexType::U16,
        topology: Topology::TriangleList,
        bounds: LogicalRect::new(px(0.0), px(0.0), px(1.0), px(1.0)),
        pipeline: GeometryPipeline::Ui,
    })
}

#[test]
fn geometry_root_matches_common_hlsl_layout() {
    let frame = root_uniform(800.0, 600.0, 1.0);
    let clip_rect = [4.0f32, 5.0, 100.0, 120.0].map(f32::to_bits);
    let draw = GeometryDrawGpu {
        id: GeometryId::for_test(0, 1),
        range: 0..3,
        transform: Transform::translate(px(11.0), px(13.0)),
        color: resolver().resolve(Color::WHITE).to_array(),
        snapshot: test_snapshot(),
    };
    let uniform = geometry_root(frame, draw, 3, clip_rect);
    let words = uniform.words();
    assert_eq!(
        &words[..8],
        &frame.words()[..8],
        "frame block is copied verbatim"
    );
    assert_eq!(&words[8..12], &[1.0, 0.0, 0.0, 1.0]);
    assert_eq!(&words[12..14], &[11.0, 13.0]);
    assert_eq!(&words[16..20], &[1.0, 1.0, 1.0, 1.0]);
    assert_eq!(&words[20..24], &[4.0, 5.0, 96.0, 115.0]);
    assert_eq!(words[24], 3.0);
    assert_eq!(words[25], 0.0);
    assert_eq!(words[26..].iter().copied().sum::<f32>(), 0.0);
}

#[test]
fn geometry_root_consumes_transform_and_color() {
    let frame = root_uniform(64.0, 32.0, 2.0);
    let draw = GeometryDrawGpu {
        id: GeometryId::for_test(0, 1),
        range: 0..3,
        transform: Transform::scale(3.0, 4.0),
        color: resolver()
            .resolve(Color::srgb(1.0, 0.0, 0.0, 1.0))
            .to_array(),
        snapshot: test_snapshot(),
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
        range: 0..3,
        transform: Transform::IDENTITY,
        color: resolver().resolve(Color::WHITE).to_array(),
        snapshot: test_snapshot(),
    };
    let uniform = geometry_root(frame, draw, 0, [0; 4]);
    let words = uniform.words();
    assert_eq!(&words[20..24], &[0.0, 0.0, 64.0, 32.0]);
    assert_eq!(words[24], 0.0);
}

#[test]
fn instance_bytes_are_128_per_primitive() {
    let scene = one_lane_scene(quad(
        LogicalRect::new(px(0.0), px(0.0), px(8.0), px(8.0)),
        Color::WHITE,
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            DevicePixelRatio::ONE,
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
            DevicePixelRatio::ONE,
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
    assert_eq!(pixel(&image, 8, 4, 4), [188, 188, 188, 128]);
}

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
        LogicalRect::new(px(16.0), px(16.0), px(16.0), px(16.0)),
        Color::srgb(1.0, 0.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            DevicePixelRatio::ONE,
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

#[test]
fn the_scroll_offset_reaches_the_gpu() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    let mut scene = Scene::empty();
    let lane = scene.new_lane();
    assert!(scene.push(
        lane,
        quad(
            LogicalRect::new(px(16.0), px(16.0), px(16.0), px(16.0)),
            Color::srgb(1.0, 0.0, 0.0, 1.0),
        )
    ));
    scene.set_order(&[lane]);
    assert!(scene.set_lane_scroll(lane, 1), "lane 必须存在");
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            DevicePixelRatio::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    assert!(
        buffer
            .ops()
            .iter()
            .any(|op| matches!(op, FrameOp::Instances { scroll: 1, .. })),
        "本用例的前提：quad 的 op 必须落在滚动槽 1（实际 {:?}）",
        buffer.ops()
    );
    let payload = FramePayload::full(buffer, vec![[0.0, 0.0], [0.0, -16.0]], Vec::new());
    let mut frame = full_plan((64, 64));
    let mut timings = FrameTimings::new();
    let stats = renderer
        .submit_snapshot(FrameIndex::from_raw(1), &payload, &mut frame, &mut timings)
        .expect("submit");
    assert_eq!(
        (stats.draw_calls, stats.instances),
        (1, 1),
        "本用例的前提：一帧一个非空实例 op（实际 {stats:?}）"
    );
    let image = renderer.readback().expect("readback");
    assert_eq!(
        pixel(&image, 64, 24, 8),
        [255, 0, 0, 255],
        "偏移 (0,-16) 之后 quad 必须画在 y = 0..16；仍是烘焙位置说明 frame 块（words 5/6）没上传"
    );
    assert_eq!(
        pixel(&image, 64, 24, 24),
        [0, 0, 0, 0],
        "烘焙位置的下半段（y = 16..32）必须留空：内容真的移动了，不是被画了两遍"
    );
}

#[test]
fn draw_calls_follow_full_and_partial_scissor_passes() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    let scene = one_lane_scene(quad(
        LogicalRect::new(px(0.0), px(0.0), px(64.0), px(64.0)),
        Color::srgb(1.0, 0.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            DevicePixelRatio::ONE,
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
        DeviceRect::new(dp(0), dp(0), dp(32), dp(64)),
        DeviceRect::new(dp(32), dp(0), dp(64), dp(64)),
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

#[test]
fn ops_outside_the_damage_are_skipped() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    #[rustfmt::skip]
    let vertices: [f32; 9] = [0.0, 0.0, -1.0, 8.0, 0.0, -1.0, 0.0, 8.0, -1.0];
    let indices: [u16; 3] = [0, 1, 2];
    let vertex_bytes: Vec<u8> = vertices.iter().flat_map(|v| v.to_ne_bytes()).collect();
    let index_bytes: Vec<u8> = indices.iter().flat_map(|v| v.to_ne_bytes()).collect();
    let mut store = crate::paint::geometry_store::GeometryStore::new();
    let id = store
        .insert(crate::paint::geometry::GeometryDesc {
            vertex_bytes: bytes::Bytes::from(vertex_bytes),
            index_bytes: bytes::Bytes::from(index_bytes),
            vertex_stride: GeometryPipeline::Ui.vertex_stride(),
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: LogicalRect::new(px(0.0), px(0.0), px(8.0), px(8.0)),
            pipeline: GeometryPipeline::Ui,
        })
        .expect("注册几何");

    let mut scene = Scene::empty();
    let lane = scene.new_lane();
    assert!(scene.push(
        lane,
        quad(
            LogicalRect::new(px(0.0), px(0.0), px(32.0), px(64.0)),
            Color::srgb(1.0, 0.0, 0.0, 1.0),
        )
    ));
    assert!(scene.push(
        lane,
        Primitive::Geometry(GeometryDraw::full(id, Transform::IDENTITY, Color::WHITE))
    ));
    assert!(scene.push(
        lane,
        quad(
            LogicalRect::new(px(40.0), px(0.0), px(24.0), px(64.0)),
            Color::srgb(0.0, 0.0, 1.0, 1.0),
        )
    ));
    scene.set_order(&[lane]);
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(&scene, DevicePixelRatio::ONE, resolver(), &store)
        .expect("encode");
    let mut timings = FrameTimings::new();

    let mut first = full_plan((64, 64));
    let stats = renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut first, &mut timings)
        .expect("full frame");
    assert_eq!(stats.draw_calls, 3, "两个实例 op + 一个几何 op");

    let before = crate::render::windows::diagnostics::snapshot();
    let mut second = partial_plan(&[DeviceRect::new(dp(0), dp(0), dp(32), dp(64))], (64, 64));
    let stats = renderer
        .submit_buffer(FrameIndex::from_raw(2), buffer, &mut second, &mut timings)
        .expect("partial frame");
    assert_eq!(
        stats.draw_calls, 2,
        "只有与 damage 相交的 op 才产生 draw call（旧的『整份重放』实现会是 3）"
    );
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

#[test]
fn a_rendered_frame_produces_no_corruption_messages() {
    use crate::render::windows::debug;

    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    let page = super::TEST_ATLAS_PAGE;
    let mut pixels = vec![0u8; (page * page) as usize];
    for y in 1..11u32 {
        for x in 1..11u32 {
            pixels[(y * page + x) as usize] = 255;
        }
    }
    let rects = crate::render::DirtyRects::full();
    renderer.queue_atlas_upload(AtlasKind::Glyph, 0, spec(page, 1), pixels, rects);

    let scene = one_lane_scene(quad(
        LogicalRect::new(px(0.0), px(0.0), px(32.0), px(32.0)),
        Color::srgb(1.0, 0.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            DevicePixelRatio::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let mut timings = FrameTimings::new();
    let mut full = full_plan((64, 64));
    renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut full, &mut timings)
        .expect("full frame");
    let mut partial = partial_plan(&[DeviceRect::new(dp(0), dp(0), dp(16), dp(16))], (64, 64));
    renderer
        .submit_buffer(FrameIndex::from_raw(2), buffer, &mut partial, &mut timings)
        .expect("partial frame");
    let _ = renderer.readback().expect("readback");

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
        LogicalRect::new(px(32.0), px(0.0), px(32.0), px(64.0)),
        Color::srgb(0.0, 0.0, 1.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &blue,
            DevicePixelRatio::ONE,
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
        LogicalRect::new(px(0.0), px(0.0), px(32.0), px(64.0)),
        Color::srgb(1.0, 0.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &red,
            DevicePixelRatio::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let damage = [DeviceRect::new(dp(0), dp(0), dp(32), dp(64))];
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

#[test]
fn geometry_ui_triangle_renders_white() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    #[rustfmt::skip]
    let vertices: [f32; 9] = [0.0, 0.0, -1.0, 32.0, 0.0, -1.0, 0.0, 32.0, -1.0];
    let indices: [u16; 3] = [0, 1, 2];
    let vertex_bytes: Vec<u8> = vertices.iter().flat_map(|v| v.to_ne_bytes()).collect();
    let index_bytes: Vec<u8> = indices.iter().flat_map(|v| v.to_ne_bytes()).collect();
    let mut store = crate::paint::geometry_store::GeometryStore::new();
    let id = store
        .insert(crate::paint::geometry::GeometryDesc {
            vertex_bytes: bytes::Bytes::from(vertex_bytes),
            index_bytes: bytes::Bytes::from(index_bytes),
            vertex_stride: GeometryPipeline::Ui.vertex_stride(),
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: LogicalRect::new(px(0.0), px(0.0), px(32.0), px(32.0)),
            pipeline: GeometryPipeline::Ui,
        })
        .expect("注册几何");

    let scene = one_lane_scene(Primitive::Geometry(GeometryDraw::full(
        id,
        Transform::IDENTITY,
        Color::WHITE,
    )));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(&scene, DevicePixelRatio::ONE, resolver(), &store)
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

#[test]
fn geometry_incremental_page_upload_updates_pixels() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };

    fn triangle(points: [[f32; 2]; 3]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(36);
        for point in points {
            bytes.extend_from_slice(&point[0].to_ne_bytes());
            bytes.extend_from_slice(&point[1].to_ne_bytes());
            bytes.extend_from_slice(&(-1.0f32).to_ne_bytes());
        }
        bytes
    }

    let mut store = crate::paint::geometry_store::GeometryStore::new();
    let id = store
        .insert(crate::paint::geometry::GeometryDesc {
            vertex_bytes: bytes::Bytes::from(triangle([[0.0, 0.0], [32.0, 0.0], [0.0, 32.0]])),
            index_bytes: bytes::Bytes::from_static(&[0, 0, 1, 0, 2, 0]),
            vertex_stride: GeometryPipeline::Ui.vertex_stride(),
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: LogicalRect::new(px(0.0), px(0.0), px(64.0), px(32.0)),
            pipeline: GeometryPipeline::Ui,
        })
        .expect("注册几何");
    let scene = || {
        one_lane_scene(Primitive::Geometry(GeometryDraw::full(
            id,
            Transform::IDENTITY,
            Color::WHITE,
        )))
    };
    let mut timings = FrameTimings::new();

    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(&scene(), DevicePixelRatio::ONE, resolver(), &store)
        .expect("encode 1");
    let mut frame = full_plan((64, 64));
    renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
        .expect("submit 1");
    let image = renderer.readback().expect("readback 1");
    assert_eq!(pixel(&image, 64, 8, 8), [255, 255, 255, 255], "帧 1 旧位置");
    assert_eq!(pixel(&image, 64, 28, 8), [0, 0, 0, 0], "帧 1 新位置为空");

    store
        .update_vertices(
            id,
            0..36,
            &triangle([[20.0, 0.0], [52.0, 0.0], [20.0, 32.0]]),
        )
        .expect("移动三角形");
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(&scene(), DevicePixelRatio::ONE, resolver(), &store)
        .expect("encode 2");
    let mut frame = full_plan((64, 64));
    renderer
        .submit_buffer(FrameIndex::from_raw(2), buffer, &mut frame, &mut timings)
        .expect("submit 2");
    let image = renderer.readback().expect("readback 2");
    assert_eq!(
        pixel(&image, 64, 8, 8),
        [0, 0, 0, 0],
        "旧位置必须变空（增量页上传把新顶点写进了 DEFAULT 缓冲）"
    );
    assert_eq!(
        pixel(&image, 64, 28, 8),
        [255, 255, 255, 255],
        "新位置必须被画出来"
    );
}

#[test]
fn geometry_subrange_draws_only_the_selected_triangles() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    #[rustfmt::skip]
    let vertices: [f32; 27] = [
        0.0, 0.0, -1.0,  20.0, 0.0, -1.0,  0.0, 20.0, -1.0,
        24.0, 0.0, -1.0, 44.0, 0.0, -1.0,  24.0, 20.0, -1.0,
        0.0, 24.0, -1.0, 20.0, 24.0, -1.0, 0.0, 44.0, -1.0,
    ];
    let indices: [u16; 9] = [0, 1, 2, 3, 4, 5, 6, 7, 8];
    let vertex_bytes: Vec<u8> = vertices.iter().flat_map(|v| v.to_ne_bytes()).collect();
    let index_bytes: Vec<u8> = indices.iter().flat_map(|v| v.to_ne_bytes()).collect();
    let mut store = crate::paint::geometry_store::GeometryStore::new();
    let id = store
        .insert(crate::paint::geometry::GeometryDesc {
            vertex_bytes: bytes::Bytes::from(vertex_bytes),
            index_bytes: bytes::Bytes::from(index_bytes),
            vertex_stride: GeometryPipeline::Ui.vertex_stride(),
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: LogicalRect::new(px(0.0), px(0.0), px(44.0), px(44.0)),
            pipeline: GeometryPipeline::Ui,
        })
        .expect("注册几何");

    let scene = one_lane_scene(Primitive::Geometry(GeometryDraw {
        id,
        transform: Transform::IDENTITY,
        color: Color::WHITE,
        range: 3..6,
    }));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(&scene, DevicePixelRatio::ONE, resolver(), &store)
        .expect("encode");
    let mut frame = full_plan((64, 64));
    let mut timings = FrameTimings::new();
    renderer
        .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
        .expect("submit");
    let image = renderer.readback().expect("readback");
    assert_eq!(
        pixel(&image, 64, 30, 6),
        [255, 255, 255, 255],
        "range 3..6 必须画出 B（`first_index = 3`、`count = 3`）"
    );
    assert_eq!(
        pixel(&image, 64, 6, 6),
        [0, 0, 0, 0],
        "A 在 range 之外：`first_index` 不能被忽略成 0"
    );
    assert_eq!(
        pixel(&image, 64, 6, 30),
        [0, 0, 0, 0],
        "C 在 range 之外：`count` 不能把区间尾之后一起画出来"
    );
}

#[test]
fn image_primitive_samples_the_image_atlas() {
    let _guard = lock();
    let page = super::TEST_ATLAS_PAGE;
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    let mut pixels = vec![0u8; (page * page * 4) as usize];
    for texel in pixels.as_chunks_mut::<4>().0 {
        texel[0] = 0xFF;
        texel[1] = 0x00;
        texel[2] = 0x00;
        texel[3] = 0xFF;
    }
    let rects = crate::render::atlas::DirtyRects::full();
    renderer.queue_atlas_upload(AtlasKind::Image, 0, spec(page, 1), pixels, rects);

    let tile = AtlasTile {
        atlas: AtlasKind::Image,
        page: 0,
        u0: 0.25,
        v0: 0.25,
        u1: 0.5,
        v1: 0.5,
        generation: 0,
        slot: 0,
        slot_epoch: 0,
    };
    let scene = one_lane_scene(Primitive::Image(Image {
        bounds: LogicalRect::new(px(0.0), px(0.0), px(32.0), px(32.0)),
        tile,
        modulation: Color::WHITE,
    }));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            DevicePixelRatio::ONE,
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

#[test]
fn image_primitive_samples_its_texture_array_layer() {
    let _guard = lock();
    let page = super::TEST_ATLAS_PAGE;
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    let mut blue = vec![0u8; (page * page * 4) as usize];
    for texel in blue.as_chunks_mut::<4>().0 {
        *texel = [0xFF, 0x00, 0x00, 0xFF];
    }
    let mut red = vec![0u8; (page * page * 4) as usize];
    for texel in red.as_chunks_mut::<4>().0 {
        *texel = [0x00, 0x00, 0xFF, 0xFF];
    }
    let rects = crate::render::atlas::DirtyRects::full();
    renderer.queue_atlas_upload(AtlasKind::Image, 0, spec(page, 2), blue, rects.clone());
    renderer.queue_atlas_upload(AtlasKind::Image, 1, spec(page, 2), red, rects);

    let tile = AtlasTile {
        atlas: AtlasKind::Image,
        page: 1,
        u0: 0.25,
        v0: 0.25,
        u1: 0.5,
        v1: 0.5,
        generation: 0,
        slot: 0,
        slot_epoch: 0,
    };
    let scene = one_lane_scene(Primitive::Image(Image {
        bounds: LogicalRect::new(px(0.0), px(0.0), px(32.0), px(32.0)),
        tile,
        modulation: Color::WHITE,
    }));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            DevicePixelRatio::ONE,
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
        pixel(&image, 64, 16, 16),
        [255, 0, 0, 255],
        "第 1 页的 tile 必须采样 layer 1（红），不是 layer 0（蓝）"
    );
}

#[test]
fn growing_the_shared_image_array_in_place_uses_the_production_rebuild_path() {
    let _guard = lock();
    let page = super::TEST_ATLAS_PAGE;
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    let rects = crate::render::atlas::DirtyRects::full();
    let blue = {
        let mut pixels = vec![0u8; (page * page * 4) as usize];
        for texel in pixels.as_chunks_mut::<4>().0 {
            *texel = [0xFF, 0x00, 0x00, 0xFF];
        }
        pixels
    };
    let red = {
        let mut pixels = vec![0u8; (page * page * 4) as usize];
        for texel in pixels.as_chunks_mut::<4>().0 {
            *texel = [0x00, 0x00, 0xFF, 0xFF];
        }
        pixels
    };

    renderer.queue_atlas_upload(
        AtlasKind::Image,
        0,
        spec(page, 1),
        blue.clone(),
        rects.clone(),
    );
    submit_image_layers(&mut renderer, 1);
    {
        let atlas = renderer.shared.atlas();
        assert_eq!(atlas.layers(AtlasKind::Image), 1, "首帧是 1 层数组");
        assert_eq!(
            atlas.resource_array_size(AtlasKind::Image),
            1,
            "资源 DepthOrArraySize 必须是 1"
        );
    }
    let image = renderer.readback().expect("readback frame 1");
    assert_eq!(pixel(&image, 64, 16, 16), [0, 0, 255, 255], "layer 0 = 蓝");

    renderer.queue_atlas_upload(AtlasKind::Image, 0, spec(page, 2), blue, rects.clone());
    renderer.queue_atlas_upload(AtlasKind::Image, 1, spec(page, 2), red, rects);
    submit_image_layers(&mut renderer, 2);
    {
        let atlas = renderer.shared.atlas();
        assert_eq!(
            atlas.layers(AtlasKind::Image),
            2,
            "同页扩层后记账层数必须是 2"
        );
        assert_eq!(
            atlas.resource_array_size(AtlasKind::Image),
            2,
            "同页扩层必须真的重建资源（DepthOrArraySize = 2）"
        );
    }
    let image = renderer.readback().expect("readback frame 2");
    assert_eq!(
        pixel(&image, 64, 16, 16),
        [0, 0, 255, 255],
        "重建后 layer 0（旧层内容）仍必须正确（App 整组重传）"
    );
    assert_eq!(
        pixel(&image, 64, 48, 16),
        [255, 0, 0, 255],
        "同页扩层后 layer 1 必须可采样（SRV 指向新纹理）"
    );
}

fn submit_image_layers(renderer: &mut super::GpuRenderer, layers: u16) {
    let mut encoder = InstanceEncoder::new();
    {
        let mut canvas = crate::paint::canvas::Canvas::new(LogicalRect::new(
            px(0.0),
            px(0.0),
            px(64.0),
            px(32.0),
        ));
        for layer in 0..layers {
            let tile = AtlasTile {
                atlas: AtlasKind::Image,
                page: layer,
                u0: 0.25,
                v0: 0.25,
                u1: 0.5,
                v1: 0.5,
                generation: 0,
                slot: 0,
                slot_epoch: 0,
            };
            canvas.push_image(Image {
                bounds: LogicalRect::new(px(f32::from(layer) * 32.0), px(0.0), px(32.0), px(32.0)),
                tile,
                modulation: Color::WHITE,
            });
        }
        let scene = one_lane_scene(Primitive::Canvas(std::sync::Arc::new(canvas)));
        encoder
            .encode(
                &scene,
                DevicePixelRatio::ONE,
                resolver(),
                &crate::paint::geometry_store::GeometryStore::new(),
            )
            .expect("encode");
    }
    let mut frame = full_plan((64, 64));
    let mut timings = FrameTimings::new();
    renderer
        .submit_buffer(
            FrameIndex::from_raw(2),
            encoder.buffer(),
            &mut frame,
            &mut timings,
        )
        .expect("submit");
}

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
    let rects = crate::render::atlas::DirtyRects::full();
    renderer.queue_atlas_upload(AtlasKind::Glyph, 0, spec(page, 1), pixels, rects);
    let tile = AtlasTile {
        atlas: AtlasKind::Glyph,
        page: 0,
        u0: 1.0 / page as f32,
        v0: 1.0 / page as f32,
        u1: 11.0 / page as f32,
        v1: 11.0 / page as f32,
        generation: 0,
        slot: 0,
        slot_epoch: 0,
    };
    let scene = one_lane_scene(Primitive::GlyphRun(GlyphRun {
        color: Color::WHITE,
        glyphs: vec![PositionedGlyph {
            tile,
            position: LogicalPoint::new(px(16.0), px(16.0)),
            size: LogicalSize::new(px(16.0), px(16.0)),
        }],
    }));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            DevicePixelRatio::ONE,
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

#[test]
fn glyph_run_samples_the_uploaded_glyph_page() {
    let _guard = lock();
    for page in [64u32, 256, 512, 1024, 2048] {
        let visible = glyph_probe(page);
        println!("page {page}: glyph visible = {visible}");
        assert!(visible, "page {page}: glyph coverage was not sampled");
    }
}
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
            DevicePixelRatio::ONE,
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
        LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0)),
        Color::srgb(0.0, 1.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            DevicePixelRatio::ONE,
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
            DevicePixelRatio::ONE,
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
            DevicePixelRatio::ONE,
            resolver(),
            &crate::paint::geometry_store::GeometryStore::new(),
        )
        .expect("encode");
    let image = renderer.render_snapshot(buffer).expect("snapshot");
    assert_eq!((image.width, image.height), (16, 8));
    assert_eq!(image.rgba8.len(), 16 * 8 * 4);
}

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

    let scene = one_lane_scene(quad(
        LogicalRect::new(px(0.0), px(0.0), px(64.0), px(64.0)),
        Color::srgb(1.0, 0.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &scene,
            DevicePixelRatio::ONE,
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

#[test]
fn the_resize_bucket_sequence_only_changes_at_alignment_boundaries() {
    use crate::platform::windows::composition::buffer_size;

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
    assert_eq!(
        buffer_size((1200, 640)).expect("fits"),
        buffer_size((1210, 648)).expect("fits")
    );
    assert!(buffer_size((0, 0)).expect("fits").0 >= 256);
}

#[test]
fn the_rtv_heap_holds_only_the_persist_descriptor() {
    let _guard = lock();
    let Some(renderer) = warp_renderer((8, 8)) else {
        return;
    };
    let persist = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 0);
    assert_ne!(persist.ptr, 0, "持久目标必须有 RTV");
    let desc = unsafe { renderer.rtv_heap.GetDesc() };
    assert_eq!(desc.NumDescriptors, 1);
}

#[test]
fn a_background_refresh_never_touches_the_main_content_persist() {
    let _guard = lock();
    let Some(mut renderer) = warp_renderer((64, 64)) else {
        return;
    };
    let red = one_lane_scene(quad(
        LogicalRect::new(px(0.0), px(0.0), px(64.0), px(64.0)),
        Color::srgb(1.0, 0.0, 0.0, 1.0),
    ));
    let mut encoder = InstanceEncoder::new();
    let buffer = encoder
        .encode(
            &red,
            DevicePixelRatio::ONE,
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
            DevicePixelRatio::ONE,
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
            partial_plan(&[DeviceRect::new(dp(0), dp(0), dp(8), dp(8))], (16, 16)).plan(),
            false
        ),
        FrameDraw::Incremental
    );

    let mut second = partial_plan(&[DeviceRect::new(dp(0), dp(0), dp(8), dp(8))], (16, 16));
    renderer
        .submit_buffer(FrameIndex::from_raw(2), buffer, &mut second, &mut timings)
        .expect("partial frame");
    assert_eq!(renderer.target_state(), TargetState::PersistValid);
}
#[test]
fn incremental_instance_upload_matches_a_full_upload() {
    let _guard = lock();
    let Some(mut incremental) = warp_renderer((64, 64)) else {
        return;
    };
    let store = crate::paint::geometry_store::GeometryStore::new();

    let mut scene = Scene::empty();
    let red = scene.new_lane();
    assert!(scene.push(
        red,
        quad(
            LogicalRect::new(px(8.0), px(8.0), px(16.0), px(16.0)),
            Color::srgb(1.0, 0.0, 0.0, 1.0)
        )
    ));
    let blue = scene.new_lane();
    assert!(scene.push(
        blue,
        quad(
            LogicalRect::new(px(32.0), px(8.0), px(16.0), px(16.0)),
            Color::srgb(0.0, 0.0, 1.0, 1.0)
        )
    ));
    scene.set_order(&[red, blue]);

    let mut encoder = InstanceEncoder::new();
    encoder
        .encode(&scene, DevicePixelRatio::ONE, resolver(), &store)
        .expect("初帧");
    let mut timings = FrameTimings::new();
    let mut frame = full_plan((64, 64));
    let first = crate::render::frame::FramePayload::from_buffer_delta(
        encoder.buffer_mut(),
        Vec::new(),
        Vec::new(),
    );
    let stats = incremental
        .submit_snapshot(FrameIndex::from_raw(1), &first, &mut frame, &mut timings)
        .expect("首帧");
    assert_eq!(stats.instances, 2);

    assert!(scene.clear_lane(blue));
    assert!(scene.push(
        blue,
        quad(
            LogicalRect::new(px(32.0), px(32.0), px(16.0), px(16.0)),
            Color::srgb(0.0, 1.0, 0.0, 1.0)
        )
    ));
    encoder
        .encode(&scene, DevicePixelRatio::ONE, resolver(), &store)
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

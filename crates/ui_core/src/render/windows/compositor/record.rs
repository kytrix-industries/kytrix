//! 帧录制：上传写槽 → clear / scissor pass 绘制 → copy 到 back buffer → 提交
//! （`FrameRecorder::record`、`submit_recorded`、`prime_detached` 的实现体）。
//! 实现体由 `compositor.rs` 的薄委托调用（零行为变更的结构拆分）。

use std::{sync::Arc, time::Instant};

use windows::Win32::{
    Foundation::RECT,
    Graphics::{Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, Direct3D12::*},
};

use super::{
    FrameRecorder, GpuRenderer, Persist, PresentTarget, PrimePersist,
    plan::{copy_content, copy_to_back},
    timings::elapsed_ns,
};
use crate::{
    error::EngineError,
    paint::{
        atlas::AtlasKind,
        instance::{FrameOp, GeometryDrawGpu},
    },
    render::{
        RenderStats,
        compositor::{FramePlan, Plan},
        frame::{FramePayload, FrameSlot},
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample, Section},
        uniform::{ROOT_UNIFORM_DWORDS, RootUniform},
        windows::{
            device::SharedGpu,
            pipeline::{GeometryRegistry, Pipeline, SrvHeap},
            swapchain::Swapchain,
        },
    },
    window::FrameIndex,
};

/// 消费者：`GpuRenderer::prime_detached`（`compositor.rs` 的薄委托）。
/// 把一帧背景色 prime 进 `swapchain`（全量 clear + 等 GPU fence），**不碰 visual**。
///
/// 用于新交换链的第一次 present：组合交换链的第一次 present 必须是全量
/// （`DirtyRectsCount = 0`），而 `WS_EX_NOREDIRECTIONBITMAP` 的客户区在第一次 present 之前
/// 完全透明。`new_for_composition` / `grow` / `maybe_shrink` / `refresh_background` 必须在
/// `SetContent` **之前**调用本方法：已 attach 但从未 present 的交换链在合成器里是透明的，
/// 若先 `SetContent` + `Commit` 再 prime，中间一次 DWM 合成就会露出桌面（P1-B）。这里用显式
/// 的 `swapchain` / `persist` / `buffer` / `content`，而不是 `renderer.swapchain`（它可能还是旧的）。
///
/// 背景层的 1×1 交换链也走这里（`persist = PrimePersist::Background`、
/// `buffer = content = (1, 1)`）：prime 只 clear 那块独立的 1×1 持久目标，再 copy 左上角
/// 1×1 —— 拷到的必然是背景色，主持久目标一个像素都不碰。
///
/// `source` 是缩容 prime 的内容来源（旧主持久目标），在 clear 之后、绘制之前把
/// `(0, 0, content)` 拷进新目标的左上角；其余 prime 为 `None`。
///
/// `sync = 0`：prime 不是用户可见帧，不值得等 vsync —— 但**必须**等这一帧的队列 fence
/// （P2-1）：`Present1` 返回只是 CPU 侧入队，`ShowWindow` / 后续帧的 waitable 之前必须有
/// 已完成的合成内容。
pub(super) fn prime_detached(
    renderer: &mut GpuRenderer,
    swapchain: &mut Swapchain,
    buffer: (u32, u32),
    content: (u32, u32),
    persist: PrimePersist,
    source: Option<&Persist>,
) -> Result<(), EngineError> {
    let slot = renderer.stream.acquire_slot();
    renderer
        .stream
        .wait_slot_fence(slot.wait_value)
        .map_err(|error| renderer.classify(error))?;
    let payload = FramePayload::empty();
    let mut frame = FramePlan::new(Plan::Full, true);
    let mut timings = FrameTimings::new();
    // prime 不是内容帧：不要让它把 `last_index`（retire 的 tag）推回 `FrameIndex::default()`。
    // `grow` / `maybe_shrink` 已经在 prime 之前取好 `retired_index`；背景层刷新则发生在帧
    // 之间，两者都不该改变「最后一次内容帧」的记账。
    let last_index = renderer.last_index;
    renderer.submit_recorded(
        slot,
        FrameIndex::default(),
        &payload,
        &mut frame,
        &mut timings,
        PresentTarget::Detached {
            swapchain,
            buffer,
            content,
            persist,
            source,
        },
    )?;
    renderer.last_index = last_index;
    swapchain
        .present1(&mut frame, 0, content)
        .map_err(|error| renderer.classify(error))?;
    // 等到本帧的 GPU 工作完成（`FrameSlot::signal_value`），不是 `Present1` 的 CPU 返回。
    renderer
        .stream
        .wait_slot_fence(slot.signal_value)
        .map_err(|error| renderer.classify(error))?;
    Ok(())
}

/// 消费者：`GpuRenderer::submit_recorded`（`compositor.rs` 的薄委托）。
/// 录制 + 提交一帧（不呈现、不等完成）。调用方先取槽并等该槽上一帧完成（§6.5）。
///
/// `target` 决定录制的呈现目标：常规帧用自己的交换链；`prime_detached` 用**还没挂到
/// visual 上**的新交换链 + 显式持久目标（grow / 缩容 prime 用主持久目标，背景层 prime 用
/// 1×1 背景持久目标）。
///
/// 共享图集的页对齐在录制之前完成：**图集读锁**在整个录制 + 提交区间内持有（共享页在帧内
/// 不会变；别的窗口的页重建是写锁，会等本帧提交完）。发现待上传页与共享页不一致时先释放
/// 读锁、在写锁里重建（[`SharedGpu::rebuild_atlas_pages`]），然后重试。
pub(super) fn submit_recorded(
    renderer: &mut GpuRenderer,
    slot: FrameSlot,
    index: FrameIndex,
    payload: &FramePayload,
    frame: &mut FramePlan,
    timings: &mut FrameTimings,
    target: PresentTarget<'_>,
) -> Result<(RenderStats, FrameSlot), EngineError> {
    if let Some(error) = renderer.shared.lost_error() {
        return Err(error);
    }
    timings.reset(index);
    debug_assert_eq!(
        timings.frame(),
        index,
        "timings must be reset for this frame"
    );
    let frame_started = Instant::now();
    let shared = Arc::clone(&renderer.shared);
    loop {
        // 图集读锁：持有到本帧提交完成（页重建是写锁，会等这里释放）。
        let atlas = shared.atlas();
        // 待上传页 ≠ 共享页 ⇒ 先落地页换代：全页上传触发重建，过期增量直接丢弃。
        let rebuild = renderer
            .atlas_uploads
            .reconcile_pages([atlas.page(AtlasKind::Glyph), atlas.page(AtlasKind::Image)]);
        if !rebuild.is_empty() {
            drop(atlas);
            shared
                .rebuild_atlas_pages(&rebuild)
                .map_err(|error| renderer.classify(error))?;
            continue;
        }
        // staging 对齐到共享页（第一次上传 / 页换代之后整组重建本窗口的 staging）。
        renderer
            .atlas_uploads
            .sync_staging(&shared, &atlas)
            .map_err(|error| renderer.classify(error))?;

        renderer
            .stream
            .reset_slot(slot)
            .map_err(|error| renderer.classify(error))?;

        let stats = {
            let (swapchain, buffer, content, timer, persist, source) = match target {
                PresentTarget::Own => (
                    renderer.swapchain.as_ref(),
                    renderer.buffer,
                    renderer.surface,
                    renderer.timer.as_ref(),
                    &renderer.persist,
                    None,
                ),
                PresentTarget::Detached {
                    swapchain,
                    buffer,
                    content,
                    persist,
                    source,
                } => {
                    let persist = match persist {
                        PrimePersist::Main => &renderer.persist,
                        PrimePersist::Background => renderer
                            .background_persist
                            .as_ref()
                            .ok_or_else(|| EngineError::Composition {
                                detail: smol_str::SmolStr::new_static(
                                    "background prime without a 1x1 background persist",
                                ),
                            })?,
                    };
                    (Some(swapchain), buffer, content, None, persist, source)
                }
            };
            // 借用拆分：录制只碰这些字段，`buffer` / `surface` / `fresh` / `last_index` 留在外层。
            let mut recorder = FrameRecorder {
                shared: &shared,
                stream: &mut renderer.stream,
                pipeline: shared.pipeline(),
                descriptors: &renderer.descriptors,
                atlas: &atlas,
                atlas_uploads: &mut renderer.atlas_uploads,
                persist,
                upload: &mut renderer.upload,
                geometry: &mut renderer.geometry,
                timer,
                swapchain,
                source,
                clear_color: renderer.clear_color,
                buffer,
                content,
                scale: renderer.scale,
            };
            recorder.record(slot, index, payload, frame, timings, frame_started)
        }
        .map_err(|error| renderer.classify(error))?;

        renderer
            .stream
            .submit(index, slot)
            .map_err(|error| renderer.classify(error))?;
        renderer.last_index = index;
        return Ok((stats, slot));
    }
}

/// 消费者：`FrameRecorder::record`（`compositor.rs` 的薄委托）。
/// 上传 + 图集 + clear/scissor 绘制 + copy 到 back buffer + 计时（不提交）。
pub(super) fn record(
    recorder: &mut FrameRecorder<'_>,
    slot: FrameSlot,
    index: FrameIndex,
    payload: &FramePayload,
    frame: &mut FramePlan,
    timings: &mut FrameTimings,
    frame_started: Instant,
) -> Result<RenderStats, EngineError> {
    let (instance_count, clip_count) = recorder.upload.write(
        recorder.shared,
        recorder.stream,
        index,
        slot.index,
        payload.instances(),
        payload.clips(),
    )?;
    let srv_heap = recorder.shared.srv_heap();
    srv_heap.write_slot(
        recorder.shared.raw(),
        recorder.descriptors.base(),
        slot.index,
        recorder.upload.slot(slot.index).instances(),
        instance_count,
        recorder.upload.slot(slot.index).clips(),
        clip_count,
    );
    timings.push(GpuTimerSample {
        section: Section::Upload,
        cpu_ns: elapsed_ns(frame_started),
        gpu_ns: 0,
    });

    let root = root_uniform(
        recorder.content.0 as f32,
        recorder.content.1 as f32,
        recorder.scale,
    );
    let full = frame.is_full();
    let scissor_whole = RECT {
        left: 0,
        top: 0,
        right: recorder.content.0 as i32,
        bottom: recorder.content.1 as i32,
    };
    // 几何绘制的下标表与 ops 同帧：`FrameOp::Geometry.index` 只有配合同一帧的表才有意义。
    let mut draw_calls = 0u32;
    // staging 扩容造成的旧句柄：录制块结束后再 retire（`list` 借用 `recorder.stream`）。
    let atlas_retired: Vec<GpuResourceHandle>;
    // 几何换代（槽位复用）产生的旧缓冲句柄，同样在录制块结束后 retire。
    let mut geometry_retired: Vec<GpuResourceHandle> = Vec::new();
    // 推进几何 registry 的帧号：`ensure` 记录 last_used，帧末 `retain_recent` 驱逐
    // 长期未绘制的几何（D-1：否则应用释放句柄后 GPU 缓冲永不回收）。
    recorder.geometry.begin_frame();
    {
        let list = recorder.stream.list(slot);
        if let Some(timer) = recorder.timer {
            timer.begin(list, slot.index);
        }
        atlas_retired = recorder.atlas_uploads.record_uploads(
            recorder.shared,
            recorder.atlas,
            list,
            slot.index,
        )?;
        if full {
            // `fresh`（交换链刚建好 / prime）必须清整块 buffer：prime 之后、UI 把内容 clip
            // 更新到新尺寸之前，visual 的 clip 可能仍指向旧内容（P1-A 保证它 ≤ 新 buffer），
            // 只有整块背景色能保证那一帧既不是桌面、也不是未定义像素（P1-B）。
            // 普通全量帧只清内容矩形：buffer 的其余部分永远不可见（copy / present dirty 都
            // 夹在内容子矩形内），省掉大 buffer 的全量 clear 带宽。
            let content_rect = [RECT {
                left: 0,
                top: 0,
                right: recorder.content.0 as i32,
                bottom: recorder.content.1 as i32,
            }];
            let rects = if frame.is_fresh() {
                None
            } else {
                Some(content_rect.as_slice())
            };
            // SAFETY: 目标 RT 处于 `RENDER_TARGET`（本帧未拷贝到 swapchain 时恒如此）；
            // `rects` 在调用期间存活且落在 buffer 内（内容 ≤ buffer）。
            unsafe {
                list.ClearRenderTargetView(recorder.persist.rtv, &recorder.clear_color, rects)
            };
        }
        // 缩容 prime：新目标整块已经 clear 成背景色（内容之外不可见），把旧目标的内容原样
        // 拷进左上角；此后这一帧的绘制 / copy 都发生在新目标上。源目标仍是 `RENDER_TARGET`。
        if let Some(source) = recorder.source {
            debug_assert!(
                full && frame.is_fresh(),
                "a source-copy prime is a fresh full frame"
            );
            copy_content(list, source, recorder.persist, recorder.content);
        }
        bind_frame(
            list,
            recorder.pipeline,
            srv_heap,
            recorder.descriptors.base(),
            recorder.persist,
            slot.index,
            root,
            recorder.content,
        );
        match full {
            true => {
                draw_calls = draw_ops(
                    list,
                    recorder.pipeline,
                    payload.ops(),
                    recorder.geometry,
                    recorder.shared,
                    &mut geometry_retired,
                    payload,
                    root,
                    scissor_whole,
                )?;
            }
            false => {
                if let Plan::Partial(damage) = frame.plan() {
                    for rect in damage.as_slice() {
                        let scissor = RECT {
                            left: rect.x0,
                            top: rect.y0,
                            right: rect.x1,
                            bottom: rect.y1,
                        };
                        // SAFETY: RTV / 视口 / 矩形由本帧持有，调用期间有效。
                        unsafe { list.RSSetScissorRects(&[scissor]) };
                        draw_calls = draw_calls.saturating_add(draw_ops(
                            list,
                            recorder.pipeline,
                            payload.ops(),
                            recorder.geometry,
                            recorder.shared,
                            &mut geometry_retired,
                            payload,
                            root,
                            scissor,
                        )?);
                    }
                }
            }
        }
        if let Some(swapchain) = recorder.swapchain {
            copy_to_back(
                list,
                recorder.persist,
                swapchain,
                *frame,
                recorder.buffer,
                recorder.content,
            )?;
        }
        if let Some(timer) = recorder.timer {
            timer.end(list, slot.index);
        }
    }
    // 录制完成后驱逐长期未绘制的几何（设备对象进同一个 retire 列表）。
    recorder.geometry.retain_recent(&mut geometry_retired);
    for handle in atlas_retired {
        recorder.stream.retire(index, handle);
    }
    for handle in geometry_retired {
        recorder.stream.retire(index, handle);
    }

    Ok(RenderStats {
        draw_calls,
        instances: instance_count,
    })
}

/// 消费者：`FrameRecorder::record`。
pub(super) fn bind_frame(
    list: &ID3D12GraphicsCommandList,
    pipeline: &Pipeline,
    srv_heap: &SrvHeap,
    descriptor_base: u32,
    persist: &Persist,
    slot: usize,
    root: RootUniform,
    content: (u32, u32),
) {
    // SAFETY: RTV / 视口 / 矩形由本帧持有，调用期间有效；共享描述符堆由调用方持有。
    unsafe {
        list.SetDescriptorHeaps(&[Some(srv_heap.heap().clone())]);
        list.SetGraphicsRootSignature(pipeline.root_signature());
        list.SetPipelineState(pipeline.quad());
        list.SetGraphicsRoot32BitConstants(
            0,
            ROOT_UNIFORM_DWORDS as u32,
            root.words().as_ptr().cast(),
            0,
        );
        list.SetGraphicsRootDescriptorTable(1, srv_heap.slot_table(descriptor_base, slot));
        list.SetGraphicsRootDescriptorTable(2, srv_heap.atlas_table());
        list.OMSetRenderTargets(1, Some(&persist.rtv), false, None);
        // 视口 = **内容**尺寸（不是 buffer）：Ndc 坐标由 root uniform 的内容尺寸给出，
        // 两者必须是同一个矩形，否则内容会被拉伸到整块 buffer 上。
        list.RSSetViewports(&[D3D12_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: content.0 as f32,
            Height: content.1 as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }]);
    }
}

/// 消费者：`FrameRecorder::record`。
pub(super) fn draw_ops(
    list: &ID3D12GraphicsCommandList,
    pipeline: &Pipeline,
    ops: &[FrameOp],
    geometry: &mut GeometryRegistry,
    shared: &SharedGpu,
    retired: &mut Vec<GpuResourceHandle>,
    payload: &FramePayload,
    root: RootUniform,
    scissor: RECT,
) -> Result<u32, EngineError> {
    let mut draw_calls = 0u32;
    // 局部刷新时跳过与 damage 不相交的 op（P1-2.1）：旧实现对每个 damage 矩形重放**整份**
    // op 列表，实测使增量帧比全量帧更慢（288 µs vs 222 µs）。
    let scissor_extent = [scissor.left, scissor.top, scissor.right, scissor.bottom];
    let mut skipped = 0u32;
    for op in ops {
        if !op.bounds().intersects(scissor_extent) {
            skipped = skipped.saturating_add(1);
            continue;
        }
        match *op {
            FrameOp::Instances { start, len, .. } => {
                if len == 0 {
                    continue;
                }
                // 实例基址只经 root constant `draw.clip.y` 进 shader，硬件基址恒 0
                // （D3D12 的 StartInstanceLocation **不**加到 SV_InstanceID 上，contract §6）。
                let uniform = root.with_instance_base(start);
                // SAFETY: PSO / 根常量 / 拓扑 / scissor 都属于同一条已打开的 list。
                unsafe {
                    list.SetPipelineState(pipeline.quad());
                    list.SetGraphicsRoot32BitConstants(
                        0,
                        ROOT_UNIFORM_DWORDS as u32,
                        uniform.words().as_ptr().cast(),
                        0,
                    );
                    list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
                    list.RSSetScissorRects(&[scissor]);
                }
                // SAFETY: 4 顶点 TriangleStrip + 实例区间 [0, len) 已在编码期校验；
                // 段起点由 shader 侧的 `draw.clip.y` 承担。
                unsafe { list.DrawInstanced(4, len, 0, 0) };
                draw_calls = draw_calls.saturating_add(1);
            }
            FrameOp::Geometry {
                index,
                clip_index,
                clip_rect,
                ..
            } => {
                let draw = payload.geometry().get(index as usize).ok_or_else(|| {
                    EngineError::GeometryRegister {
                        detail: smol_str::format_smolstr!("geometry op index {index} out of range"),
                    }
                })?;
                // 首次见到该 id（或槽位换代）时现场建缓冲；`desc` 随帧过线。
                let entry = geometry.ensure(shared, draw.id, &draw.desc, retired)?;
                let uniform = geometry_root(root, draw.clone(), clip_index, clip_rect);
                // SAFETY: 顶点/索引缓冲由 `GeometryRegistry` 持有到本帧结束；
                // `entry.topology()` 与注册时的图元类型一致。
                unsafe {
                    list.SetPipelineState(pipeline.geometry(entry.pipeline()));
                    list.SetGraphicsRoot32BitConstants(
                        0,
                        ROOT_UNIFORM_DWORDS as u32,
                        uniform.words().as_ptr().cast(),
                        0,
                    );
                    list.IASetPrimitiveTopology(entry.topology());
                    list.IASetVertexBuffers(0, Some(&[entry.vertex()]));
                    list.IASetIndexBuffer(Some(&entry.index()));
                    list.RSSetScissorRects(&[scissor]);
                    list.DrawIndexedInstanced(entry.index_count(), 1, 0, 0, 0);
                }
                draw_calls = draw_calls.saturating_add(1);
            }
        }
    }
    Ok(draw_calls)
}

/// 消费者：`FrameRecorder::record`。
/// `RootUniform`（112 B）：`frame @0`、`draw @32`（P07 §2.7）。
///
/// 布局由 `render::uniform::RootUniform` 唯一实现；本函数只补 Windows 的 DPI（`scale` 来自
/// 窗口，经 `GpuRenderer::scale` 传入）。
pub(super) fn root_uniform(width: f32, height: f32, scale: f32) -> RootUniform {
    RootUniform::frame(width, height, scale)
}

/// 消费者：`draw_ops`。
/// 逐 geometry 的 draw uniform：在帧级常量之上覆盖 draw 块。
///
/// 布局由 `render::uniform::RootUniform` 唯一实现（contract §6）；本函数只负责把编码期
/// 的 clip 前缀换算成契约要求的 `(x, y, w, h)`。颜色与变换已经在 UI 线程的编码器里解析成
/// GPU 可直接消费的形式（`GeometryDrawGpu`），GPU 线程不做色彩空间运算。
pub(super) fn geometry_root(
    frame: RootUniform,
    draw: GeometryDrawGpu,
    clip_index: u32,
    clip_rect: [u32; 4],
) -> RootUniform {
    // draw.clip_rect @20 = (x, y, w, h) 物理像素。无裁剪时用视口，避免 inf 进根常量；
    // 有裁剪时把编码器给的 AABB `(x0, y0, x1, y1)` 换成 `(x, y, w, h)`。
    let rect = if clip_index == 0 {
        [0.0, 0.0, frame.words()[0], frame.words()[1]]
    } else {
        let x0 = f32::from_bits(clip_rect[0]);
        let y0 = f32::from_bits(clip_rect[1]);
        let x1 = f32::from_bits(clip_rect[2]);
        let y1 = f32::from_bits(clip_rect[3]);
        [x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)]
    };
    RootUniform::geometry(frame, draw.transform, draw.color, clip_index, rect)
}

//! 帧录制：上传写槽 → clear / scissor pass 绘制 → （必要时）copy 到 back buffer → 提交
//! （`FrameRecorder::record`、`submit_recorded`、`prime_detached` 的实现体）。
//!
//! 本帧的绘制模式由 [`super::plan::frame_draw`] 决定（R08-8）：
//! - `Direct`：整帧**直接画进 back buffer** 并整面 present —— 零拷贝；
//! - `Full`：整帧画进 persist 再整块拷贝（离屏 / 快照目标，没有 back buffer）；
//! - `Incremental`：局部画进 persist，按 [`super::plan::BufferSyncs`] 记账只拷矩形。
//!
//! damage 在**本模块入口**夹紧到当前内容矩形（R08-10 的后端二次防御），夹紧后的同一份集合
//! 同时用于 scissor / copy / `Present1` —— 三个消费点不再各自从 `FramePlan` 重取。

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use windows::Win32::{
    Foundation::RECT,
    Graphics::{Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, Direct3D12::*},
};

use super::{
    FrameRecorder, GpuRenderer, RecordedFrame,
    plan::{
        self, BufferSyncs, CopyExtent, FrameDraw, TargetState, clamp_damage, copy_to_back,
        transition,
    },
    timings::elapsed_ns,
};
use crate::{
    error::EngineError,
    paint::{
        atlas::AtlasKind,
        geometry::GeometryPipeline,
        instance::{FrameOp, GeometryDrawGpu, PhysicalBounds, scroll_offset},
    },
    render::{
        RenderStats,
        compositor::{DamageRects, FramePlan, MAX_DRAW_PASSES, Plan},
        frame::{FramePayload, FrameSlot},
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample, Section},
        uniform::{ROOT_UNIFORM_DWORDS, RootUniform},
        windows::{
            device::AtlasRebuildOutcome,
            diagnostics,
            hresult::PresentOutcome,
            pipeline::{GeometryRegistry, Pipeline, SrvHeap},
            swapchain::Swapchain,
        },
    },
    window::FrameIndex,
};

/// 图集页换代遇到「目标描述符代仍被在飞引用」时的重试间隔（C3）。
///
/// 页换代是稀有事件（首建上传 / 字体或 DPI 换代）；一次退避只等一轮 GPU 完成观测，正常情况
/// 1–3 次重试即可拿到空闲代。
const ATLAS_REBUILD_DEFER_RETRY: Duration = Duration::from_millis(1);

/// 图集页换代退避的上限（C3）。超时说明已提交工作长时间没有完成 —— 设备健康但无法推进，
/// 按显式错误上报（不静默丢 damage、不无限自旋）。
const ATLAS_REBUILD_DEFER_LIMIT: Duration = Duration::from_secs(1);

/// 消费者：`GpuRenderer::prime_detached`（`compositor.rs` 的薄委托）。
///
/// 把一条**还没挂到 visual 上**的交换链 prime 成背景色：clear 整块 back buffer + `Present1`
/// 整面，等 GPU 完成。用于：首建（内容层 + 1×1 背景层）、`ResizeBuffers` 失败后的回退重建、
/// 背景色变化时换掉 1×1 背景层。
///
/// `WS_EX_NOREDIRECTIONBITMAP` 的客户区在第一次 present 之前完全透明，而「已 attach 但从未
/// present」的交换链在合成器里也是透明的：所以必须**先** prime、**后** `SetContent` + `Commit`。
///
/// prime 走**正常的 `Direct` 直绘路径**（`FramePayload::empty()` + `Plan::Full`）：clear 整块
/// back buffer 后整面 present，不再借主持久目标 —— 1×1 背景层因此不需要自己的持久目标，
/// `grow` 那套「prime 用的临时持久目标」也不再存在。`sync = 0`：prime 不是用户可见帧，不值得
/// 等 vsync。
pub(super) fn prime_detached(
    renderer: &mut GpuRenderer,
    swapchain: &mut Swapchain,
) -> Result<(), EngineError> {
    let slot = renderer.stream.acquire_slot();
    if !renderer
        .stream
        .wait_slot_fence(slot.wait_value)
        .map_err(|error| renderer.classify(error))?
        .is_signaled()
    {
        return Err(EngineError::AppInit {
            detail: smol_str::SmolStr::new_static(
                "prime requires an idle frame slot (device healthy but GPU did not respond)",
            ),
        });
    }
    renderer
        .stream
        .reset_slot(slot)
        .map_err(|error| renderer.classify(error))?;
    // 空载荷：prime 只 clear + present，不画任何图元（也**不**消费图集上传路径）。
    let payload = FramePayload::empty();
    let mut frame = FramePlan::new(Plan::Full, true);
    let mut timings = FrameTimings::new();
    let mut target = TargetState::Fresh;
    let mut syncs = BufferSyncs::new();
    let mut buckets = OpBuckets::new();
    let shared = Arc::clone(&renderer.shared);
    let atlas = shared.atlas();
    let recorded = {
        let mut recorder = FrameRecorder {
            shared: &shared,
            stream: &mut renderer.stream,
            pipeline: shared.pipeline(),
            descriptors: &renderer.descriptors,
            atlas: &atlas,
            atlas_uploads: &mut renderer.atlas_uploads,
            persist: &renderer.persist,
            upload: &mut renderer.upload,
            geometry_upload: &mut renderer.geometry_upload,
            mirror: &mut renderer.mirror,
            geometry: &mut renderer.geometry,
            timer: None,
            swapchain: Some(swapchain),
            clear_color: renderer.clear_color,
            content: swapchain.buffer(),
            scale: renderer.scale,
            target: &mut target,
            syncs: &mut syncs,
            buckets: &mut buckets,
            prime: true,
        };
        recorder.record(slot, &payload, &mut frame, &mut timings, Instant::now())?
    };
    renderer
        .stream
        .submit(slot)
        .map_err(|error| renderer.classify(error))?;
    match swapchain.present1(recorded.present.as_ref(), 0) {
        PresentOutcome::Presented | PresentOutcome::Clipped | PresentOutcome::Occluded => {}
        other => {
            return Err(EngineError::Swapchain {
                detail: smol_str::format_smolstr!("prime Present1: {other:?}"),
            });
        }
    }
    if !renderer
        .stream
        .wait_slot_fence(slot.signal_value)
        .map_err(|error| renderer.classify(error))?
        .is_signaled()
    {
        return Err(EngineError::Swapchain {
            detail: smol_str::SmolStr::new_static(
                "prime Present1 did not complete (device healthy but GPU did not respond)",
            ),
        });
    }
    Ok(())
}
/// 消费者：`GpuRenderer::submit_recorded`（`compositor.rs` 的薄委托）。
/// 录制 + 提交一帧（不呈现、不等完成）。调用方先取槽并等该槽上一帧完成（§6.5）。
///
/// 共享图集的页对齐在录制之前完成：**图集读锁**在整个录制 + 提交区间内持有（共享页在帧内
/// 不会变；别的窗口的页重建是写锁，会等本帧提交完）。发现待上传页与共享页不一致时先释放
/// 读锁、在写锁里重建（[`SharedGpu::rebuild_atlas_pages`]），然后重试。
///
/// C3：页换代可能返回 [`AtlasRebuildOutcome::Deferred`]（目标描述符代仍有在飞引用）。此时**本帧
/// 尚未提交任何东西**，短暂退避后重试即可（帧请求仍由调用方持有，damage 不丢）；退避有上限
/// （[`ATLAS_REBUILD_DEFER_LIMIT`]），超时说明 GPU 长时间没有完成已提交工作，按设备健康但无法
/// 推进的显式错误上报。
pub(super) fn submit_recorded(
    renderer: &mut GpuRenderer,
    slot: FrameSlot,
    index: FrameIndex,
    payload: &FramePayload,
    frame: &mut FramePlan,
    timings: &mut FrameTimings,
) -> Result<(RecordedFrame, FrameSlot), EngineError> {
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
    let mut defer_deadline: Option<Instant> = None;
    loop {
        if let Some(error) = shared.lost_error() {
            return Err(error);
        }
        // 图集读锁：持有到本帧提交完成（页重建是写锁，会等这里释放）。
        let atlas = shared.atlas();
        // 待上传页 ≠ 共享页 ⇒ 先落地页换代：全页上传触发重建，过期增量直接丢弃。
        let rebuild = renderer
            .atlas_uploads
            .reconcile_pages([atlas.page(AtlasKind::Glyph), atlas.page(AtlasKind::Image)]);
        if !rebuild.is_empty() {
            drop(atlas);
            match shared
                .rebuild_atlas_pages(&rebuild)
                .map_err(|error| renderer.classify(error))?
            {
                AtlasRebuildOutcome::Applied => {}
                AtlasRebuildOutcome::Deferred => {
                    let deadline = *defer_deadline
                        .get_or_insert_with(|| Instant::now() + ATLAS_REBUILD_DEFER_LIMIT);
                    if Instant::now() >= deadline {
                        return Err(EngineError::Swapchain {
                            detail: smol_str::SmolStr::new_static(
                                "atlas page rebuild kept waiting for a free descriptor generation",
                            ),
                        });
                    }
                    std::thread::sleep(ATLAS_REBUILD_DEFER_RETRY);
                }
            }
            continue;
        }
        // staging 对齐到共享页（第一次上传 / 页换代之后整组重建本窗口的 staging）。
        // 被换下来的旧 staging 句柄必须走 retire（R08-2：D3D12 不替应用保活命令列表引用）。
        let mut staging_retired: Vec<GpuResourceHandle> = Vec::new();
        renderer
            .atlas_uploads
            .sync_staging(&shared, &atlas, &mut staging_retired)
            .map_err(|error| renderer.classify(error))?;

        renderer
            .stream
            .reset_slot(slot)
            .map_err(|error| renderer.classify(error))?;

        let recorded = {
            let swapchain = renderer.swapchain.as_ref();
            let mut recorder = FrameRecorder {
                shared: &shared,
                stream: &mut renderer.stream,
                pipeline: shared.pipeline(),
                descriptors: &renderer.descriptors,
                atlas: &atlas,
                atlas_uploads: &mut renderer.atlas_uploads,
                persist: &renderer.persist,
                upload: &mut renderer.upload,
                geometry_upload: &mut renderer.geometry_upload,
                mirror: &mut renderer.mirror,
                geometry: &mut renderer.geometry,
                timer: renderer.timer.as_ref(),
                swapchain,
                clear_color: renderer.clear_color,
                content: renderer.surface,
                scale: renderer.scale,
                target: &mut renderer.target,
                syncs: &mut renderer.buffer_syncs,
                buckets: &mut renderer.op_buckets,
                prime: false,
            };
            recorder.record(slot, payload, frame, timings, frame_started)
        }
        .map_err(|error| renderer.classify(error))?;

        renderer
            .stream
            .submit(slot)
            .map_err(|error| renderer.classify(error))?;
        renderer.last_sequence = slot.signal_value;
        for handle in staging_retired {
            renderer.stream.retire(slot.signal_value, handle);
        }
        return Ok((recorded, slot));
    }
}

/// 消费者：`FrameRecorder::record`（`compositor.rs` 的薄委托）。
/// 上传 + 图集 + 绘制（直绘 / persist）+ copy + 计时（不提交）。
pub(super) fn record(
    recorder: &mut FrameRecorder<'_>,
    slot: FrameSlot,
    payload: &FramePayload,
    frame: &mut FramePlan,
    timings: &mut FrameTimings,
    frame_started: Instant,
) -> Result<RecordedFrame, EngineError> {
    // prime 帧（`FramePayload::empty()`）不画任何图元：不写上传缓冲、也不碰图集上传路径
    // （prime 与图集页换代的 retire 记账不能混在一起）。
    let (_instance_count, _clip_count) = if recorder.prime {
        (0, 0)
    } else {
        let written = recorder.upload.write(
            recorder.mirror,
            recorder.shared,
            recorder.stream,
            slot.signal_value,
            slot.index,
            payload,
        )?;
        recorder.shared.srv_heap().write_slot(
            recorder.shared.raw(),
            recorder.descriptors.base(),
            slot.index,
            recorder.upload.slot(slot.index).instances(),
            written.0,
            recorder.upload.slot(slot.index).clips(),
            written.1,
        );
        written
    };
    let srv_heap = recorder.shared.srv_heap();
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
    let content = recorder.content;
    // R08-11：窗口路径上 `FramePlan` 的 fresh 必须与 `TargetState` 同源（唯一状态源的不变量；
    // 离屏 / 快照路径由调用方显式构造计划，fresh 只影响这里已经不再使用的 present 语义）。
    debug_assert!(
        recorder.swapchain.is_none() || frame.is_fresh() == recorder.target.is_fresh(),
        "FramePlan::fresh must be derived from TargetState (R08-11)"
    );
    // R08-10：damage 在这里**夹紧到当前内容**，之后 scissor / copy / present 共用同一份集合。
    // 夹紧之后没有可画矩形的局部帧**升级为整帧**（不是错误：旧 surface 的 damage 对新 surface
    // 没有意义，而这一帧的可见区域必然有变化）。
    let damage = clamp_damage(frame.present_dirty().as_ref(), content);
    let draw = if plan::requires_full_draw(*frame, damage.as_ref()) {
        plan::frame_draw(*recorder.target, Plan::Full, recorder.swapchain.is_some())
    } else {
        plan::frame_draw(*recorder.target, frame.plan(), recorder.swapchain.is_some())
    };
    let scissor_whole = RECT {
        left: 0,
        top: 0,
        right: content.0 as i32,
        bottom: content.1 as i32,
    };
    let mut draw_calls = 0u32;
    // staging 扩容 / 页换代造成的旧句柄：录制块结束后再 retire（`list` 借用 `recorder.stream`）。
    let atlas_retired: Vec<GpuResourceHandle>;
    // 几何换代（槽位复用）产生的旧缓冲句柄，同样在录制块结束后 retire。
    let mut geometry_retired: Vec<GpuResourceHandle> = Vec::new();
    // 推进几何 registry 的帧号：`ensure` 记录 last_used，帧末 `retain_recent` 驱逐
    // 长期未绘制的几何（D-1：否则应用释放句柄后 GPU 缓冲永不回收）。
    if !recorder.prime {
        recorder.geometry.begin_frame();
    }
    let mut geometry_uploaded_bytes = 0usize;
    let present;
    {
        let list = recorder.stream.list(slot);
        if let Some(timer) = recorder.timer {
            timer.begin(list, slot.index);
        }
        atlas_retired = if recorder.prime {
            Vec::new()
        } else {
            recorder.atlas_uploads.record_uploads(
                recorder.shared,
                recorder.atlas,
                list,
                slot.index,
            )?
        };
        // X5：几何 DEFAULT 缓冲的页级增量上传（staging + `CopyBufferRegion`）。必须在任何
        // copy / draw 之前、在**同一条命令列表**里：队列按序执行 ⇒ 上一帧的顶点/索引读取一定
        // 先于本帧的 copy 完成（DEFAULT 堆因此不需要额外同步）。
        if !recorder.prime {
            geometry_uploaded_bytes = recorder.geometry.prepare(
                recorder.shared,
                list,
                recorder.geometry_upload,
                payload.geometry(),
                slot.index,
                &mut geometry_retired,
            )?;
        }
        match draw {
            FrameDraw::Direct => {
                // 整帧直绘当前 back buffer：**零拷贝**（R08-8）。
                let swapchain = recorder.swapchain.ok_or_else(|| EngineError::Swapchain {
                    detail: smol_str::SmolStr::new_static(
                        "direct draw requires a composition swapchain",
                    ),
                })?;
                let (buffer_index, back) = swapchain.back_buffer()?;
                transition(
                    list,
                    &back,
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                );
                let rtv = swapchain.rtv(buffer_index);
                // 整块 buffer 清成背景色（present 是整面，内容之外的像素也必须有定义）。
                // SAFETY: back buffer 处于 RENDER_TARGET；RTV 由交换链持有且在容量内。
                unsafe { list.ClearRenderTargetView(rtv, &recorder.clear_color, None) };
                bind_frame(
                    list,
                    recorder.pipeline,
                    srv_heap,
                    recorder.descriptors.base(),
                    recorder.shared.atlas_generation(),
                    rtv,
                    slot.index,
                    root,
                    content,
                );
                // SAFETY: 视口 / RTV 由本帧持有；矩形在内容内。
                unsafe { list.RSSetScissorRects(&[scissor_whole]) };
                draw_calls = draw_ops(
                    list,
                    recorder.pipeline,
                    payload.ops(),
                    None,
                    recorder.geometry,
                    payload,
                    root,
                    scissor_whole,
                )?
                .0;
                transition(
                    list,
                    &back,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    D3D12_RESOURCE_STATE_PRESENT,
                );
                // 直绘之后 persist 内容过期；全部 buffer 的同步记账作废（下次整帧重画）。
                recorder.target.on_direct_draw();
                recorder.syncs.invalidate();
                present = None;
            }
            FrameDraw::Full => {
                // 整帧画进 persist，再整块拷贝（离屏 / 快照目标；交换链路径上的「persist 过期」
                // 场景已经在 `frame_draw` 里改走直绘）。
                let content_rect = [RECT {
                    left: 0,
                    top: 0,
                    right: content.0 as i32,
                    bottom: content.1 as i32,
                }];
                // SAFETY: 目标 RT 处于 `RENDER_TARGET`；`content_rect` 落在 buffer 内。
                unsafe {
                    list.ClearRenderTargetView(
                        recorder.persist.rtv,
                        &recorder.clear_color,
                        Some(&content_rect),
                    )
                };
                bind_frame(
                    list,
                    recorder.pipeline,
                    srv_heap,
                    recorder.descriptors.base(),
                    recorder.shared.atlas_generation(),
                    recorder.persist.rtv,
                    slot.index,
                    root,
                    content,
                );
                // SAFETY: 视口 / RTV 由本帧持有；矩形在内容内。
                unsafe { list.RSSetScissorRects(&[scissor_whole]) };
                draw_calls = draw_ops(
                    list,
                    recorder.pipeline,
                    payload.ops(),
                    None,
                    recorder.geometry,
                    payload,
                    root,
                    scissor_whole,
                )?
                .0;
                if let Some(swapchain) = recorder.swapchain {
                    let (buffer_index, back) = swapchain.back_buffer()?;
                    copy_to_back(list, recorder.persist, &back, CopyExtent::Full)?;
                    // 当前 buffer 与 persist 一致；其余 buffer 错过了整帧（下一次整块拷）。
                    let _ = recorder.syncs.take_copy(buffer_index, None, content);
                    recorder.syncs.record_missed_others(buffer_index, None);
                } else {
                    recorder.syncs.invalidate();
                }
                recorder.target.on_full_persist_draw();
                present = None;
            }
            FrameDraw::Incremental => {
                // 局部帧：只在 persist 上按 damage scissor 重画，再按 buffer 记账只拷错过的矩形。
                // `frame_draw` 只在 persist 有效且本帧是**有矩形的**局部帧时返回
                // `Incremental`；这里取不到集合说明前面的一致性检查被绕过，按契约硬失败。
                let Some(damage) = damage else {
                    debug_assert!(false, "incremental frame requires a clamped damage set");
                    return Err(EngineError::AppInit {
                        detail: smol_str::SmolStr::new_static(
                            "incremental frame without a clamped damage set",
                        ),
                    });
                };
                bind_frame(
                    list,
                    recorder.pipeline,
                    srv_heap,
                    recorder.descriptors.base(),
                    recorder.shared.atlas_generation(),
                    recorder.persist.rtv,
                    slot.index,
                    root,
                    content,
                );
                recorder
                    .buckets
                    .build(payload.ops(), &damage, payload.scroll());
                for rect in damage.as_slice() {
                    let scissor = RECT {
                        left: rect.x0,
                        top: rect.y0,
                        right: rect.x1,
                        bottom: rect.y1,
                    };
                    // SAFETY: RTV / 视口 / 矩形由本帧持有，调用期间有效。
                    unsafe { list.RSSetScissorRects(&[scissor]) };
                    draw_calls = draw_calls.saturating_add(
                        draw_ops(
                            list,
                            recorder.pipeline,
                            payload.ops(),
                            Some(&*recorder.buckets),
                            recorder.geometry,
                            payload,
                            root,
                            scissor,
                        )?
                        .0,
                    );
                }
                diagnostics::op_visits(recorder.buckets.visits, recorder.buckets.budget);
                if let Some(swapchain) = recorder.swapchain {
                    let (buffer_index, back) = swapchain.back_buffer()?;
                    let extent = recorder
                        .syncs
                        .take_copy(buffer_index, Some(&damage), content);
                    copy_to_back(list, recorder.persist, &back, extent)?;
                    recorder
                        .syncs
                        .record_missed_others(buffer_index, Some(&damage));
                }
                present = Some(damage);
            }
        }
        if let Some(timer) = recorder.timer {
            timer.end(list, slot.index);
        }
    }
    // 录制完成后驱逐长期未绘制的几何（设备对象进同一个 retire 列表）。
    if !recorder.prime {
        recorder.geometry.retain_recent(&mut geometry_retired);
        for handle in atlas_retired {
            recorder.stream.retire(slot.signal_value, handle);
        }
        for handle in geometry_retired {
            recorder.stream.retire(slot.signal_value, handle);
        }
    }

    Ok(RecordedFrame {
        stats: RenderStats {
            draw_calls,
            instances: payload.drawn_instances(),
            geometry_uploaded_bytes,
        },
        present,
    })
}

/// 消费者：`FrameRecorder::record`。
pub(super) fn bind_frame(
    list: &ID3D12GraphicsCommandList,
    pipeline: &Pipeline,
    srv_heap: &SrvHeap,
    descriptor_base: u32,
    atlas_generation: u8,
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    slot: usize,
    root: RootUniform,
    content: (u32, u32),
) {
    // SAFETY: RTV / 视口 / 矩形由本帧持有，调用期间有效；共享描述符堆由调用方持有。
    unsafe {
        list.SetDescriptorHeaps(&[Some(srv_heap.heap().clone())]);
        list.SetGraphicsRootSignature(pipeline.root_signature());
        list.SetPipelineState(pipeline.quad());
        // pass 起点的**帧级默认值**（`scroll = 0`）。它**不是**本趟的最终值：`draw_ops` 在每次
        // draw 之前整份重写这 112 B（`frame.params.yz` 是每 op 的滚动偏移，见 D-1）。保留这次
        // 写入是为了让常量槽在任何 draw 之前就有确定内容（prime 帧只走这里，不画图元）。
        list.SetGraphicsRoot32BitConstants(
            0,
            ROOT_UNIFORM_DWORDS as u32,
            root.words().as_ptr().cast(),
            0,
        );
        list.SetGraphicsRootDescriptorTable(1, srv_heap.slot_table(descriptor_base, slot));
        // C3：绑定**本帧的图集描述符代**（代际在录制期间由图集读锁固定；页换代只写另一代）。
        list.SetGraphicsRootDescriptorTable(2, srv_heap.atlas_table(atlas_generation));
        list.OMSetRenderTargets(1, Some(&rtv), false, None);
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

/// 已绑定的 PSO（quad 与三种 geometry 管线互不相同，不能用 `GeometryPipeline::Ui` 冒充 quad）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PsoTag {
    Quad,
    Geometry(GeometryPipeline),
}

/// 一帧的 op 分桶缓存（R08-13）：按 damage 矩形把 op 下标分组，复用 `Vec` 容量。
///
/// 旧实现对每个 damage 矩形重放**整份** op 列表，实测让增量帧（288 µs）比全量帧（222 µs）
/// 更慢。这里在每帧录制时做一次「op × 矩形」相交（512 × ≤4 次整数比较，微秒级），之后每个
/// 矩形的绘制只遍历命中项 —— 局部帧遍历次数从 `ops × passes` 降到「命中 op 数」。
#[derive(Default)]
pub(super) struct OpBuckets {
    buckets: Vec<(PhysicalBounds, Vec<u32>)>,
    /// 本帧实际会遍历的 op 数（诊断指标 `op_visits`）。
    visits: u32,
    /// 整份重放会遍历的数量（`ops.len() × passes`，诊断指标 `op_replay_budget`）。
    budget: u32,
}

impl OpBuckets {
    pub(super) fn new() -> Self {
        Self {
            buckets: Vec::with_capacity(MAX_DRAW_PASSES),
            visits: 0,
            budget: 0,
        }
    }

    /// 按本帧的（已夹紧）damage 重建分桶。`rects` 最多 `MAX_DRAW_PASSES` 个。
    ///
    /// 内层 `Vec` 跨帧保留容量（稳态零分配）：每帧只做「op × 矩形」整数相交。
    ///
    /// `scroll` = 本帧的滚动偏移表（`FramePayload::scroll`）：op 的 `bounds` 是**烘焙坐标**
    /// （不含滚动偏移），求交前必须按本 op 的槽位平移到屏幕坐标，否则滚动帧会把本该绘制的
    /// op 提前剔掉（横向滚动尤其明显）。
    fn build(&mut self, ops: &[FrameOp], damage: &DamageRects, scroll: &[[f32; 2]]) {
        let rects = damage.as_slice();
        self.buckets.truncate(rects.len());
        self.buckets
            .resize_with(rects.len(), || (PhysicalBounds::EMPTY, Vec::new()));
        self.visits = 0;
        for (slot, rect) in self.buckets.iter_mut().zip(rects) {
            let extent = [rect.x0, rect.y0, rect.x1, rect.y1];
            slot.0 = PhysicalBounds {
                x0: rect.x0,
                y0: rect.y0,
                x1: rect.x1,
                y1: rect.y1,
            };
            slot.1.clear();
            for (index, op) in ops.iter().enumerate() {
                if op
                    .bounds()
                    .translated(scroll_offset(scroll, *op))
                    .intersects(extent)
                {
                    slot.1.push(index as u32);
                }
            }
            self.visits = self.visits.saturating_add(slot.1.len() as u32);
        }
        self.budget = (ops.len().saturating_mul(rects.len())) as u32;
    }

    /// 本帧某个 damage 矩形的命中 op 下标（`None` = 没有分桶，退化为整份列表）。
    fn hits(&self, scissor: RECT) -> Option<&[u32]> {
        self.buckets
            .iter()
            .find(|(bounds, _)| {
                bounds.x0 == scissor.left
                    && bounds.y0 == scissor.top
                    && bounds.x1 == scissor.right
                    && bounds.y1 == scissor.bottom
            })
            .map(|(_, bucket)| bucket.as_slice())
    }
}

/// 消费者：`FrameRecorder::record`。
///
/// `buckets = Some` 时只遍历该 scissor 对应的命中 op（局部帧）；`None` 时遍历整份列表。
/// 返回 `(draw_calls, visited)`。
pub(super) fn draw_ops(
    list: &ID3D12GraphicsCommandList,
    pipeline: &Pipeline,
    ops: &[FrameOp],
    buckets: Option<&OpBuckets>,
    geometry: &mut GeometryRegistry,
    payload: &FramePayload,
    root: RootUniform,
    scissor: RECT,
) -> Result<(u32, u32), EngineError> {
    let mut draw_calls = 0u32;
    let mut visited = 0u32;
    // PSO / 拓扑只在变化时重设（R08-28：同一 pass 内的重设是纯命令流开销）。
    let mut last_pipeline: Option<PsoTag> = None;
    let mut last_topology: Option<i32> = None;
    // 命中下标（局部帧，零分配借用）或整份列表（整帧）。
    let hits = buckets.and_then(|buckets| buckets.hits(scissor));
    let count = hits.map_or(ops.len(), <[u32]>::len);
    for position in 0..count {
        let index = match hits {
            Some(hits) => hits[position],
            None => position as u32,
        };
        let Some(op) = ops.get(index as usize) else {
            continue;
        };
        // 整帧路径没有分桶：仍然按 scissor 过滤（整帧 scissor = 内容，恒命中）。
        // op 的 bounds 是烘焙坐标 ⇒ 先按本 op 的滚动槽平移到屏幕坐标。
        if !op
            .bounds()
            .translated(scroll_offset(payload.scroll(), *op))
            .intersects([scissor.left, scissor.top, scissor.right, scissor.bottom])
        {
            continue;
        }
        visited = visited.saturating_add(1);
        match *op {
            FrameOp::Instances {
                start, len, scroll, ..
            } => {
                if len == 0 {
                    continue;
                }
                // 实例基址只经 root constant `draw.clip.y` 进 shader，硬件基址恒 0
                // （D3D12 的 StartInstanceLocation **不**加到 SV_InstanceID 上，contract §6）。
                // 合成端滚动偏移：只在本 op 的 root constant 里加，实例字节一个都不动
                // （B6-2 / 架构决策 §4）。槽位越界按无偏移处理（保守：与不滚动等价）。
                let scroll_offset = payload
                    .scroll()
                    .get(scroll as usize)
                    .copied()
                    .unwrap_or([0.0, 0.0]);
                let uniform = root.with_instance_base(start).with_scroll(scroll_offset);
                // 滚动槽的屏幕裁剪区：容器视口 clip 不进实例裁剪链（见 `paint/pass.rs`），
                // 这里把 scissor 收窄到它 —— 否则滚动内容会画到容器外。
                let narrowed = crate::paint::instance::scrolled_scissor(
                    payload.scroll_clip(),
                    scroll,
                    [scissor.left, scissor.top, scissor.right, scissor.bottom],
                );
                if let Some(rect) = narrowed {
                    if rect[0] >= rect[2] || rect[1] >= rect[3] {
                        // 完全被容器视口裁掉：本 op 一个像素都不该画。
                        continue;
                    }
                    let narrow = RECT {
                        left: rect[0],
                        top: rect[1],
                        right: rect[2],
                        bottom: rect[3],
                    };
                    // SAFETY: 收窄后的矩形是帧 scissor 与该槽屏幕裁剪区的交集（都在目标内）。
                    unsafe { list.RSSetScissorRects(&[narrow]) };
                }
                // SAFETY: PSO / 根常量 / 拓扑 / scissor 都属于同一条已打开的 list。
                unsafe {
                    if last_pipeline != Some(PsoTag::Quad) {
                        list.SetPipelineState(pipeline.quad());
                        last_pipeline = Some(PsoTag::Quad);
                    }
                    // **整份** 112 B（frame + draw）：`frame.params.yz` 是本 op 的合成端滚动偏移，
                    // 每 op 都可能不同 ⇒ 只推 draw 块会让偏移永远停在 `bind_frame` 的 0 上
                    // （D-1：Windows 滚动内容不移动）。与 Linux 的 push constants / macOS 的
                    // `setVertexBytes` 完全同一条上传路径，三端因此没有可漂移的余地。
                    list.SetGraphicsRoot32BitConstants(
                        0,
                        ROOT_UNIFORM_DWORDS as u32,
                        uniform.words().as_ptr().cast(),
                        0,
                    );
                    if last_topology != Some(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP.0) {
                        list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
                        last_topology = Some(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP.0);
                    }
                }
                // SAFETY: 4 顶点 TriangleStrip + 实例区间 [0, len) 已在编码期校验；
                // 段起点由 shader 侧的 `draw.clip.y` 承担。
                unsafe { list.DrawInstanced(4, len, 0, 0) };
                if narrowed.is_some() {
                    // SAFETY: 恢复本趟 damage 矩形的 scissor（同一份 `RECT`）。
                    unsafe { list.RSSetScissorRects(&[scissor]) };
                }
                draw_calls = draw_calls.saturating_add(1);
            }
            FrameOp::Geometry {
                index: draw_index,
                clip_index,
                clip_rect,
                ..
            } => {
                let draw = payload.geometry().get(draw_index as usize).ok_or_else(|| {
                    EngineError::GeometryRegister {
                        detail: smol_str::format_smolstr!(
                            "geometry op index {draw_index} out of range"
                        ),
                    }
                })?;
                // 缓冲由帧开始前的 `prepare` 建好/更新（X5）；这里只查表 + 记使用帧号。
                geometry.touch(draw.id);
                let entry = geometry
                    .get(draw.id)
                    .ok_or_else(|| EngineError::GeometryRegister {
                        detail: smol_str::format_smolstr!(
                            "geometry {}:{} was not prepared before drawing",
                            draw.id.index(),
                            draw.id.generation()
                        ),
                    })?;
                let chunk = draw.snapshot.chunk(draw.chunk).ok_or_else(|| {
                    EngineError::GeometryRegister {
                        detail: smol_str::format_smolstr!(
                            "geometry {}:{} has no chunk {}",
                            draw.id.index(),
                            draw.id.generation(),
                            draw.chunk
                        ),
                    }
                })?;
                let uniform = geometry_root(root, draw.clone(), clip_index, clip_rect);
                debug_assert!(
                    chunk.index_first + chunk.index_count <= entry.index_count(),
                    "geometry chunk range must stay inside the uploaded index buffer"
                );
                // SAFETY: 顶点/索引缓冲由 `GeometryRegistry` 持有到本帧结束；
                // `entry.topology()` 与注册时的图元类型一致。
                unsafe {
                    if last_pipeline != Some(PsoTag::Geometry(entry.pipeline())) {
                        list.SetPipelineState(pipeline.geometry(entry.pipeline()));
                        last_pipeline = Some(PsoTag::Geometry(entry.pipeline()));
                    }
                    // 同样整份上传：geometry 的偏移已经并进 `draw.translation`（`geometry_root`），
                    // 但**帧块必须与实例路径走同一个入口**，否则「哪些 dword 会到 GPU」又变成
                    // 每个后端各自的隐式知识（D-1 的成因）。
                    list.SetGraphicsRoot32BitConstants(
                        0,
                        ROOT_UNIFORM_DWORDS as u32,
                        uniform.words().as_ptr().cast(),
                        0,
                    );
                    if last_topology != Some(entry.topology().0) {
                        list.IASetPrimitiveTopology(entry.topology());
                        last_topology = Some(entry.topology().0);
                    }
                    list.IASetVertexBuffers(0, Some(&[entry.vertex()]));
                    list.IASetIndexBuffer(Some(&entry.index()));
                    // 分块绘制：块的索引区间是 `[index_first, index_first + index_count)`；
                    // 顶点缓冲是**整块几何**的（块只切索引），因此 vertex offset 恒 0。
                    list.DrawIndexedInstanced(chunk.index_count, 1, chunk.index_first, 0, 0);
                }
                draw_calls = draw_calls.saturating_add(1);
            }
        }
    }
    // 保存一份空桶缓存（下一帧 `build` 会清空复用）。
    Ok((draw_calls, visited))
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

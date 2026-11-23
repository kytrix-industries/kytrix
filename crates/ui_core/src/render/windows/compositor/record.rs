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
            device::{AtlasRebuildOutcome, SharedGpu},
            diagnostics,
            hresult::PresentOutcome,
            pipeline::{GeometryRegistry, Pipeline, SrvHeap},
            swapchain::Swapchain,
        },
    },
    window::FrameIndex,
};

const ATLAS_REBUILD_DEFER_RETRY: Duration = Duration::from_millis(1);

const ATLAS_REBUILD_DEFER_LIMIT: Duration = Duration::from_secs(1);

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
        let rebuild = {
            let atlas = shared.atlas();
            renderer
                .atlas_uploads
                .reconcile_pages([atlas.spec(AtlasKind::Glyph), atlas.spec(AtlasKind::Image)])?
        };
        if !rebuild.is_empty() {
            let mut atlas = shared.atlas_write();
            match shared
                .rebuild_atlas_pages_locked(&mut atlas, &rebuild)
                .map_err(|error| renderer.classify(error))?
            {
                AtlasRebuildOutcome::Applied => {}
                AtlasRebuildOutcome::Deferred => {
                    drop(atlas);
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
                    continue;
                }
            }
            return record_and_submit(
                renderer,
                &shared,
                &atlas,
                slot,
                payload,
                frame,
                timings,
                frame_started,
            );
        }
        let atlas = shared.atlas();
        return record_and_submit(
            renderer,
            &shared,
            &atlas,
            slot,
            payload,
            frame,
            timings,
            frame_started,
        );
    }
}

fn record_and_submit(
    renderer: &mut GpuRenderer,
    shared: &Arc<SharedGpu>,
    atlas: &crate::render::windows::atlas::SharedAtlas,
    slot: FrameSlot,
    payload: &FramePayload,
    frame: &mut FramePlan,
    timings: &mut FrameTimings,
    frame_started: Instant,
) -> Result<(RecordedFrame, FrameSlot), EngineError> {
    let shared_gpu = shared.as_ref();
    let mut staging_retired: Vec<GpuResourceHandle> = Vec::new();
    renderer
        .atlas_uploads
        .sync_staging(shared_gpu, atlas, &mut staging_retired)
        .map_err(|error| renderer.classify(error))?;

    renderer
        .stream
        .reset_slot(slot)
        .map_err(|error| renderer.classify(error))?;

    let recorded = {
        let swapchain = renderer.swapchain.as_ref();
        let mut recorder = FrameRecorder {
            shared: shared_gpu,
            stream: &mut renderer.stream,
            pipeline: shared_gpu.pipeline(),
            descriptors: &renderer.descriptors,
            atlas,
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
    Ok((recorded, slot))
}

pub(super) fn record(
    recorder: &mut FrameRecorder<'_>,
    slot: FrameSlot,
    payload: &FramePayload,
    frame: &mut FramePlan,
    timings: &mut FrameTimings,
    frame_started: Instant,
) -> Result<RecordedFrame, EngineError> {
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
    debug_assert!(
        recorder.swapchain.is_none() || frame.is_fresh() == recorder.target.is_fresh(),
        "FramePlan::fresh must be derived from TargetState (R08-11)"
    );
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
    let atlas_retired: Vec<GpuResourceHandle>;
    let mut geometry_retired: Vec<GpuResourceHandle> = Vec::new();
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
                recorder.target.on_direct_draw();
                recorder.syncs.invalidate();
                present = None;
            }
            FrameDraw::Full => {
                let content_rect = [RECT {
                    left: 0,
                    top: 0,
                    right: content.0 as i32,
                    bottom: content.1 as i32,
                }];
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
                    let _ = recorder.syncs.take_copy(buffer_index, None, content);
                    recorder.syncs.record_missed_others(buffer_index, None);
                } else {
                    recorder.syncs.invalidate();
                }
                recorder.target.on_full_persist_draw();
                present = None;
            }
            FrameDraw::Incremental => {
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
        list.SetGraphicsRootDescriptorTable(2, srv_heap.atlas_table(atlas_generation));
        list.OMSetRenderTargets(1, Some(&rtv), false, None);
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum PsoTag {
    Quad,
    Geometry(GeometryPipeline),
}

#[derive(Default)]
pub(super) struct OpBuckets {
    buckets: Vec<(PhysicalBounds, Vec<u32>)>,
    visits: u32,
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
    let mut last_pipeline: Option<PsoTag> = None;
    let mut last_topology: Option<i32> = None;
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
                let scroll_offset = payload
                    .scroll()
                    .get(scroll as usize)
                    .copied()
                    .unwrap_or([0.0, 0.0]);
                let uniform = root.with_instance_base(start).with_scroll(scroll_offset);
                let narrowed = crate::paint::instance::scrolled_scissor(
                    payload.scroll_clip(),
                    scroll,
                    [scissor.left, scissor.top, scissor.right, scissor.bottom],
                );
                if let Some(rect) = narrowed {
                    if rect[0] >= rect[2] || rect[1] >= rect[3] {
                        continue;
                    }
                    let narrow = RECT {
                        left: rect[0],
                        top: rect[1],
                        right: rect[2],
                        bottom: rect[3],
                    };
                    unsafe { list.RSSetScissorRects(&[narrow]) };
                }
                unsafe {
                    if last_pipeline != Some(PsoTag::Quad) {
                        list.SetPipelineState(pipeline.quad());
                        last_pipeline = Some(PsoTag::Quad);
                    }
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
                unsafe { list.DrawInstanced(4, len, 0, 0) };
                if narrowed.is_some() {
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
                let args = draw.draw_indexed(entry.index_count())?;
                let uniform = geometry_root(root, draw.clone(), clip_index, clip_rect);
                unsafe {
                    if last_pipeline != Some(PsoTag::Geometry(entry.pipeline())) {
                        list.SetPipelineState(pipeline.geometry(entry.pipeline()));
                        last_pipeline = Some(PsoTag::Geometry(entry.pipeline()));
                    }
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
                    list.DrawIndexedInstanced(args.count, 1, args.first_index, args.base_vertex, 0);
                }
                draw_calls = draw_calls.saturating_add(1);
            }
        }
    }
    Ok((draw_calls, visited))
}

pub(super) fn root_uniform(width: f32, height: f32, scale: f32) -> RootUniform {
    RootUniform::frame(width, height, scale)
}

pub(super) fn geometry_root(
    frame: RootUniform,
    draw: GeometryDrawGpu,
    clip_index: u32,
    clip_rect: [u32; 4],
) -> RootUniform {
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

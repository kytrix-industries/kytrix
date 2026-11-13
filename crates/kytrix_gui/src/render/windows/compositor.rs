//! 帧编排：上传 → 逐 damage scissor 画进持久 offscreen → copy 到 back buffer → `Present1`
//! （`design/phase-08-d3d12.md` §5.6 / §6）。
//!
//! 结构要点（DEVIATIONS F-71）：**只有持久 offscreen 是绘制目标**；swapchain 的 back buffer
//! 只作 `COPY_DEST`（不建 RTV、不挂 scissor 绘制），因此不对它持有 RTV 引用（resize 前提）。

use std::time::Instant;

use windows::Win32::{
    Foundation::RECT,
    Graphics::{
        Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
        Direct3D12::*,
        Dxgi::Common::{
            DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_UNKNOWN,
            DXGI_SAMPLE_DESC,
        },
    },
};

#[cfg(any(test, feature = "test-support"))]
use crate::render::windows::readback::{Readback, ReadbackImage};
use crate::{
    error::EngineError,
    paint::{
        atlas::AtlasKind,
        instance::{FrameOp, GpuClipEntry, InstanceBuffer},
    },
    render::{
        compositor::{FramePlan, Plan},
        frame::MAX_FRAMES_IN_FLIGHT,
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample, Section},
        windows::{
            atlas::AtlasTextures,
            device::{Device, init_error_from},
            pipeline::{GeometryRegistry, Pipeline},
            queue::Queue,
            swapchain::Swapchain,
            timer::GpuTimer,
        },
    },
    window::FrameIndex,
};

/// 测试构建的图集页（2048² 在 WARP 上代价过高）。
#[cfg(test)]
const TEST_ATLAS_PAGE: u32 = 64;

/// `RootUniform` 的字节数（P07 §2.7：112 B = 28 DWORD）。
const ROOT_UNIFORM_DWORDS: usize = 28;

/// 一个窗口的 GPU 渲染器（`Window.renderer` 的载荷）。
pub(crate) struct Renderer {
    device: Device,
    queue: Queue,
    pipeline: Pipeline,
    atlas: AtlasTextures,
    persist: Persist,
    upload: UploadArena,
    geometry: GeometryRegistry,
    timer: Option<GpuTimer>,
    swapchain: Option<Swapchain>,
    rtv_heap: ID3D12DescriptorHeap,
    clear_color: [f32; 4],
    surface: (u32, u32),
    clips: Vec<GpuClipEntry>,
    #[cfg(any(test, feature = "test-support"))]
    readback: Readback,
}

impl Renderer {
    /// 产品路径：硬件设备 + 离屏目标（P09 在此之后 attach 原生窗口）。
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Self::with_device(
            Device::new()?,
            size,
            crate::render::windows::atlas::ATLAS_PAGE,
        )
    }

    /// 测试用的 WARP 渲染器（图集页 = 测试页尺寸）。
    #[cfg(test)]
    pub(crate) fn new_warp_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Self::with_device(Device::new_warp()?, size, TEST_ATLAS_PAGE)
    }

    /// WARP + 指定图集页尺寸（`TestApp::snapshot` 必须与 `App` 的分配器同页尺寸，
    /// 否则字形页的整页上传会被尺寸断言拒绝）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_warp_offscreen_with_page(
        size: (u32, u32),
        atlas_page: u32,
    ) -> Result<Self, EngineError> {
        Self::with_device(Device::new_warp()?, size, atlas_page)
    }

    /// 真实 HWND 的 swapchain（P09 的窗口层）。
    ///
    /// 用**硬件**设备：WARP 设备不能 present 到 HWND（本机实测 `Present1` 直接
    /// `DXGI_ERROR_DEVICE_REMOVED`），因此无硬件设备的 CI 上由调用方跳过。
    pub(crate) fn new_for_hwnd(hwnd: isize, size: (u32, u32)) -> Result<Self, EngineError> {
        let mut renderer = Self::with_device(
            Device::new()?,
            size,
            crate::render::windows::atlas::ATLAS_PAGE,
        )?;
        renderer.swapchain = Some(Swapchain::for_hwnd(&renderer.queue, hwnd, size)?);
        Ok(renderer)
    }

    fn with_device(device: Device, size: (u32, u32), atlas_page: u32) -> Result<Self, EngineError> {
        let queue = Queue::new(&device)?;
        let pipeline = Pipeline::new(&device)?;
        let atlas = AtlasTextures::new(&device, atlas_page)?;
        pipeline.write_atlas(&device, AtlasKind::Glyph, atlas.texture(AtlasKind::Glyph));
        pipeline.write_atlas(&device, AtlasKind::Image, atlas.texture(AtlasKind::Image));

        // SAFETY: COM 创建调用：设备 / 工厂句柄有效，描述与输出指针在调用期间存活，失败按 HRESULT 返回。
        let rtv_heap: ID3D12DescriptorHeap = unsafe {
            device
                .raw()
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })
        }
        .map_err(|error| init_error_from("CreateDescriptorHeap(RTV)", error))?;
        // SAFETY: 描述符堆由 `Pipeline` / `Renderer` 持有，句柄在堆生命周期内有效。
        let rtv = unsafe { rtv_heap.GetCPUDescriptorHandleForHeapStart() };

        let mut device = device;
        let persist = Persist::new(&mut device, size, rtv)?;
        let upload = UploadArena::new(&mut device)?;
        let timer = GpuTimer::new(&device, &queue)?;

        Ok(Self {
            device,
            queue,
            pipeline,
            atlas,
            persist,
            upload,
            geometry: GeometryRegistry::new(),
            timer,
            swapchain: None,
            rtv_heap,
            clear_color: [0.0, 0.0, 0.0, 0.0],
            surface: size,
            clips: Vec::with_capacity(8),
            #[cfg(any(test, feature = "test-support"))]
            readback: Readback::new(),
        })
    }

    /// 当前离屏目标的物理尺寸。
    pub(crate) fn surface(&self) -> (u32, u32) {
        self.surface
    }

    /// 下一次 present 是否必须全量（新建 / resize 之后为 `true`）。
    pub(crate) fn is_fresh(&self) -> bool {
        self.swapchain.as_ref().is_none_or(Swapchain::is_fresh)
    }

    /// resize：等 GPU 空闲 → 复位全部命令列表（丢弃 back buffer 引用）→ `ResizeBuffers` →
    /// 重建持久 offscreen（DEVIATIONS F-86 / V-55）。
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        if size == self.surface {
            return Ok(());
        }
        self.queue.wait_idle()?;
        self.queue.reset_in_flight_lists()?;
        if let Some(swapchain) = self.swapchain.as_mut() {
            swapchain.resize(size)?;
        }
        self.persist.recreate(&mut self.device, size)?;
        self.surface = size;
        Ok(())
    }

    /// 一帧的全部 GPU 工作（同步提交）。
    pub(crate) fn submit(
        &mut self,
        index: FrameIndex,
        buffer: &InstanceBuffer,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
    ) -> Result<(), EngineError> {
        if let Some(error) = self.device.lost_error() {
            return Err(error);
        }
        timings.reset(index);
        let frame_started = Instant::now();

        buffer.write_clips_gpu(&mut self.clips);
        let slot = match self.queue.begin(index) {
            Ok(slot) => slot,
            Err(error) => return Err(self.classify(error)),
        };

        let Renderer {
            device,
            queue,
            pipeline,
            atlas,
            persist,
            upload,
            geometry,
            timer,
            swapchain,
            rtv_heap,
            clear_color,
            surface,
            clips,
            ..
        } = self;
        let _ = rtv_heap;

        let uploaded = upload.write(device, queue, index, slot.index, buffer.as_bytes(), clips);
        let (instance_count, clip_count) = uploaded?;
        pipeline.write_slot(
            device,
            slot.index,
            upload.slot(slot.index).instances(),
            instance_count,
            upload.slot(slot.index).clips(),
            clip_count,
        );
        timings.push(GpuTimerSample {
            section: Section::Upload,
            cpu_ns: elapsed_ns(frame_started),
            gpu_ns: 0,
        });

        let width = surface.0 as f32;
        let height = surface.1 as f32;
        let root = root_uniform(width, height);
        let full = frame.is_full();
        let scissor_whole = RECT {
            left: 0,
            top: 0,
            right: surface.0 as i32,
            bottom: surface.1 as i32,
        };

        let draw_result = {
            let list = queue.list(slot);
            if let Some(timer) = timer.as_ref() {
                timer.begin(list, slot.index);
            }
            if atlas.has_pending() {
                atlas.record_uploads(device, list, slot.index)?;
            }
            if full {
                // SAFETY: 目标 RT 处于 `RENDER_TARGET`（本帧未拷贝到 swapchain 时恒如此）。
                unsafe { list.ClearRenderTargetView(persist.rtv, clear_color, None) };
            }
            bind_frame(list, pipeline, persist, slot.index, &root);
            match full {
                true => draw_ops(list, pipeline, buffer, geometry, &root, scissor_whole)?,
                false => match frame.plan() {
                    Plan::Idle | Plan::Full => {}
                    Plan::Partial(damage) => {
                        for rect in damage.as_slice() {
                            let scissor = RECT {
                                left: rect.x0,
                                top: rect.y0,
                                right: rect.x1,
                                bottom: rect.y1,
                            };
                            // SAFETY: RTV / 视口 / 矩形由本帧持有，调用期间有效。
                            unsafe { list.RSSetScissorRects(&[scissor]) };
                            draw_ops(list, pipeline, buffer, geometry, &root, scissor)?;
                        }
                    }
                },
            }
            if let Some(swapchain) = swapchain.as_ref() {
                copy_to_back(list, persist, swapchain, frame.copy_plan())?;
            }
            if let Some(timer) = timer.as_ref() {
                timer.end(list, slot.index);
            }
            Ok(())
        };
        if let Err(error) = draw_result {
            return Err(self.classify(error));
        }

        if let Err(error) = self.queue.submit(slot) {
            return Err(self.classify(error));
        }

        if let Some(timer) = self.timer.as_ref()
            && let Some(sample) = timer.resolve(slot.index)
        {
            timings.push(sample);
        }
        debug_assert_eq!(timings.frame(), index);
        debug_assert!(timings.samples().len() <= 3);

        let present_started = Instant::now();
        if let Some(swapchain) = self.swapchain.as_mut()
            && let Err(error) = swapchain.present(frame, 1)
        {
            return Err(self.classify(error));
        }
        timings.push(GpuTimerSample {
            section: Section::Present,
            cpu_ns: elapsed_ns(present_started),
            gpu_ns: 0,
        });

        self.queue.release_completed(index, &mut self.device);
        Ok(())
    }

    /// 读回持久 offscreen（快照；阻塞到 GPU 完成）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn readback(&mut self) -> Result<ReadbackImage, EngineError> {
        self.queue.wait_idle()?;
        self.readback.capture(
            &self.device,
            &mut self.queue,
            &self.persist.texture,
            self.surface,
        )
    }

    /// 一次性渲染整帧并读回（快照用；不推进窗口的 `FrameScheduler`）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn render_snapshot(
        &mut self,
        buffer: &InstanceBuffer,
    ) -> Result<ReadbackImage, EngineError> {
        let mut timings = FrameTimings::new();
        let mut frame = FramePlan::new(Plan::Full, true);
        self.submit(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)?;
        self.readback()
    }

    /// 测试用的 clear 颜色（sRGB 编码断言需要非透明/半透明值）。
    #[cfg(test)]
    pub(crate) fn set_clear_color(&mut self, color: [f32; 4]) {
        self.clear_color = color;
    }

    /// 图集上传入口：P09 的字形页（整页）与 P15 的图片页共用。
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        pixels: &[u8],
        rows: crate::render::atlas::DirtyRows,
    ) {
        self.atlas.queue_upload(kind, pixels, rows);
    }

    /// 测试用的静态几何注册入口（P09 的 `GeometryStore` 接上后语义不变）。
    #[cfg(test)]
    pub(crate) fn register_geometry(
        &mut self,
        id: crate::paint::geometry::GeometryId,
        vertices: &[u8],
        indices: &[u8],
        pipeline: crate::paint::geometry::GeometryPipeline,
        topology: crate::paint::geometry::Topology,
        index_type: crate::paint::geometry::IndexType,
    ) -> Result<(), EngineError> {
        self.geometry.register(
            &mut self.device,
            id,
            vertices,
            indices,
            pipeline,
            topology,
            index_type,
        )
    }

    /// 把底层错误归类为设备丢失（粘性）或原样返回。
    fn classify(&mut self, error: EngineError) -> EngineError {
        self.device.note_failure().unwrap_or(error)
    }
}

/// 持久 offscreen（唯一绘制目标；资源格式 UNORM + RTV 视图 sRGB，DEVIATIONS F-80）。
struct Persist {
    texture: ID3D12Resource,
    handle: GpuResourceHandle,
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    size: (u32, u32),
}

impl Persist {
    fn new(
        device: &mut Device,
        size: (u32, u32),
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Result<Self, EngineError> {
        let texture = create_target(device, size)?;
        let handle = device.allocate(texture.clone());
        device.create_render_target_view(&texture, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, rtv);
        Ok(Self {
            texture,
            handle,
            rtv,
            size,
        })
    }

    fn recreate(&mut self, device: &mut Device, size: (u32, u32)) -> Result<(), EngineError> {
        let texture = create_target(device, size)?;
        let handle = device.allocate(texture.clone());
        device.create_render_target_view(&texture, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, self.rtv);
        // resize 前已经 `wait_idle`，旧资源可立即释放。
        device.release(self.handle);
        self.texture = texture;
        self.handle = handle;
        self.size = size;
        Ok(())
    }
}

/// 每 in-flight 槽的实例 / 裁剪上传缓冲（只写不重建容量；扩容时才换资源）。
struct UploadArena {
    slots: Vec<UploadSlot>,
}

struct UploadSlot {
    instances: ID3D12Resource,
    instance_handle: GpuResourceHandle,
    instance_capacity: u32,
    clips: ID3D12Resource,
    clip_handle: GpuResourceHandle,
    clip_capacity: u32,
}

impl UploadArena {
    fn new(device: &mut Device) -> Result<Self, EngineError> {
        let mut slots = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let instances = create_upload(device, 256 * 128)?;
            let instance_handle = device.allocate(instances.clone());
            let clips = create_upload(device, 8 * 48)?;
            let clip_handle = device.allocate(clips.clone());
            slots.push(UploadSlot {
                instances,
                instance_handle,
                instance_capacity: 256,
                clips,
                clip_handle,
                clip_capacity: 8,
            });
        }
        Ok(Self { slots })
    }

    fn slot(&self, slot: usize) -> &UploadSlot {
        &self.slots[slot]
    }

    /// 写入本帧的实例/裁剪字节；容量不够时扩容（旧缓冲交 retire 队列）。返回 `(实例数, 裁剪层数)`。
    fn write(
        &mut self,
        device: &mut Device,
        queue: &mut Queue,
        index: FrameIndex,
        slot: usize,
        instances: &[u8],
        clips: &[GpuClipEntry],
    ) -> Result<(u32, u32), EngineError> {
        let instance_count = (instances.len() / 128) as u32;
        let clip_count = clips.len() as u32;
        let needs_instances = instance_count > self.slots[slot].instance_capacity;
        if needs_instances {
            let capacity = instance_count.next_power_of_two().max(256);
            let resource = create_upload(device, u64::from(capacity) * 128)?;
            let handle = device.allocate(resource.clone());
            let old = self.slots[slot].instance_handle;
            self.slots[slot].instances = resource;
            self.slots[slot].instance_handle = handle;
            self.slots[slot].instance_capacity = capacity;
            queue.retire(index, old);
        }
        let needs_clips = clip_count > self.slots[slot].clip_capacity;
        if needs_clips {
            let capacity = clip_count.next_power_of_two().max(8);
            let resource = create_upload(device, u64::from(capacity) * 48)?;
            let handle = device.allocate(resource.clone());
            let old = self.slots[slot].clip_handle;
            self.slots[slot].clips = resource;
            self.slots[slot].clip_handle = handle;
            self.slots[slot].clip_capacity = capacity;
            queue.retire(index, old);
        }

        write_upload(&self.slots[slot].instances, instances)?;
        write_upload(&self.slots[slot].clips, bytemuck::cast_slice(clips))?;
        Ok((instance_count, clip_count))
    }
}

impl UploadSlot {
    fn instances(&self) -> &ID3D12Resource {
        &self.instances
    }

    fn clips(&self) -> &ID3D12Resource {
        &self.clips
    }
}

fn write_upload(resource: &ID3D12Resource, bytes: &[u8]) -> Result<(), EngineError> {
    if bytes.is_empty() {
        return Ok(());
    }
    let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: 映射区间由 `Map` 独占，直到同一函数的 `Unmap`；容量由该槽缓冲的创建尺寸保证。
    unsafe { resource.Map(0, None, Some(&mut pointer)) }
        .map_err(|error| init_error_from("Map(upload)", error))?;
    // SAFETY: 上传缓冲容量在 `UploadArena::write` 里已按 `bytes.len()` 扩容。
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len());
        resource.Unmap(0, None);
    }
    Ok(())
}

fn bind_frame(
    list: &ID3D12GraphicsCommandList,
    pipeline: &Pipeline,
    persist: &Persist,
    slot: usize,
    root: &[f32; ROOT_UNIFORM_DWORDS],
) {
    // SAFETY: RTV / 视口 / 矩形由本帧持有，调用期间有效。
    unsafe {
        list.SetDescriptorHeaps(&[Some(pipeline.heap().clone())]);
        list.SetGraphicsRootSignature(pipeline.root_signature());
        list.SetPipelineState(pipeline.quad());
        list.SetGraphicsRoot32BitConstants(0, ROOT_UNIFORM_DWORDS as u32, root.as_ptr().cast(), 0);
        list.SetGraphicsRootDescriptorTable(1, pipeline.slot_table(slot));
        list.OMSetRenderTargets(1, Some(&persist.rtv), false, None);
        list.RSSetViewports(&[D3D12_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: persist.size.0 as f32,
            Height: persist.size.1 as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }]);
    }
}

fn draw_ops(
    list: &ID3D12GraphicsCommandList,
    pipeline: &Pipeline,
    buffer: &InstanceBuffer,
    geometry: &GeometryRegistry,
    root: &[f32; ROOT_UNIFORM_DWORDS],
    scissor: RECT,
) -> Result<(), EngineError> {
    let mut quad_bound = false;
    for op in buffer.ops() {
        match *op {
            FrameOp::Instances { start, len } => {
                if len == 0 {
                    continue;
                }
                if !quad_bound {
                    // SAFETY: PSO / 根常量 / 拓扑 / scissor 都属于同一条已打开的 list。
                    unsafe {
                        list.SetPipelineState(pipeline.quad());
                        list.SetGraphicsRoot32BitConstants(
                            0,
                            ROOT_UNIFORM_DWORDS as u32,
                            root.as_ptr().cast(),
                            0,
                        );
                        list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
                        list.RSSetScissorRects(&[scissor]);
                    }
                    quad_bound = true;
                }
                // SAFETY: 4 顶点 TriangleStrip + 实例区间 [start, start+len) 已在编码期校验。
                unsafe { list.DrawInstanced(4, len, 0, start) };
            }
            FrameOp::Geometry {
                index,
                clip_index,
                clip_rect,
            } => {
                let draw = buffer
                    .geometry()
                    .get(index as usize)
                    .map(|draw| draw.id)
                    .ok_or_else(|| EngineError::GeometryRegister {
                        detail: smol_str::format_smolstr!("geometry op index {index} out of range"),
                    })?;
                let entry = geometry
                    .lookup(draw)
                    .ok_or_else(|| EngineError::GeometryRegister {
                        detail: smol_str::format_smolstr!(
                            "geometry id {}:{} has no GPU buffers (GeometryStore lands in P09)",
                            draw.index(),
                            draw.generation()
                        ),
                    })?;
                let uniform = geometry_root(root, clip_index, clip_rect);
                // SAFETY: 顶点/索引缓冲由 `GeometryRegistry` 持有到本帧结束；
                // `entry.topology()` 与注册时的图元类型一致（P07 §2.7）。
                unsafe {
                    list.SetPipelineState(pipeline.geometry(entry.pipeline()));
                    list.SetGraphicsRoot32BitConstants(
                        0,
                        ROOT_UNIFORM_DWORDS as u32,
                        uniform.as_ptr().cast(),
                        0,
                    );
                    list.IASetPrimitiveTopology(entry.topology());
                    list.IASetVertexBuffers(0, Some(&[entry.vertex()]));
                    list.IASetIndexBuffer(Some(&entry.index()));
                    list.RSSetScissorRects(&[scissor]);
                    list.DrawIndexedInstanced(entry.index_count(), 1, 0, 0, 0);
                }
                quad_bound = false;
            }
        }
    }
    Ok(())
}
fn copy_to_back(
    list: &ID3D12GraphicsCommandList,
    persist: &Persist,
    swapchain: &Swapchain,
    plan: Plan,
) -> Result<(), EngineError> {
    let back = swapchain.back_buffer()?;
    transition(
        list,
        &persist.texture,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    transition(
        list,
        &back,
        D3D12_RESOURCE_STATE_PRESENT,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    match plan {
        // 全帧也用 `CopyTextureRegion`（与局部同一路径，逐矩形）：
        // swapchain back buffer 上 `CopyResource` 在 flip model 下会被拒绝（本机实测 DEVICE_REMOVED）。
        Plan::Full => {
            let whole =
                crate::math::PhysicalRect::new(0, 0, persist.size.0 as i32, persist.size.1 as i32);
            copy_rect(list, &persist.texture, &back, &whole);
        }
        Plan::Partial(damage) => {
            for rect in damage.as_slice() {
                copy_rect(list, &persist.texture, &back, rect);
            }
        }
        Plan::Idle => {}
    }
    transition(
        list,
        &persist.texture,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
    );
    transition(
        list,
        &back,
        D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_STATE_PRESENT,
    );
    Ok(())
}

fn copy_rect(
    list: &ID3D12GraphicsCommandList,
    source: &ID3D12Resource,
    destination: &ID3D12Resource,
    rect: &crate::math::PhysicalRect,
) {
    let mut destination_location = D3D12_TEXTURE_COPY_LOCATION {
        pResource: core::mem::ManuallyDrop::new(Some(destination.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: 0,
        },
    };
    let mut source_location = D3D12_TEXTURE_COPY_LOCATION {
        pResource: core::mem::ManuallyDrop::new(Some(source.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: 0,
        },
    };
    let region = D3D12_BOX {
        left: rect.x0 as u32,
        top: rect.y0 as u32,
        front: 0,
        right: rect.x1 as u32,
        bottom: rect.y1 as u32,
        back: 1,
    };
    // SAFETY: 两个 location 在本次调用期间有效，`region` 的边界由调用方夹紧到 surface 内。
    unsafe {
        list.CopyTextureRegion(
            &destination_location,
            rect.x0 as u32,
            rect.y0 as u32,
            0,
            &source_location,
            Some(&region),
        )
    };
    // SAFETY: `pResource` 是 `ManuallyDrop`，结构体 drop 不释放 COM 引用；不显式释放会让
    // swapchain back buffer 永远存活（DEVIATIONS F-86）。
    unsafe {
        core::mem::ManuallyDrop::drop(&mut destination_location.pResource);
        core::mem::ManuallyDrop::drop(&mut source_location.pResource);
    }
}

fn transition(
    list: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: core::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: core::mem::ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    };
    // SAFETY: `barrier` 在本次调用期间独占；`slice::from_ref` 不转移所有权。
    unsafe { list.ResourceBarrier(core::slice::from_ref(&barrier)) };
    // SAFETY: union 载荷与 `pResource` 都是 `ManuallyDrop`，结构体 drop **不会**释放 COM 引用；
    // 被 barrier 过的资源（尤其是 swapchain back buffer）会被永久钉住，导致 `ResizeBuffers`
    // 返回 `DXGI_ERROR_INVALID_CALL`（DEVIATIONS F-86）。记录已经提交，这里显式释放。
    unsafe {
        let transition: &mut D3D12_RESOURCE_TRANSITION_BARRIER = &mut barrier.Anonymous.Transition;
        core::mem::ManuallyDrop::drop(&mut transition.pResource);
    }
}

/// `RootUniform`（112 B）：`frame @0`、`draw @32`（P07 §2.7）。
fn root_uniform(width: f32, height: f32) -> [f32; ROOT_UNIFORM_DWORDS] {
    let mut uniform = [0.0f32; ROOT_UNIFORM_DWORDS];
    uniform[0] = width;
    uniform[1] = height;
    uniform[2] = if width > 0.0 { 2.0 / width } else { 0.0 };
    uniform[3] = if height > 0.0 { 2.0 / height } else { 0.0 };
    uniform[4] = 1.0; // frame.params.scale（geometry 的逻辑→物理）
    uniform[8] = 1.0; // draw.basis.m11
    uniform[11] = 1.0; // draw.basis.m22
    uniform
}

/// 逐 geometry 的 draw uniform（`FrameOp::Geometry` 已经带 clip；颜色通道 P09 补齐）。
/// geometry 的 draw uniform（112 B；布局严格对照 `common.hlsl` 的 `GeometryDrawUniform`）：
/// basis @8、translation @12、color0 @16、clip_rect @20（x,y,w,h）、clip @24（`.x` = 索引）。
fn geometry_root(
    frame: &[f32; ROOT_UNIFORM_DWORDS],
    clip_index: u32,
    clip_rect: [u32; 4],
) -> [f32; ROOT_UNIFORM_DWORDS] {
    let mut uniform = [0.0f32; ROOT_UNIFORM_DWORDS];
    uniform[..8].copy_from_slice(&frame[..8]);
    // draw.basis @8 = 单位矩阵（P08 只有恒等变换，S-77）。
    uniform[8] = 1.0;
    uniform[11] = 1.0;
    // draw.translation @12 = (0, 0, 0, 0)。
    // draw.color0 @16 = 不透明白（预乘线性 RGBA；几何着色属 P09，S-77）。
    uniform[16] = 1.0;
    uniform[17] = 1.0;
    uniform[18] = 1.0;
    uniform[19] = 1.0;
    // draw.clip_rect @20 = (x, y, w, h) 物理像素。无裁剪时用视口，避免 inf 进根常量；
    // 有裁剪时把编码器给的 AABB `(x0, y0, x1, y1)` 换成 `(x, y, w, h)`。
    let (x, y, w, h) = if clip_index == 0 {
        (0.0, 0.0, frame[0], frame[1])
    } else {
        let x0 = f32::from_bits(clip_rect[0]);
        let y0 = f32::from_bits(clip_rect[1]);
        let x1 = f32::from_bits(clip_rect[2]);
        let y1 = f32::from_bits(clip_rect[3]);
        (x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
    };
    uniform[20] = x;
    uniform[21] = y;
    uniform[22] = w;
    uniform[23] = h;
    // draw.clip @24：`.x = clip_index`（uint，逐位写入，不能做 f32 数值转换）。
    uniform[24] = f32::from_bits(clip_index);
    uniform
}

fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn create_target(device: &mut Device, size: (u32, u32)) -> Result<ID3D12Resource, EngineError> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(size.0.max(1)),
        Height: size.1.max(1),
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        ..D3D12_RESOURCE_DESC::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    };
    let clear = D3D12_CLEAR_VALUE {
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Anonymous: D3D12_CLEAR_VALUE_0 {
            Color: [0.0, 0.0, 0.0, 0.0],
        },
    };
    let mut out: Option<ID3D12Resource> = None;
    // SAFETY: COM 调用：设备 / 工厂 / swapchain 句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
    unsafe {
        device.raw().CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            Some(&clear),
            &mut out,
        )
    }
    .map_err(|error| init_error_from("CreateCommittedResource(persist)", error))?;
    out.ok_or_else(|| EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("d3d12 persist target missing"),
    })
}

fn create_upload(device: &mut Device, bytes: u64) -> Result<ID3D12Resource, EngineError> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes.max(4),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..D3D12_RESOURCE_DESC::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_UPLOAD,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    };
    let mut out: Option<ID3D12Resource> = None;
    // SAFETY: COM 调用：设备 / 工厂 / swapchain 句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
    unsafe {
        device.raw().CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_GENERIC_READ,
            None,
            &mut out,
        )
    }
    .map_err(|error| init_error_from("CreateCommittedResource(upload)", error))?;
    out.ok_or_else(|| EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("d3d12 upload buffer missing"),
    })
}

#[cfg(test)]
mod tests {
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, WPARAM},
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, RegisterClassW, WNDCLASSW, WS_POPUP,
            },
        },
        core::PCWSTR,
    };

    use super::{Renderer, geometry_root, root_uniform};
    use crate::{
        math::{PhysicalRect, Point, Rect, Scale, Size, Transform},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            color::{Color, ColorResolver, Interpolation},
            geometry::{GeometryId, GeometryPipeline, IndexType, Topology},
            instance::{FrameOp, InstanceEncoder},
            primitive::{
                GeometryDraw, GlyphRun, Image, PositionedGlyph, Primitive, Quad, QuadPaint,
            },
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
    fn warp_renderer(size: (u32, u32)) -> Option<Renderer> {
        match Renderer::new_warp_offscreen(size) {
            Ok(renderer) => Some(renderer),
            Err(error) => {
                eprintln!("skip: WARP renderer unavailable: {error}");
                None
            }
        }
    }

    fn resolver() -> ColorResolver {
        ColorResolver::new(Interpolation::default())
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
        let uniform = root_uniform(800.0, 600.0);
        assert_eq!(uniform[0], 800.0);
        assert_eq!(uniform[1], 600.0);
        assert_eq!(uniform[2], 2.0 / 800.0);
        assert_eq!(uniform[3], 2.0 / 600.0);
        assert_eq!(uniform[4], 1.0);
        assert_eq!(uniform[8], 1.0);
        assert_eq!(uniform[11], 1.0);
        assert_eq!(uniform.len(), 28);
    }

    /// 逐 dword 对照 `common.hlsl` 的 `GeometryDrawUniform`（B1）。
    #[test]
    fn geometry_root_matches_common_hlsl_layout() {
        let frame = root_uniform(800.0, 600.0);
        let clip_rect = [4.0f32, 5.0, 100.0, 120.0].map(f32::to_bits);
        let uniform = geometry_root(&frame, 3, clip_rect);
        assert_eq!(uniform[..8], frame[..8], "frame block is copied verbatim");
        // draw.basis @8
        assert_eq!(
            [uniform[8], uniform[9], uniform[10], uniform[11]],
            [1.0, 0.0, 0.0, 1.0]
        );
        // draw.translation @12
        assert_eq!(
            [uniform[12], uniform[13], uniform[14], uniform[15]],
            [0.0; 4]
        );
        // draw.color0 @16 = premultiplied white
        assert_eq!(
            [uniform[16], uniform[17], uniform[18], uniform[19]],
            [1.0, 1.0, 1.0, 1.0]
        );
        // draw.clip_rect @20 = (x, y, w, h)
        assert_eq!(
            [uniform[20], uniform[21], uniform[22], uniform[23]],
            [4.0, 5.0, 96.0, 115.0]
        );
        // draw.clip @24 = uint clip_index（逐位）
        assert_eq!(uniform[24].to_bits(), 3);
        assert_eq!(uniform[25..].iter().copied().sum::<f32>(), 0.0);
    }

    #[test]
    fn geometry_root_without_clip_uses_the_viewport() {
        let frame = root_uniform(64.0, 32.0);
        let uniform = geometry_root(&frame, 0, [0; 4]);
        assert_eq!(
            [uniform[20], uniform[21], uniform[22], uniform[23]],
            [0.0, 0.0, 64.0, 32.0]
        );
        assert_eq!(uniform[24].to_bits(), 0);
    }

    #[test]
    fn instance_bytes_are_128_per_primitive() {
        let scene = one_lane_scene(quad(Rect::new(0.0, 0.0, 8.0, 8.0), Color::WHITE));
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        assert_eq!(buffer.as_bytes().len(), 128);
        assert_eq!(buffer.ops().len(), 1);
        assert!(matches!(
            buffer.ops()[0],
            FrameOp::Instances { start: 0, len: 1 }
        ));
    }

    #[test]
    fn clear_half_encodes_once_to_srgb() {
        let _guard = lock();
        let Some(mut renderer) = warp_renderer((8, 8)) else {
            return;
        };
        renderer.set_clear_color([0.5, 0.5, 0.5, 0.5]);
        let scene = Scene::empty();
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut frame = full_plan((8, 8));
        let mut timings = FrameTimings::new();
        renderer
            .submit(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
            .expect("submit");
        let image = renderer.readback().expect("readback");
        // probe：persist 资源 UNORM + RTV 视图 SRGB ⇒ clear(0.5) 的 RGB 只编码一次 = 188；
        // alpha 不参与 sRGB 变换（0.5 → 128）。
        assert_eq!(pixel(&image, 8, 4, 4), [188, 188, 188, 128]);
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
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut frame = full_plan((64, 64));
        let mut timings = FrameTimings::new();
        renderer
            .submit(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
            .expect("submit");
        let image = renderer.readback().expect("readback");
        assert_eq!(pixel(&image, 64, 24, 24), [255, 0, 0, 255]);
        assert_eq!(pixel(&image, 64, 4, 4), [0, 0, 0, 0]);
    }

    /// 白底 + 红 damage：两帧都是非空场景，damage 之外的蓝块必须原样保留（B5）。
    #[test]
    fn damage_isolates_pixels_across_two_real_frames() {
        let _guard = lock();
        let Some(mut renderer) = warp_renderer((64, 64)) else {
            return;
        };
        renderer.set_clear_color([1.0, 1.0, 1.0, 1.0]);
        let size = (64, 64);

        let blue = one_lane_scene(quad(
            Rect::new(32.0, 0.0, 32.0, 64.0),
            Color::srgb(0.0, 0.0, 1.0, 1.0),
        ));
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&blue, Scale::ONE, &resolver());
        let mut first = full_plan(size);
        let mut timings = FrameTimings::new();
        renderer
            .submit(FrameIndex::from_raw(1), buffer, &mut first, &mut timings)
            .expect("first frame");

        let red = one_lane_scene(quad(
            Rect::new(0.0, 0.0, 32.0, 64.0),
            Color::srgb(1.0, 0.0, 0.0, 1.0),
        ));
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&red, Scale::ONE, &resolver());
        let damage = [PhysicalRect::new(0, 0, 32, 64)];
        let mut second = partial_plan(&damage, size);
        assert!(matches!(second.plan(), Plan::Partial(_)));
        renderer
            .submit(FrameIndex::from_raw(2), buffer, &mut second, &mut timings)
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
        #[rustfmt::skip]
        let vertices: [f32; 6] = [0.0, 0.0, 32.0, 0.0, 0.0, 32.0];
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
        }));
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut frame = full_plan((64, 64));
        let mut timings = FrameTimings::new();
        renderer
            .submit(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
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
        renderer.queue_atlas_upload(AtlasKind::Image, &pixels, rows);

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
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut frame = full_plan((64, 64));
        let mut timings = FrameTimings::new();
        renderer
            .submit(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
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
        let Some(mut renderer) = super::Renderer::new_warp_offscreen_with_page((64, 64), page).ok()
        else {
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
        renderer.queue_atlas_upload(AtlasKind::Glyph, &pixels, rows);
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
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut frame = full_plan((64, 64));
        let mut timings = FrameTimings::new();
        if renderer
            .submit(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
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
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut frame = full_plan((63, 17));
        let mut timings = FrameTimings::new();
        renderer
            .submit(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
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
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut frame = full_plan((32, 32));
        let mut timings = FrameTimings::new();
        renderer
            .submit(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
            .expect("first submit");

        renderer.resize((96, 64)).expect("resize");
        assert_eq!(renderer.surface(), (96, 64));
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut frame = full_plan((96, 64));
        renderer
            .submit(FrameIndex::from_raw(2), buffer, &mut frame, &mut timings)
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
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let image = renderer.render_snapshot(buffer).expect("snapshot");
        assert_eq!((image.width, image.height), (16, 8));
        assert_eq!(image.rgba8.len(), 16 * 8 * 4);
    }

    /// `Present1` 的实机往返（硬件设备 + 隐藏窗口）。
    ///
    /// **默认忽略**：本机非交互会话 + 隐藏窗口下 `GetDeviceRemovedReason` 在录制后就返回
    /// `0x887A0001`（DEVIATIONS V-57），swapchain 的实机验收属 P09 的可见窗口会话（V-55）。
    /// 手动跑：`cargo test -p kytrix_gui --lib -- --ignored hidden_window`。
    #[test]
    #[ignore = "V-55/V-57: real Present1 needs a visible-window session (P09 gate)"]
    fn hidden_window_swapchain_accepts_a_full_present() {
        let _guard = lock();

        extern "system" fn wndproc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            // SAFETY: 只把消息转发给默认处理。
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        fn wide(value: &str) -> Vec<u16> {
            value.encode_utf16().chain(std::iter::once(0)).collect()
        }

        // SAFETY: 测试用隐藏窗口：Win32 句柄只在当前测试线程内使用，不跨线程。
        let instance = unsafe { GetModuleHandleW(None) }.expect("module handle");
        let class = wide("KytrixGuiP08Test");
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..WNDCLASSW::default()
        };
        // SAFETY: 测试用隐藏窗口：Win32 句柄只在当前测试线程内使用，不跨线程。
        let _ = unsafe { RegisterClassW(&window_class) };
        let title = wide("kg-p08");
        // SAFETY: 测试用隐藏窗口：Win32 句柄只在当前测试线程内使用，不跨线程。
        let hwnd = unsafe {
            CreateWindowExW(
                Default::default(),
                PCWSTR(class.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                0,
                0,
                64,
                64,
                None,
                None,
                Some(instance.into()),
                None,
            )
        }
        .expect("hidden window");

        let Ok(mut renderer) = Renderer::new_for_hwnd(hwnd.0 as isize, (64, 64)) else {
            eprintln!("skip: no hardware D3D12 device for the swapchain test");
            return;
        };
        let scene = Scene::empty();
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut timings = FrameTimings::new();
        let mut first = full_plan((64, 64));
        renderer
            .submit(FrameIndex::from_raw(1), buffer, &mut first, &mut timings)
            .expect("presented frame");
        let damage = [PhysicalRect::new(0, 0, 32, 32)];
        let mut second = partial_plan(&damage, (64, 64));
        renderer
            .submit(FrameIndex::from_raw(2), buffer, &mut second, &mut timings)
            .expect("dirty frame");
    }
}

//! 帧编排：上传 → 逐 damage scissor 画进持久 offscreen → 整面 blit 到 drawable → present
//! （`design/phase-12-macos-linux.md` §5.6 / §5.9）。
//!
//! **两条与 Windows 不同的硬约束**（F-121）：
//! 1. `FramePlan::copy_plan()` / `present_dirty()` 是 D3D12 专用：macOS/Present 目标的内容跨帧无保证，
//!    因此**每帧整面 blit**；`plan()`（scissor）与 `is_full()`（clear）仍是共用语义。
//! 2. 没有 barrier / 资源状态机：Metal 自动跟踪 hazard。

use std::time::Instant;

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder,
    MTLDevice, MTLLoadAction, MTLOrigin, MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder,
    MTLRenderPassDescriptor, MTLResourceOptions, MTLScissorRect, MTLSize, MTLStoreAction,
    MTLTexture, MTLTextureDescriptor, MTLTextureUsage, MTLViewport,
};
use objc2_quartz_core::CAMetalDrawable;

#[cfg(any(test, feature = "test-support"))]
use crate::render::macos::readback::{Readback, ReadbackImage};
use crate::{
    error::EngineError,
    paint::{
        atlas::AtlasKind,
        instance::{FrameOp, GpuClipEntry, InstanceBuffer},
    },
    render::{
        compositor::{FramePlan, Plan},
        frame::MAX_FRAMES_IN_FLIGHT,
        macos::{
            atlas::{ATLAS_PAGE, AtlasTextures},
            device::{Device, GpuObject, init_error},
            pipeline::Pipeline,
            queue::Queue,
            swapchain::Swapchain,
            timer::GpuTimer,
        },
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample, Section},
    },
    window::FrameIndex,
};

/// `RootUniform` 的字节数（P07 §2.7：112 B）。
const ROOT_UNIFORM_BYTES: usize = 112;

/// 一个窗口的 GPU 渲染器（`Window.renderer` 的载荷）。
pub(crate) struct Renderer {
    device: Device,
    queue: Queue,
    pipeline: Pipeline,
    atlas: AtlasTextures,
    persist: Persist,
    upload: UploadArena,
    root: Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>,
    timer: Option<GpuTimer>,
    swapchain: Option<Swapchain>,
    clear_color: [f32; 4],
    surface: (u32, u32),
    clips: Vec<GpuClipEntry>,
    #[cfg(any(test, feature = "test-support"))]
    readback: Readback,
}

impl Renderer {
    /// 产品路径（离屏 / 快照）：系统默认设备 + 默认图集页。
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Self::with_device(Device::new()?, size, ATLAS_PAGE)
    }

    /// 指定图集页（快照必须与 `App` 的分配器同页尺寸）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_offscreen_with_page(
        size: (u32, u32),
        atlas_page: u32,
    ) -> Result<Self, EngineError> {
        Self::with_device(Device::new()?, size, atlas_page)
    }

    /// 真实窗口的图层（平台层在建窗后调用；`layer()` 交给 `NSView::setLayer`）。
    pub(crate) fn new_for_layer(size: (u32, u32), scale: f64) -> Result<Self, EngineError> {
        let mut renderer = Self::with_device(Device::new()?, size, ATLAS_PAGE)?;
        renderer.swapchain = Some(Swapchain::new(&renderer.device, size, scale)?);
        Ok(renderer)
    }

    fn with_device(device: Device, size: (u32, u32), atlas_page: u32) -> Result<Self, EngineError> {
        let queue = Queue::new(&device)?;
        let pipeline = Pipeline::new(&device, MTLPixelFormat::BGRA8Unorm_sRGB)?;
        let atlas = AtlasTextures::new(&device, atlas_page)?;
        let root = device
            .raw()
            .newBufferWithLength_options(ROOT_UNIFORM_BYTES, MTLResourceOptions::StorageModeShared)
            .ok_or_else(|| init_error("newBufferWithLength_options(root)", "returned nil"))?;
        let mut device = device;
        let persist = Persist::new(&mut device, size)?;
        let upload = UploadArena::new(&mut device)?;
        let timer = GpuTimer::new();
        Ok(Self {
            device,
            queue,
            pipeline,
            atlas,
            persist,
            upload,
            root,
            timer,
            swapchain: None,
            clear_color: [0.0, 0.0, 0.0, 0.0],
            surface: (size.0.max(1), size.1.max(1)),
            clips: Vec::with_capacity(8),
            #[cfg(any(test, feature = "test-support"))]
            readback: Readback::new(),
        })
    }

    /// 绘图图层的强引用（平台层用它挂到 `NSView`，并在缩放变化时更新 `contentsScale`）。
    pub(crate) fn layer_handle(&self) -> Option<Retained<objc2_quartz_core::CAMetalLayer>> {
        self.swapchain.as_ref().map(|swapchain| {
            let layer = swapchain.layer();
            // SAFETY: 图层由 swapchain 持有且非空；retain 后由返回的 `Retained` 管理。
            unsafe { Retained::retain(layer as *const _ as *mut _) }.expect("layer is non-null")
        })
    }

    pub(crate) fn surface(&self) -> (u32, u32) {
        self.surface
    }

    pub(crate) fn is_fresh(&self) -> bool {
        self.swapchain.as_ref().is_none_or(Swapchain::is_fresh)
    }

    /// resize：重建持久 offscreen（旧纹理走 retire）+ 调图层 `drawableSize`。
    ///
    /// 同步提交下「等 GPU 空闲」是恒真（[`Queue::wait_idle`] 是 no-op）。
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        let size = (size.0.max(1), size.1.max(1));
        if size == self.surface {
            return Ok(());
        }
        self.queue.wait_idle();
        if let Some(swapchain) = self.swapchain.as_mut() {
            swapchain.resize(size);
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
    ) -> Result<crate::render::RenderStats, EngineError> {
        if let Some(error) = self.device.lost_error() {
            return Err(error);
        }
        timings.reset(index);
        debug_assert_eq!(
            timings.frame(),
            index,
            "timings must be reset for this frame"
        );
        debug_assert!(timings.samples().is_empty(), "timings must start empty");
        let frame_started = Instant::now();
        // **不要**按 `copy_plan()` 做局部拷贝：drawable 的内容跨帧无保证（结论 #4 / F-121）。
        // 这里只断言「partial 帧确实带 dirty 集」，本帧仍然整面 blit。
        debug_assert!(
            !matches!(frame.plan(), Plan::Partial(_)) || frame.present_dirty().is_some(),
            "partial plans must carry their dirty set"
        );
        buffer.write_clips_gpu(&mut self.clips);
        // 先拿 drawable：拿不到就没有可写目标，**不要**开始这一帧（否则 ring 前进但没有提交，
        // 下一次 `begin` 的断言会把这一帧算成丢帧）。
        let drawable = match self.swapchain.as_ref() {
            Some(swapchain) => match swapchain.next_drawable() {
                Some(drawable) => Some(drawable),
                None => return Ok(crate::render::RenderStats::default()),
            },
            None => None,
        };
        let (slot, command) = self.queue.begin(index)?;

        let uploaded = self.upload.write(
            &mut self.device,
            &mut self.queue,
            index,
            slot.index,
            buffer.as_bytes(),
            &self.clips,
        )?;
        let (instance_count, _clip_count) = uploaded;
        write_root(&self.root, self.surface);
        timings.push(GpuTimerSample {
            section: Section::Upload,
            cpu_ns: elapsed_ns(frame_started),
            gpu_ns: 0,
        });

        let full = frame.is_full();
        let mut draw_calls = 0u32;

        if self.atlas.has_pending() {
            let blit = command
                .blitCommandEncoder()
                .ok_or_else(|| init_error("blitCommandEncoder(atlas)", "returned nil"))?;
            self.atlas.record_uploads(&blit);
            blit.endEncoding();
        }

        let draw_result = {
            let pass = MTLRenderPassDescriptor::new();
            // SAFETY: 索引 0 恒存在（渲染通道至少一个颜色附件槽）。
            let color = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
            color.setTexture(Some(self.persist.texture()));
            color.setLoadAction(if full {
                MTLLoadAction::Clear
            } else {
                MTLLoadAction::Load
            });
            color.setStoreAction(MTLStoreAction::Store);
            color.setClearColor(self.clear_color(full));
            let encoder = command
                .renderCommandEncoderWithDescriptor(&pass)
                .ok_or_else(|| init_error("renderCommandEncoder", "returned nil"))?;
            self.bind(&encoder, slot.index);
            // 逐 scissor 画：全帧 = 整面一次；局部 = 每个 damage 矩形一次。
            match full {
                true => {
                    encoder.setScissorRect(whole_scissor(self.surface));
                    draw_calls = draw_ops(&encoder, buffer, instance_count)?;
                }
                false => match frame.plan() {
                    Plan::Idle | Plan::Full => {}
                    Plan::Partial(damage) => {
                        for rect in damage.as_slice() {
                            encoder.setScissorRect(scissor_for(rect));
                            draw_calls = draw_calls.saturating_add(draw_ops(
                                &encoder,
                                buffer,
                                instance_count,
                            )?);
                        }
                    }
                },
            }
            encoder.endEncoding();

            if let Some(drawable) = drawable.as_ref() {
                let target = drawable.texture();
                let blit = command
                    .blitCommandEncoder()
                    .ok_or_else(|| init_error("blitCommandEncoder", "returned nil"))?;
                blit_persist_to(&blit, self.persist.texture(), &target, self.surface);
                blit.endEncoding();
            }
            Ok(())
        };
        if let Err(error) = draw_result {
            return Err(self.classify(Some(&error_message(&error))));
        }

        if let Err(error) = self.queue.submit(slot, &command) {
            // 命令缓冲失败：优先分类为设备丢失（粘性），否则原样返回。
            let buffer_error = command.error();
            if let Some(lost) = self
                .device
                .note_failure(buffer_error.as_deref(), index.get() as u64)
            {
                return Err(lost);
            }
            return Err(error);
        }

        if let Some(timer) = self.timer.as_ref()
            && let Some(sample) = timer.resolve(&command)
        {
            timings.push(sample);
        }

        let present_started = Instant::now();
        if let Some(drawable) = drawable.as_ref() {
            // `MTLDrawable::present`（`presentDrawable:` 的等价物，`commit` 之后调用）。
            Swapchain::present_drawable(drawable);
        }
        if let Some(swapchain) = self.swapchain.as_mut()
            && drawable.is_some()
        {
            swapchain.presented();
        }
        frame.presented();
        timings.push(GpuTimerSample {
            section: Section::Present,
            cpu_ns: elapsed_ns(present_started),
            gpu_ns: 0,
        });
        debug_assert!(
            timings.samples().len() <= 3,
            "at most three sections per frame"
        );

        self.queue.release_completed(index, &mut self.device);
        Ok(crate::render::RenderStats {
            draw_calls,
            instances: instance_count,
        })
    }

    /// 读回持久 offscreen（快照；同步等待 GPU 完成）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn readback(&mut self) -> Result<ReadbackImage, EngineError> {
        self.queue.wait_idle();
        self.readback.capture(
            &self.device,
            &self.queue,
            self.persist.texture(),
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

    /// 图集上传入口（P09 的字形页与 P15 的图片页共用）。
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        pixels: &[u8],
        rows: crate::render::atlas::DirtyRows,
    ) {
        self.atlas.queue_upload(kind, pixels, rows);
    }

    fn clear_color(&self, full: bool) -> MTLClearColor {
        let [r, g, b, a] = self.clear_color;
        let _ = full;
        MTLClearColor {
            red: f64::from(r),
            green: f64::from(g),
            blue: f64::from(b),
            alpha: f64::from(a),
        }
    }

    fn bind(&self, encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>, slot: usize) {
        encoder.setRenderPipelineState(self.pipeline.quad());
        let (instances, clips) = self.upload.slot(slot);
        // SAFETY: 缓冲/纹理都在本渲染器生命周期内；索引与 P07 的绑定表逐字一致。
        unsafe {
            encoder.setVertexBuffer_offset_atIndex(Some(&self.root), 0, 2);
            encoder.setVertexBuffer_offset_atIndex(Some(instances), 0, 0);
            encoder.setFragmentBuffer_offset_atIndex(Some(instances), 0, 0);
            encoder.setFragmentBuffer_offset_atIndex(Some(clips), 0, 1);
            encoder.setFragmentTexture_atIndex(Some(self.atlas.texture(AtlasKind::Glyph)), 0);
            encoder.setFragmentTexture_atIndex(Some(self.atlas.texture(AtlasKind::Image)), 1);
            encoder.setFragmentSamplerState_atIndex(Some(self.pipeline.sampler()), 0);
            encoder.setViewport(MTLViewport {
                originX: 0.0,
                originY: 0.0,
                width: f64::from(self.surface.0),
                height: f64::from(self.surface.1),
                znear: 0.0,
                zfar: 1.0,
            });
        }
    }

    /// 把 GPU 错误归类为设备丢失（粘性）或原样返回。
    fn classify(&mut self, message: Option<&str>) -> EngineError {
        let error =
            message.map(|message| smol_str::format_smolstr!("metal submit failed: {message}"));
        match error {
            Some(detail) => EngineError::Swapchain { detail },
            None => EngineError::AppInit {
                detail: smol_str::SmolStr::new_static("metal submit failed"),
            },
        }
    }
}

/// 错误消息（`classify` 的输入；`EngineError` 不可 `Clone`）。
fn error_message(error: &EngineError) -> String {
    error.to_string()
}

/// 持久 offscreen（唯一绘制目标；`BGRA8Unorm_sRGB` / Private）。
struct Persist {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    handle: GpuResourceHandle,
    size: (u32, u32),
}

impl Persist {
    fn new(device: &mut Device, size: (u32, u32)) -> Result<Self, EngineError> {
        let texture = create_target(device, size)?;
        let handle = device.allocate(GpuObject::Texture(texture.clone()));
        Ok(Self {
            texture,
            handle,
            size,
        })
    }

    fn recreate(&mut self, device: &mut Device, size: (u32, u32)) -> Result<(), EngineError> {
        let texture = create_target(device, size)?;
        let handle = device.allocate(GpuObject::Texture(texture.clone()));
        // 同步提交：`wait_idle` 之后旧纹理可以立即释放。
        device.release(self.handle);
        self.texture = texture;
        self.handle = handle;
        self.size = size;
        Ok(())
    }

    fn texture(&self) -> &ProtocolObject<dyn MTLTexture> {
        &self.texture
    }
}

fn create_target(
    device: &Device,
    size: (u32, u32),
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, EngineError> {
    // SAFETY: 纯构造函数；尺寸至少 1（`with_device` 已夹紧）。
    let descriptor = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm_sRGB,
            size.0.max(1) as usize,
            size.1.max(1) as usize,
            false,
        )
    };
    descriptor.setUsage(MTLTextureUsage::RenderTarget);
    descriptor.setStorageMode(objc2_metal::MTLStorageMode::Private);
    device
        .raw()
        .newTextureWithDescriptor(&descriptor)
        .ok_or_else(|| init_error("newTextureWithDescriptor(persist)", "returned nil"))
}

/// 每 in-flight 槽的实例 / 裁剪上传缓冲（只写不重建容量；扩容时才换资源）。
struct UploadArena {
    slots: Vec<UploadSlot>,
}

struct UploadSlot {
    instances: Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>,
    instance_handle: GpuResourceHandle,
    instance_capacity: u32,
    clips: Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>,
    clip_handle: GpuResourceHandle,
    clip_capacity: u32,
}

impl UploadArena {
    fn new(device: &mut Device) -> Result<Self, EngineError> {
        let mut slots = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let instances = create_upload(device, 256 * 128)?;
            let instance_handle = device.allocate(GpuObject::Buffer(instances.clone()));
            let clips = create_upload(device, 8 * 48)?;
            let clip_handle = device.allocate(GpuObject::Buffer(clips.clone()));
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

    fn slot(
        &self,
        slot: usize,
    ) -> (
        &Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>,
        &Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>,
    ) {
        let slot = &self.slots[slot];
        (&slot.instances, &slot.clips)
    }

    /// 写入本帧的实例/裁剪字节；容量不够时扩容（旧缓冲交 retire 队列）。
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
        if instance_count > self.slots[slot].instance_capacity {
            let capacity = instance_count.next_power_of_two().max(256);
            let buffer = create_upload(device, u64::from(capacity) * 128)?;
            let handle = device.allocate(GpuObject::Buffer(buffer.clone()));
            let old = self.slots[slot].instance_handle;
            self.slots[slot].instances = buffer;
            self.slots[slot].instance_handle = handle;
            self.slots[slot].instance_capacity = capacity;
            queue.retire(index, old);
        }
        if clip_count > self.slots[slot].clip_capacity {
            let capacity = clip_count.next_power_of_two().max(8);
            let buffer = create_upload(device, u64::from(capacity) * 48)?;
            let handle = device.allocate(GpuObject::Buffer(buffer.clone()));
            let old = self.slots[slot].clip_handle;
            self.slots[slot].clips = buffer;
            self.slots[slot].clip_handle = handle;
            self.slots[slot].clip_capacity = capacity;
            queue.retire(index, old);
        }
        write_upload(&self.slots[slot].instances, instances);
        write_upload(&self.slots[slot].clips, bytemuck::cast_slice(clips));
        Ok((instance_count, clip_count))
    }
}

fn create_upload(
    device: &Device,
    bytes: u64,
) -> Result<Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>, EngineError> {
    device
        .raw()
        .newBufferWithLength_options(bytes as usize, MTLResourceOptions::StorageModeShared)
        .ok_or_else(|| init_error("newBufferWithLength_options(upload)", "returned nil"))
}

fn write_upload(buffer: &ProtocolObject<dyn objc2_metal::MTLBuffer>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    // SAFETY: 缓冲容量由 `UploadArena::write` 按 `bytes.len()` 扩容；shared 内存 CPU 可见。
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            buffer.contents().as_ptr().cast::<u8>(),
            bytes.len(),
        );
    }
}

/// `RootUniform`（112 B）：`frame @0`、`draw @32`；实例绘制只用 `frame.viewport`（P07 §2.7）。
fn write_root(buffer: &ProtocolObject<dyn objc2_metal::MTLBuffer>, surface: (u32, u32)) {
    let width = surface.0 as f32;
    let height = surface.1 as f32;
    let mut uniform = [0.0f32; ROOT_UNIFORM_BYTES / 4];
    uniform[0] = width;
    uniform[1] = height;
    uniform[2] = if width > 0.0 { 2.0 / width } else { 0.0 };
    uniform[3] = if height > 0.0 { 2.0 / height } else { 0.0 };
    let bytes = bytemuck::cast_slice::<f32, u8>(&uniform);
    // SAFETY: root buffer 恒 112 B（构造时分配）；只写同一长度。
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            buffer.contents().as_ptr().cast::<u8>(),
            bytes.len(),
        );
    }
}

fn draw_ops(
    encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
    buffer: &InstanceBuffer,
    instance_count: u32,
) -> Result<u32, EngineError> {
    let mut draw_calls = 0u32;
    for op in buffer.ops() {
        match *op {
            FrameOp::Instances { start, len } => {
                if len == 0 {
                    continue;
                }
                debug_assert!(start.saturating_add(len) <= instance_count);
                // SAFETY: 4 顶点 unit quad + 实例区间 [start, start+len)（编码期校验）。
                unsafe {
                    encoder.drawPrimitives_vertexStart_vertexCount_instanceCount_baseInstance(
                        MTLPrimitiveType::TriangleStrip,
                        0usize,
                        4usize,
                        len as usize,
                        start as usize,
                    );
                }
                draw_calls = draw_calls.saturating_add(1);
            }
            FrameOp::Geometry { index, .. } => {
                return Err(EngineError::GeometryRegister {
                    detail: smol_str::format_smolstr!(
                        "geometry op index {index} has no GPU buffers (GeometryStore lands in P13)"
                    ),
                });
            }
        }
    }
    Ok(draw_calls)
}

/// 整面 blit（**每帧**，因为 drawable 内容跨帧无保证）。
fn blit_persist_to(
    blit: &ProtocolObject<dyn objc2_metal::MTLBlitCommandEncoder>,
    persist: &ProtocolObject<dyn MTLTexture>,
    target: &ProtocolObject<dyn MTLTexture>,
    size: (u32, u32),
) {
    // SAFETY: 源/目标都是本渲染器独占的纹理；尺寸一致（persist 与 drawable 都由 surface 决定）。
    unsafe {
        blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
            persist,
            0,
            0,
            MTLOrigin { x: 0, y: 0, z: 0 },
            MTLSize {
                width: size.0 as usize,
                height: size.1 as usize,
                depth: 1,
            },
            target,
            0,
            0,
            MTLOrigin { x: 0, y: 0, z: 0 },
        );
    }
}

fn whole_scissor(surface: (u32, u32)) -> MTLScissorRect {
    MTLScissorRect {
        x: 0,
        y: 0,
        width: surface.0 as usize,
        height: surface.1 as usize,
    }
}

fn scissor_for(rect: &crate::math::PhysicalRect) -> MTLScissorRect {
    MTLScissorRect {
        x: rect.x0.max(0) as usize,
        y: rect.y0.max(0) as usize,
        width: rect.width().max(0) as usize,
        height: rect.height().max(0) as usize,
    }
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos() as u64
}

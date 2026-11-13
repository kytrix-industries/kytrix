//! 帧编排：上传 → 逐 damage scissor 画进持久 offscreen → 整面 copy 到 swapchain → present
//! （`design/phase-12-macos-linux.md` §6.6 / §6.9）。
//!
//! **两条与 Windows 不同的硬约束**（F-121）：
//! 1. `FramePlan::copy_plan()` 是 D3D12 专用：swapchain image 的内容 present 后未定义，
//!    因此**每帧整面 `cmd_copy_image`**；`present_dirty()` 只作为 `VK_KHR_incremental_present` 的 hint。
//! 2. 没有自动 barrier：本文件是 barrier 的**唯一**写点（表见 §6.6）。
//!
//! `VkRenderPass` 的 `loadOp` 是通道属性 ⇒ 建两个通道（CLEAR / LOAD），逐帧二选一（F-126）。

use std::time::Instant;

use ash::vk;

#[cfg(any(test, feature = "test-support"))]
use crate::render::linux::readback::{Readback, ReadbackImage};
use crate::{
    error::EngineError,
    paint::{
        atlas::AtlasKind,
        instance::{FrameOp, GpuClipEntry, InstanceBuffer},
    },
    render::{
        compositor::{FramePlan, Plan},
        frame::{FrameSlot, MAX_FRAMES_IN_FLIGHT},
        linux::{
            atlas::{ATLAS_PAGE, AtlasTextures},
            device::{Device, GpuObject, init_error},
            instance::VulkanInstance,
            pipeline::{Pipeline, ROOT_UNIFORM_BYTES},
            swapchain::Swapchain,
            timer::GpuTimer,
        },
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample, Section},
    },
    window::FrameIndex,
};

/// `minStorageBufferOffsetAlignment` 的保守上界（256 B）：clips 段在此偏移处开始。
const CLIP_ALIGN: u64 = 256;

/// 一个窗口的 GPU 渲染器（`Window.renderer` 的载荷）。
pub(crate) struct Renderer {
    /// 真实窗口才有 swapchain；离屏（快照）渲染器只有 `persist`。
    swapchain: Option<Swapchain>,
    pipeline: Pipeline,
    atlas: AtlasTextures,
    persist: Persist,
    upload: UploadArena,
    command_pool: vk::CommandPool,
    commands: [vk::CommandBuffer; MAX_FRAMES_IN_FLIGHT],
    timer: Option<GpuTimer>,
    clear_color: [f32; 4],
    surface: (u32, u32),
    /// 渲染目标格式（swapchain 的格式，或离屏时的 `B8G8R8A8_SRGB`）。
    format: vk::Format,
    clips: Vec<GpuClipEntry>,
    /// 最近一次提交的 timeline 值（`FrameIndex` 的镜像；`wait_idle` 用）。
    submitted: u64,
    #[cfg(any(test, feature = "test-support"))]
    readback: Readback,
    // 设备与实例必须**最后**声明：Rust 按声明顺序 drop，子资源先于设备释放、surface 后于设备释放。
    device: Device,
    instance: VulkanInstance,
}

/// 离屏渲染目标格式：与 Wayland swapchain 的首选格式一致（`swapchain.rs` 的选择顺序）。
const OFFSCREEN_FORMAT: vk::Format = vk::Format::B8G8R8A8_SRGB;

impl Renderer {
    /// 真实窗口（Wayland surface 的裸指针由平台层提供）。
    pub(crate) fn new_for_surface(
        display: *mut std::ffi::c_void,
        surface_ptr: *mut std::ffi::c_void,
        size: (u32, u32),
        _scale: f32,
    ) -> Result<Self, EngineError> {
        let size = (size.0.max(1), size.1.max(1));
        let instance = VulkanInstance::new(display, surface_ptr)?;
        let mut device = Device::new(&instance)?;
        let swapchain = Swapchain::new(&instance, &device, size)?;
        let format = swapchain.format();
        let pipeline = Pipeline::new(&device, format)?;
        let atlas = AtlasTextures::new(&device, ATLAS_PAGE)?;
        pipeline.write_atlas(atlas.glyph_view(), atlas.image_view(), atlas.sampler());
        let persist = Persist::new(&device, &pipeline, size, format)?;
        let upload = UploadArena::new(&mut device)?;
        let (command_pool, commands) = create_commands(&device)?;
        let timer = GpuTimer::new(&device)?;
        Ok(Self {
            swapchain: Some(swapchain),
            pipeline,
            atlas,
            persist,
            upload,
            command_pool,
            commands,
            timer,
            clear_color: [0.0, 0.0, 0.0, 0.0],
            surface: size,
            format,
            clips: Vec::with_capacity(8),
            submitted: 0,
            #[cfg(any(test, feature = "test-support"))]
            readback: Readback::new(),
            device,
            instance,
        })
    }

    /// 离屏渲染器（无窗口 / 快照）：不建 surface 与 swapchain，只渲染到持久 `persist`。
    ///
    /// Linux 的**产品**窗口路径必须有真实 `wl_surface`（F-137）；这里给的是「没有窗口也要画」
    /// 的路径（`Window::enable_offscreen_renderer` 与 `test-support` 的像素快照）。
    ///
    /// # Errors
    /// 实例 / 设备 / 管线 / 图集 / 持久目标任一创建失败。
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Self::with_headless(size, ATLAS_PAGE)
    }

    /// 指定图集页（快照必须与 `App` 的分配器同页尺寸）。
    ///
    /// # Errors
    /// 同 [`Self::new_offscreen`]。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_offscreen_with_page(
        size: (u32, u32),
        atlas_page: u32,
    ) -> Result<Self, EngineError> {
        Self::with_headless(size, atlas_page)
    }

    fn with_headless(size: (u32, u32), atlas_page: u32) -> Result<Self, EngineError> {
        let size = (size.0.max(1), size.1.max(1));
        let instance = VulkanInstance::new_headless()?;
        let mut device = Device::new(&instance)?;
        let format = OFFSCREEN_FORMAT;
        let pipeline = Pipeline::new(&device, format)?;
        let atlas = AtlasTextures::new(&device, atlas_page)?;
        pipeline.write_atlas(atlas.glyph_view(), atlas.image_view(), atlas.sampler());
        let persist = Persist::new(&device, &pipeline, size, format)?;
        let upload = UploadArena::new(&mut device)?;
        let (command_pool, commands) = create_commands(&device)?;
        let timer = GpuTimer::new(&device)?;
        Ok(Self {
            swapchain: None,
            pipeline,
            atlas,
            persist,
            upload,
            command_pool,
            commands,
            timer,
            clear_color: [0.0, 0.0, 0.0, 0.0],
            surface: size,
            format,
            clips: Vec::with_capacity(8),
            submitted: 0,
            #[cfg(any(test, feature = "test-support"))]
            readback: Readback::new(),
            device,
            instance,
        })
    }

    pub(crate) fn surface(&self) -> (u32, u32) {
        self.surface
    }

    pub(crate) fn is_fresh(&self) -> bool {
        self.swapchain.as_ref().is_none_or(Swapchain::is_fresh)
    }

    /// resize：等 GPU 空闲 → 重建 swapchain + persist（旧资源走 retire）。
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        let size = (size.0.max(1), size.1.max(1));
        if size == self.surface {
            return Ok(());
        }
        self.wait_idle()?;
        if let Some(swapchain) = self.swapchain.as_mut() {
            swapchain.resize(&self.instance, &self.device, size)?;
        }
        let persist = Persist::new(&self.device, &self.pipeline, size, self.format)?;
        // `wait_idle` 之后旧 persist 可以立即 drop（`Persist::drop` 自己销毁句柄）。
        self.persist = persist;
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
        let upload_started = Instant::now();
        buffer.write_clips_gpu(&mut self.clips);
        let slot: FrameSlot = self.device.begin(index)?;

        let instances = (buffer.as_bytes().len() / 128) as u32;
        let (_instance_bytes, clip_offset, _clip_bytes) = self.upload.write(
            &mut self.device,
            index,
            slot.index,
            buffer.as_bytes(),
            bytemuck::cast_slice(&self.clips),
        )?;
        self.pipeline
            .write_slot(slot.index, self.upload.buffer(slot.index), clip_offset);

        let image_index = match self.swapchain.as_mut() {
            Some(swapchain) => {
                let mut index_now = swapchain.acquire(&self.device, index.get() as u64)?;
                if index_now == u32::MAX {
                    // OUT_OF_DATE：**必须**重建（`Renderer::resize` 的同尺寸短路只服务窗口尺寸变化，
                    // 这里尺寸没变也得换 swapchain）。重建后重试一次 acquire。
                    swapchain.resize(&self.instance, &self.device, self.surface)?;
                    index_now = swapchain.acquire(&self.device, index.get() as u64)?;
                    if index_now == u32::MAX {
                        return Ok(crate::render::RenderStats::default());
                    }
                }
                Some(index_now)
            }
            None => None,
        };
        timings.push(GpuTimerSample {
            section: Section::Upload,
            cpu_ns: elapsed_ns(upload_started),
            gpu_ns: 0,
        });

        let command = self.commands[slot.index];
        // SAFETY: 命令缓冲由本结构的命令池分配；同一时刻只有一个在录制。
        unsafe {
            self.device
                .raw()
                .reset_command_buffer(command, vk::CommandBufferResetFlags::empty())
                .map_err(|error| init_error("reset_command_buffer", format!("{error:?}")))?;
            self.device
                .raw()
                .begin_command_buffer(
                    command,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|error| init_error("begin_command_buffer", format!("{error:?}")))?;
        }

        if let Some(timer) = self.timer.as_ref() {
            timer.begin(command, slot.index);
        }
        if self.atlas.has_pending() {
            self.atlas.record_uploads(&self.device, command);
        }

        let full = frame.is_full();
        let draw_calls = self.draw(command, slot.index, buffer, frame, full)?;
        if let Some(image_index) = image_index {
            self.copy_to_swapchain(command, image_index)?;
        }
        if let Some(timer) = self.timer.as_ref() {
            timer.end(command, slot.index);
        }
        // SAFETY: 命令录制的收尾。
        unsafe {
            self.device
                .raw()
                .end_command_buffer(command)
                .map_err(|error| init_error("end_command_buffer", format!("{error:?}")))?;
        }

        self.submit_commands(slot, command, index.get() as u64)?;

        let present_started = Instant::now();
        if let Some(timer) = self.timer.as_ref()
            && let Some(sample) = timer.resolve(slot.index)
        {
            timings.push(sample);
        }
        if let (Some(swapchain), Some(image_index)) = (self.swapchain.as_mut(), image_index)
            && let Err(error) = swapchain.present(&self.device, slot.index, image_index, *frame)
        {
            // present 路径上的 device-lost 也要进粘性记录（否则下一帧才在 submit 里发现）。
            if matches!(error, EngineError::DeviceLost { .. }) {
                let _ = self
                    .device
                    .note_failure(vk::Result::ERROR_DEVICE_LOST, index.get() as u64);
            }
            return Err(error);
        }
        frame.presented();
        timings.push(GpuTimerSample {
            section: Section::Present,
            cpu_ns: elapsed_ns(present_started),
            gpu_ns: 0,
        });
        debug_assert!(
            timings.samples().len() <= 4,
            "at most four sections per frame"
        );

        self.submitted = index.get() as u64;
        self.device.drain_retire(index);
        Ok(crate::render::RenderStats {
            draw_calls,
            instances,
        })
    }

    /// 读回持久 offscreen（快照路径；同步等待 GPU）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn readback(&mut self) -> Result<ReadbackImage, EngineError> {
        self.wait_idle()?;
        let command = self.commands[0];
        let image = self.persist.image;
        let size = self.surface;
        // SAFETY: 命令缓冲由本结构独占；录制前已 `wait_idle` 并复位。
        unsafe {
            self.device
                .raw()
                .reset_command_buffer(command, vk::CommandBufferResetFlags::empty())
                .map_err(|error| {
                    init_error("reset_command_buffer(readback)", format!("{error:?}"))
                })?;
            self.device
                .raw()
                .begin_command_buffer(
                    command,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|error| {
                    init_error("begin_command_buffer(readback)", format!("{error:?}"))
                })?;
        }
        self.readback.record(&self.device, command, image, size)?;
        // SAFETY: 收尾；提交在下面同步完成。
        unsafe {
            self.device
                .raw()
                .end_command_buffer(command)
                .map_err(|error| {
                    init_error("end_command_buffer(readback)", format!("{error:?}"))
                })?;
        }
        let commands = [command];
        let submit = vk::SubmitInfo::default().command_buffers(&commands);
        // SAFETY: 单次提交 + 等队列空闲（快照是测试路径）。
        unsafe {
            self.device
                .raw()
                .queue_submit(self.device.queue(), &[submit], vk::Fence::null())
                .map_err(|error| init_error("queue_submit(readback)", format!("{error:?}")))?;
            self.device
                .raw()
                .queue_wait_idle(self.device.queue())
                .map_err(|error| init_error("queue_wait_idle(readback)", format!("{error:?}")))?;
        }
        self.readback.fetch(&self.device, size)
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

    fn wait_idle(&mut self) -> Result<(), EngineError> {
        if self.submitted == 0 {
            return Ok(());
        }
        self.device.wait_idle(self.submitted)
    }

    fn draw(
        &mut self,
        command: vk::CommandBuffer,
        slot: usize,
        buffer: &InstanceBuffer,
        frame: &FramePlan,
        full: bool,
    ) -> Result<u32, EngineError> {
        let mut draw_calls = 0u32;
        let extent = self.extent();
        let clear = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: self.clear_color,
            },
        };
        let clears = [clear];
        let framebuffer = self.persist.framebuffer(full);
        // SAFETY: 渲染通道/framebuffer 归本结构独占；`loadOp` 与 `clear` 值匹配。
        unsafe {
            self.device.raw().cmd_begin_render_pass(
                command,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(self.pipeline.render_pass(full))
                    .framebuffer(framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent,
                    })
                    .clear_values(&clears),
                vk::SubpassContents::INLINE,
            );
            self.device.raw().cmd_bind_pipeline(
                command,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline.quad(),
            );
            self.device.raw().cmd_bind_descriptor_sets(
                command,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline.layout(),
                0,
                &[self.pipeline.set(slot)],
                &[],
            );
            let root = root_uniform(extent);
            let push_bytes = bytemuck::cast_slice::<f32, u8>(&root);
            self.device.raw().cmd_push_constants(
                command,
                self.pipeline.layout(),
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                push_bytes,
            );
            self.device.raw().cmd_set_viewport(
                command,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: extent.width as f32,
                    height: extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
        }

        // 逐 damage 矩形：每个矩形一次 `cmd_set_scissor` + 全部 draw op。
        match full {
            true => {
                // SAFETY: 同上。
                unsafe {
                    self.device.raw().cmd_set_scissor(
                        command,
                        0,
                        &[vk::Rect2D {
                            offset: vk::Offset2D { x: 0, y: 0 },
                            extent,
                        }],
                    );
                }
                draw_calls = draw_ops(&self.device, command, buffer)?;
            }
            false => {
                if let Plan::Partial(damage) = frame.plan() {
                    for rect in damage.as_slice() {
                        let scissor = vk::Rect2D {
                            offset: vk::Offset2D {
                                x: rect.x0.max(0),
                                y: rect.y0.max(0),
                            },
                            extent: vk::Extent2D {
                                width: rect.width().max(0) as u32,
                                height: rect.height().max(0) as u32,
                            },
                        };
                        // SAFETY: 矩形已由 `render::compositor::plan` 夹到 surface 内。
                        unsafe {
                            self.device.raw().cmd_set_scissor(command, 0, &[scissor]);
                        }
                        draw_calls =
                            draw_calls.saturating_add(draw_ops(&self.device, command, buffer)?);
                    }
                }
            }
        }

        // SAFETY: 收尾渲染通道。
        unsafe {
            self.device.raw().cmd_end_render_pass(command);
        }
        Ok(draw_calls)
    }

    /// 渲染目标的像素尺寸（有 swapchain 时与它一致；离屏时等于 `persist` 尺寸）。
    fn extent(&self) -> vk::Extent2D {
        match self.swapchain.as_ref() {
            Some(swapchain) => swapchain.extent(),
            None => vk::Extent2D {
                width: self.surface.0,
                height: self.surface.1,
            },
        }
    }

    /// persist → swapchain 整面 copy + 两侧 barrier（§6.6 的表）。
    fn copy_to_swapchain(
        &mut self,
        command: vk::CommandBuffer,
        image_index: u32,
    ) -> Result<(), EngineError> {
        let Some(swapchain) = self.swapchain.as_ref() else {
            return Ok(());
        };
        let extent = swapchain.extent();
        let target = swapchain.image(image_index);
        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .layer_count(1);
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let to_src = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .image(self.persist.image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
        let to_dst = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(target)
            .subresource_range(range)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
        let to_present = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
            .image(target)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE);
        let back = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .image(self.persist.image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
        // SAFETY: 两张 image 都由本结构独占；copy 的范围 = swapchain extent = persist 尺寸。
        unsafe {
            self.device.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_src],
            );
            self.device.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            );
            self.device.raw().cmd_copy_image(
                command,
                self.persist.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                target,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::ImageCopy::default()
                    // `default()` 的 subresource 是 aspect=0 / layer_count=0（非法），必须显式给。
                    .src_subresource(layers)
                    .dst_subresource(layers)
                    .extent(vk::Extent3D {
                        width: extent.width,
                        height: extent.height,
                        depth: 1,
                    })],
            );
            self.device.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_present],
            );
            self.device.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[back],
            );
        }
        Ok(())
    }

    fn submit_commands(
        &mut self,
        slot: FrameSlot,
        command: vk::CommandBuffer,
        timeline_value: u64,
    ) -> Result<(), EngineError> {
        let commands = [command];
        // 离屏路径没有 swapchain：既不 wait acquire 信号量，也不 signal render-finished。
        let waits: Vec<vk::Semaphore> = self
            .swapchain
            .as_ref()
            .map(|swapchain| swapchain.wait_semaphore())
            .into_iter()
            .collect();
        let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let mut signals = Vec::with_capacity(2);
        if let Some(swapchain) = self.swapchain.as_ref() {
            signals.push(swapchain.render_finished(slot.index));
        }
        signals.push(self.device.timeline());
        // timeline semaphore **必须带值**：`wait_semaphore_values` 覆盖全部 wait 信号量
        // （binary 信号量的值被忽略），`signal_semaphore_values` 给出每个信号量的目标值。
        let wait_values = vec![0u64; waits.len()];
        let signal_values: Vec<u64> = {
            let mut values = vec![0u64; signals.len()];
            if let Some(last) = values.last_mut() {
                *last = timeline_value;
            }
            values
        };
        let mut timeline_info = vk::TimelineSemaphoreSubmitInfo::default()
            .wait_semaphore_values(&wait_values)
            .signal_semaphore_values(&signal_values);
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&waits)
            .wait_dst_stage_mask(&stages[..waits.len().min(stages.len())])
            .command_buffers(&commands)
            .signal_semaphores(&signals)
            .push_next(&mut timeline_info);
        // fence 的唯一 reset 点：reset 之后必须立刻 submit（否则该槽永远不再 signal）。
        self.device.reset_fence(slot.index)?;
        // SAFETY: 命令缓冲已录制完毕；fence 由 `Device` 持有（每槽一个）。
        let result = unsafe {
            self.device.raw().queue_submit(
                self.device.queue(),
                &[submit],
                self.device.fence(slot.index),
            )
        };
        if let Err(error) = result {
            if let Some(lost) = self.device.note_failure(error, timeline_value) {
                return Err(lost);
            }
            return Err(EngineError::Swapchain {
                detail: smol_str::format_smolstr!("queue_submit: {error:?}"),
            });
        }
        // SAFETY: fence 由本次提交 signal；`true` = 等到。
        unsafe {
            self.device
                .raw()
                .wait_for_fences(&[self.device.fence(slot.index)], true, u64::MAX)
                .map_err(|error| init_error("wait_for_fences", format!("{error:?}")))?;
        }
        self.device.advance(slot);
        Ok(())
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: 先等队列空闲，再销毁命令池；其余句柄由各字段的 `Drop` 按声明顺序释放
        // （`device` / `instance` 声明在最后 ⇒ 最后释放）。
        unsafe {
            let _ = self.device.raw().queue_wait_idle(self.device.queue());
            self.device
                .raw()
                .destroy_command_pool(self.command_pool, None);
        }
    }
}

/// 持久 offscreen（唯一绘制目标；与 swapchain 同格式）。
struct Persist {
    device: ash::Device,
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
    framebuffer_clear: vk::Framebuffer,
    framebuffer_load: vk::Framebuffer,
}

impl Persist {
    fn new(
        device: &Device,
        pipeline: &Pipeline,
        size: (u32, u32),
        format: vk::Format,
    ) -> Result<Self, EngineError> {
        // SAFETY: 句柄归本结构独占；`Drop` / `retire` 里销毁。
        let image = unsafe {
            device.raw().create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D {
                        width: size.0,
                        height: size.1,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(
                        vk::ImageUsageFlags::COLOR_ATTACHMENT
                            | vk::ImageUsageFlags::TRANSFER_SRC
                            | vk::ImageUsageFlags::SAMPLED,
                    )
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }
        .map_err(|error| init_error("create_image(persist)", format!("{error:?}")))?;
        // SAFETY: 只读查询；句柄由本结构独占，返回值在本次调用期间有效。
        let requirements = unsafe { device.raw().get_image_memory_requirements(image) };
        let memory_type = device
            .memory_type(
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
            .ok_or_else(|| init_error("persist memory", "no device-local memory type"))?;
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let memory = unsafe {
            device.raw().allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(memory_type),
                None,
            )
        }
        .map_err(|error| init_error("allocate_memory(persist)", format!("{error:?}")))?;
        // SAFETY: 目标句柄已按 Vulkan 校验规则创建，绑定/写入的范围在其分配内。
        unsafe { device.raw().bind_image_memory(image, memory, 0) }
            .map_err(|error| init_error("bind_image_memory(persist)", format!("{error:?}")))?;
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let view = unsafe {
            device.raw().create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
        }
        .map_err(|error| init_error("create_image_view(persist)", format!("{error:?}")))?;
        let attachments = [view];
        let framebuffer_clear =
            create_framebuffer(device, pipeline.render_pass(true), &attachments, size)?;
        let framebuffer_load =
            create_framebuffer(device, pipeline.render_pass(false), &attachments, size)?;
        Ok(Self {
            device: device.raw().clone(),
            image,
            view,
            memory,
            framebuffer_clear,
            framebuffer_load,
        })
    }

    fn framebuffer(&self, clear: bool) -> vk::Framebuffer {
        if clear {
            self.framebuffer_clear
        } else {
            self.framebuffer_load
        }
    }
}

impl Drop for Persist {
    fn drop(&mut self) {
        // SAFETY: 句柄归本结构独占；调用前队列已空闲（`Renderer::resize` 先 `wait_idle`）。
        unsafe {
            self.device
                .destroy_framebuffer(self.framebuffer_clear, None);
            self.device.destroy_framebuffer(self.framebuffer_load, None);
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

fn create_framebuffer(
    device: &Device,
    pass: vk::RenderPass,
    attachments: &[vk::ImageView],
    size: (u32, u32),
) -> Result<vk::Framebuffer, EngineError> {
    // SAFETY: 附件是本结构持有的 color view；尺寸与 image 一致。
    unsafe {
        device.raw().create_framebuffer(
            &vk::FramebufferCreateInfo::default()
                .render_pass(pass)
                .attachments(attachments)
                .width(size.0)
                .height(size.1)
                .layers(1),
            None,
        )
    }
    .map_err(|error| init_error("create_framebuffer", format!("{error:?}")))
}

/// 每 in-flight 槽的上传缓冲（实例 + 裁剪共用一块分配，clips 在 256 B 对齐处）。
struct UploadArena {
    device: ash::Device,
    slots: Vec<UploadSlot>,
}

struct UploadSlot {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    buffer_handle: GpuResourceHandle,
    memory_handle: GpuResourceHandle,
    instance_capacity: u32,
    clip_capacity: u32,
    clip_offset: u64,
    size: u64,
}

impl Drop for UploadArena {
    fn drop(&mut self) {
        for slot in &self.slots {
            // SAFETY: 句柄归本结构独占；`Renderer::drop` 已等队列空闲。
            unsafe {
                self.device.destroy_buffer(slot.buffer, None);
                self.device.free_memory(slot.memory, None);
            }
        }
    }
}

impl UploadArena {
    fn new(device: &mut Device) -> Result<Self, EngineError> {
        let mut slots = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let slot = UploadSlot::new(device, 256, 8)?;
            slots.push(slot);
        }
        Ok(Self {
            device: device.raw().clone(),
            slots,
        })
    }

    fn buffer(&self, slot: usize) -> vk::Buffer {
        self.slots[slot].buffer
    }

    /// 写本帧的实例/裁剪字节；容量不够时扩容（旧缓冲走 retire）。
    ///
    /// 返回 `(instance_bytes, clip_offset, clip_bytes)`。
    fn write(
        &mut self,
        device: &mut Device,
        index: FrameIndex,
        slot: usize,
        instances: &[u8],
        clips: &[u8],
    ) -> Result<(u64, u64, u64), EngineError> {
        let instance_count = (instances.len() / 128) as u32;
        let clip_count = (clips.len() / 48) as u32;
        if instance_count > self.slots[slot].instance_capacity
            || clip_count > self.slots[slot].clip_capacity
        {
            let instances_capacity = instance_count.next_power_of_two().max(256);
            let clips_capacity = clip_count.next_power_of_two().max(8);
            let replacement = UploadSlot::new(device, instances_capacity, clips_capacity)?;
            let old = std::mem::replace(&mut self.slots[slot], replacement);
            // 旧缓冲/内存在本帧完成之后由 retire 队列销毁（先 buffer 后 memory）。
            device.retire(index, old.buffer_handle);
            device.retire(index, old.memory_handle);
        }
        let slot_ref = &self.slots[slot];
        // SAFETY: 缓冲容量已按 `instance_count` / `clip_count` 扩容；映射范围在分配内。
        unsafe {
            let pointer = device
                .raw()
                .map_memory(
                    slot_ref.memory,
                    0,
                    slot_ref.size,
                    vk::MemoryMapFlags::empty(),
                )
                .map_err(|error| init_error("map_memory(upload)", format!("{error:?}")))?;
            let base = pointer.cast::<u8>();
            if !instances.is_empty() {
                std::ptr::copy_nonoverlapping(instances.as_ptr(), base, instances.len());
            }
            if !clips.is_empty() {
                std::ptr::copy_nonoverlapping(
                    clips.as_ptr(),
                    base.add(slot_ref.clip_offset as usize),
                    clips.len(),
                );
            }
            device.raw().unmap_memory(slot_ref.memory);
        }
        Ok((
            instances.len() as u64,
            slot_ref.clip_offset,
            clips.len() as u64,
        ))
    }
}

impl UploadSlot {
    fn new(
        device: &mut Device,
        instance_capacity: u32,
        clip_capacity: u32,
    ) -> Result<Self, EngineError> {
        let instance_bytes = u64::from(instance_capacity) * 128;
        let clip_offset = instance_bytes.div_ceil(CLIP_ALIGN) * CLIP_ALIGN;
        let size = clip_offset + u64::from(clip_capacity) * 48;
        // SAFETY: 句柄归本结构独占；扩容时旧句柄走 retire（`UploadArena::write` 已挂）。
        let buffer = unsafe {
            device.raw().create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(
                        vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
                    )
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }
        .map_err(|error| init_error("create_buffer(upload)", format!("{error:?}")))?;
        // SAFETY: 只读查询；句柄由本结构独占，返回值在本次调用期间有效。
        let requirements = unsafe { device.raw().get_buffer_memory_requirements(buffer) };
        let memory_type = device
            .memory_type(
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )
            .ok_or_else(|| init_error("upload memory", "no host-visible memory type"))?;
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let memory = unsafe {
            device.raw().allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(memory_type),
                None,
            )
        }
        .map_err(|error| init_error("allocate_memory(upload)", format!("{error:?}")))?;
        // SAFETY: 目标句柄已按 Vulkan 校验规则创建，绑定/写入的范围在其分配内。
        unsafe { device.raw().bind_buffer_memory(buffer, memory, 0) }
            .map_err(|error| init_error("bind_buffer_memory(upload)", format!("{error:?}")))?;
        let buffer_handle = device.allocate(GpuObject::Buffer(buffer));
        let memory_handle = device.allocate(GpuObject::Memory(memory));
        Ok(Self {
            buffer,
            memory,
            buffer_handle,
            memory_handle,
            instance_capacity,
            clip_capacity,
            clip_offset,
            size,
        })
    }
}

fn draw_ops(
    device: &Device,
    command: vk::CommandBuffer,
    buffer: &InstanceBuffer,
) -> Result<u32, EngineError> {
    let mut draw_calls = 0u32;
    for op in buffer.ops() {
        match *op {
            FrameOp::Instances { start, len } => {
                if len == 0 {
                    continue;
                }
                // SAFETY: 4 顶点 unit quad + 实例区间 [start, start+len)（编码期校验）。
                unsafe {
                    device.raw().cmd_draw(command, 4, len, start, 0);
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

fn create_commands(
    device: &Device,
) -> Result<(vk::CommandPool, [vk::CommandBuffer; MAX_FRAMES_IN_FLIGHT]), EngineError> {
    // SAFETY: 池与命令缓冲归 `Renderer` 独占；`Drop` 里销毁池。
    unsafe {
        let pool = device
            .raw()
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(device.queue_family())
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
            .map_err(|error| init_error("create_command_pool", format!("{error:?}")))?;
        let buffers = device
            .raw()
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(MAX_FRAMES_IN_FLIGHT as u32),
            )
            .map_err(|error| init_error("allocate_command_buffers", format!("{error:?}")))?;
        let mut commands = [vk::CommandBuffer::null(); MAX_FRAMES_IN_FLIGHT];
        for (target, source) in commands.iter_mut().zip(buffers) {
            *target = source;
        }
        Ok((pool, commands))
    }
}

/// `RootUniform`（112 B）：`frame @0`、`draw @32`；实例绘制只用 `frame.viewport`（P07 §2.7）。
fn root_uniform(extent: vk::Extent2D) -> [f32; ROOT_UNIFORM_BYTES as usize / 4] {
    let mut uniform = [0.0f32; ROOT_UNIFORM_BYTES as usize / 4];
    let width = extent.width as f32;
    let height = extent.height as f32;
    uniform[0] = width;
    uniform[1] = height;
    uniform[2] = if width > 0.0 { 2.0 / width } else { 0.0 };
    uniform[3] = if height > 0.0 { 2.0 / height } else { 0.0 };
    uniform
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos() as u64
}

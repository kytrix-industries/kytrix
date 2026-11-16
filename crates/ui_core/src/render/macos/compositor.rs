//! 帧编排：上传 → 逐 damage scissor 画进持久 offscreen → 整面 blit 到 drawable → present
//! （`design/async-render-pipeline.md` §5.2 / §5.6）。
//!
//! **本模块不取 drawable**：窗口路径的 drawable 由 `CAMetalDisplayLink` 的 update 提供，
//! `CAMetalLayer::nextDrawable` 已删除（§5.7）。离屏/快照路径不给 drawable，只画进 persist。
//!
//! **两条与 Windows 不同的硬约束**（F-121）：
//! 1. `FramePlan::copy_plan()` / `present_dirty()` 是 D3D12 专用：Metal 的 present 目标内容跨帧无保证，
//!    因此**每帧整面 blit**；`plan()`（scissor）与 `is_full()`（clear）仍是共用语义。
//! 2. 没有 barrier / 资源状态机：Metal 自动跟踪 hazard。
//!
//! 线程纪律：窗口路径的所有成员只属于 `kytrix-metal` 线程；persist 重建**不等待在飞帧**，
//! 旧纹理按帧序号进 retire 队列（§4.3 / I4）。图层属性一个都不在这里写（§5.3）。

use std::{ffi::c_void, ptr::NonNull, time::Instant};

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandBufferStatus,
    MTLCommandEncoder, MTLDevice, MTLIndexType, MTLLoadAction, MTLOrigin, MTLPixelFormat,
    MTLPrimitiveType, MTLRenderCommandEncoder, MTLRenderPassDescriptor, MTLResourceOptions,
    MTLScissorRect, MTLSize, MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
    MTLViewport,
};
use objc2_quartz_core::CAMetalDrawable;

#[cfg(any(test, feature = "test-support"))]
use crate::paint::instance::InstanceBuffer;
#[cfg(any(test, feature = "test-support"))]
use crate::render::macos::readback::{Readback, ReadbackImage};
use crate::{
    error::EngineError,
    paint::{
        atlas::AtlasKind,
        instance::{FrameOp, GpuClipEntry},
    },
    render::{
        compositor::{FramePlan, Plan},
        frame::{FramePayload, MAX_FRAMES_IN_FLIGHT},
        macos::{
            atlas::AtlasTextures,
            device::{Device, GpuObject, init_error, on_gpu_thread},
            geometry::GeometryRegistry,
            pipeline::Pipeline,
            queue::Queue,
        },
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample, Section},
        uniform::RootUniform,
    },
    window::FrameIndex,
};

/// `RootUniform` 的字节数（P07 §2.7：112 B）。
const ROOT_UNIFORM_BYTES: usize = 112;

/// 一个 GPU 渲染器（窗口路径由 `kytrix-metal` 线程独占；离屏路径同步调用）。
pub(crate) struct GpuRenderer {
    device: Device,
    queue: Queue,
    /// 已取到、尚未交给 `begin` 的上传槽（P1-2.12）。
    ///
    /// permit 必须与**具体槽**绑定：`try_acquire` 取的是某一槽的许可，`begin` 必须用
    /// 同一个槽，否则上传会写进别的在飞帧正在读的缓冲。
    pending_slot: Option<crate::render::frame::FrameSlot>,
    pipeline: Pipeline,
    atlas: AtlasTextures,
    persist: Persist,
    upload: UploadArena,
    /// 全量帧的 clear color（**linear**：`BGRA8Unorm_sRGB` 的 clear 由硬件做 sRGB 编码）。
    ///
    /// 窗口路径 = `WindowOptions.background` 的 linear 值（persist 重建后新 drawable 内的条带）；
    /// 离屏 / 快照路径保持 `[0, 0, 0, 0]`（透明，快照语义）。
    clear_color: [f32; 4],
    /// 当前持久目标 / drawable 的物理尺寸（drawable 的实际尺寸是最后真值，§5.4）。
    surface: (u32, u32),
    /// 窗口 DPI（`root.frame.params.x`；geometry 通道的逻辑→物理换算）。
    scale: f32,
    /// 静态几何的 GPU 缓冲（按需创建；换代产生的旧缓冲交 retire 队列）。
    geometry: GeometryRegistry,
    /// 下一次 present 是否必须全量（新建 / 重建之后为 `true`）。
    fresh: bool,
    /// 最后一次**提交**的帧序号：重建产生的旧句柄以它作为 retire 的 tag（§4.3）。
    last_index: FrameIndex,
    /// 本渲染器是否是**窗口**目标（§10.1 的线程归属断言只对窗口路径成立：离屏 / 快照由门面
    /// 在 UI 线程同步调用）。
    window_target: bool,
    #[cfg(any(test, feature = "test-support"))]
    readback: Readback,
}

// SAFETY: `GpuRenderer` 在构造完成后只整体移动一次到 `kytrix-metal` 线程，此后主线程不再访问
// 其中任何 Metal 对象；`CAMetalLayer` 甚至不在本结构里（§5.3 的唯一写者是主线程）。
// Metal 对象本身按 Apple 文档允许跨线程使用；这里保证的是「单线程独占」而不是共享。
unsafe impl Send for GpuRenderer {}

impl GpuRenderer {
    /// 产品路径（离屏 / 快照）：系统默认设备；图集页懒分配（DEVIATIONS F-208）。
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Self::with_device(Device::new()?, size, false, [0.0; 4])
    }

    /// 真实窗口：设备由主线程建好（图层已绑定它），这里只接管 GPU 侧对象。
    ///
    /// `background_linear` 是 persist clear color（linear）：重建 persist 后新 drawable 内部的
    /// 条带由它填（旧 drawable 之外的条带由 layer 的 `backgroundColor` 填）。
    pub(crate) fn new_window(
        device: Device,
        size: (u32, u32),
        background_linear: [f32; 4],
    ) -> Result<Self, EngineError> {
        Self::with_device(device, size, true, background_linear)
    }

    fn with_device(
        device: Device,
        size: (u32, u32),
        window_target: bool,
        clear_color: [f32; 4],
    ) -> Result<Self, EngineError> {
        let size = (size.0.max(1), size.1.max(1));
        let queue = Queue::new(&device)?;
        let pipeline = Pipeline::new(&device, MTLPixelFormat::BGRA8Unorm_sRGB)?;
        let mut device = device;
        // 图集页懒分配：初始化只有 1×1 占位纹理，绑定始终有效（DEVIATIONS F-208）。
        let atlas = AtlasTextures::new(&mut device)?;
        let persist = Persist::new(&mut device, size)?;
        let upload = UploadArena::new(&mut device)?;
        Ok(Self {
            device,
            queue,
            pipeline,
            atlas,
            persist,
            upload,
            clear_color,
            surface: size,
            scale: 1.0,
            geometry: GeometryRegistry::new(),
            pending_slot: None,
            fresh: true,
            last_index: FrameIndex::default(),
            window_target,
            #[cfg(any(test, feature = "test-support"))]
            readback: Readback::new(),
        })
    }

    /// §10.1 的线程归属断言：**窗口**渲染器的所有入口只属于 `kytrix-metal` 线程。
    ///
    /// 离屏 / 快照渲染器由门面在 UI 线程同步调用，因此不能做无条件断言（否则快照路径误报）。
    fn debug_assert_gpu_thread(&self, entry: &'static str) {
        debug_assert!(
            !self.window_target || on_gpu_thread(),
            "Metal window target: {entry} may only run on the kytrix-metal thread"
        );
    }

    /// 当前目标的物理尺寸（GPU 线程侧的真值；重建后更新）。
    pub(crate) fn surface(&self) -> (u32, u32) {
        self.surface
    }

    /// 下一次 present 是否必须全量（新建 / 重建之后为 `true`）。
    pub(crate) fn is_fresh(&self) -> bool {
        self.fresh
    }

    /// 非阻塞取一个上传槽（三缓冲背压，I1）：`false` = 三帧在飞，本帧跳帧。
    pub(crate) fn try_acquire_upload_slot(&mut self) -> bool {
        self.pending_slot = self.queue.try_acquire();
        self.pending_slot.is_some()
    }

    /// 归还一个已经取到、但本帧没有提交的槽位（防御路径）。
    pub(crate) fn release_upload_slot(&mut self) {
        if let Some(slot) = self.pending_slot.take() {
            self.queue.release(slot);
        }
    }

    /// 目标尺寸变化时重建 persist：**不等待在飞帧**（§5.4）。
    ///
    /// 旧 persist 可能仍被在飞命令缓冲引用，因此它按「当前最后提交序号」入 retire 队列，
    /// 完成通知到达后才释放（I4）。返回 `true` 表示本帧必须全量。
    pub(crate) fn prepare_target(&mut self, size: (u32, u32)) -> Result<bool, EngineError> {
        self.debug_assert_gpu_thread("prepare_target");
        let size = (size.0.max(1), size.1.max(1));
        if size == self.surface {
            return Ok(false);
        }
        let old = self.persist.recreate(&mut self.device, size)?;
        self.queue.retire(self.last_index, old);
        self.surface = size;
        self.fresh = true;
        Ok(true)
    }

    /// 写入窗口 DPI（`root.frame.params.x`；只影响 geometry 通道）。
    pub(crate) fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
    }

    /// 改窗口背景色（GPU 线程）：写 persist 的 clear color（**linear**，不是 sRGB 编码）。
    ///
    /// 新 drawable **内部**的条带由下一次全量帧的 clear 填上；旧 drawable **之外**的区域由主线程
    /// 写的 layer `backgroundColor` 负责。只被窗口路径调用（`Renderer::set_background_color` 的
    /// 离屏分支是 no-op）。
    pub(crate) fn set_background_color(&mut self, linear: [f32; 4]) {
        self.debug_assert_gpu_thread("set_background_color");
        self.clear_color = linear;
    }

    /// 窗口路径：把一帧画进 persist 后整面 blit 到 `drawable`，登记完成通知并 `commit`（I2）。
    ///
    /// 调用方（GPU 循环）已经：取到 drawable、非阻塞拿到三槽上传槽、按 drawable 实际尺寸
    /// 调过 [`GpuRenderer::prepare_target`]、并构造好 `frame`。
    pub(crate) fn submit_frame(
        &mut self,
        request: &crate::render::frame::FrameRequest,
        drawable: &ProtocolObject<dyn CAMetalDrawable>,
        frame: &mut FramePlan,
        completed: impl Fn(Retained<ProtocolObject<dyn MTLCommandBuffer>>) + Send + 'static,
    ) -> Result<(crate::render::RenderStats, Vec<GpuTimerSample>), EngineError> {
        debug_assert!(
            on_gpu_thread(),
            "Metal window frames may only be submitted on the kytrix-metal thread"
        );
        if let Some(error) = self.device.lost_error() {
            return Err(error);
        }
        // DPI 与 surface 同帧过线：geometry 通道的顶点是逻辑像素，root uniform 的
        // `frame.params.x` 必须与这一帧的 surface 成对（contract §6）。
        self.scale = request.scale;
        let mut timings = FrameTimings::new();
        timings.reset(request.index);
        let (slot, command, stats) = self.record_frame(
            request.index,
            &request.payload,
            frame,
            Some(drawable),
            &mut timings,
        )?;
        // **在 commit 前登记呈现**（Apple 推荐的 Metal 生命周期）：drawable 在 GPU 完成时立即
        // 归还给 `CAMetalLayer`，而不是等某个同步等待之后。
        let mtl_drawable: &ProtocolObject<dyn objc2_metal::MTLDrawable> = drawable.as_ref();
        command.presentDrawable(mtl_drawable);
        let present_started = Instant::now();
        self.queue
            .submit(request.index, slot, &command, completed)?;
        self.fresh = false;
        frame.presented();
        self.last_index = request.index;
        timings.push(GpuTimerSample {
            section: Section::Present,
            cpu_ns: elapsed_ns(present_started),
            gpu_ns: 0,
        });
        debug_assert!(
            timings.samples().len() <= 3,
            "at most three sections per frame"
        );
        Ok((stats, timings.samples().to_vec()))
    }

    /// 离屏 / 快照：画进 persist 后**同步**等本帧完成（readback 的可读性前提，§4.5 下半段）。
    pub(crate) fn submit_snapshot(
        &mut self,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
    ) -> Result<crate::render::RenderStats, EngineError> {
        if let Some(error) = self.device.lost_error() {
            return Err(error);
        }
        timings.reset(index);
        let (slot, command, stats) = self.record_frame(index, payload, frame, None, timings)?;
        if let Some(sample) = self
            .queue
            .submit_blocking(index, slot, &command, &mut self.device)?
        {
            timings.push(sample);
        }
        let present_started = Instant::now();
        self.fresh = false;
        frame.presented();
        self.last_index = index;
        timings.push(GpuTimerSample {
            section: Section::Present,
            cpu_ns: elapsed_ns(present_started),
            gpu_ns: 0,
        });
        Ok(stats)
    }

    /// 完成通知到达（`kytrix-metal` 线程 drain）：失败分类 / GPU 时间 / retire（I4）。
    ///
    /// 第二个返回值是新的**连续完成前沿**（`CompletionOrder`）：调用方只在它推进时按提交
    /// 顺序发布反馈（完成回调可能乱序，见 `renderer::FeedbackQueue`）。
    pub(crate) fn on_frame_completed(
        &mut self,
        index: FrameIndex,
        buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    ) -> Result<(Option<GpuTimerSample>, Option<FrameIndex>), EngineError> {
        debug_assert!(
            on_gpu_thread(),
            "command buffer completions may only be drained on the kytrix-metal thread"
        );
        if buffer.status() == MTLCommandBufferStatus::Error {
            return Err(self.queue.failure(index, buffer, &mut self.device));
        }
        Ok(self.queue.on_completed(index, buffer, &mut self.device))
    }

    /// 读回持久 offscreen（快照；`submit_snapshot` 已保证本帧完成）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn readback(&mut self) -> Result<ReadbackImage, EngineError> {
        self.debug_assert_gpu_thread("readback");
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
        let payload = FramePayload::from_buffer(buffer);
        let mut timings = FrameTimings::new();
        let mut frame = FramePlan::new(Plan::Full, true);
        self.submit_snapshot(FrameIndex::from_raw(1), &payload, &mut frame, &mut timings)?;
        self.readback()
    }

    /// 图集上传入口（P09 的字形页与 P15 的图片页共用）。
    ///
    /// `pixels` 按**所有权**移动进来（`AtlasTextures` 的待上传槽直接持有它），避免槽 → 纹理
    /// 之间再复制一次整页（图片页 16 MiB）；与 Windows 的 `queue_atlas_upload` 同一约定。
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        page: u32,
        pixels: Vec<u8>,
        rows: crate::render::atlas::DirtyRows,
    ) {
        self.debug_assert_gpu_thread("queue_atlas_upload");
        self.atlas.queue_upload(kind, page, pixels, rows);
    }

    /// 该图集是否有一笔上传已入队、还没被命令缓冲消费（离屏目标的空闲判据）。
    pub(crate) fn atlas_pending(&self, kind: AtlasKind) -> bool {
        self.atlas.pending(kind)
    }

    /// 录制一帧（不呈现、不提交）：取槽 → 上传 → 图集 blit → persist 绘制 → 整面 blit。
    ///
    /// 返回本帧槽位与命令缓冲；调用方决定是异步 `submit` 还是同步 `submit_blocking`。
    fn record_frame(
        &mut self,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &FramePlan,
        drawable: Option<&ProtocolObject<dyn CAMetalDrawable>>,
        timings: &mut FrameTimings,
    ) -> Result<
        (
            crate::render::frame::FrameSlot,
            Retained<ProtocolObject<dyn MTLCommandBuffer>>,
            crate::render::RenderStats,
        ),
        EngineError,
    > {
        self.debug_assert_gpu_thread("record_frame");
        debug_assert_eq!(
            timings.frame(),
            index,
            "timings must be reset for this frame"
        );
        // **不要**按 `copy_plan()` 做局部拷贝：drawable 的内容跨帧无保证（结论 #4 / F-121）。
        debug_assert!(
            !matches!(frame.plan(), Plan::Partial(_))
                || frame.is_full()
                || frame.present_dirty().is_some(),
            "partial plans must carry their dirty set"
        );
        let frame_started = Instant::now();
        // 用 `try_acquire` 已经取到的那个槽：permit 与上传槽必须一一对应（P1-2.12）。
        //
        // 离屏 / 快照路径走 `submit_blocking`（每帧同步等完成），没有窗口路径的背压，
        // 因此那里不会预先取槽 —— 此时现场取一个即可：上一帧已经 `waitUntilCompleted`，
        // 该槽必然空闲。取不到说明三槽都在飞，属于状态破坏，按 `AppInit` 上报。
        let slot = match self.pending_slot.take() {
            Some(slot) => slot,
            None => self.queue.try_acquire().ok_or_else(|| EngineError::AppInit {
                detail: smol_str::format_smolstr!(
                    "record_frame could not acquire an upload slot (all {} slots busy on a synchronous path)",
                    crate::render::frame::MAX_FRAMES_IN_FLIGHT
                ),
            })?,
        };
        let command = self.queue.begin()?;
        let (instance_count, _clip_count) = self.upload.write(
            &mut self.device,
            &mut self.queue,
            index,
            slot.index,
            payload.instances(),
            payload.clips(),
        )?;
        timings.push(GpuTimerSample {
            section: Section::Upload,
            cpu_ns: elapsed_ns(frame_started),
            gpu_ns: 0,
        });

        let full = frame.is_full();
        let mut draw_calls = 0u32;
        // 几何换代（槽位复用）产生的旧缓冲：录制块结束后交给 retire 队列。
        let mut geometry_retired: Vec<GpuResourceHandle> = Vec::new();
        // 推进几何 registry 的帧号：`ensure` 记录 last_used，帧末 `retain_recent` 驱逐
        // 长期未绘制的几何（D-1：否则应用释放句柄后 GPU 缓冲永不回收）。
        self.geometry.begin_frame();
        // 页尺寸变化在编码 blit 之前落地：新纹理建好、旧纹理句柄按本帧序号 retire
        // （DEVIATIONS F-208）。纹理绑定是每帧 `setFragmentTexture` 现场写的，无需重写。
        let _rebuilt = self
            .atlas
            .prepare(&mut self.device, &mut self.queue, index)?;
        if self.atlas.has_pending() {
            let blit = command
                .blitCommandEncoder()
                .ok_or_else(|| init_error("blitCommandEncoder(atlas)", "returned nil"))?;
            // 每 in-flight 槽一块常驻 staging（DEVIATIONS F-212）：槽位复用由三槽 semaphore
            // 保证「上一帧已完成」，因此覆盖写安全。
            self.atlas.record_uploads(
                &mut self.device,
                &mut self.queue,
                index,
                slot.index,
                &blit,
            )?;
            blit.endEncoding();
        }
        {
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
            let [r, g, b, a] = self.clear_color;
            color.setClearColor(MTLClearColor {
                red: f64::from(r),
                green: f64::from(g),
                blue: f64::from(b),
                alpha: f64::from(a),
            });
            let encoder = command
                .renderCommandEncoderWithDescriptor(&pass)
                .ok_or_else(|| init_error("renderCommandEncoder", "returned nil"))?;
            let root = self.bind(&encoder, slot.index);
            // 逐 scissor 画：全帧 = 整面一次；局部 = 每个 damage 矩形一次。
            match full {
                true => {
                    encoder.setScissorRect(whole_scissor(self.surface));
                    draw_calls = draw_ops(
                        &encoder,
                        payload,
                        instance_count,
                        [0, 0, self.surface.0 as i32, self.surface.1 as i32],
                        root,
                        &self.pipeline,
                        &mut self.geometry,
                        &mut self.device,
                        &mut geometry_retired,
                    )?;
                }
                false => match frame.plan() {
                    Plan::Idle | Plan::Full => {}
                    Plan::Partial(damage) => {
                        for rect in damage.as_slice() {
                            // `request.surface` 与 drawable 允许 1 px 取整差：裁剪后可能没有
                            // 面积（`x == surface.0` 等）。0 面积的 scissor 是 Metal validation
                            // 的未定义边缘，直接跳过这个 damage 矩形。
                            let Some(scissor) = scissor_for(rect, self.surface) else {
                                continue;
                            };
                            encoder.setScissorRect(scissor);
                            draw_calls = draw_calls.saturating_add(draw_ops(
                                &encoder,
                                payload,
                                instance_count,
                                [rect.x0, rect.y0, rect.x1, rect.y1],
                                root,
                                &self.pipeline,
                                &mut self.geometry,
                                &mut self.device,
                                &mut geometry_retired,
                            )?);
                        }
                    }
                },
            }
            encoder.endEncoding();
        }
        // 几何换代的旧缓冲按当前帧序号入 retire 队列（等该帧完成后释放，I4）；同时驱逐
        // 长期未绘制的几何（它们的设备对象进同一个 retire 列表）。
        self.geometry.retain_recent(&mut geometry_retired);
        for handle in geometry_retired {
            self.queue.retire(self.last_index, handle);
        }
        if let Some(drawable) = drawable {
            let target = drawable.texture();
            let blit = command
                .blitCommandEncoder()
                .ok_or_else(|| init_error("blitCommandEncoder", "returned nil"))?;
            blit_persist_to(&blit, self.persist.texture(), &target, self.surface);
            blit.endEncoding();
        }
        Ok((
            slot,
            command,
            crate::render::RenderStats {
                draw_calls,
                instances: instance_count,
            },
        ))
    }

    fn bind(
        &self,
        encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        slot: usize,
    ) -> RootUniform {
        encoder.setRenderPipelineState(self.pipeline.quad());
        let (instances, clips) = self.upload.slot(slot);
        // `RootUniform` 用 `setVertexBytes`（≤ 4 KB）在**录制时复制**进命令缓冲：没有跨帧的
        // CPU 可写缓冲，因此在飞帧读到的永远是它自己那一帧的常量（D3D12 root constants /
        // Vulkan push constants 的等价物）。单个 shared buffer + 每帧 `write` 会让异步执行的
        // 旧帧读到新帧的 viewport / 尺寸。
        let root = root_uniform(self.surface, self.scale);
        // SAFETY: 缓冲/纹理都在本渲染器生命周期内；索引与 P07 的绑定表逐字一致；
        // `root` 在本次调用期间存活（`setVertexBytes` 复制它的字节）。
        unsafe {
            encoder.setVertexBytes_length_atIndex(
                NonNull::from(root.words()).cast::<c_void>(),
                ROOT_UNIFORM_BYTES,
                2,
            );
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
        root
    }
}

/// 持久 offscreen（唯一绘制目标；`BGRA8Unorm_sRGB` / Private）。
struct Persist {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    handle: GpuResourceHandle,
}

impl Persist {
    fn new(device: &mut Device, size: (u32, u32)) -> Result<Self, EngineError> {
        let texture = create_target(device, size)?;
        let handle = device.allocate(GpuObject::Texture(texture.clone()));
        Ok(Self { texture, handle })
    }

    /// 换一张新尺寸纹理，返回**旧句柄**（调用方把它交给 retire 队列，§4.3）。
    fn recreate(
        &mut self,
        device: &mut Device,
        size: (u32, u32),
    ) -> Result<GpuResourceHandle, EngineError> {
        let texture = create_target(device, size)?;
        let handle = device.allocate(GpuObject::Texture(texture.clone()));
        let old = std::mem::replace(&mut self.handle, handle);
        self.texture = texture;
        Ok(old)
    }

    fn texture(&self) -> &ProtocolObject<dyn MTLTexture> {
        &self.texture
    }
}

fn create_target(
    device: &Device,
    size: (u32, u32),
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, EngineError> {
    // SAFETY: 纯构造函数；尺寸至少 1（调用方已夹紧）。
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
    instances: Retained<ProtocolObject<dyn MTLBuffer>>,
    instance_handle: GpuResourceHandle,
    instance_capacity: u32,
    clips: Retained<ProtocolObject<dyn MTLBuffer>>,
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
        &Retained<ProtocolObject<dyn MTLBuffer>>,
        &Retained<ProtocolObject<dyn MTLBuffer>>,
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
) -> Result<Retained<ProtocolObject<dyn MTLBuffer>>, EngineError> {
    device
        .raw()
        .newBufferWithLength_options(bytes as usize, MTLResourceOptions::StorageModeShared)
        .ok_or_else(|| init_error("newBufferWithLength_options(upload)", "returned nil"))
}

fn write_upload(buffer: &ProtocolObject<dyn MTLBuffer>, bytes: &[u8]) {
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

/// 逐 geometry 的 draw uniform：在帧级常量之上覆盖 draw 块（与 Windows 同一实现）。
fn geometry_root(
    frame: RootUniform,
    draw: &crate::paint::instance::GeometryDrawGpu,
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

/// `RootUniform`（112 B）：`frame @0`、`draw @32`；布局由 `render::uniform::RootUniform` 唯一实现。
///
/// `setVertexBytes` 要求 CPU 侧数据可读，Metal 在录制时把它复制进命令缓冲；共享类型是
/// `#[repr(C)]` 的 `[f32; 28]`，16 B 对齐满足 `float4` 常量布局。
fn root_uniform(surface: (u32, u32), scale: f32) -> RootUniform {
    RootUniform::frame(surface.0 as f32, surface.1 as f32, scale)
}

fn draw_ops(
    encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
    payload: &FramePayload,
    instance_count: u32,
    scissor: [i32; 4],
    root: RootUniform,
    pipeline: &Pipeline,
    geometry: &mut GeometryRegistry,
    device: &mut Device,
    retired: &mut Vec<GpuResourceHandle>,
) -> Result<u32, EngineError> {
    let mut draw_calls = 0u32;
    // 局部刷新时跳过与 damage 不相交的 op（P1-2.1）：旧实现对每个 damage 矩形重放整份 op 列表。
    for op in payload.ops() {
        if !op.bounds().intersects(scissor) {
            continue;
        }
        match *op {
            FrameOp::Instances { start, len, .. } => {
                if len == 0 {
                    continue;
                }
                debug_assert!(start.saturating_add(len) <= instance_count);
                // 实例基址只经 root constant `draw.clip.y` 进 shader，硬件基址恒 0
                // （Metal 的 instance_id 会包含 baseInstance，与 D3D12 相反；统一走 uniform
                //  才能三平台一致，contract §6）。
                let uniform = root.with_instance_base(start);
                // SAFETY: 4 顶点 unit quad + 实例区间 [0, len)；段起点由 shader 侧承担。
                unsafe {
                    encoder.setRenderPipelineState(pipeline.quad());
                    encoder.setVertexBytes_length_atIndex(
                        NonNull::from(uniform.words()).cast::<c_void>(),
                        ROOT_UNIFORM_BYTES,
                        2,
                    );
                    encoder.drawPrimitives_vertexStart_vertexCount_instanceCount_baseInstance(
                        MTLPrimitiveType::TriangleStrip,
                        0usize,
                        4usize,
                        len as usize,
                        0usize,
                    );
                }
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
                let entry = geometry.ensure(device, draw.id, &draw.desc, retired)?;
                let uniform = geometry_root(root, draw, clip_index, clip_rect);
                // SAFETY: 顶点/索引缓冲由 `GeometryRegistry` 持有到本帧结束；
                // root uniform 由 `setVertexBytes` 在录制时复制。
                unsafe {
                    encoder.setRenderPipelineState(pipeline.geometry(entry.pipeline()));
                    encoder.setVertexBytes_length_atIndex(
                        NonNull::from(uniform.words()).cast::<c_void>(),
                        ROOT_UNIFORM_BYTES,
                        2,
                    );
                    encoder.setVertexBuffer_offset_atIndex(Some(entry.vertex()), 0, 0);
                    encoder.setFragmentBytes_length_atIndex(
                        NonNull::from(uniform.words()).cast::<c_void>(),
                        ROOT_UNIFORM_BYTES,
                        2,
                    );
                    encoder.setFragmentBuffer_offset_atIndex(Some(entry.index()), 0, 1);
                    encoder
                        .drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset(
                            metal_topology(entry.topology()),
                            entry.index_count() as usize,
                            metal_index_type(entry.index_type()),
                            entry.index(),
                            0,
                        );
                }
                draw_calls = draw_calls.saturating_add(1);
            }
        }
    }
    Ok(draw_calls)
}

/// 注册拓扑 → Metal 图元类型。
fn metal_topology(topology: crate::paint::geometry::Topology) -> MTLPrimitiveType {
    match topology {
        crate::paint::geometry::Topology::TriangleList => MTLPrimitiveType::Triangle,
        crate::paint::geometry::Topology::TriangleStrip => MTLPrimitiveType::TriangleStrip,
    }
}

/// 索引格式 → Metal 索引类型。
fn metal_index_type(kind: crate::paint::geometry::IndexType) -> MTLIndexType {
    match kind {
        crate::paint::geometry::IndexType::U16 => MTLIndexType::UInt16,
        crate::paint::geometry::IndexType::U32 => MTLIndexType::UInt32,
    }
}

/// 整面 blit（**每帧**，因为 drawable 内容跨帧无保证）。
fn blit_persist_to(
    blit: &ProtocolObject<dyn MTLBlitCommandEncoder>,
    persist: &ProtocolObject<dyn MTLTexture>,
    target: &ProtocolObject<dyn MTLTexture>,
    size: (u32, u32),
) {
    // SAFETY: 源/目标都是本渲染器独占的纹理；尺寸一致（persist 由 drawable 的实际尺寸决定）。
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

/// damage 矩形 → scissor，**裁剪到实际目标尺寸**；裁剪后没有面积时返回 `None`（调用点跳过）。
///
/// `request.surface` 与 drawable 之间允许 1 px 的 DPI 取整差（见 `renderer.rs` 的尺寸校验），
/// 因此 damage 矩形可能比实际目标（drawable 尺寸）多出边界像素。只夹 `width/height` 会把 `x/y`
/// 留在目标之外（`x == surface.0` 时 `width = 0`），越界或 0 面积的 `MTLScissorRect` 会被 Metal
/// validation 拒绝；这里先把起止点都夹到 `[0, surface]`，再判断面积。
fn scissor_for(rect: &crate::math::PhysicalRect, surface: (u32, u32)) -> Option<MTLScissorRect> {
    let limit_x = surface.0.min(i32::MAX as u32) as i32;
    let limit_y = surface.1.min(i32::MAX as u32) as i32;
    let x0 = rect.x0.clamp(0, limit_x);
    let y0 = rect.y0.clamp(0, limit_y);
    let x1 = rect.x1.clamp(0, limit_x);
    let y1 = rect.y1.clamp(0, limit_y);
    let width = x1.saturating_sub(x0) as u32;
    let height = y1.saturating_sub(y0) as u32;
    if width == 0 || height == 0 {
        return None;
    }
    Some(MTLScissorRect {
        x: x0 as usize,
        y: y0 as usize,
        width: width as usize,
        height: height as usize,
    })
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::{scissor_for, whole_scissor};
    use crate::math::PhysicalRect;

    #[test]
    fn damage_inside_the_surface_is_preserved() {
        let scissor = scissor_for(&PhysicalRect::new(4, 6, 40, 60), (100, 80)).expect("scissor");
        assert_eq!(scissor.x, 4);
        assert_eq!(scissor.y, 6);
        assert_eq!(scissor.width, 36);
        assert_eq!(scissor.height, 54);
    }

    /// Partial damage 与 1 px 容差的交界：damage 坐标在 `request.surface` 空间，drawable 可以比
    /// 它小 1 px（DPI 取整）。越界像素必须被裁掉，而不是产生越界 / 0 面积的 scissor。
    #[test]
    fn one_pixel_dpi_rounding_clips_damage_to_the_drawable() {
        // 请求 1400 宽，drawable 1399 宽；damage 横跨右边缘。
        let scissor =
            scissor_for(&PhysicalRect::new(1398, 10, 1401, 20), (1399, 1040)).expect("scissor");
        assert_eq!(scissor.x, 1398);
        assert_eq!(scissor.y, 10);
        assert_eq!(scissor.width, 1, "只保留落在 drawable 内的 1 px");
        assert_eq!(scissor.height, 10);
    }

    #[test]
    fn a_partial_damage_rect_past_the_edge_is_skipped_not_left_empty() {
        // `x == surface.0` 会裁剪出 0 宽矩形：不是合法 scissor，调用点必须跳过。
        let surface = (1399, 1040);
        assert!(
            scissor_for(&PhysicalRect::new(1399, 0, 1400, 1040), surface).is_none(),
            "整列都在 drawable 之外"
        );
        // 同一 damage 往左扩展 1 px 后仍可画：不能把整个矩形丢掉。
        let scissor =
            scissor_for(&PhysicalRect::new(1398, 0, 1400, 1040), surface).expect("scissor");
        assert_eq!((scissor.x, scissor.width), (1398, 1));
    }

    #[test]
    fn a_damage_rect_entirely_outside_the_surface_has_no_scissor() {
        let surface = (1400, 1040);
        for rect in [
            PhysicalRect::new(1400, 0, 1420, 10),
            PhysicalRect::new(-20, 0, 0, 10),
            PhysicalRect::new(0, 1040, 10, 1060),
            PhysicalRect::new(0, -20, 10, 0),
            PhysicalRect::new(2000, 2000, 2010, 2010),
        ] {
            assert!(
                scissor_for(&rect, surface).is_none(),
                "{rect:?} 在目标之外必须没有 scissor"
            );
        }
    }

    #[test]
    fn damage_straddling_the_origin_is_clamped_to_zero() {
        let scissor = scissor_for(&PhysicalRect::new(-8, -4, 6, 10), (100, 50)).expect("scissor");
        assert_eq!((scissor.x, scissor.y), (0, 0));
        assert_eq!((scissor.width, scissor.height), (6, 10));
    }

    #[test]
    fn the_full_frame_scissor_covers_the_target() {
        let scissor = whole_scissor((1400, 1040));
        assert_eq!((scissor.x, scissor.y), (0, 0));
        assert_eq!((scissor.width, scissor.height), (1400, 1040));
    }
}

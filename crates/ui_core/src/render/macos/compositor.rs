//! 帧编排：上传 → 逐 damage scissor 画进持久 offscreen → 把 persist 与 drawable 的差异 blit
//! 过去 → present（`design/async-render-pipeline.md` §5.2 / §5.6）。
//!
//! **本模块不取 drawable**：窗口路径的 drawable 由 `CAMetalDisplayLink` 的 update 提供，
//! `CAMetalLayer::nextDrawable` 已删除（§5.7）。离屏/快照路径不给 drawable，只画进 persist。
//!
//! **两条与 Windows 不同的硬约束**（F-121）：
//! 1. `FramePlan::copy_plan()` / `present_dirty()` 是 D3D12 专用：Metal 没有 dirty present 扩展。
//!    局部帧的 persist → drawable 同步按 damage 矩形逐块 blit（P2-6）；`plan()`（scissor）与
//!    `is_full()`（clear）仍是共用语义。
//! 2. 没有 barrier / 资源状态机：Metal 自动跟踪 hazard。
//!
//! **persist ↔ drawable 的同步契约**（P2-6）：`CAMetalLayer` 的 drawable 从内部纹理池轮换
//! （QuartzCore 头文件：池里的纹理要被复用，前提是上一个 `CAMetalDrawable` 已经释放），所以
//! 「这一帧拿到的是哪一张、它停在几帧前」只能按**纹理身份**追踪。每张 drawable 记下自己错过的
//! damage，present 前把它与 persist 的差异一次补齐；**任何无法证明同步的状态**（新纹理、
//! 全量帧、累计面积达到整面）都整面拷一次（见 [`DrawableSyncs`]）。
//!
//! 由此付的代价与前提：
//! - 局部帧拷的是「这张 drawable 错过的最近几帧 damage 的并集」，最坏约为**单帧 damage 的 3 倍**
//!   （池深 3），而不是整面；累计面积达到整面时退化为一次整面拷，因此**带宽上界不超过改动前**。
//! - 同一纹理对象在两次 present 之间保留上次写入的内容（池化复用的直接结果，不是 Apple 的显式
//!   承诺）。局部同步只在**同一纹理身份**上做：对象换代（resize / 图层换池）后一律整面拷。
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
        compositor::{DamageRects, FramePlan, MAX_DRAW_PASSES, Plan},
        damage::MAX_DAMAGE_RECTS,
        frame::{FramePayload, MAX_FRAMES_IN_FLIGHT},
        macos::{
            atlas::AtlasTextures,
            device::{Device, GpuObject, init_error, on_gpu_thread},
            geometry::GeometryRegistry,
            pipeline::Pipeline,
            queue::Queue,
            swapchain::MAXIMUM_DRAWABLE_COUNT,
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
    /// persist → drawable 的同步状态（按 drawable 纹理身份；离屏路径没有 drawable，保持空表）。
    drawable_syncs: DrawableSyncs,
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
            drawable_syncs: DrawableSyncs::new(),
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
        // 新尺寸的 drawable 是图层池里新建的纹理，旧表项持有的是旧尺寸纹理：同步状态整体作废
        // （否则旧纹理被表项强引用着不会释放，新纹理也会被当成「已同步」）。
        self.drawable_syncs.entries.clear();
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

    /// 窗口路径：把一帧画进 persist 后，把这张 drawable 落后于 persist 的部分 blit 过去，登记
    /// 完成通知并 `commit`（I2）。
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

    /// 录制一帧（不呈现、不提交）：取槽 → 上传 → 图集 blit → persist 绘制 → drawable 差异 blit。
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
        // 局部帧必须带非空 damage 集：drawable 的补拷集完全由它决定（`Plan::Partial` 负责携带；
        // 断言调用方不是用一个空的「假 Partial」绕过同步追踪）。
        debug_assert!(
            !matches!(frame.plan(), Plan::Partial(damage) if damage.is_empty()),
            "a partial plan must carry a non-empty dirty set"
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
                            let Some(scissor) = clipped_rect(rect, self.surface) else {
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
            // `present_dirty()` 就是**本帧要同步进 drawable 的 damage 集**（`None` = 整面：`fresh`、
            // `Plan::Full`、`Idle`）。非全量的 `Idle` 帧不会带 drawable 走到这里（`gpu_loop` 在提交前
            // 就拦下），归到整面只影响正确性方向（多拷）。
            let damage = match frame.present_dirty() {
                Some(damage) => FrameDamage::Partial(damage),
                None => FrameDamage::Full,
            };
            let copy = self.drawable_syncs.plan(&target, damage, self.surface);
            let blit = command
                .blitCommandEncoder()
                .ok_or_else(|| init_error("blitCommandEncoder", "returned nil"))?;
            match copy {
                DrawableCopy::Full => blit_persist_rect(
                    &blit,
                    self.persist.texture(),
                    &target,
                    whole_scissor(self.surface),
                ),
                DrawableCopy::Rects(rects) => {
                    for rect in rects.as_slice() {
                        // `request.surface` 与 drawable 允许 1 px 取整差：裁剪后没有面积的矩形
                        // 落在目标之外，跳过（与渲染 scissor 同一条裁剪规则）。
                        let Some(region) = clipped_rect(rect, self.surface) else {
                            continue;
                        };
                        blit_persist_rect(&blit, self.persist.texture(), &target, region);
                    }
                }
            }
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

/// drawable 同步表容量 = 图层池深度：同时被表项强引用着的纹理数与 `CAMetalLayer` 自己的池相同，
/// 不会多占显存；表满时淘汰最久未用的条目（它下次出现时按新纹理整面拷）。
const DRAWABLE_SYNC_SLOTS: usize = MAXIMUM_DRAWABLE_COUNT;

/// 本帧对 persist 的改动（`FramePlan` 的像素语义）。
#[derive(Clone, Copy, Debug)]
enum FrameDamage {
    /// 全量帧：persist 的每个像素都可能变了。
    Full,
    /// 局部帧：只有这些矩形变了（`Plan::Partial` 的 damage 集）。
    Partial(DamageRects),
}

/// 一帧要往当前 drawable 拷的范围。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DrawableCopy {
    /// 整面：这张 drawable 的内容无法证明与 persist 一致。
    Full,
    /// 只拷这些矩形：这张 drawable 自上次同步以来 persist 变化过的区域并集。
    Rects(DamageRects),
}

/// 一张 drawable 纹理落后 persist 多少。
#[derive(Clone, Copy, Debug)]
struct DrawableSyncState {
    /// 自上次同步以来错过的 damage（本帧 present 它时必须补拷）。
    pending: DamageRects,
    /// 自上次同步以来出现过全量帧 / 内容从未同步过：整面不可信。
    full: bool,
}

impl DrawableSyncState {
    /// 刚插入表项的状态（纹理第一次出现）：内容从未与 persist 同步过，必须整面拷。
    const fn unsynced() -> Self {
        Self {
            pending: DamageRects::new(),
            full: true,
        }
    }

    /// 本帧 present 这张 drawable：返回要补拷的范围，并把状态推进到「内容 = persist」。
    ///
    /// 补拷集是「它错过的 `pending`」∪「本帧 damage」——两者都是 persist 的变化，并集就是这张
    /// drawable 与当前 persist 的全部差异；拷完之后它的每个像素都与 persist 相等，因此后续帧
    /// 只需继续记录新的变化（`record_missed`）。
    fn take_copy(&mut self, frame: FrameDamage, surface: (u32, u32)) -> DrawableCopy {
        let copy = match frame {
            // 全量帧：整面都变过，逐矩形没有意义。
            FrameDamage::Full => DrawableCopy::Full,
            // 期间出现过全量帧：累计矩形没有覆盖整面，内容不可信。
            FrameDamage::Partial(_) if self.full => DrawableCopy::Full,
            FrameDamage::Partial(damage) => {
                let mut rects = self.pending;
                merge_damage(&mut rects, &damage);
                // 裁剪后累计面积达到整面：矩形之间可能重叠，一次整面拷反而更便宜。
                let surface_area = u64::from(surface.0) * u64::from(surface.1);
                if copy_area(&rects, surface) >= surface_area {
                    DrawableCopy::Full
                } else {
                    DrawableCopy::Rects(rects)
                }
            }
        };
        *self = Self {
            pending: DamageRects::new(),
            full: false,
        };
        copy
    }

    /// 本帧没有 present 这张 drawable：把它又错过的改动记下来。
    fn record_missed(&mut self, frame: FrameDamage) {
        match frame {
            FrameDamage::Full => {
                self.full = true;
                self.pending = DamageRects::new();
            }
            // 已经整面不可信时累计矩形没有意义（下次反正整面拷）。
            FrameDamage::Partial(damage) if !self.full => merge_damage(&mut self.pending, &damage),
            FrameDamage::Partial(_) => {}
        }
    }
}

/// 把 `damage` 并入累计集。容量不够时先按绘制预算合并腾空位（与 `Plan::merge` 同一套规则）。
fn merge_damage(pending: &mut DamageRects, damage: &DamageRects) {
    for rect in damage.as_slice() {
        if pending.push(*rect) {
            continue;
        }
        // 单帧 damage 至多 `MAX_DRAW_PASSES` 个（`plan` / `Plan::merge` 都合并到这个上限），
        // 合并到该值以下必然腾出足够空位放下本帧剩余的矩形。
        pending.coalesce(MAX_DAMAGE_RECTS - MAX_DRAW_PASSES);
        assert!(
            pending.push(*rect),
            "a frame's damage set exceeds the draw-pass budget"
        );
    }
}

/// 裁剪到目标后各矩形面积之和（重叠部分重复计）：这次 blit 实际要付的像素带宽。
fn copy_area(rects: &DamageRects, surface: (u32, u32)) -> u64 {
    rects
        .as_slice()
        .iter()
        .filter_map(|rect| clipped_rect(rect, surface))
        .map(|rect| rect.width as u64 * rect.height as u64)
        .sum()
}

/// 按 drawable 纹理身份追踪 persist 同步状态的固定容量表（只有窗口路径有内容）。
///
/// `CAMetalLayer` 从内部纹理池里按需交出 drawable（3 张轮换），下一帧拿到的纹理通常停在 2~3
/// 帧前：只拷「本帧 damage」会漏掉它错过的那几帧，屏幕上会闪回旧内容。这里按身份累计每张
/// drawable 的 [`DrawableSyncState::pending`]，present 前一次补齐；纹理第一次出现或表项被淘汰
/// 时按新纹理整面拷（正确性优先于命中率）。
struct DrawableSyncs {
    /// 顺序 = 最近使用（末尾最新）。
    entries: Vec<DrawableEntry>,
}

struct DrawableEntry {
    /// 纹理身份。**持有强引用**：表项存活期间这个对象不会被释放，地址也就不可能被新纹理复用
    /// （否则「同址新纹理」会被误判成已同步而留下残影）。
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    state: DrawableSyncState,
}

impl DrawableSyncs {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(DRAWABLE_SYNC_SLOTS),
        }
    }

    /// 本帧 present 的 drawable：返回补拷范围，把它推进到「已同步」，并给其它 drawable 记账。
    fn plan(
        &mut self,
        texture: &Retained<ProtocolObject<dyn MTLTexture>>,
        frame: FrameDamage,
        surface: (u32, u32),
    ) -> DrawableCopy {
        // persist 与 drawable 同尺寸是本模块的同步前提（`prepare_target` 用 drawable 的实际尺寸
        // 重建 persist）：表里的矩形都在这个坐标系里，尺寸一变表就整体作废。
        debug_assert_eq!(
            (texture.width(), texture.height()),
            (surface.0 as usize, surface.1 as usize),
            "persist and drawable must share one size"
        );
        let current = match self
            .entries
            .iter()
            .position(|entry| Retained::as_ptr(&entry.texture) == Retained::as_ptr(texture))
        {
            Some(index) => {
                // LRU：刚用过的移到队尾。
                if index + 1 != self.entries.len() {
                    let entry = self.entries.remove(index);
                    self.entries.push(entry);
                }
                self.entries.len() - 1
            }
            None => {
                if self.entries.len() == DRAWABLE_SYNC_SLOTS {
                    self.entries.remove(0);
                }
                self.entries.push(DrawableEntry {
                    texture: texture.clone(),
                    state: DrawableSyncState::unsynced(),
                });
                self.entries.len() - 1
            }
        };
        let copy = self.entries[current].state.take_copy(frame, surface);
        for (index, entry) in self.entries.iter_mut().enumerate() {
            if index != current {
                entry.state.record_missed(frame);
            }
        }
        copy
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
                    // **不要**在这里绑 fragment buffer 1：那个槽是 `bind()` 绑的 clips 表，
                    // `geometry.metal` 的三个 fragment 入口都声明
                    // `device const ClipEntry* clips [[buffer(1)]]`。索引缓冲只通过
                    // `drawIndexedPrimitives` 的参数消费，不需要 `setFragmentBuffer`——
                    // 误绑会让本帧之后所有带 clip 的绘制从索引缓冲里读 `ClipEntry`（越界读
                    // + clip 掩码全乱）。
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

/// 把 persist 的 `region` 逐像素 blit 到 drawable 的同名区域。
///
/// `region` 由 [`clipped_rect`] 夹到实际目标尺寸内；整面同步传 [`whole_scissor`]。
///
/// **为什么选 blit 而不是「用 render pass 重画 damage 矩形」**：persist 里存的是图元绘制结果，
/// 重画要重新绑定 PSO / 顶点流，而且除了 `plan()` 的 scissor 之外没有「只画一部分」的语义可复用；
/// 而 `copyFromTexture:...` 在源 / 目标同为同尺寸 `BGRA8Unorm_sRGB` 时是**纯拷贝**：不做颜色
/// 空间转换（编码由 sRGB 格式本身决定）、不翻转坐标、也不需要 scissor 对齐。也就是说整面 blit
/// 除了「搬 persist 的像素」不承担任何额外职责，局部化不会破坏别的东西。
///
/// 每帧最多 [`MAX_DAMAGE_RECTS`] 次拷贝（累计矩形由 [`merge_damage`] 截到该上限）；面积上界是
/// 一次整面拷 —— [`copy_area`] 达到整面时直接退化为 [`DrawableCopy::Full`]。
fn blit_persist_rect(
    blit: &ProtocolObject<dyn MTLBlitCommandEncoder>,
    persist: &ProtocolObject<dyn MTLTexture>,
    target: &ProtocolObject<dyn MTLTexture>,
    region: MTLScissorRect,
) {
    let origin = MTLOrigin {
        x: region.x,
        y: region.y,
        z: 0,
    };
    let size = MTLSize {
        width: region.width,
        height: region.height,
        depth: 1,
    };
    // SAFETY: 两张纹理同尺寸、同像素格式（persist 由 drawable 的实际尺寸创建）；`region` 已夹到
    // 实际尺寸内，源与目标使用同一区域。
    unsafe {
        blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
            persist, 0, 0, origin, size, target, 0, 0, origin,
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

/// damage 矩形 → 目标上的整数像素矩形，**裁剪到实际目标尺寸**；裁剪后没有面积时返回 `None`
/// （调用点跳过）。
///
/// 渲染 scissor 与 persist → drawable 的 blit 区域共用这一条裁剪规则：`request.surface` 与
/// drawable 之间允许 1 px 的 DPI 取整差（见 `renderer.rs` 的尺寸校验），因此 damage 矩形可能比
/// 实际目标（drawable 尺寸）多出边界像素。只夹 `width/height` 会把 `x/y` 留在目标之外
/// （`x == surface.0` 时 `width = 0`），越界或 0 面积的 `MTLScissorRect` 会被 Metal validation
/// 拒绝、0 面积的 blit region 同样非法；这里先把起止点都夹到 `[0, surface]`，再判断面积。
fn clipped_rect(rect: &crate::math::PhysicalRect, surface: (u32, u32)) -> Option<MTLScissorRect> {
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
    use super::{
        DrawableCopy, DrawableSyncState, FrameDamage, clipped_rect, merge_damage, whole_scissor,
    };
    use crate::{
        math::PhysicalRect,
        render::{
            compositor::{DamageRects, MAX_DRAW_PASSES},
            damage::MAX_DAMAGE_RECTS,
        },
    };

    fn damage(rects: &[(i32, i32, i32, i32)]) -> DamageRects {
        let mut set = DamageRects::new();
        for &(x0, y0, x1, y1) in rects {
            assert!(set.push(PhysicalRect::new(x0, y0, x1, y1)));
        }
        set
    }

    /// 三张 drawable 轮换的一次 present：当前 drawable 取补拷集，其它 drawable 记下错过的变化
    /// （与 `DrawableSyncs::plan` 的记账顺序一致，只是省略了纹理身份查表）。
    fn present(
        states: &mut [DrawableSyncState; 3],
        current: usize,
        frame: FrameDamage,
        surface: (u32, u32),
    ) -> DrawableCopy {
        let copy = states[current].take_copy(frame, surface);
        for (index, state) in states.iter_mut().enumerate() {
            if index != current {
                state.record_missed(frame);
            }
        }
        copy
    }

    #[test]
    fn damage_inside_the_surface_is_preserved() {
        let rect = clipped_rect(&PhysicalRect::new(4, 6, 40, 60), (100, 80)).expect("rect");
        assert_eq!(rect.x, 4);
        assert_eq!(rect.y, 6);
        assert_eq!(rect.width, 36);
        assert_eq!(rect.height, 54);
    }

    /// Partial damage 与 1 px 容差的交界：damage 坐标在 `request.surface` 空间，drawable 可以比
    /// 它小 1 px（DPI 取整）。越界像素必须被裁掉，而不是产生越界 / 0 面积的区域。
    #[test]
    fn one_pixel_dpi_rounding_clips_damage_to_the_drawable() {
        // 请求 1400 宽，drawable 1399 宽；damage 横跨右边缘。
        let rect =
            clipped_rect(&PhysicalRect::new(1398, 10, 1401, 20), (1399, 1040)).expect("rect");
        assert_eq!(rect.x, 1398);
        assert_eq!(rect.y, 10);
        assert_eq!(rect.width, 1, "只保留落在 drawable 内的 1 px");
        assert_eq!(rect.height, 10);
    }

    #[test]
    fn a_partial_damage_rect_past_the_edge_is_skipped_not_left_empty() {
        // `x == surface.0` 会裁剪出 0 宽矩形：不是合法 scissor / blit 区域，调用点必须跳过。
        let surface = (1399, 1040);
        assert!(
            clipped_rect(&PhysicalRect::new(1399, 0, 1400, 1040), surface).is_none(),
            "整列都在 drawable 之外"
        );
        // 同一 damage 往左扩展 1 px 后仍可画：不能把整个矩形丢掉。
        let rect = clipped_rect(&PhysicalRect::new(1398, 0, 1400, 1040), surface).expect("rect");
        assert_eq!((rect.x, rect.width), (1398, 1));
    }

    #[test]
    fn a_damage_rect_entirely_outside_the_surface_has_no_region() {
        let surface = (1400, 1040);
        for rect in [
            PhysicalRect::new(1400, 0, 1420, 10),
            PhysicalRect::new(-20, 0, 0, 10),
            PhysicalRect::new(0, 1040, 10, 1060),
            PhysicalRect::new(0, -20, 10, 0),
            PhysicalRect::new(2000, 2000, 2010, 2010),
        ] {
            assert!(
                clipped_rect(&rect, surface).is_none(),
                "{rect:?} 在目标之外必须没有区域"
            );
        }
    }

    #[test]
    fn damage_straddling_the_origin_is_clamped_to_zero() {
        let rect = clipped_rect(&PhysicalRect::new(-8, -4, 6, 10), (100, 50)).expect("rect");
        assert_eq!((rect.x, rect.y), (0, 0));
        assert_eq!((rect.width, rect.height), (6, 10));
    }

    #[test]
    fn the_full_frame_region_covers_the_target() {
        let rect = whole_scissor((1400, 1040));
        assert_eq!((rect.x, rect.y), (0, 0));
        assert_eq!((rect.width, rect.height), (1400, 1040));
    }

    /// 新 drawable（图层刚建 / resize 后 / 表项被淘汰）的内容不可信：整面拷。
    #[test]
    fn an_unsynced_drawable_copies_the_whole_surface() {
        let surface = (100, 80);
        let mut state = DrawableSyncState::unsynced();
        assert_eq!(
            state.take_copy(FrameDamage::Partial(damage(&[(10, 10, 20, 20)])), surface),
            DrawableCopy::Full
        );
        assert!(!state.full, "整面拷之后内容已与 persist 一致");
        assert!(state.pending.is_empty(), "补拷集已被消费");
    }

    /// 已同步的 drawable 只补拷本帧 damage：局部更新只付局部带宽。
    #[test]
    fn a_synced_drawable_only_copies_this_frames_damage() {
        let surface = (100, 80);
        let mut state = DrawableSyncState {
            pending: DamageRects::new(),
            full: false,
        };
        let current = damage(&[(10, 10, 20, 20)]);
        assert_eq!(
            state.take_copy(FrameDamage::Partial(current), surface),
            DrawableCopy::Rects(current)
        );
    }

    /// 三张 drawable 轮换：第 4 帧轮到 A 时，A 的内容停在它上次 present 的时候，必须补上它错过
    /// 的第 2、3 帧 damage，否则屏幕上会闪回旧内容（残影）。
    #[test]
    fn a_rotating_drawable_catches_up_every_damage_it_missed() {
        let surface = (100, 80);
        let mut states = [DrawableSyncState::unsynced(); 3];
        // 第 1 帧：全量帧（fresh）present 给 A，B/C 记下整面不可信。
        assert_eq!(
            present(&mut states, 0, FrameDamage::Full, surface),
            DrawableCopy::Full
        );
        // 第 2、3 帧：局部帧分别 present 给 B、C（它们的旧内容不可信，各自整面拷）。
        let second = damage(&[(0, 0, 10, 10)]);
        assert_eq!(
            present(&mut states, 1, FrameDamage::Partial(second), surface),
            DrawableCopy::Full
        );
        let third = damage(&[(10, 0, 20, 10)]);
        assert_eq!(
            present(&mut states, 2, FrameDamage::Partial(third), surface),
            DrawableCopy::Full
        );
        // 第 4 帧：又轮到 A。它错过了第 2、3 帧的 damage，本次补拷「错过 ∪ 本帧」。
        let fourth = damage(&[(20, 0, 30, 10)]);
        let copy = present(&mut states, 0, FrameDamage::Partial(fourth), surface);
        assert_eq!(
            copy,
            DrawableCopy::Rects(damage(&[(0, 0, 10, 10), (10, 0, 20, 10), (20, 0, 30, 10),]))
        );
        // 第 5 帧：A 上一次已经补齐，只需要本帧 damage。
        let fifth = damage(&[(30, 0, 40, 10)]);
        assert_eq!(
            present(&mut states, 0, FrameDamage::Partial(fifth), surface),
            DrawableCopy::Rects(fifth)
        );
    }

    /// 期间出现过全量帧（`fresh` / `Plan::Full` / resize 退化）：其它 drawable 的旧内容再也不可能
    /// 靠局部矩形补齐，下次 present 必须整面拷。
    #[test]
    fn a_full_frame_forces_every_other_drawable_to_copy_the_whole_surface() {
        let surface = (100, 80);
        let mut states = [DrawableSyncState::unsynced(); 3];
        // 第 1 帧 A 整面同步；B/C 记下整面不可信。
        assert_eq!(
            present(&mut states, 0, FrameDamage::Full, surface),
            DrawableCopy::Full
        );
        // 第 2 帧 B（旧内容不可信）整面拷，A 记下本帧 damage。
        let first = damage(&[(0, 0, 10, 10)]);
        assert_eq!(
            present(&mut states, 1, FrameDamage::Partial(first), surface),
            DrawableCopy::Full
        );
        // 第 3 帧又是 A：只补它错过的 `first` 与本帧 damage。
        let second = damage(&[(10, 0, 20, 10)]);
        assert_eq!(
            present(&mut states, 0, FrameDamage::Partial(second), surface),
            DrawableCopy::Rects(damage(&[(0, 0, 10, 10), (10, 0, 20, 10)]))
        );
        // 全量帧 present 给 C：A、B 的内容都落后了整整一帧的全屏改动。
        assert_eq!(
            present(&mut states, 2, FrameDamage::Full, surface),
            DrawableCopy::Full
        );
        for current in [0, 1] {
            assert_eq!(
                present(
                    &mut states,
                    current,
                    FrameDamage::Partial(damage(&[(5, 5, 6, 6)])),
                    surface
                ),
                DrawableCopy::Full
            );
        }
    }

    /// 累计矩形的总面积达到整面：矩形之间可能重叠，一次整面拷比逐块更便宜，也是带宽上界。
    #[test]
    fn accumulated_damage_covering_the_whole_surface_falls_back_to_a_full_copy() {
        let surface = (100, 80);
        let mut state = DrawableSyncState::unsynced();
        assert_eq!(
            state.take_copy(FrameDamage::Full, surface),
            DrawableCopy::Full
        );
        // 错过上半屏 60%，本帧 damage 又是下半屏 60%：总面积 > 整面。
        state.record_missed(FrameDamage::Partial(damage(&[(0, 0, 100, 48)])));
        assert_eq!(
            state.take_copy(FrameDamage::Partial(damage(&[(0, 32, 100, 80)])), surface),
            DrawableCopy::Full
        );
    }

    /// 一张 drawable 可能连续错过很多帧：累计集必须被压回 `MAX_DAMAGE_RECTS` 以内，不能无界增长。
    #[test]
    fn missed_damage_stays_within_the_fixed_rect_budget() {
        let surface = (1000, 1000);
        let mut state = DrawableSyncState::unsynced();
        assert_eq!(
            state.take_copy(FrameDamage::Full, surface),
            DrawableCopy::Full
        );
        // `MAX_DAMAGE_RECTS` 帧、每帧 `MAX_DRAW_PASSES` 个互不重叠的小矩形。
        for frame in 0..MAX_DAMAGE_RECTS {
            let mut frame_damage = DamageRects::new();
            for pass in 0..MAX_DRAW_PASSES {
                let x = ((frame * MAX_DRAW_PASSES + pass) * 2) as i32;
                assert!(frame_damage.push(PhysicalRect::new(x, 0, x + 1, 1)));
            }
            state.record_missed(FrameDamage::Partial(frame_damage));
            assert!(
                state.pending.as_slice().len() <= MAX_DAMAGE_RECTS,
                "累计集不能超过固定容量"
            );
        }
        match state.take_copy(
            FrameDamage::Partial(damage(&[(900, 900, 901, 901)])),
            surface,
        ) {
            DrawableCopy::Rects(rects) => {
                assert!(rects.as_slice().len() <= MAX_DAMAGE_RECTS, "补拷矩形数有界");
            }
            other => panic!("expected a bounded rect copy, got {other:?}"),
        }
    }

    /// 容量满时先合并再追加：被合并的矩形必须**覆盖**原 damage（不能丢），否则 drawable 会漏拷。
    #[test]
    fn merging_into_a_full_set_keeps_every_rect_covered() {
        let mut pending = DamageRects::new();
        let mut originals = Vec::with_capacity(MAX_DAMAGE_RECTS + 2);
        for index in 0..MAX_DAMAGE_RECTS {
            let x = index as i32 * 2;
            let rect = PhysicalRect::new(x, 0, x + 1, 1);
            assert!(pending.push(rect));
            originals.push(rect);
        }
        let extra = damage(&[(100, 100, 101, 101), (102, 100, 103, 101)]);
        originals.extend(extra.as_slice());
        merge_damage(&mut pending, &extra);
        assert!(pending.as_slice().len() <= MAX_DAMAGE_RECTS);
        for rect in originals {
            assert!(
                pending.as_slice().iter().any(|union| {
                    union.x0 <= rect.x0
                        && union.y0 <= rect.y0
                        && union.x1 >= rect.x1
                        && union.y1 >= rect.y1
                }),
                "{rect:?} 必须被某个累计矩形覆盖"
            );
        }
    }
}

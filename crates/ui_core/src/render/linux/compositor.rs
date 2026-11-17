//! 帧编排：上传 → 逐 damage scissor 画进持久 offscreen → **差分 copy** 到 swapchain → present
//! （`design/phase-12-macos-linux.md` §6.6 / §6.9、`design/async-render-pipeline.md` §7）。
//!
//! **两条硬约束**：
//! 1. swapchain image 的内容在 present 之后对应用不可信 ⇒ 每张 image 相对 persist 还差哪些矩形
//!    必须**记账**（[`ImageCopyPlan`]，与 D3D12 的 `copy_plan` / Metal 的 `DrawableSyncs` 同构）。
//!    旧实现每帧整面 `cmd_copy_image`（1080p@120Hz ≈ 1.0 GB/s，4K ≈ 4 GB/s 纯带宽），把
//!    damage/scissor 的收益全部抵消（R11-12）。`present_dirty()` 现在是**实际拷贝区域**，
//!    同时作为 `VK_KHR_incremental_present` 的 hint —— 两者必须一致。
//! 2. 没有自动 barrier：本文件是 barrier 的**唯一**写点（表见 §6.6）。
//!
//! 所有权（§3.1 / P1-2.9）：[`GpuRenderer`] 是一个**窗口**的渲染器（GPU 线程独占）。进程级共享
//! 资源（instance / device / queue / PSO / 描述符池 / 图集纹理 / 内存池）由 [`SharedGpu`] 持有；
//! 本结构只保留每窗口对象：`VkSurfaceKHR` / swapchain / 持久目标 / 帧同步（fence、
//! `image_available`、自己的 timeline）/ 命令池 / 上传缓冲 / 几何缓冲 / 图集 staging / 描述符段。
//! 构造发生在窗口创建时（UI 线程），构造完成后整体移交给 `kytrix-vulkan` 线程
//! （[`super::renderer`]）。离屏 / 快照路径不走线程：门面在 UI 线程同步调用。
//!
//! **timeline 值只有一个来源**（R11-1）：`slot.signal_value`（ring 的提交序号）。它同时是
//! submit 时 signal 的值、retire 队列的键、teardown 的等待值。`FrameIndex`（UI 帧序号）会跳号
//! （`Idle` 帧 / 最新帧槽合并 / `Retry`），只作为反馈与诊断标签。
//!
//! 异步（I1 / I2 / I4）：`vkQueueSubmit` 后立即返回；`resize` 不再等设备空闲（只用
//! `oldSwapchain` 建新的，旧资源按提交序号延迟释放）；retire 由本窗口 timeline 非阻塞推进。
//! 阻塞只发生在 GPU 线程上：槽 fence（有界 2 s）、`vkAcquireNextImageKHR`（100 ms 上限）、
//! 以及 swapchain 重建后的队列 idle（有界 2 s，R11-3）。
//!
//! 共享图集的并发契约（多窗口）：**读锁**覆盖「页对齐 → 录制 → 提交」整个区间（页在帧内不会
//! 变）；待上传页与共享页不一致时先释放读锁，在 `SharedGpu::rebuild_atlas_pages` 的**写锁**里
//! 落地页换代，然后重试。多个窗口同时往同一张纹理上传因此是安全的（同一队列按提交顺序执行，
//! 屏障序列不交错）。

use std::{collections::VecDeque, sync::Arc, time::Instant};

use ash::vk;
use wayland_client::protocol::wl_surface;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

#[cfg(feature = "test-support")]
use super::readback::{Readback, ReadbackImage};
use super::{
    atlas::AtlasUploads,
    device::{DescriptorLease, FrameSync, GpuBuffer, SharedGpu, init_error, on_gpu_thread},
    geometry::GeometryRegistry,
    memory::MemoryNeed,
    pipeline::{Pipeline, RENDER_FORMAT},
    swapchain::{Acquire, Present, Surface, Swapchain},
    timer::GpuTimer,
};
use crate::{
    error::EngineError,
    math::Size,
    paint::{atlas::AtlasKind, instance::FrameOp},
    platform::linux::dpi,
    render::{
        RenderStats,
        compositor::{CopyPlan, DamageRects, FramePlan, ImageCopyPlan, Plan},
        frame::{FramePayload, FrameRequest, FrameSlot, InstanceMirror, MAX_FRAMES_IN_FLIGHT},
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample, Section},
        uniform::RootUniform,
    },
    window::FrameIndex,
};

/// `minStorageBufferOffsetAlignment` 的保守上界（256 B）：clips 段在此偏移处开始。
const CLIP_ALIGN: u64 = 256;

/// 连续 `OUT_OF_DATE` / `SUBOPTIMAL` 的重建上限：正常合成器一次重建就能恢复；超过说明 surface
/// 已失效或参数始终不匹配（R11-7 的「每帧重建自旋」）。
const MAX_REBUILDS: u32 = 4;

/// 重建之后至少多少帧不再重建（即使 acquire / present 仍报 `SUBOPTIMAL`）。
///
/// `VK_SUBOPTIMAL_KHR` 可以**持续**返回（extent / transform / colorSpace 与合成器期望不符），
/// 而每次重建都要 `vkCreateSwapchainKHR` + `vkGetSwapchainImagesKHR` + N 个信号量。没有这个门槛
/// 时，持续 SUBOPTIMAL 会退化成每帧重建：帧率崩塌 + 句柄/显存抖动，现象像「卡顿」而不是错误。
const REBUILD_COOLDOWN_FRAMES: u64 = 60;

/// 一帧异步提交的结果（窗口路径）。
pub(crate) enum Submitted {
    /// 已提交（可能已呈现）；`stats` / `samples` 是本帧的，`slot` 是提交用的 ring 槽位
    /// （完成观测点据此 resolve GPU 采样），`sequence` 是本帧 submit 时 signal 的 timeline
    /// 值（完成判据，§4.1）。
    Presented {
        stats: RenderStats,
        samples: Vec<GpuTimerSample>,
        slot: usize,
        sequence: u64,
    },
    /// 本帧没有产生 GPU 工作（acquire 未就绪 / 刚重建 swapchain）：调用方保留帧请求重试。
    Retry,
}

/// 已提交、尚未被 timeline 确认完成的帧的**提交序号**（timeline 的 signal 值 = ring 的提交序号）。
///
/// UI 帧序号（`FrameIndex`）不参与完成判据：它只随反馈 / 诊断走（`renderer::SubmittedFrame`），
/// 放在这里会诱使后来者拿它去比 timeline 值 —— 那正是 R11-1 的成因。
type InFlightSequence = u64;

/// 一个窗口的 GPU 渲染器（**GPU 线程独占**；离屏路径由门面同步调用）。
pub(crate) struct GpuRenderer {
    /// 真实窗口才有 surface / swapchain；离屏（快照）渲染器只有 `persist`。
    ///
    /// 字段顺序即销毁顺序：**swapchain 必须先于 surface 销毁**（Vulkan 要求），两者又都先于
    /// 最后一个 `Arc<SharedGpu>` 的 device / instance。
    swapchain: Option<Swapchain>,
    vk_surface: Option<Surface>,
    /// 当前持久绘制目标（尺寸 = swapchain extent，格式 = `RENDER_FORMAT`）。
    persist: Persist,
    /// 旧 persist 的延迟销毁（I4）：`u64` 提交序号完成之后才 drop（`Persist::drop` 销毁句柄）。
    retired_persists: Vec<(u64, Persist)>,
    /// 旧 swapchain 的延迟销毁（§7.4 / R11-3）：销毁前必须有一次**队列 idle**（presentation
    /// engine 与在飞批次都可能仍引用它们）。
    retired_swapchains: Vec<Swapchain>,
    /// 本窗口在进程级描述符池里的段（`Drop` 归还）。
    descriptors: DescriptorLease,
    /// 每帧槽上一次写进 descriptor set 的 `(上传缓冲, clips 偏移)`（R11-10）。
    ///
    /// `vkUpdateDescriptorSets` 只在句柄真的变化时调用：上传缓冲仅在扩容时换句柄，而
    /// `vkUpdateDescriptorSets` 要被进程级池锁串行化、在 Mesa/NVIDIA 上会触发 descriptor cache
    /// 失效。旧实现每帧 2 次持锁写，120 Hz × 多窗口下是纯 CPU 开销。
    written_slots: [Option<(vk::Buffer, u64)>; MAX_FRAMES_IN_FLIGHT],
    /// 本窗口的图集上传路径（待上传页 + 每槽 staging）。
    atlas_uploads: AtlasUploads,
    upload: UploadArena,
    /// 实例缓冲的持久镜像（增量写：只写变化区间，见 `render/frame.rs` 的 `InstanceMirror`）。
    mirror: InstanceMirror,
    command_pool: vk::CommandPool,
    commands: [vk::CommandBuffer; MAX_FRAMES_IN_FLIGHT],
    timer: Option<GpuTimer>,
    /// 本窗口的帧同步原语（fence / `image_available` / retire timeline / ring）。
    sync: FrameSync,
    /// persist 的 clear color（linear；窗口背景色，离屏保持透明黑）。
    clear_color: [f32; 4],
    /// `wp_viewport`（`None` = 整数回退：没有 viewporter 或没有 fractional manager，
    /// 见 V-110）：destination 由 GPU 线程在 present **之前**写入，与 WSI 的
    /// `wl_surface.commit` 同一次生效（D1 / D2）。
    viewport: Option<WpViewport>,
    /// 最近一次真正写入 `wp_viewport` 的 destination（`None` = 还没写过）。只在变化时下发。
    last_destination: Option<(i32, i32)>,
    /// `wl_surface` 的代理副本（GPU 线程写 `set_buffer_scale` 用；`None` = 离屏路径）。
    ///
    /// 与 `viewport` 一样，窗口系统状态必须在 `vkQueuePresentKHR` **之前**写入，才能与 WSI 的
    /// attach/commit 同一次生效。
    wl_surface: Option<wl_surface::WlSurface>,
    /// 最近一次写入 `wl_surface.set_buffer_scale` 的值（初始 1 = 协议默认值）。
    ///
    /// fractional 模式恒为 1（destination 负责缩放）；整数回退是整数 scale `N`（V-110）。
    last_buffer_scale: i32,
    /// UI 请求的像素尺寸（GPU 线程据此判断是否要重建；渲染目标尺寸见 [`Self::extent`]）。
    surface: (u32, u32),
    /// 窗口 DPI（root.frame.params.x；geometry 通道的逻辑到物理换算）。
    scale: f32,
    /// 静态几何的 GPU 缓冲（按需创建；长期未绘制的条目按最近使用帧数驱逐）。
    geometry: GeometryRegistry,
    /// 下一次 present 是否必须全量：新建 / 重建之后为 `true`，present 成功后清除。
    fresh: bool,
    /// 最后一次**真正提交**的提交序号（重建资源入 retire 队列时的 tag，§4.3）。
    last_sequence: u64,
    /// 已提交、尚未被 timeline 确认完成的提交序号（**不含** UI 帧序号：完成判据只有一套值）。
    submitted: VecDeque<InFlightSequence>,
    /// timeline 已确认完成的最大提交序号（retire / 延迟销毁的推进点）。
    completed_sequence: u64,
    /// 每张 presentable image 的差分拷贝状态（R11-12）。
    image_copy_plan: ImageCopyPlan,
    /// 本帧实际拷贝进 presentable image 的矩形（`None` = 整面）。
    ///
    /// `copy_to_swapchain` 写入、`present` 读取：`VK_KHR_incremental_present` 的 region 必须与
    /// 实际搬运的像素一致（R11-12）。
    frame_copy: Option<DamageRects>,
    /// present 成功后需要重建 swapchain（`SUBOPTIMAL`，§7.3）。
    rebuild_swapchain: bool,
    /// 连续 `OUT_OF_DATE` 次数（每次成功 present 清零）：超上限才报错，不无限自旋。
    out_of_date: u32,
    /// 最近一段时间的 `SUBOPTIMAL` 重建次数与上次重建的提交序号（R11-7 的门槛）。
    suboptimal_rebuilds: u32,
    last_rebuild_sequence: u64,
    /// 已经放弃重建（`SUBOPTIMAL` 次数超上限）：只记一次诊断，继续用现有 swapchain 呈现。
    rebuild_abandoned: bool,
    /// 快照帧序号（离屏路径的一次性帧；timeline 值必须单调递增）。
    #[cfg(feature = "test-support")]
    snapshot_index: u32,
    #[cfg(feature = "test-support")]
    readback: Readback,
    // 共享 GPU 必须**最后**声明：Rust 按声明顺序 drop，先释放全部每窗口资源，再让最后一个
    // `Arc` 的 drop 销毁 device / instance（swapchain → surface → device → instance）。
    shared: Arc<SharedGpu>,
}

impl GpuRenderer {
    /// 真实窗口（Wayland surface 的裸指针由平台层提供，`surface` 是同一对象的代理副本）。
    ///
    /// `background` 是 persist 的 clear color（linear，`render::color::background_linear`）；
    /// `viewport` 是 `wp_viewport`（`None` = 整数回退，见 V-110）。`surface` 用来在 present 前写
    /// 整数回退的 `set_buffer_scale`（fractional 模式恒 1，等价于不调用）。
    pub(crate) fn new_for_surface(
        display: *mut std::ffi::c_void,
        surface_ptr: *mut std::ffi::c_void,
        size: (u32, u32),
        background: [f32; 4],
        viewport: Option<WpViewport>,
        surface: wl_surface::WlSurface,
    ) -> Result<Self, EngineError> {
        let size = (size.0.max(1), size.1.max(1));
        let shared = SharedGpu::process_windowed(display, surface_ptr)?;
        let vk_surface = Surface::new(&shared, display, surface_ptr)?;
        let swapchain = Swapchain::new(&shared, vk_surface.raw(), size)?;
        Self::with_gpu(
            shared,
            Some(vk_surface),
            Some(swapchain),
            size,
            background,
            viewport,
            Some(surface),
        )
    }

    /// 离屏渲染器（无窗口 / 快照）：不建 surface 与 swapchain，只渲染到持久 `persist`。
    ///
    /// Linux 的**产品**窗口路径必须有真实 `wl_surface`（F-137）；这里给的是「没有窗口也要画」
    /// 的路径（`Window::enable_offscreen_renderer` 与 `test-support` 的像素快照）。
    ///
    /// # Errors
    /// 实例 / 设备 / 管线 / 图集 / 持久目标任一创建失败。
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        let size = (size.0.max(1), size.1.max(1));
        let shared = SharedGpu::process_headless()?;
        // 离屏路径没有窗口背景色：保持既有的透明黑（golden / 快照不变）。
        Self::with_gpu(shared, None, None, size, [0.0; 4], None, None)
    }

    /// 每窗口资源的构造（窗口 / 离屏共用）。
    fn with_gpu(
        shared: Arc<SharedGpu>,
        vk_surface: Option<Surface>,
        swapchain: Option<Swapchain>,
        size: (u32, u32),
        clear_color: [f32; 4],
        viewport: Option<WpViewport>,
        wl_surface: Option<wl_surface::WlSurface>,
    ) -> Result<Self, EngineError> {
        let descriptors = shared.window_descriptors()?;
        // 渲染目标尺寸的来源：窗口路径用 swapchain 的 `imageExtent`（clamp / currentExtent /
        // transform 之后），离屏路径用请求尺寸（R11-9）。
        let target = swapchain.as_ref().map_or(size, |swapchain| {
            let extent = swapchain.extent();
            (extent.width, extent.height)
        });
        let persist = Persist::new(&shared, target)?;
        let swapchain_images = swapchain.as_ref().map_or(1, Swapchain::image_count);
        let upload = UploadArena::new(&shared)?;
        let mirror = InstanceMirror::new(MAX_FRAMES_IN_FLIGHT);
        let (command_pool, commands) = create_commands(&shared)?;
        let timer = GpuTimer::new(&shared)?;
        let sync = FrameSync::new(&shared)?;
        let renderer = Self {
            vk_surface,
            swapchain,
            persist,
            retired_persists: Vec::new(),
            retired_swapchains: Vec::new(),
            descriptors,
            atlas_uploads: AtlasUploads::new(),
            upload,
            mirror,
            command_pool,
            commands,
            timer,
            sync,
            written_slots: [None; MAX_FRAMES_IN_FLIGHT],
            clear_color,
            viewport,
            last_destination: None,
            wl_surface,
            last_buffer_scale: 1,
            surface: size,
            scale: 1.0,
            geometry: GeometryRegistry::new(),
            fresh: true,
            last_sequence: 0,
            submitted: VecDeque::with_capacity(MAX_FRAMES_IN_FLIGHT),
            completed_sequence: 0,
            image_copy_plan: ImageCopyPlan::new(swapchain_images),
            frame_copy: None,
            rebuild_swapchain: false,
            out_of_date: 0,
            suboptimal_rebuilds: 0,
            last_rebuild_sequence: 0,
            rebuild_abandoned: false,
            #[cfg(feature = "test-support")]
            snapshot_index: 0,
            #[cfg(feature = "test-support")]
            readback: Readback::new(),
            shared,
        };
        Ok(renderer)
    }

    pub(crate) fn surface(&self) -> (u32, u32) {
        self.surface
    }

    /// 改 persist 的 clear color（linear；窗口背景色）。
    ///
    /// 只写本结构的状态，下一次**全量**录制（`clear` 分支的 render pass）读它；背景色变化由
    /// `Window` 标全量 damage 保证。GPU 线程调用（离屏路径不设背景色）。
    pub(crate) fn set_clear_color(&mut self, color: [f32; 4]) {
        self.clear_color = color;
    }

    /// 下一次 present 是否必须全量（新建 / 重建之后为 `true`）。
    pub(crate) fn is_fresh(&self) -> bool {
        self.fresh
    }

    /// resize（§7.4）：**不排空设备**。新 swapchain 走 `oldSwapchain`，旧 swapchain / 旧 persist
    /// 进延迟销毁队列（旧 swapchain 在下一次队列 idle 时销毁，见 [`Self::drain_retired_swapchains`]；
    /// 旧 persist 等提交序号完成，I4）。
    ///
    /// 渲染目标尺寸的**唯一来源是 swapchain extent**（clamp / `currentExtent` / transform 之后，
    /// R11-9），不是 UI 请求值 —— 旧实现让 persist 用未 clamp 的请求尺寸，于是 `renderArea` 可能
    /// 超出 framebuffer（`VUID-VkRenderPassBeginInfo-*`）、`vkCmdCopyImage` 的 extent 可能超出源图。
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        // 窗口目标的 resize 只能发生在 GPU 线程（§7.3）；离屏目标由门面同步调用。
        debug_assert!(
            self.swapchain.is_none() || on_gpu_thread(),
            "resizing a window target may only run on the kytrix-vulkan thread"
        );
        let size = (size.0.max(1), size.1.max(1));
        if size == self.surface {
            return Ok(());
        }
        if self.swapchain.is_some() {
            // 窗口路径：`install_swapchain` 按新链的 `imageExtent` 保证 persist 尺寸（单一来源）。
            return self.replace_swapchain(size);
        }
        // 离屏路径：没有 swapchain，渲染目标尺寸就是请求尺寸。
        let persist = Persist::new(&self.shared, size)?;
        // 离屏路径本来就同步（`finish_sync` 已等完每次提交）⇒ 旧 persist 可以立刻释放。
        drop(std::mem::replace(&mut self.persist, persist));
        self.surface = size;
        self.fresh = true;
        Ok(())
    }

    /// 窗口路径：提交一帧（**不等 GPU 完成**，I2）。
    ///
    /// 调用方（`renderer.rs` 的 GPU 循环）已经在 `request.surface` 变化时调用 [`Self::resize`]。
    /// `Retry` 表示本帧没有产生 GPU 工作，调用方必须**保留**该请求（跳过它会把 damage 弄丢）。
    pub(crate) fn submit_frame(
        &mut self,
        request: &FrameRequest,
        frame: &mut FramePlan,
    ) -> Result<Submitted, EngineError> {
        debug_assert!(
            on_gpu_thread(),
            "Vulkan 窗口帧只能在 kytrix-vulkan 线程提交"
        );
        debug_assert!(
            self.swapchain.is_some(),
            "window frames require a swapchain"
        );
        if let Some(error) = self.shared.lost_error() {
            return Err(error);
        }
        // DPI 与 surface 同帧过线：geometry 通道的顶点是逻辑像素，root uniform 的
        // `frame.params.x` 必须与这一帧的 surface 成对（contract §6）。
        self.scale = request.scale;
        let slot = self.sync.slot();
        // ① 槽 fence：复用命令缓冲 / 上传缓冲的**前置条件**（允许阻塞：GPU 线程，§8.2）。
        //
        // 必须在 acquire **之前**：`image_available[slot]` 由 `acquire` signal，而该信号量可能
        // 仍被本槽上一次提交的 wait 持有；只有等本槽 fence 才能证明那次提交已经消费了它。
        // acquire 拿一个已 signal 的二值信号量再次 signal 是非法用法
        // （VUID-vkAcquireNextImageKHR-semaphore-01286）。这也是 Khronos 的 frames-in-flight 顺序。
        // 复位 fence 仍然只在 `submit_commands` 里（acquire 失败时不会留下未 signal 的 fence）。
        self.sync
            .wait_slot(slot)
            .map_err(|error| self.classify(error))?;
        // ② acquire：节奏原语（I3），100 ms 上限只为响应 shutdown。
        let acquire = {
            let swapchain = self.swapchain.as_ref().expect("checked above");
            swapchain.acquire(&self.shared, self.sync.image_available(slot.index))
        };
        let image_index = match acquire {
            Ok(Acquire::Image { index, suboptimal }) => {
                if suboptimal {
                    // 本帧照常 present，之后按门槛重建（§7.3 / R11-7）。
                    self.rebuild_swapchain = true;
                }
                index
            }
            Ok(Acquire::NotReady) => return Ok(Submitted::Retry),
            Ok(Acquire::OutOfDate) => {
                self.rebuild_out_of_date("acquire reported OUT_OF_DATE")?;
                return Ok(Submitted::Retry);
            }
            // R11-11：surface 永久失效 ⇒ 重建 `VkSurfaceKHR` + swapchain（旧实现把 acquire 的
            // `SURFACE_LOST` 混进 `OutOfDate`，于是 present 侧的同类错误直接结束 GPU 线程、窗口黑屏）。
            Ok(Acquire::SurfaceLost) => {
                self.recover_surface("acquire reported SURFACE_LOST")?;
                return Ok(Submitted::Retry);
            }
            Err(error) => return Err(self.classify(error)),
        };

        // ③ 录制 + 提交（`vkQueueSubmit` 后立即返回）。返回本帧 submit 时 signal 的 timeline 值：
        // 完整判据必须与 `vkGetSemaphoreCounterValue` 同源，且**只能是 `slot.signal_value`**
        // （R11-1：UI 帧序号会跳号，不能当 timeline 值）。
        let mut timings = FrameTimings::new();
        let (stats, sequence) = self.record_and_submit(
            request.index,
            slot,
            &request.payload,
            frame,
            Some(image_index),
            &mut timings,
        )?;

        // ③b destination：必须在 `vkQueuePresentKHR` **之前**写入（WSI 规范要求窗口系统变更先于
        // present；present 内部的 `wl_surface.commit` 会让 attach 与 destination 原子生效）。
        // 与 buffer 同帧（`request.surface_local` 来自同一 UI 帧），旧 buffer 不会被新 destination
        // 单独缩放（D1 / D2）。整数回退（没有 viewporter / fractional manager）没有 destination：
        // 它的 `set_buffer_scale` 在同一次 present 之前写入（见下），两套路径互斥（V-110）。
        if let Some(viewport) = self.viewport.as_ref()
            && let Some((width, height)) =
                destination_update(self.last_destination, request.surface_local)
        {
            viewport.set_destination(width, height);
            self.last_destination = Some((width, height));
        }

        // ③c 整数回退的 buffer scale：必须与 ③b 的 destination 一样在 `vkQueuePresentKHR`
        // **之前**写入（同一次 `wl_surface.commit` 生效），否则本帧 buffer 会按旧的 scale 显示。
        // fractional 模式恒为 1（destination 负责缩放）；整数回退可能 >1（N=1 时为 1，V-110）。
        //
        // `wl_surface.attach` 要求 buffer 尺寸是 buffer_scale 的整数倍，否则 `invalid_size`
        // （wlroots 丢弃 commit，严格 compositor 断连）；平台层把 Wayland 逻辑尺寸取整后
        // buffer = `round(L × N)` 天然满足，这里只做 debug 期不变量检查。
        debug_assert!(
            request.buffer_scale >= 1,
            "buffer_scale must be >= 1, got {}",
            request.buffer_scale
        );
        debug_assert!(
            request.buffer_scale == 1
                || (request
                    .surface
                    .0
                    .is_multiple_of(request.buffer_scale as u32)
                    && request
                        .surface
                        .1
                        .is_multiple_of(request.buffer_scale as u32)),
            "buffer {:?} is not a multiple of buffer_scale {} (wl_surface.invalid_size)",
            request.surface,
            request.buffer_scale
        );
        if let Some(surface) = self.wl_surface.as_ref()
            && request.buffer_scale != self.last_buffer_scale
        {
            surface.set_buffer_scale(request.buffer_scale);
            self.last_buffer_scale = request.buffer_scale;
        }

        // ④ present：等**这张 image** 的 render-finished（§7.2 硬规则 1）。
        //
        // `incremental_present` 的 hint 必须与本次**实际拷贝的矩形**一致（R11-12）：旧实现把
        // UI 的 damage 当 hint、实际又拷整面 —— 改成差分拷贝之后两者必须同源，否则合成器会保留
        // 我们其实已经重画过的像素（浪费）或反之。
        // 本帧写进 persist 的矩形：**必须在 `frame.presented()` 之前取**（那会清 `fresh`，
        // 而 `fresh` 是「整面重画」的判据；取晚了会把整帧误当成局部帧，让其余 image 少拷像素）。
        let written = frame_written(frame);
        let present_started = Instant::now();
        let presented = {
            // 本帧的拷贝计划（在 `copy_to_swapchain` 里录制，present 时只读同样的事实）。
            let copied = self.frame_copy;
            let swapchain = self.swapchain.as_ref().expect("checked above");
            swapchain.present(&self.shared, image_index, copied)
        };
        let presented = match presented {
            Ok(presented) => presented,
            Err(error) => return Err(self.classify(error)),
        };
        timings.push(GpuTimerSample {
            section: Section::Present,
            cpu_ns: elapsed_ns(present_started),
            gpu_ns: 0,
        });
        match presented {
            Present::Presented | Present::Suboptimal => {
                if matches!(presented, Present::Suboptimal) {
                    self.rebuild_swapchain = true;
                }
                // 成功 present ⇒ 连续 OUT_OF_DATE 计数清零（上界只约束「连续」失败）。
                self.out_of_date = 0;
                frame.presented();
                self.fresh = false;
                // 本 image 已经被这一帧追平；其余 image 累积本帧 damage（R11-12）。
                self.image_copy_plan.commit(image_index, written);
                // 旧 swapchain 的释放条件（R11-3）：队列 idle，而不是「新链某帧的 timeline」。
                self.drain_retired_swapchains()?;
            }
            Present::OutOfDate => {
                // 本帧已经提交（timeline 会 signal），不能再重录；立刻重建，由下一帧补画。
                self.rebuild_swapchain = false;
                self.rebuild_out_of_date("present reported OUT_OF_DATE")?;
            }
            // R11-11：present 侧的 `SURFACE_LOST_KHR` 走与 acquire 同一条恢复路径。
            Present::SurfaceLost => {
                self.recover_surface("present reported SURFACE_LOST")?;
            }
        }
        self.submitted.push_back(sequence);
        // GPU 采样**不在这里** resolve：该槽的 timestamp 可能还被在飞帧写；完成观测点
        // （[`Self::resolve_frame_timer`]）只在 timeline 确认之后调用（§4.1）。
        let samples = timings.samples().to_vec();

        // SUBOPTIMAL：present 之后按门槛重建（§7.3 / R11-7），旧 swapchain 进延迟销毁队列。
        if self.rebuild_swapchain {
            self.rebuild_swapchain = false;
            self.request_rebuild("present reported SUBOPTIMAL")?;
        }
        Ok(Submitted::Presented {
            stats,
            samples,
            slot: slot.index,
            sequence,
        })
    }

    /// 离屏 / 快照路径：提交一份跨线程快照并**同步**等完成（§4.5 的下半段）。
    pub(crate) fn submit_snapshot(
        &mut self,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
    ) -> Result<RenderStats, EngineError> {
        debug_assert!(
            self.swapchain.is_none(),
            "the snapshot path must be offscreen"
        );
        if let Some(error) = self.shared.lost_error() {
            return Err(error);
        }
        let slot = self.sync.slot();
        // 离屏路径本来就同步：复用槽位前等 fence（与窗口路径同一前置条件）。
        self.sync.wait_slot(slot)?;
        let (stats, sequence) =
            self.record_and_submit(index, slot, payload, frame, None, timings)?;
        self.finish_sync(slot, sequence, timings)?;
        // 与 macOS 的 `submit_snapshot` 一致：本帧已经**同步完成**，目标内容是确定的，
        // 因此不再是「新建/未呈现」状态。旧实现从不清 `fresh`，于是离屏/基准路径每帧
        // 都走全量（掩盖了 partial 的问题，也让基准测不到真实的分支）。
        self.fresh = false;
        frame.presented();
        Ok(stats)
    }

    /// 窗口路径的非阻塞 retire 推进（GPU 循环每轮调用，I4）。
    ///
    /// 返回 timeline 的完成值（`vkGetSemaphoreCounterValue`，**不等待**）。调用方按提交顺序
    /// 发布 `signal <= 完成值` 的帧反馈 —— 与 retire 同一观测点（§4.1）。timeline 只在这里读
    /// （§7.2 硬规则 2：不用于 acquire/present）。旧 swapchain / 旧 persist 在本方法里真正
    /// drop —— 即「等对应帧完成之后才释放」。
    pub(crate) fn drain_completed(&mut self) -> Result<u64, EngineError> {
        if self.swapchain.is_none() {
            return Ok(0);
        }
        let value = self.sync.completed_value()?;
        while let Some(sequence) = self.submitted.front().copied() {
            if sequence > value {
                break;
            }
            self.submitted.pop_front();
            self.completed_sequence = sequence;
        }
        if self.completed_sequence != 0 {
            // retire 的键是**提交序号**（R11-1 / R06-1）：`FrameIndex` 是 `u32` 且会饱和，
            // 饱和后「按帧延迟释放」会退化成「立刻释放」。
            self.sync.drain_retire(self.completed_sequence);
            self.retired_persists
                .retain(|(sequence, _)| *sequence > self.completed_sequence);
        }
        Ok(value)
    }

    /// 该帧**完成之后**读它的 GPU 时间（fence / timeline 已确认，timestamp 不再被写）。
    ///
    /// 必须在完成观测点调用：提交点读到的可能仍是该槽上一帧的值（§4.1）。在飞 ≤ 1 的门禁
    /// （`frame_ready`，§7.3）保证本槽在下一次录制覆盖 query 对之前已经被观测并发布。
    pub(crate) fn resolve_frame_timer(&self, slot: usize) -> Option<GpuTimerSample> {
        self.timer.as_ref().and_then(|timer| timer.resolve(slot))
    }

    /// 图集上传入口（P09 的字形页与 P15 的图片页共用）。
    ///
    /// `pixels` 按**所有权**移动进来（[`AtlasUploads`] 的待上传槽直接持有它），避免槽 → 纹理
    /// 之间再复制一次整页（2048² 的图片页是 16 MiB）。入队时同时记录**当前共享页**
    /// （`SharedGpu::atlas_page`），供录制前区分「App 换代的新页」与「已被别人换代的旧页」。
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        page: u32,
        pixels: Vec<u8>,
        rows: crate::render::atlas::DirtyRows,
    ) {
        debug_assert!(
            self.swapchain.is_none() || on_gpu_thread(),
            "window atlas uploads may only be queued on the kytrix-vulkan thread"
        );
        let queued_page = self.shared.atlas_page(kind);
        self.atlas_uploads
            .queue_upload(kind, page, queued_page, pixels, rows);
    }

    /// 该图集是否有一笔上传已入队、还没被命令缓冲消费（离屏目标的空闲判据）。
    pub(crate) fn atlas_pending(&self, kind: AtlasKind) -> bool {
        self.atlas_uploads.pending(kind)
    }

    /// 读回持久 offscreen（快照路径；同步等待 GPU）。
    ///
    /// 调用方（`submit_snapshot` → `finish_sync`）已经等过本帧的槽 fence，所以这里只需要
    /// 读回命令自己的同步等待；**不用** timeline 等帧（§7.2 硬规则 2：timeline 只做 retire）。
    #[cfg(feature = "test-support")]
    pub(crate) fn readback(&mut self) -> Result<ReadbackImage, EngineError> {
        let command = self.commands[0];
        let image = self.persist.image;
        // 读回的尺寸必须与 persist 的实际尺寸一致（= swapchain extent，见 `resize`，R11-9）。
        let size = self.persist.size;
        // SAFETY: 命令缓冲由本结构独占；上一次使用它的提交已由 `finish_sync` 等过槽 fence。
        unsafe {
            self.shared
                .raw()
                .reset_command_buffer(command, vk::CommandBufferResetFlags::empty())
                .map_err(|error| {
                    init_error("reset_command_buffer(readback)", format!("{error:?}"))
                })?;
            self.shared
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
        self.readback.record(&self.shared, command, image, size)?;
        // SAFETY: 收尾；提交在下面同步完成。
        unsafe {
            self.shared
                .raw()
                .end_command_buffer(command)
                .map_err(|error| {
                    init_error("end_command_buffer(readback)", format!("{error:?}"))
                })?;
        }
        // 帧外的一次性提交：用临时 fence 同步等完成（读回只在测试 / 快照路径发生）。
        // SAFETY: fence 由本函数创建并销毁；命令缓冲只提交一次。
        let fence = unsafe {
            self.shared
                .raw()
                .create_fence(&vk::FenceCreateInfo::default(), None)
        }
        .map_err(|error| init_error("create_fence(readback)", format!("{error:?}")))?;
        let outcome = (|| -> Result<(), EngineError> {
            let commands = [command];
            let submit = vk::SubmitInfo::default().command_buffers(&commands);
            self.shared
                .with_queue(|queue| unsafe {
                    self.shared.raw().queue_submit(queue, &[submit], fence)
                })
                .map_err(|error| {
                    self.shared.note_failure(error, 0).unwrap_or_else(|| {
                        init_error("queue_submit(readback)", format!("{error:?}"))
                    })
                })?;
            self.shared.wait_fence(fence, "wait_for_fences(readback)")
        })();
        // SAFETY: fence 归本函数独占；等待已完成（或设备已丢失）。
        unsafe { self.shared.raw().destroy_fence(fence, None) };
        outcome?;
        self.readback.fetch(size)
    }

    /// 一次性渲染整帧并读回（快照用；不推进窗口的 `FrameScheduler`）。
    #[cfg(feature = "test-support")]
    pub(crate) fn render_snapshot(
        &mut self,
        buffer: &crate::paint::instance::InstanceBuffer,
    ) -> Result<ReadbackImage, EngineError> {
        // 一次性帧的序号在渲染器内部单调递增：timeline 的 signal 值必须严格递增。
        self.snapshot_index += 1;
        let index = FrameIndex::from_raw(self.snapshot_index);
        let payload = FramePayload::full(buffer);
        let mut timings = FrameTimings::new();
        let mut frame = FramePlan::new(Plan::Full, true);
        self.submit_snapshot(index, &payload, &mut frame, &mut timings)?;
        self.readback()
    }

    /// 录制 + 提交一帧（不呈现、不等完成）。返回本帧的统计与 submit 时 signal 的 timeline 值
    /// （调用方按它判断完成，不自己重复计算）。
    ///
    /// **timeline 值只有一个来源**（R11-1）：`slot.signal_value`（ring 的提交序号）。它同时是
    /// ① submit 的 signal 值、② retire / 延迟销毁的键、③ teardown 的等待值。UI 的 `FrameIndex`
    /// 只作为反馈标签 —— 旧实现拿它当 timeline 值 signal，而等待侧用 ring 的提交计数，序号一跳号
    /// 两者永久错位。
    fn record_and_submit(
        &mut self,
        index: FrameIndex,
        slot: FrameSlot,
        payload: &FramePayload,
        frame: &mut FramePlan,
        image_index: Option<u32>,
        timings: &mut FrameTimings,
    ) -> Result<(RenderStats, u64), EngineError> {
        timings.reset(index);
        debug_assert_eq!(
            timings.frame(),
            index,
            "timings must be reset for this frame"
        );
        debug_assert!(
            slot.signal_value > self.last_sequence,
            "timeline signal values must be strictly increasing (R11-22)"
        );
        let shared = Arc::clone(&self.shared);
        let upload_started = Instant::now();
        // 本帧换代 / 扩容换下的句柄：在录制尝试之后统一挂 retire（错误路径由 teardown 兜底）。
        let mut retired: Vec<GpuResourceHandle> = Vec::new();
        let upload = self
            .upload
            .write(&mut self.mirror, slot.index, payload, &mut retired);
        let (_instance_bytes, clip_offset, _clip_bytes) = match upload {
            Ok(values) => values,
            Err(error) => {
                // 上传缓冲扩容失败：已经换下的旧句柄同样要进 retire 队列（错误路径兜底）。
                for handle in retired {
                    self.sync.retire(slot.signal_value, handle);
                }
                return Err(error);
            }
        };
        timings.push(GpuTimerSample {
            section: Section::Upload,
            cpu_ns: elapsed_ns(upload_started),
            gpu_ns: 0,
        });
        // 本帧的几何帧号：`ensure` 记录 last_used，帧末 `retain_recent` 驱逐长期未绘制的几何。
        self.geometry.begin_frame();
        self.frame_copy = None;
        let recorded = self.record_frame(
            &shared,
            slot,
            payload,
            frame,
            image_index,
            clip_offset,
            &mut retired,
        );
        // 无论录制 / 提交是否成功，本帧已经换下的句柄都进 retire 队列：提交失败的帧永远不会
        // 完成，它们的句柄由本窗口 teardown 的 `drain_all_retired` 释放。
        for handle in retired {
            self.sync.retire(slot.signal_value, handle);
        }
        let draw_calls = recorded.map_err(|error| self.classify(error))?;
        Ok((
            RenderStats {
                draw_calls,
                instances: payload.drawn_instances(),
            },
            slot.signal_value,
        ))
    }

    /// 图集读锁内的「页对齐 → 录制 → 提交」循环（页换代需要释放读锁重试）。
    fn record_frame(
        &mut self,
        shared: &SharedGpu,
        slot: FrameSlot,
        payload: &FramePayload,
        frame: &mut FramePlan,
        image_index: Option<u32>,
        clip_offset: u64,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<u32, EngineError> {
        let full = frame.is_full();
        loop {
            // 图集读锁：持有到本帧提交完成（页重建是写锁，会等这里释放）。
            let atlas = shared.atlas();
            // 待上传页 ≠ 共享页 ⇒ 先落地页换代：App 换代的新页触发重建，过期上传直接丢弃。
            let rebuild = self
                .atlas_uploads
                .reconcile_pages([atlas.page(AtlasKind::Glyph), atlas.page(AtlasKind::Image)]);
            if !rebuild.is_empty() {
                drop(atlas);
                shared
                    .rebuild_atlas_pages(&rebuild)
                    .map_err(|error| self.classify(error))?;
                continue;
            }
            // R11-10：描述符只在句柄真的变化时重写。上传缓冲只在扩容时换句柄，`clips` 的偏移
            // 随容量固定 ⇒ 大多数帧这里什么都不做（旧实现每帧 2 次 `vkUpdateDescriptorSets`，
            // 且要持进程级描述符池锁）。
            let upload_buffer = self.upload.buffer(slot.index);
            let previous = self.written_slots[slot.index];
            if previous != Some((upload_buffer, clip_offset)) {
                shared.write_slot(
                    self.descriptors.sets(),
                    slot.index,
                    upload_buffer,
                    clip_offset,
                );
                self.written_slots[slot.index] = Some((upload_buffer, clip_offset));
            }
            let command = self.commands[slot.index];
            // SAFETY: 命令缓冲由本结构的命令池分配；本槽的上一次提交已由 `wait_slot` 等到。
            unsafe {
                shared
                    .raw()
                    .reset_command_buffer(command, vk::CommandBufferResetFlags::empty())
                    .map_err(|error| init_error("reset_command_buffer", format!("{error:?}")))?;
                shared
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
            let atlas_retired = self
                .atlas_uploads
                .record_uploads(shared, &atlas, slot.index, command)?;
            retired.extend(atlas_retired);
            let (draw_calls, geometry_retired) =
                self.draw(shared, command, slot.index, payload, frame, full)?;
            retired.extend(geometry_retired);
            if let Some(image_index) = image_index {
                self.copy_to_swapchain(shared, command, image_index, frame_written(frame))?;
            }
            if let Some(timer) = self.timer.as_ref() {
                timer.end(command, slot.index);
            }
            // SAFETY: 命令录制的收尾。
            unsafe {
                shared
                    .raw()
                    .end_command_buffer(command)
                    .map_err(|error| init_error("end_command_buffer", format!("{error:?}")))?;
            }
            self.submit_commands(shared, slot, command, slot.signal_value, image_index)?;
            self.sync.advance(slot);
            self.last_sequence = slot.signal_value;
            // 读锁在这里释放（present 不需要它）。
            drop(atlas);
            return Ok(draw_calls);
        }
    }

    /// 同步路径的收尾：等本帧完成 → 读本帧 GPU 时间 → 推进 retire（I4）。
    fn finish_sync(
        &mut self,
        slot: FrameSlot,
        sequence: u64,
        timings: &mut FrameTimings,
    ) -> Result<(), EngineError> {
        // 同步语义：本帧命令必须完成，GPU 时间与随后的读回才有意义。
        self.sync.wait_slot(slot)?;
        if let Some(sample) = self
            .timer
            .as_ref()
            .and_then(|timer| timer.resolve(slot.index))
        {
            timings.push(sample);
        }
        let present_started = Instant::now();
        timings.push(GpuTimerSample {
            section: Section::Present,
            cpu_ns: elapsed_ns(present_started),
            gpu_ns: 0,
        });
        // 同步完成：本帧（以及更早的提交）都已经完成，按提交序号推进 retire。
        self.completed_sequence = sequence;
        self.sync.drain_retire(sequence);
        self.retired_persists
            .retain(|(retired_at, _)| *retired_at > sequence);
        Ok(())
    }

    /// 用 `oldSwapchain` 建新 swapchain（§7.4）：**不等设备空闲**，旧结构进延迟销毁队列。
    fn recreate_swapchain(&mut self) -> Result<(), EngineError> {
        let size = self.surface;
        self.replace_swapchain(size)
    }

    /// 目标尺寸变化时的 swapchain 替换（同尺寸的 `OUT_OF_DATE` / `SUBOPTIMAL` 走
    /// [`Self::request_rebuild`]）。
    fn replace_swapchain(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        let surface = self.vk_surface.as_ref().map(Surface::raw);
        let old = self.swapchain.take();
        let created = match (surface, old.as_ref()) {
            (Some(surface), Some(old)) => {
                Swapchain::replace(&self.shared, surface, size, old.raw())
            }
            (Some(surface), None) => Swapchain::new(&self.shared, surface, size),
            (None, _) => {
                return Err(EngineError::Swapchain {
                    detail: smol_str::SmolStr::new_static(
                        "swapchain replacement requires a window surface",
                    ),
                });
            }
        };
        match created {
            Ok(swapchain) => self.install_swapchain(swapchain, old, size),
            Err(error) => {
                if let Some(old) = old {
                    // 规范：传给 `oldSwapchain` 的旧 swapchain 即使创建失败也已 retired，必须销毁
                    // （销毁前置条件由 `drain_retired_swapchains` 的队列 idle 保证，R11-3）。
                    self.retired_swapchains.push(old);
                }
                // `VK_ERROR_SURFACE_LOST_KHR` 的恢复：**重建 `VkSurfaceKHR`** 后重试一次。
                // 合成器重启 / 显卡复位后旧 surface 永久失效，只重建 swapchain 会一直失败；
                // 重试用尽才把错误上报（有界，不无限自旋）。
                let Some(surface) = self.vk_surface.as_mut() else {
                    return Err(error);
                };
                surface.recreate()?;
                let raw = self.vk_surface.as_ref().map(Surface::raw);
                let Some(raw) = raw else {
                    return Err(error);
                };
                let swapchain = Swapchain::new(&self.shared, raw, size)?;
                self.install_swapchain(swapchain, None, size)
            }
        }
    }

    /// 装上新链：旧链进延迟销毁队列，差分拷贝表与描述符记账一起重置。
    ///
    /// 渲染目标的尺寸只有一个来源：**swapchain 的 `imageExtent`**（`currentExtent` / clamp /
    /// transform 之后）。同尺寸重建时它也可能变（合成器改了 `currentExtent`），所以这里比较一次，
    /// 不同就重建 persist —— 否则 `renderArea` 会超出 framebuffer（R11-9）。
    fn install_swapchain(
        &mut self,
        swapchain: Swapchain,
        old: Option<Swapchain>,
        size: (u32, u32),
    ) -> Result<(), EngineError> {
        if let Some(old) = old {
            self.retired_swapchains.push(old);
        }
        // 新链的 image 索引与旧链无关（R11-12 / R11-9）：差分拷贝表必须整体重置。
        self.image_copy_plan.reset(swapchain.image_count());
        // 每张 image 的 render_finished 是新的；每帧槽的 `image_available` 由 `FrameSync` 持有
        // 且跨重建存活（规范允许），它们的等待值不受影响。
        let extent = swapchain.extent();
        self.swapchain = Some(swapchain);
        self.fresh = true;
        self.surface = size;
        self.frame_copy = None;
        if self.persist.size != (extent.width, extent.height) {
            let persist = Persist::new(&self.shared, (extent.width, extent.height))?;
            let previous = std::mem::replace(&mut self.persist, persist);
            // 旧 persist 仍可能被在飞命令引用 ⇒ 按提交序号延迟释放（I4）。
            self.retired_persists.push((self.last_sequence, previous));
        }
        Ok(())
    }

    /// 需要重建 swapchain 时的**门槛**入口（R11-7，`SUBOPTIMAL` 专用）。
    ///
    /// `VK_SUBOPTIMAL_KHR` 可以**持续**返回（extent / transform / colorSpace 与合成器期望不符），
    /// 旧实现每收到一次就 `vkCreateSwapchainKHR` + `get_swapchain_images` + N 个新信号量 —— 持续
    /// SUBOPTIMAL 时退化成每帧重建（帧率崩塌 + 句柄/显存抖动，现象像「卡顿」而不是错误）。两道
    /// 门槛：
    /// 1. 冷却期：刚重建过 [`REBUILD_COOLDOWN_FRAMES`] 个提交内不再重建；
    /// 2. 预算：最近一轮里超过 [`MAX_REBUILDS`] 次就**放弃重建**，记一次诊断后继续用现有链呈现
    ///    （并由 `swapchain.rs` 的 `log_plan` 打出实际参数）。
    fn request_rebuild(&mut self, reason: &'static str) -> Result<(), EngineError> {
        if self.rebuild_abandoned {
            return Ok(());
        }
        let since_last = self
            .last_sequence
            .saturating_sub(self.last_rebuild_sequence);
        if self.suboptimal_rebuilds > 0 && since_last < REBUILD_COOLDOWN_FRAMES {
            return Ok(());
        }
        if since_last >= REBUILD_COOLDOWN_FRAMES {
            // 一整段稳定期过去了：预算重新计算（健康的会话不应该因为很久以前的几次重建被卡死）。
            self.suboptimal_rebuilds = 0;
        }
        self.suboptimal_rebuilds += 1;
        if self.suboptimal_rebuilds > MAX_REBUILDS {
            self.rebuild_abandoned = true;
            eprintln!(
                "kytrix: vulkan swapchain keeps reporting {reason} after {MAX_REBUILDS} rebuilds; \
                 keeping the current swapchain (extent={:?} format={:?} color_space={:?})",
                self.swapchain.as_ref().map(Swapchain::extent),
                self.swapchain
                    .as_ref()
                    .map(|swapchain| swapchain.key().format),
                self.swapchain
                    .as_ref()
                    .map(|swapchain| swapchain.key().color_space),
            );
            return Ok(());
        }
        self.last_rebuild_sequence = self.last_sequence;
        self.recreate_swapchain()
    }

    /// `OUT_OF_DATE` 的重建入口：**不能**被冷却期拦住（链已经不能 present，等冷却只会继续丢帧），
    /// 但仍然有上界（[`MAX_REBUILDS`] 次连续失败之后报强类型错误，而不是无限自旋）。
    fn rebuild_out_of_date(&mut self, reason: &'static str) -> Result<(), EngineError> {
        self.out_of_date += 1;
        if self.out_of_date > MAX_REBUILDS {
            return Err(EngineError::Swapchain {
                detail: smol_str::format_smolstr!(
                    "swapchain stayed out of date after {MAX_REBUILDS} rebuilds ({reason})"
                ),
            });
        }
        self.last_rebuild_sequence = self.last_sequence;
        self.recreate_swapchain()
    }

    /// `VK_ERROR_SURFACE_LOST_KHR` 的恢复（R11-11）：重建 `VkSurfaceKHR` 与整条 swapchain。
    fn recover_surface(&mut self, reason: &'static str) -> Result<(), EngineError> {
        let Some(surface) = self.vk_surface.as_mut() else {
            return Err(EngineError::Swapchain {
                detail: smol_str::format_smolstr!("{reason}: no window surface"),
            });
        };
        eprintln!("kytrix: vulkan {reason}: recreating VkSurfaceKHR");
        // 旧 swapchain 必须先用掉（它引用旧 surface），随后进延迟销毁队列。
        let old = self.swapchain.take();
        if let Some(old) = old {
            self.retired_swapchains.push(old);
        }
        surface.recreate()?;
        let size = self.surface;
        let raw =
            self.vk_surface
                .as_ref()
                .map(Surface::raw)
                .ok_or_else(|| EngineError::Swapchain {
                    detail: smol_str::SmolStr::new_static("surface recreate lost the surface"),
                })?;
        let swapchain = Swapchain::new(&self.shared, raw, size)?;
        // `install_swapchain` 会在 extent 变化时重建 persist（单一尺寸来源）；旧 surface 的
        // 所有参数都可能变，因此这里不再重复建 persist。
        self.install_swapchain(swapchain, None, size)?;
        self.drain_retired_swapchains()?;
        Ok(())
    }

    /// 旧 swapchain 的延迟销毁（§7.4 / R11-3）：销毁前必须有一次**有界队列 idle**。
    ///
    /// `vkDestroySwapchainKHR` 要求「所有从该 swapchain 获取的 image 的使用已完成」，
    /// `vkDestroySemaphore` 要求「引用该信号量的批次已完成」—— present 的 `waitSemaphores`
    /// 也算引用。队列 idle 是这两条的共同前置条件（旧实现等的是「新链某帧的 timeline」，与旧链
    /// 的 present 之间没有任何依赖关系，因此不构成保证）。
    fn drain_retired_swapchains(&mut self) -> Result<(), EngineError> {
        if self.retired_swapchains.is_empty() {
            return Ok(());
        }
        self.shared
            .wait_queue_idle()
            .map_err(|error| self.classify(error))?;
        self.retired_swapchains.clear();
        Ok(())
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

    /// 画一帧的 persist 内容（渲染通道 + 逐 damage scissor），返回 draw call 数与本帧换下的
    /// 几何句柄（调用方 retire）。
    fn draw(
        &mut self,
        shared: &SharedGpu,
        command: vk::CommandBuffer,
        slot: usize,
        payload: &FramePayload,
        frame: &FramePlan,
        full: bool,
    ) -> Result<(u32, Vec<GpuResourceHandle>), EngineError> {
        let mut draw_calls = 0u32;
        let extent = self.extent();
        let clear = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: self.clear_color,
            },
        };
        let clears = [clear];
        let framebuffer = self.persist.framebuffer(full);
        // 帧级常量：viewport + DPI。每次 draw op 在它之上叠加实例基址（`with_instance_base`）。
        let root = root_uniform(extent, self.scale);
        let pipeline = shared.pipeline();
        let sets = self.descriptors.sets();
        // SAFETY: 渲染通道/framebuffer 归本结构独占；`loadOp` 与 `clear` 值匹配。
        unsafe {
            shared.raw().cmd_begin_render_pass(
                command,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(pipeline.render_pass(full))
                    .framebuffer(framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent,
                    })
                    .clear_values(&clears),
                vk::SubpassContents::INLINE,
            );
            shared.raw().cmd_bind_pipeline(
                command,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.quad(),
            );
            shared.raw().cmd_bind_descriptor_sets(
                command,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.layout(),
                0,
                &[sets[slot]],
                &[],
            );
            let push_bytes = bytemuck::cast_slice::<f32, u8>(root.words());
            shared.raw().cmd_push_constants(
                command,
                pipeline.layout(),
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                push_bytes,
            );
            shared.raw().cmd_set_viewport(
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

        // 几何换代（槽位复用）产生的旧设备对象：录制块结束后交给 retire 队列。
        let mut geometry_retired: Vec<GpuResourceHandle> = Vec::new();
        // 逐 damage 矩形：每个矩形一次 `cmd_set_scissor` + 全部 draw op。
        match full {
            true => {
                // SAFETY: 同上。
                unsafe {
                    shared.raw().cmd_set_scissor(
                        command,
                        0,
                        &[vk::Rect2D {
                            offset: vk::Offset2D { x: 0, y: 0 },
                            extent,
                        }],
                    );
                }
                draw_calls = draw_ops(
                    shared,
                    command,
                    pipeline,
                    &mut self.geometry,
                    &mut geometry_retired,
                    payload,
                    root,
                    [0, 0, extent.width as i32, extent.height as i32],
                )?;
            }
            false => {
                if let Plan::Partial(damage) = frame.plan() {
                    // R11-16：damage 是相对 **UI 物理 surface** 算的，而 renderArea 是 swapchain
                    // extent —— 两者在 clamp / `currentExtent` 生效时不等（R11-9）。scissor 超出
                    // renderArea 会判 `VUID-vkCmdSetScissor-None-00595`；夹紧与 copy / present
                    // region 共用同一份实现（`DamageRects::clamped`）。
                    let clamped = damage.clamped(extent.width, extent.height);
                    for rect in clamped.as_slice() {
                        let scissor = vk::Rect2D {
                            offset: vk::Offset2D {
                                x: rect.x0,
                                y: rect.y0,
                            },
                            extent: vk::Extent2D {
                                width: rect.width() as u32,
                                height: rect.height() as u32,
                            },
                        };
                        // SAFETY: 矩形已夹到 renderArea（= framebuffer 尺寸）内。
                        unsafe {
                            shared.raw().cmd_set_scissor(command, 0, &[scissor]);
                        }
                        draw_calls = draw_calls.saturating_add(draw_ops(
                            shared,
                            command,
                            pipeline,
                            &mut self.geometry,
                            &mut geometry_retired,
                            payload,
                            root,
                            [rect.x0, rect.y0, rect.x1, rect.y1],
                        )?);
                    }
                }
            }
        }

        // SAFETY: 收尾渲染通道。
        unsafe {
            shared.raw().cmd_end_render_pass(command);
        }
        // 录制完成：驱逐长期未绘制的几何（设备对象进同一个 retire 列表，D-1）。
        self.geometry.retain_recent(&mut geometry_retired);
        Ok((draw_calls, geometry_retired))
    }

    /// persist → swapchain 的**差分** copy + 两侧 barrier（§6.6 的表、R11-12）。
    ///
    /// `written` 是本帧真正写进 persist 的矩形（`None` = 整面重画）。拷贝范围由
    /// [`ImageCopyPlan::plan`] 决定：该 image 内容未知 / 本帧整面重画 / 差集超出容量 ⇒ 整面；
    /// 否则只拷 `pending ∪ written`。实际拷贝的矩形存进 `frame_copy`，present 用它生成
    /// `VK_KHR_incremental_present` 的 region（两者必须一致）。
    fn copy_to_swapchain(
        &mut self,
        shared: &SharedGpu,
        command: vk::CommandBuffer,
        image_index: u32,
        written: Option<DamageRects>,
    ) -> Result<(), EngineError> {
        let Some(swapchain) = self.swapchain.as_ref() else {
            return Ok(());
        };
        let extent = swapchain.extent();
        let target = swapchain.image(image_index);
        // 差分拷贝计划（R11-12）。`regions = None` ⇒ 整面；空集 ⇒ 本帧不需要搬像素。
        let plan: CopyPlan = self.image_copy_plan.plan(image_index, written);
        // R11-16：damage 是相对 UI 物理 surface 算的，swapchain 的 `imageExtent` 可能更小
        // （`currentExtent` / clamp / transform，R11-9）。**夹紧只有一份实现**：这里夹出来的集合
        // 既是 `vkCmdCopyImage` 的矩形，也是 present 的 `VkPresentRegionKHR`（`frame_copy`）——
        // 两者逐字一致，合成器不会把没写的像素当成新的（R11-12）。
        let regions = plan
            .regions()
            .map(|regions| regions.clamped(extent.width, extent.height));
        self.frame_copy = regions;
        let mut copies: Vec<vk::ImageCopy> = Vec::new();
        if let Some(regions) = regions.as_ref() {
            for rect in regions.as_slice() {
                copies.push(image_copy(
                    rect.x0 as u32,
                    rect.y0 as u32,
                    rect.width() as u32,
                    rect.height() as u32,
                ));
            }
        }
        if copies.is_empty() && regions.is_none() {
            copies.push(image_copy(0, 0, extent.width, extent.height));
        }
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        // ① swapchain image 的 layout 转换必须**先**发生（它的首次使用是 TRANSFER 阶段的
        //    transition + copy）。src/dst 都是 TRANSFER，才能排在 `image_available` 的 acquire
        //    wait（同样在 TRANSFER 阶段）之后；用 `TOP_OF_PIPE` 会与 acquire 信号量没有依赖链，
        //    syncval 报 `SYNC-HAZARD-WRITE-AFTER-READ`（F-168）。
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
        if copies.is_empty() {
            // 该 image 与 persist 已经一致：只做两次 layout 转换（没有像素要搬）。
            // SAFETY: target 由本窗口的 swapchain 持有；range 是单层单级。
            unsafe {
                shared.raw().cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_dst],
                );
                shared.raw().cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_present],
                );
            }
            return Ok(());
        }
        let to_src = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .image(self.persist.image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
        let back = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .image(self.persist.image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
        // SAFETY: 两张 image 都由本结构独占；copy 的矩形已夹到 swapchain extent 内。
        unsafe {
            shared.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_src],
            );
            shared.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            );
            shared.raw().cmd_copy_image(
                command,
                self.persist.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                target,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &copies,
            );
            shared.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_present],
            );
            shared.raw().cmd_pipeline_barrier(
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

    /// 提交本帧命令。
    ///
    /// `sequence` 是 `slot.signal_value`（**唯一**的 timeline 值来源，R11-1）：它被写进
    /// `signalSemaphoreValues` 的最后一个元素（timeline 信号量），并作为诊断文件里的帧标签。
    fn submit_commands(
        &mut self,
        shared: &SharedGpu,
        slot: FrameSlot,
        command: vk::CommandBuffer,
        sequence: u64,
        image_index: Option<u32>,
    ) -> Result<(), EngineError> {
        let commands = [command];
        // 窗口路径：等本帧槽的 `image_available`（acquire 时 signal），signal 本 image 的
        // `render_finished`。离屏路径两者都没有。
        let waits: Vec<vk::Semaphore> = image_index
            .map(|_| self.sync.image_available(slot.index))
            .into_iter()
            .collect();
        // 第一次使用 swapchain image 的是 TRANSFER 阶段（layout transition + 整面 copy），
        // 不是 `COLOR_ATTACHMENT_OUTPUT`（本后端不直接往 swapchain image 上画）。在错误的阶段
        // 等待会让 acquire 与 copy 之间没有依赖链（F-168，§7.3）。
        let stages = [vk::PipelineStageFlags::TRANSFER];
        let mut signals = Vec::with_capacity(2);
        if let Some(image_index) = image_index {
            let swapchain = self.swapchain.as_ref().expect("image implies swapchain");
            signals.push(swapchain.render_finished(image_index));
        }
        signals.push(self.sync.timeline());
        // timeline semaphore **必须带值**：`wait_semaphore_values` 覆盖全部 wait 信号量
        // （binary 信号量的值被忽略），`signal_semaphore_values` 给出每个信号量的目标值。
        let wait_values = vec![0u64; waits.len()];
        let signal_values: Vec<u64> = {
            let mut values = vec![0u64; signals.len()];
            if let Some(last) = values.last_mut() {
                *last = sequence;
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
        self.sync.reset_fence(slot.index)?;
        // SAFETY: 命令缓冲已录制完毕；fence 由 `FrameSync` 持有（每帧槽一个）；共享队列的
        // 主机访问经 `with_queue` 外部同步。
        let result = shared.with_queue(|queue| unsafe {
            shared
                .raw()
                .queue_submit(queue, &[submit], self.sync.fence(slot.index))
        });
        if let Err(error) = result {
            if let Some(lost) = shared.note_failure(error, sequence) {
                return Err(lost);
            }
            return Err(EngineError::Swapchain {
                detail: smol_str::format_smolstr!("queue_submit: {error:?}"),
            });
        }
        Ok(())
    }

    /// 把底层错误归类为设备丢失（粘性）或原样返回。
    fn classify(&mut self, error: EngineError) -> EngineError {
        if matches!(error, EngineError::DeviceLost { .. })
            && let Some(sticky) = self
                .shared
                .note_failure(vk::Result::ERROR_DEVICE_LOST, self.last_sequence)
        {
            return sticky;
        }
        error
    }
}

impl Drop for GpuRenderer {
    fn drop(&mut self) {
        // 协议对象先于 `wl_surface` 销毁：正常路径下 renderer 在 `WaylandState` 之前 drop
        // （`event_loop` 的显式顺序），因此这里销毁 viewport 时 surface 仍然存活。例外是 renderer
        // 挂载后的 fallible setup 早退路径（V-122 ⑨，既有 P2-3，本轮不修）。请求发不出去
        // （连接已断）也无害：wayland-client 的 destructor 忽略错误，服务端随连接关闭清理。
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
        // 退出路径（§7.7 / R11-1 / R11-3）两步，缺一不可：
        //
        // ① 等**本窗口**的 timeline 到「最后一次提交时 signal 的那个值」。这个值就是
        //    `slot.signal_value`（ring 的提交序号）—— 旧实现等的是 ring 的提交计数、signal 的却是
        //    UI 帧序号，序号一跳号等待值就偏小，最后一帧仍在飞时会立刻返回，随后销毁 command pool /
        //    persist / swapchain / 信号量（`VUID-vkFreeMemory-memory-00677` 一类）。
        // ② 队列 idle：`timeline` 到达只证明命令执行完毕，**不证明 presentation engine 已消费
        //    present**。`vkDestroySwapchainKHR`（VUID-…-01282）与 `vkDestroySemaphore`
        //    （VUID-…-01137）都要求前者；`wait_queue_idle` 是有界的（2 s），设备挂死时窗口仍能关。
        //
        // 两条失败的语义：**设备已经不可用**。此时不再"假装成功"继续销毁（旧实现 `let _ =`
        // 吞掉错误照样销毁），而是跳过延迟销毁队列（把句柄留给 `SharedGpu::drop` 兜底），
        // 只记一条诊断。GpuRenderer 的字段仍会 drop，但那时设备已丢失，驱动不会再执行命令。
        let timeline_idle = self.sync.wait_idle();
        let queue_idle = self.shared.wait_queue_idle();
        let teardown_safe = timeline_idle.is_ok() && queue_idle.is_ok();
        if !teardown_safe {
            let reason = timeline_idle
                .err()
                .or(queue_idle.err())
                .map(|error| error.to_string())
                .unwrap_or_default();
            eprintln!(
                "kytrix: vulkan teardown could not prove the GPU is idle ({reason}); \
                 leaking this window's deferred objects instead of destroying in-flight handles"
            );
            // 跳过「危险销毁」：不再销毁命令池 / 延迟队列（把句柄留给进程级兜底）。
            self.retired_persists.clear();
            self.retired_swapchains.clear();
            self.geometry.release(&self.shared);
            self.upload.release(&self.shared);
            self.sync.drain_all_retired();
            return;
        }
        // SAFETY: ① 本窗口命令已完成；② 队列已 idle ⇒ 没有在飞批次引用命令池。
        unsafe {
            self.shared
                .raw()
                .destroy_command_pool(self.command_pool, None);
        }
        // 旧 swapchain / 旧 persist：队列 idle 之后 presentation engine 与在飞批次都不再引用它们。
        self.retired_persists.clear();
        self.retired_swapchains.clear();
        // 每窗口注册进共享设备句柄表的资源必须主动注销（设备不会随窗口销毁）。
        self.geometry.release(&self.shared);
        self.upload.release(&self.shared);
        // retire 队列的残留句柄（含录制失败、从未提交的帧）一次性释放。
        self.sync.drain_all_retired();
    }
}

/// 持久 offscreen（唯一绘制目标；与 swapchain 同格式）。
///
/// `Drop` 销毁全部句柄；窗口路径的旧 persist 进延迟销毁队列，等对应帧完成后才被 drop（I4）。
struct Persist {
    device: ash::Device,
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
    framebuffer_clear: vk::Framebuffer,
    framebuffer_load: vk::Framebuffer,
    /// 实际尺寸（= 创建时的 swapchain extent；读回与 `renderArea` 的唯一来源，R11-9）。
    size: (u32, u32),
}

impl Persist {
    fn new(shared: &SharedGpu, size: (u32, u32)) -> Result<Self, EngineError> {
        let device = shared.raw();
        let pipeline = shared.pipeline();
        // SAFETY: 句柄归本结构独占；`Drop` / 延迟销毁里销毁。
        let image = unsafe {
            device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(RENDER_FORMAT)
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
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        let memory_type = shared
            .memory_type(requirements.memory_type_bits, MemoryNeed::DEVICE_IMAGE)
            .ok_or_else(|| init_error("persist memory", "no device-local memory type"))?;
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(memory_type),
                None,
            )
        }
        .map_err(|error| init_error("allocate_memory(persist)", format!("{error:?}")))?;
        // SAFETY: 目标句柄已按 Vulkan 校验规则创建，绑定/写入的范围在其分配内。
        unsafe { device.bind_image_memory(image, memory, 0) }
            .map_err(|error| init_error("bind_image_memory(persist)", format!("{error:?}")))?;
        // SAFETY: 句柄与参数都由本结构独占，调用期间有效（Vulkan 的外部同步已由 fence 保证）。
        let view = unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(RENDER_FORMAT)
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
            device: device.clone(),
            image,
            view,
            memory,
            framebuffer_clear,
            framebuffer_load,
            size,
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
        // SAFETY: 句柄归本结构独占；调用前队列已空闲（退出路径）或该帧已完成（延迟销毁）。
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
    device: &ash::Device,
    pass: vk::RenderPass,
    attachments: &[vk::ImageView],
    size: (u32, u32),
) -> Result<vk::Framebuffer, EngineError> {
    // SAFETY: 附件是本结构持有的 color view；尺寸与 image 一致。
    unsafe {
        device.create_framebuffer(
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
///
/// 走 `SharedGpu` 的子分配器（R11-6）：每槽一块 `vkAllocateMemory` 的时代结束 —— 缓冲只在
/// 「本槽容量不够」时重建，重建出来的旧块区间由 retire 队列归还（块本身复用，不 `vkFreeMemory`）。
struct UploadArena {
    shared: Arc<SharedGpu>,
    slots: Vec<UploadSlot>,
}

struct UploadSlot {
    /// 子分配缓冲：句柄 + 块内区间 + **持久映射**的主机写入指针（整块 map 一次）。
    ///
    /// 旧实现每帧 map/unmap：驱动每次都要走一遍页表操作，是纯开销 —— Windows 后端一直是
    /// 持久映射。`HOST_COHERENT` 内存保证写入无需 flush（P2-7）。
    buffer: GpuBuffer,
    instance_capacity: u32,
    clip_capacity: u32,
    clip_offset: u64,
}

impl UploadArena {
    fn new(shared: &Arc<SharedGpu>) -> Result<Self, EngineError> {
        let mut slots = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            slots.push(UploadSlot::new(shared, 256, 8)?);
        }
        Ok(Self {
            shared: Arc::clone(shared),
            slots,
        })
    }

    fn buffer(&self, slot: usize) -> vk::Buffer {
        self.slots[slot].buffer.buffer
    }

    /// 写本帧的实例/裁剪字节；容量不够时扩容（旧句柄进 `retired`，本帧完成后释放）。
    ///
    /// 实例走**增量写**：`InstanceMirror` 持有完整实例字节，本帧只把变化区间（或整块，当该槽
    /// 需要整块重写时）拷进当前槽的持久映射；未变化的段保留该槽上次的内容。裁剪表仍然全量写
    /// （小表，且表项索引必须与实例里内嵌的 `clip_index` 同帧一致）。
    ///
    /// 返回 `(instance_count, clip_offset, clip_bytes)`。
    fn write(
        &mut self,
        mirror: &mut InstanceMirror,
        slot: usize,
        payload: &FramePayload,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(u32, u64, u64), EngineError> {
        let instance_count = payload.instance_count();
        let clips = payload.clips();
        let clip_count = clips.len() as u32;
        if instance_count > self.slots[slot].instance_capacity
            || clip_count > self.slots[slot].clip_capacity
        {
            let instances_capacity = instance_count.next_power_of_two().max(256);
            let clips_capacity = clip_count.next_power_of_two().max(64);
            let replacement = UploadSlot::new(&self.shared, instances_capacity, clips_capacity)?;
            let old = std::mem::replace(&mut self.slots[slot], replacement);
            // 旧缓冲在本帧完成之后由 retire 队列销毁（其块区间随之回到内存池）。
            retired.push(old.buffer.handle);
            // 新缓冲内容未定义：这个槽的增量欠账全部作废，下一次必须整块重写。
            mirror.invalidate(slot);
        }
        let slot_ref = &self.slots[slot];
        let Some(base) = slot_ref.buffer.pointer else {
            return Err(init_error(
                "upload memory",
                "suballocated upload buffer has no host mapping",
            ));
        };
        let full = mirror.prepare(slot, payload);
        let instance_bytes = instance_count as usize * 128;
        let clip_offset = slot_ref.clip_offset;
        // SAFETY: 持久映射覆盖整个块；写入范围由本槽容量保证在自己的子分配内（上面的扩容刚做过）。
        unsafe {
            if full {
                let bytes = &mirror.bytes()[..instance_bytes];
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), base, bytes.len());
            } else {
                for range in mirror.writes() {
                    let start = range.start as usize * 128;
                    let end = range.end as usize * 128;
                    std::ptr::copy_nonoverlapping(
                        mirror.bytes()[start..end].as_ptr(),
                        base.add(start),
                        end - start,
                    );
                }
            }
            if !clips.is_empty() {
                std::ptr::copy_nonoverlapping(
                    bytemuck::cast_slice::<crate::paint::instance::GpuClipEntry, u8>(clips)
                        .as_ptr(),
                    base.add(clip_offset as usize),
                    clips.len() * 48,
                );
            }
        }
        mirror.finish(slot);
        Ok((instance_count, clip_offset, clips.len() as u64))
    }

    /// teardown：把本窗口上传缓冲的句柄交回共享设备。
    fn release(&self, shared: &SharedGpu) {
        for slot in &self.slots {
            let _ = shared.release(slot.buffer.handle);
        }
    }
}

impl UploadSlot {
    fn new(
        shared: &SharedGpu,
        instance_capacity: u32,
        clip_capacity: u32,
    ) -> Result<Self, EngineError> {
        let instance_bytes = u64::from(instance_capacity) * 128;
        let clip_offset = instance_bytes.div_ceil(CLIP_ALIGN) * CLIP_ALIGN;
        let size = clip_offset + u64::from(clip_capacity) * 48;
        let buffer = shared.allocate_buffer(
            size,
            vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            MemoryNeed::HOST_WRITABLE,
        )?;
        if buffer.pointer.is_none() {
            // `HOST_WRITABLE` 要求 `HOST_VISIBLE` ⇒ 子分配器必然给出映射。
            let _ = shared.release(buffer.handle);
            return Err(init_error(
                "upload memory",
                "suballocated upload buffer has no host mapping",
            ));
        }
        Ok(Self {
            buffer,
            instance_capacity,
            clip_capacity,
            clip_offset,
        })
    }
}

fn draw_ops(
    shared: &SharedGpu,
    command: vk::CommandBuffer,
    pipeline: &Pipeline,
    geometry: &mut GeometryRegistry,
    retired: &mut Vec<GpuResourceHandle>,
    payload: &FramePayload,
    root: RootUniform,
    scissor: [i32; 4],
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
                // 实例基址只经 push constant `draw.clip.y` 进 shader，硬件基址恒 0
                // （Vulkan 的 firstInstance 会加到 gl_InstanceIndex 上，与 D3D12 相反；
                //  统一走 uniform 才能三平台一致，contract §6）。
                let uniform = root.with_instance_base(start);
                let push_bytes = bytemuck::cast_slice::<f32, u8>(uniform.words());
                // SAFETY: push constant 的范围由本帧的 pipeline layout 声明（[0, 112)）。
                unsafe {
                    shared.raw().cmd_bind_pipeline(
                        command,
                        vk::PipelineBindPoint::GRAPHICS,
                        pipeline.quad(),
                    );
                    shared.raw().cmd_push_constants(
                        command,
                        pipeline.layout(),
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        push_bytes,
                    );
                    // 4 顶点 unit quad + 实例区间 [0, len)；段起点由 shader 侧的 draw.clip.y 承担。
                    shared.raw().cmd_draw(command, 4, len, 0, 0);
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
                let entry = geometry.ensure(shared, draw.id, &draw.desc, retired)?;
                let uniform = geometry_root(root, draw, clip_index, clip_rect);
                let push_bytes = bytemuck::cast_slice::<f32, u8>(uniform.words());
                // SAFETY: 顶点/索引缓冲由 `GeometryRegistry` 持有到本帧结束；
                // push constant 范围由 pipeline layout 声明。
                unsafe {
                    shared.raw().cmd_bind_pipeline(
                        command,
                        vk::PipelineBindPoint::GRAPHICS,
                        pipeline.geometry(entry.pipeline(), entry.topology()),
                    );
                    shared.raw().cmd_push_constants(
                        command,
                        pipeline.layout(),
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        push_bytes,
                    );
                    shared
                        .raw()
                        .cmd_bind_vertex_buffers(command, 0, &[entry.vertex()], &[0]);
                    shared.raw().cmd_bind_index_buffer(
                        command,
                        entry.index(),
                        0,
                        entry.index_type(),
                    );
                    shared
                        .raw()
                        .cmd_draw_indexed(command, entry.index_count(), 1, 0, 0, 0);
                }
                draw_calls = draw_calls.saturating_add(1);
            }
        }
    }
    Ok(draw_calls)
}

fn create_commands(
    shared: &SharedGpu,
) -> Result<(vk::CommandPool, [vk::CommandBuffer; MAX_FRAMES_IN_FLIGHT]), EngineError> {
    // SAFETY: 池与命令缓冲归 `GpuRenderer` 独占；`Drop` 里销毁池。
    unsafe {
        let pool = shared
            .raw()
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(shared.queue_family())
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
            .map_err(|error| init_error("create_command_pool", format!("{error:?}")))?;
        let buffers = shared
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
/// `scale` 是窗口 DPI（`frame.params.x`）：geometry 通道的顶点是逻辑像素，shader 用它换物理。
fn root_uniform(extent: vk::Extent2D, scale: f32) -> RootUniform {
    RootUniform::frame(extent.width as f32, extent.height as f32, scale)
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos() as u64
}

/// 本帧真正写进 persist 的矩形（`None` = 整面）。
///
/// 差分拷贝的输入（R11-12）：`fresh` / `Plan::Full` 意味着这一帧整面重画了 persist，
/// 任何「只拷差集」的推理都不成立。
fn frame_written(frame: &FramePlan) -> Option<DamageRects> {
    if frame.is_full() {
        return None;
    }
    match frame.plan() {
        Plan::Partial(damage) => Some(damage),
        Plan::Idle | Plan::Full => None,
    }
}

/// 一个 `VkImageCopy`（persist → swapchain image，同格式、单层、无 3D 分量）。
fn image_copy(x: u32, y: u32, width: u32, height: u32) -> vk::ImageCopy {
    let layers = vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1);
    vk::ImageCopy::default()
        // `default()` 的 subresource 是 aspect=0 / layer_count=0（非法），必须显式给。
        .src_subresource(layers)
        .dst_subresource(layers)
        .src_offset(vk::Offset3D {
            x: x as i32,
            y: y as i32,
            z: 0,
        })
        .dst_offset(vk::Offset3D {
            x: x as i32,
            y: y as i32,
            z: 0,
        })
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
}

/// 本帧要写入的 destination（`None` = 与上次相同，不调用协议）。
///
/// `wp_viewport.set_destination` 写**逻辑尺寸**（向上取整）：compositor 按它显示 buffer，而
/// buffer 是 `round(逻辑 × scale)`。只在变化时下发 —— 每帧一次协议请求没有必要，而且新的
/// destination 必须与新的 buffer 配对出现（D1 / D2）。
fn destination_update(last: Option<(i32, i32)>, surface_local: Size) -> Option<(i32, i32)> {
    let destination = dpi::viewport_destination(surface_local);
    (last != Some(destination)).then_some(destination)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{GpuRenderer, destination_update};
    use crate::{
        math::Size,
        paint::atlas::AtlasKind,
        render::{
            DirtyRows,
            compositor::{FramePlan, Plan},
            frame::FramePayload,
            linux::device::{SharedGpu, gpu_test_lock},
            timer::FrameTimings,
        },
        window::FrameIndex,
    };

    #[test]
    fn a_destination_is_written_only_when_it_changes() {
        assert_eq!(
            destination_update(None, Size::new(800.0, 600.0)),
            Some((800, 600))
        );
        assert_eq!(
            destination_update(Some((800, 600)), Size::new(800.0, 600.0)),
            None,
            "逻辑尺寸没变就不下发"
        );
        assert_eq!(
            destination_update(Some((800, 600)), Size::new(701.0, 501.0)),
            Some((701, 501))
        );
    }

    #[test]
    fn the_destination_is_the_logical_size_rounded_up() {
        // 与 buffer 尺寸（`round(701 × 1.25) = 876`）无关：destination 永远是逻辑尺寸。
        assert_eq!(
            destination_update(None, Size::new(701.0, 501.0)),
            Some((701, 501))
        );
        assert_eq!(
            destination_update(None, Size::new(700.2, 500.8)),
            Some((701, 501)),
            "小数逻辑尺寸向上取整，否则最右 / 最下 1 px 被裁掉"
        );
        assert_eq!(
            destination_update(None, Size::new(0.0, 0.0)),
            Some((1, 1)),
            "退化尺寸至少 1 px"
        );
    }

    /// 关窗不泄漏：渲染器销毁后，它注册进共享设备句柄表的资源必须全部注销。
    ///
    /// 进程级设备不会随窗口销毁，所以上传缓冲 / 图集 staging / 几何缓冲的句柄必须在
    /// `GpuRenderer::drop` 里主动交回（否则每关一个窗口就泄漏一份显存）。
    #[test]
    fn dropping_a_renderer_releases_its_shared_device_resources() {
        let _guard = gpu_test_lock();
        let shared = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        assert_eq!(shared.live_resources(), 0, "共享 GPU 创建时不注册资源");
        {
            let mut renderer = GpuRenderer::with_gpu(
                Arc::clone(&shared),
                None,
                None,
                (16, 16),
                [0.0; 4],
                None,
                None,
            )
            .expect("renderer");
            let mut rows = DirtyRows::default();
            rows.mark(0, u32::MAX);
            renderer.queue_atlas_upload(AtlasKind::Glyph, 64, vec![0u8; (64 * 64) as usize], rows);
            let mut frame = FramePlan::new(Plan::Full, true);
            let mut timings = FrameTimings::new();
            renderer
                .submit_snapshot(
                    FrameIndex::from_raw(1),
                    &FramePayload::empty(),
                    &mut frame,
                    &mut timings,
                )
                .expect("submit");
            assert!(
                shared.live_resources() > 0,
                "渲染器必须持有上传缓冲 / 图集 staging 资源"
            );
        }
        assert_eq!(
            shared.live_resources(),
            0,
            "关窗后渲染器注册的所有资源句柄必须注销"
        );
    }

    /// 多窗口共享契约：两个渲染器共享同一个进程级 `SharedGpu`，描述符段互不重叠。
    #[test]
    fn offscreen_renderers_share_the_process_gpu() {
        let _guard = gpu_test_lock();
        let first = GpuRenderer::new_offscreen((8, 8)).expect("first renderer");
        let second = GpuRenderer::new_offscreen((8, 8)).expect("second renderer");
        assert!(
            Arc::ptr_eq(&first.shared, &second.shared),
            "两个窗口必须共享同一个 SharedGpu（device / queue / PSO / 图集纹理）"
        );
        assert_ne!(
            first.descriptors.sets()[0],
            second.descriptors.sets()[0],
            "每个窗口在共享描述符池里必须有各自的段"
        );
    }

    /// 两个窗口并发录制 + 提交到**同一条进程级队列**：各自的 persist / 上传缓冲 / 描述符段
    /// 互不干扰，像素回归证明提交串行化正确（两侧都走同一条 graphics queue）。
    #[cfg(feature = "test-support")]
    #[test]
    fn two_renderers_render_concurrently_on_one_shared_gpu() {
        use crate::{
            math::{Rect, Scale},
            paint::{
                color::{Color, ColorResolver},
                instance::InstanceEncoder,
                primitive::{Primitive, Quad, QuadPaint},
                scene::Scene,
            },
        };

        fn red_quad_scene() -> Scene {
            let mut scene = Scene::empty();
            let lane = scene.new_lane();
            assert!(scene.push(
                lane,
                Primitive::Quad(Quad {
                    bounds: Rect::new(2.0, 2.0, 2.0, 2.0),
                    corner_radius: [0.0; 4],
                    paint: QuadPaint::Solid {
                        fill: Color::srgb(1.0, 0.0, 0.0, 1.0),
                        border: None,
                    },
                }),
            ));
            scene.set_order(&[lane]);
            scene
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

        let _guard = gpu_test_lock();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::with_capacity(2);
        for _ in 0..2 {
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                let mut renderer = GpuRenderer::new_offscreen((8, 8)).expect("shared renderer");
                // 两个线程同时往**同一张共享图集纹理**整页上传相同内容：页重建是进程级串行
                // 事件（写锁 + 有界队列 idle），上传屏障序列在同一队列上不会交错。
                let mut rows = crate::render::DirtyRows::default();
                rows.mark(0, u32::MAX);
                renderer.queue_atlas_upload(AtlasKind::Glyph, 64, vec![0x40u8; 64 * 64], rows);
                let scene = red_quad_scene();
                let mut encoder = InstanceEncoder::new();
                let buffer = encoder
                    .encode(
                        &scene,
                        Scale::ONE,
                        &ColorResolver,
                        &crate::paint::geometry_store::GeometryStore::new(),
                    )
                    .expect("encode");
                // 尽量让两个线程的录制 / 提交重叠（真正的并发点）。
                barrier.wait();
                let image = renderer
                    .render_snapshot(buffer)
                    .expect("concurrent snapshot");
                assert_eq!(pixel(&image, 8, 3, 3), [255, 0, 0, 255]);
                assert_eq!(pixel(&image, 8, 0, 0), [0, 0, 0, 0]);
            }));
        }
        for thread in threads {
            thread.join().expect("concurrent gpu thread must not panic");
        }
    }
}

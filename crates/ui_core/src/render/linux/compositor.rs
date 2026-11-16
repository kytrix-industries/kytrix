//! 帧编排：上传 → 逐 damage scissor 画进持久 offscreen → 整面 copy 到 swapchain → present
//! （`design/phase-12-macos-linux.md` §6.6 / §6.9、`design/async-render-pipeline.md` §7）。
//!
//! **两条与 Windows 不同的硬约束**（F-121）：
//! 1. `FramePlan::copy_plan()` 是 D3D12 专用：swapchain image 的内容 present 后未定义，
//!    因此**每帧整面 `cmd_copy_image`**；`present_dirty()` 只作为 `VK_KHR_incremental_present` 的 hint。
//! 2. 没有自动 barrier：本文件是 barrier 的**唯一**写点（表见 §6.6）。
//!
//! 所有权（§3.1）：[`GpuRenderer`] 是 **GPU 线程独占**的对象（device / queue / swapchain / persist /
//! upload / retire / 计时）；构造发生在窗口创建时（UI 线程），构造完成后整体移交给 `kytrix-vulkan`
//! 线程（[`super::renderer`]）。离屏 / 快照路径不走线程：门面在 UI 线程同步调用，语义与 golden 不变。
//!
//! 异步（I1 / I2 / I4）：`vkQueueSubmit` 后立即返回；`resize` 不再 `vkDeviceWaitIdle`（只用
//! `oldSwapchain` 建新的，旧资源按帧序号延迟释放）；retire 由 timeline 非阻塞推进。阻塞只发生在
//! GPU 线程上：槽 fence（复用命令/上传缓冲的前置条件）与 `vkAcquireNextImageKHR`（100 ms 上限）。

use std::{collections::VecDeque, time::Instant};

use ash::vk;
use wayland_client::protocol::wl_surface;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

use crate::{
    error::EngineError,
    math::Size,
    paint::{atlas::AtlasKind, instance::FrameOp},
    platform::linux::dpi,
    render::{
        RenderStats,
        compositor::{FramePlan, Plan},
        frame::{FramePayload, FrameRequest, FrameSlot, MAX_FRAMES_IN_FLIGHT},
        linux::{
            atlas::AtlasTextures,
            device::{Device, GpuObject, init_error, on_gpu_thread},
            geometry::GeometryRegistry,
            instance::VulkanInstance,
            pipeline::Pipeline,
            swapchain::{Acquire, Present, Swapchain},
            timer::GpuTimer,
        },
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample, Section},
        uniform::RootUniform,
    },
    window::FrameIndex,
};
#[cfg(feature = "test-support")]
use crate::{
    paint::instance::InstanceBuffer,
    render::linux::readback::{Readback, ReadbackImage},
};

/// `minStorageBufferOffsetAlignment` 的保守上界（256 B）：clips 段在此偏移处开始。
const CLIP_ALIGN: u64 = 256;

/// 连续 `OUT_OF_DATE` 的重建上限：正常合成器一次重建就能恢复；超过说明 surface 已失效，
/// 上报强类型错误（不 panic、不无限自旋）。
const MAX_OUT_OF_DATE_RETRIES: u32 = 4;

/// 离屏渲染目标格式：与 Wayland swapchain 的首选格式一致（`swapchain.rs` 的选择顺序）。
const OFFSCREEN_FORMAT: vk::Format = vk::Format::B8G8R8A8_SRGB;

/// 一帧异步提交的结果（窗口路径）。
pub(crate) enum Submitted {
    /// 已提交（可能已呈现）；`stats` / `samples` 是本帧的，`slot` 是提交用的 ring 槽位
    /// （完成观测点据此 resolve GPU 采样），`timeline_value` 是本帧 submit 时 signal 的 timeline
    /// 值（完成判据，§4.1）。
    Presented {
        stats: RenderStats,
        samples: Vec<GpuTimerSample>,
        slot: usize,
        timeline_value: u64,
    },
    /// 本帧没有产生 GPU 工作（acquire 未就绪 / 刚重建 swapchain）：调用方保留帧请求重试。
    Retry,
}

/// 已提交、尚未被 timeline 确认完成的帧（timeline 信号值 → 序号）。
#[derive(Clone, Copy)]
struct InFlightFrame {
    signal: u64,
    index: FrameIndex,
}

/// 一个窗口的 GPU 渲染器（**GPU 线程独占**；离屏路径由门面同步调用）。
pub(crate) struct GpuRenderer {
    /// 真实窗口才有 swapchain；离屏（快照）渲染器只有 `persist`。
    swapchain: Option<Swapchain>,
    /// 当前持久绘制目标（与 swapchain 同尺寸、同格式）。
    persist: Persist,
    /// 旧 persist 的延迟销毁（I4）：`FrameIndex` 完成之后才 drop（`Persist::drop` 销毁句柄）。
    retired_persists: Vec<(FrameIndex, Persist)>,
    /// 旧 swapchain 的延迟销毁（§7.4）：`Some(index)` = 新 swapchain 在该帧成功 present 过；
    /// `None` = 新 swapchain 还没 present 成功（不能销毁，合成器可能仍在用旧缓冲）。
    retired_swapchains: Vec<(Option<FrameIndex>, Swapchain)>,
    pipeline: Pipeline,
    atlas: AtlasTextures,
    upload: UploadArena,
    command_pool: vk::CommandPool,
    commands: [vk::CommandBuffer; MAX_FRAMES_IN_FLIGHT],
    timer: Option<GpuTimer>,
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
    surface: (u32, u32),
    /// 窗口 DPI（root.frame.params.x；geometry 通道的逻辑到物理换算）。
    scale: f32,
    /// 静态几何的 GPU 缓冲（按需创建；换代产生的旧缓冲交 retire 队列）。
    geometry: GeometryRegistry,
    /// 渲染目标格式（swapchain 的格式，或离屏时的 `B8G8R8A8_SRGB`）。
    format: vk::Format,
    /// 下一次 present 是否必须全量：新建 / 重建之后为 `true`，present 成功后清除。
    fresh: bool,
    /// 最后一次**真正提交**的帧序号（重建资源入 retire 队列时的 tag，§4.3）。
    last_index: FrameIndex,
    /// 已提交、尚未被 timeline 确认完成的 `(timeline 值, 序号)`。
    ///
    /// `FrameIndex` 有跳号（GPU 线程跳过的帧不产生 GPU 工作），因此不能用 timeline 值反推
    /// 序号：这里记下每个真正提交的帧，完成时直接拿到对应的真实序号。
    submitted: VecDeque<InFlightFrame>,
    /// timeline 已确认完成的最大帧序号（retire 的推进点）。
    completed: FrameIndex,
    /// present 成功后需要重建 swapchain（`SUBOPTIMAL`，§7.3）。
    rebuild_swapchain: bool,
    /// 连续 `OUT_OF_DATE` 次数（防自旋）。
    out_of_date: u32,
    /// 快照帧序号（离屏路径的一次性帧；timeline 值必须单调递增）。
    #[cfg(feature = "test-support")]
    snapshot_index: u32,
    #[cfg(feature = "test-support")]
    readback: Readback,
    // 设备与实例必须**最后**声明：Rust 按声明顺序 drop，子资源先于设备释放、surface 后于设备释放。
    device: Device,
    instance: VulkanInstance,
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
        let instance = VulkanInstance::new(display, surface_ptr)?;
        let device = Device::new(&instance)?;
        let swapchain = Swapchain::new(&instance, &device, size)?;
        Self::with_device(
            instance,
            device,
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
        Self::with_headless(size)
    }

    fn with_headless(size: (u32, u32)) -> Result<Self, EngineError> {
        let size = (size.0.max(1), size.1.max(1));
        let instance = VulkanInstance::new_headless()?;
        let device = Device::new(&instance)?;
        // 离屏路径没有窗口背景色：保持既有的透明黑（golden / 快照不变）。
        Self::with_device(instance, device, None, size, [0.0; 4], None, None)
    }

    fn with_device(
        instance: VulkanInstance,
        device: Device,
        swapchain: Option<Swapchain>,
        size: (u32, u32),
        clear_color: [f32; 4],
        viewport: Option<WpViewport>,
        wl_surface: Option<wl_surface::WlSurface>,
    ) -> Result<Self, EngineError> {
        let mut device = device;
        let format = swapchain
            .as_ref()
            .map_or(OFFSCREEN_FORMAT, Swapchain::format);
        let pipeline = Pipeline::new(&device, format)?;
        // 图集页懒分配：初始化只有 1×1 占位页，描述符集合始终有效（DEVIATIONS F-208）。
        let atlas = AtlasTextures::new(&mut device)?;
        pipeline.write_atlas(atlas.glyph_view(), atlas.image_view(), atlas.sampler());
        let persist = Persist::new(&device, &pipeline, size, format)?;
        let upload = UploadArena::new(&mut device)?;
        let (command_pool, commands) = create_commands(&device)?;
        let timer = GpuTimer::new(&device)?;
        Ok(Self {
            swapchain,
            persist,
            retired_persists: Vec::new(),
            retired_swapchains: Vec::new(),
            pipeline,
            atlas,
            upload,
            command_pool,
            commands,
            timer,
            clear_color,
            viewport,
            last_destination: None,
            wl_surface,
            last_buffer_scale: 1,
            surface: size,
            scale: 1.0,
            geometry: GeometryRegistry::new(),
            format,
            fresh: true,
            last_index: FrameIndex::default(),
            submitted: VecDeque::with_capacity(MAX_FRAMES_IN_FLIGHT),
            completed: FrameIndex::default(),
            rebuild_swapchain: false,
            out_of_date: 0,
            #[cfg(feature = "test-support")]
            snapshot_index: 0,
            #[cfg(feature = "test-support")]
            readback: Readback::new(),
            device,
            instance,
        })
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
    /// 进延迟销毁队列（等新 swapchain 的首帧完成，I4）。
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
        // 先建新 persist：失败时状态完全不变（新 persist 未被任何提交引用，drop 即安全）。
        let persist = Persist::new(&self.device, &self.pipeline, size, self.format)?;
        if self.swapchain.is_some() {
            self.replace_swapchain(size)?;
        }
        let old = std::mem::replace(&mut self.persist, persist);
        if self.swapchain.is_some() {
            // 窗口路径：旧 persist 可能仍被在飞命令缓冲引用 ⇒ 按帧序号延迟释放（I4）。
            self.retired_persists.push((self.last_index, old));
        } else {
            // 离屏路径本来就同步（`finish_sync` 已等完每次提交）⇒ 旧 persist 可以立刻释放。
            drop(old);
        }
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
        if let Some(error) = self.device.lost_error() {
            return Err(error);
        }
        // DPI 与 surface 同帧过线：geometry 通道的顶点是逻辑像素，root uniform 的
        // `frame.params.x` 必须与这一帧的 surface 成对（contract §6）。
        self.scale = request.scale;
        let slot = self.device.slot();
        // ① 槽 fence：复用命令缓冲 / 上传缓冲的**前置条件**（允许阻塞：GPU 线程，§8.2）。
        //
        // 必须在 acquire **之前**：`image_available[slot]` 由 `acquire` signal，而该信号量可能
        // 仍被本槽上一次提交的 wait 持有；只有等本槽 fence 才能证明那次提交已经消费了它。
        // acquire 拿一个已 signal 的二值信号量再次 signal 是非法用法
        // （VUID-vkAcquireNextImageKHR-semaphore-01286）。这也是 Khronos 的 frames-in-flight 顺序。
        // 复位 fence 仍然只在 `submit_commands` 里（acquire 失败时不会留下未 signal 的 fence）。
        self.device
            .wait_slot(slot)
            .map_err(|error| self.classify(error))?;
        // ② acquire：节奏原语（I3），100 ms 上限只为响应 shutdown。
        let acquire = {
            let swapchain = self.swapchain.as_ref().expect("checked above");
            swapchain.acquire(self.device.image_available(slot.index))
        };
        let image_index = match acquire {
            Ok(Acquire::Image { index, suboptimal }) => {
                if suboptimal {
                    // 本帧照常 present，之后重建（§7.3）。
                    self.rebuild_swapchain = true;
                }
                index
            }
            Ok(Acquire::NotReady) => return Ok(Submitted::Retry),
            Ok(Acquire::OutOfDate) => {
                self.out_of_date += 1;
                if self.out_of_date > MAX_OUT_OF_DATE_RETRIES {
                    return Err(EngineError::Swapchain {
                        detail: smol_str::format_smolstr!(
                            "swapchain stayed out of date after {MAX_OUT_OF_DATE_RETRIES} rebuilds"
                        ),
                    });
                }
                self.recreate_swapchain()?;
                return Ok(Submitted::Retry);
            }
            Err(error) => return Err(self.classify(error)),
        };
        self.out_of_date = 0;

        // ③ 录制 + 提交（`vkQueueSubmit` 后立即返回）。返回本帧 submit 时 signal 的 timeline 值：
        // 完成判据必须与 `vkGetSemaphoreCounterValue` 同源，不能拿 ring 的提交计数去比。
        let mut timings = FrameTimings::new();
        let (stats, timeline_value) = self.record_and_submit(
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
        let present_started = Instant::now();
        let presented = {
            let swapchain = self.swapchain.as_ref().expect("checked above");
            swapchain.present(&self.device, image_index, *frame)
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
                frame.presented();
                self.fresh = false;
                // 新 swapchain 已经成功 present 过一帧 ⇒ 旧 swapchain 的销毁条件已成立（§7.4）。
                self.arm_retired_swapchains(request.index);
            }
            Present::OutOfDate => {
                // 本帧已经提交（timeline 会 signal），不能再重录；重建后由下一帧补画。
                self.rebuild_swapchain = true;
            }
        }
        self.submitted.push_back(InFlightFrame {
            signal: timeline_value,
            index: request.index,
        });
        // GPU 采样**不在这里** resolve：该槽的 timestamp 可能还被在飞帧写；完成观测点
        // （[`Self::resolve_frame_timer`]）只在 timeline 确认之后调用（§4.1）。
        let samples = timings.samples().to_vec();

        // SUBOPTIMAL：present 之后立刻重建（§7.3），旧 swapchain 进延迟销毁队列。
        if self.rebuild_swapchain {
            self.rebuild_swapchain = false;
            self.recreate_swapchain()?;
        }
        Ok(Submitted::Presented {
            stats,
            samples,
            slot: slot.index,
            timeline_value,
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
        if let Some(error) = self.device.lost_error() {
            return Err(error);
        }
        let slot = self.device.slot();
        // 离屏路径本来就同步：复用槽位前等 fence（与窗口路径同一前置条件）。
        self.device.wait_slot(slot)?;
        let (stats, _) = self.record_and_submit(index, slot, payload, frame, None, timings)?;
        self.finish_sync(index, slot, timings)?;
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
        let value = self.device.completed_value()?;
        while let Some(frame) = self.submitted.front().copied() {
            if frame.signal > value {
                break;
            }
            self.submitted.pop_front();
            self.completed = frame.index;
        }
        if self.completed != FrameIndex::default() {
            self.device.drain_retire(self.completed);
            self.retired_persists.retain(|(at, _)| *at > self.completed);
            self.retired_swapchains
                .retain(|(armed, _)| armed.is_none_or(|at| at > self.completed));
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
    /// `pixels` 按**所有权**移动进来（`AtlasTextures` 的待上传槽直接持有它），避免槽 → 纹理
    /// 之间再复制一次整页（2048² 的图片页是 16 MiB）。
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        page: u32,
        pixels: Vec<u8>,
        rows: crate::render::atlas::DirtyRows,
    ) {
        self.atlas.queue_upload(kind, page, pixels, rows);
    }

    /// 该图集是否有一笔上传已入队、还没被命令缓冲消费（离屏目标的空闲判据）。
    pub(crate) fn atlas_pending(&self, kind: AtlasKind) -> bool {
        self.atlas.pending(kind)
    }

    /// 读回持久 offscreen（快照路径；同步等待 GPU）。
    ///
    /// 调用方（`submit_snapshot` → `finish_sync`）已经等过本帧的槽 fence，所以这里只需要
    /// 读回命令自己的 `queue_wait_idle`；**不用** timeline 等帧（§7.2 硬规则 2：timeline 只做 retire）。
    #[cfg(feature = "test-support")]
    pub(crate) fn readback(&mut self) -> Result<ReadbackImage, EngineError> {
        let command = self.commands[0];
        let image = self.persist.image;
        let size = self.surface;
        // SAFETY: 命令缓冲由本结构独占；上一次使用它的提交已由 `finish_sync` 等过槽 fence。
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
    #[cfg(feature = "test-support")]
    pub(crate) fn render_snapshot(
        &mut self,
        buffer: &InstanceBuffer,
    ) -> Result<ReadbackImage, EngineError> {
        // 一次性帧的序号在渲染器内部单调递增：timeline 的 signal 值必须严格递增。
        self.snapshot_index += 1;
        let index = FrameIndex::from_raw(self.snapshot_index);
        let payload = FramePayload::from_buffer(buffer);
        let mut timings = FrameTimings::new();
        let mut frame = FramePlan::new(Plan::Full, true);
        self.submit_snapshot(index, &payload, &mut frame, &mut timings)?;
        self.readback()
    }

    /// 录制 + 提交一帧（不呈现、不等完成）。返回本帧的统计与 submit 时 signal 的 timeline 值
    /// （调用方按它判断完成，不自己重复计算）。
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
        let upload_started = Instant::now();
        let (_instance_bytes, clip_offset, _clip_bytes) = self.upload.write(
            &mut self.device,
            index,
            slot.index,
            payload.instances(),
            bytemuck::cast_slice(payload.clips()),
        )?;
        self.pipeline
            .write_slot(slot.index, self.upload.buffer(slot.index), clip_offset);
        timings.push(GpuTimerSample {
            section: Section::Upload,
            cpu_ns: elapsed_ns(upload_started),
            gpu_ns: 0,
        });

        let command = self.commands[slot.index];
        // 图集页尺寸变化必须在开始录制之前落地：重建 image / view 要重写描述符集合，而旧
        // image 必须等 GPU 空闲后才能销毁 ⇒ `prepare` 内部先 `queue_wait_idle`
        // （DEVIATIONS F-208）。
        let atlas_rebuilt = self.atlas.prepare(&mut self.device)?;
        if atlas_rebuilt.glyph || atlas_rebuilt.image {
            self.pipeline.write_atlas(
                self.atlas.glyph_view(),
                self.atlas.image_view(),
                self.atlas.sampler(),
            );
        }
        // SAFETY: 命令缓冲由本结构的命令池分配；本槽的上一次提交已由 `wait_slot` 等到。
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
            self.atlas
                .record_uploads(&mut self.device, index, slot.index, command)?;
        }

        let full = frame.is_full();
        let draw_calls = self.draw(command, slot.index, payload, frame, full)?;
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

        // timeline 值 = 本帧序号：完成观测点（`vkGetSemaphoreCounterValue`）与 retire 都按它推进。
        let timeline_value = u64::from(index.get());
        self.submit_commands(slot, command, timeline_value, image_index)?;
        self.device.advance(slot);
        self.last_index = index;
        Ok((
            RenderStats {
                draw_calls,
                instances: (payload.instances().len() / 128) as u32,
            },
            timeline_value,
        ))
    }

    /// 同步路径的收尾：等本帧完成 → 读本帧 GPU 时间 → 推进 retire（I4）。
    fn finish_sync(
        &mut self,
        index: FrameIndex,
        slot: FrameSlot,
        timings: &mut FrameTimings,
    ) -> Result<(), EngineError> {
        // 同步语义：本帧命令必须完成，GPU 时间与随后的读回才有意义。
        self.device.wait_slot(slot)?;
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
        self.device.drain_retire(index);
        Ok(())
    }

    /// 用 `oldSwapchain` 建新 swapchain（§7.4）：**不等设备空闲**，旧结构进延迟销毁队列。
    fn recreate_swapchain(&mut self) -> Result<(), EngineError> {
        let size = self.surface;
        self.replace_swapchain(size)
    }

    /// 目标尺寸变化时的 swapchain 替换（同尺寸的 `OUT_OF_DATE` / `SUBOPTIMAL` 走
    /// [`Self::recreate_swapchain`]）。
    fn replace_swapchain(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        let old = self.swapchain.take();
        let created = match old.as_ref() {
            Some(old) => {
                Swapchain::replace(&self.instance, &self.device, size, old.raw(), self.format)
            }
            None => Swapchain::new(&self.instance, &self.device, size),
        };
        match created {
            Ok(swapchain) => {
                if let Some(old) = old {
                    // `None` = 还没在新 swapchain 上 present 成功，暂不销毁（§7.4）。
                    self.retired_swapchains.push((None, old));
                }
                self.swapchain = Some(swapchain);
                self.fresh = true;
                self.surface = size;
                Ok(())
            }
            Err(error) => {
                if let Some(old) = old {
                    // 规范：传给 `oldSwapchain` 的旧 swapchain 即使创建失败也已 retired，必须销毁；
                    // 在飞帧的完成由 timeline 记账（等 `last_index` 完成即可释放）。
                    self.retired_swapchains.push((Some(self.last_index), old));
                }
                Err(error)
            }
        }
    }

    /// 新 swapchain 成功 present 之后：旧 swapchain 的销毁条件成立（§7.4）。
    fn arm_retired_swapchains(&mut self, index: FrameIndex) {
        for (armed, _) in &mut self.retired_swapchains {
            if armed.is_none() {
                *armed = Some(index);
            }
        }
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

    fn draw(
        &mut self,
        command: vk::CommandBuffer,
        slot: usize,
        payload: &FramePayload,
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
        // 帧级常量：viewport + DPI。每次 draw op 在它之上叠加实例基址（`with_instance_base`）。
        let root = root_uniform(extent, self.scale);
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
            let push_bytes = bytemuck::cast_slice::<f32, u8>(root.words());
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

        // 几何换代（槽位复用）产生的旧设备对象：录制块结束后交给 retire 队列。
        let mut geometry_retired: Vec<GpuObject> = Vec::new();
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
                draw_calls = draw_ops(
                    &self.device,
                    command,
                    self.pipeline.layout(),
                    &self.pipeline,
                    &mut self.geometry,
                    &mut geometry_retired,
                    payload,
                    root,
                    [0, 0, extent.width as i32, extent.height as i32],
                )?;
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
                        draw_calls = draw_calls.saturating_add(draw_ops(
                            &self.device,
                            command,
                            self.pipeline.layout(),
                            &self.pipeline,
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
            self.device.raw().cmd_end_render_pass(command);
        }
        // 几何换代产生的旧设备对象：按当前帧序号入 retire 队列（等该帧完成后释放，I4）。
        for object in geometry_retired {
            let handle = self.device.allocate(object);
            self.device.retire(self.last_index, handle);
        }
        Ok(draw_calls)
    }

    /// persist → swapchain 整面 copy + 两侧 barrier（§6.6 的表）。
    fn copy_to_swapchain(
        &self,
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
            // swapchain image 的首次使用是本次 layout transition + 整面 copy（都是 TRANSFER）：
            // src/dst 都必须是 TRANSFER，才能排在 `image_available` 的 acquire wait（同样在
            // TRANSFER 阶段）之后。用 `TOP_OF_PIPE` 会与 acquire 信号量没有依赖链，syncval 报
            // `SYNC-HAZARD-WRITE-AFTER-READ`（F-168）。
            self.device.raw().cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
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
        image_index: Option<u32>,
    ) -> Result<(), EngineError> {
        let commands = [command];
        // 窗口路径：等本帧槽的 `image_available`（acquire 时 signal），signal 本 image 的
        // `render_finished`。离屏路径两者都没有。
        let waits: Vec<vk::Semaphore> = image_index
            .map(|_| self.device.image_available(slot.index))
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
        Ok(())
    }

    /// 把底层错误归类为设备丢失（粘性）或原样返回。
    fn classify(&mut self, error: EngineError) -> EngineError {
        if matches!(error, EngineError::DeviceLost { .. })
            && let Some(sticky) = self.device.note_failure(
                vk::Result::ERROR_DEVICE_LOST,
                u64::from(self.last_index.get()),
            )
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
        // 退出路径（§7.7）：`vkDeviceWaitIdle` 允许，且这是唯一一次。之后所有延迟销毁的资源
        // 都可以直接释放（旧 swapchain / 旧 persist / retire 队列里的句柄）。
        // SAFETY: 队列由本结构的设备创建；销毁命令池前必须等队列空闲。
        unsafe {
            let _ = self.device.raw().queue_wait_idle(self.device.queue());
            self.device
                .raw()
                .destroy_command_pool(self.command_pool, None);
        }
        self.retired_persists.clear();
        self.retired_swapchains.clear();
        let last = self.last_index;
        self.device.drain_retire(last);
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
}

impl Persist {
    fn new(
        device: &Device,
        pipeline: &Pipeline,
        size: (u32, u32),
        format: vk::Format,
    ) -> Result<Self, EngineError> {
        // SAFETY: 句柄归本结构独占；`Drop` / 延迟销毁里销毁。
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
            // SAFETY: 句柄归本结构独占；`GpuRenderer::drop` 已等队列空闲。
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
    layout: vk::PipelineLayout,
    pipeline: &Pipeline,
    geometry: &mut GeometryRegistry,
    retired: &mut Vec<GpuObject>,
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
                    device.raw().cmd_bind_pipeline(
                        command,
                        vk::PipelineBindPoint::GRAPHICS,
                        pipeline.quad(),
                    );
                    device.raw().cmd_push_constants(
                        command,
                        layout,
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        push_bytes,
                    );
                    // 4 顶点 unit quad + 实例区间 [0, len)；段起点由 shader 侧的 draw.clip.y 承担。
                    device.raw().cmd_draw(command, 4, len, 0, 0);
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
                let push_bytes = bytemuck::cast_slice::<f32, u8>(uniform.words());
                // SAFETY: 顶点/索引缓冲由 `GeometryRegistry` 持有到本帧结束；
                // push constant 范围由 pipeline layout 声明。
                unsafe {
                    device.raw().cmd_bind_pipeline(
                        command,
                        vk::PipelineBindPoint::GRAPHICS,
                        pipeline.geometry(entry.pipeline()),
                    );
                    device.raw().cmd_push_constants(
                        command,
                        layout,
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        push_bytes,
                    );
                    device
                        .raw()
                        .cmd_bind_vertex_buffers(command, 0, &[entry.vertex()], &[0]);
                    device.raw().cmd_bind_index_buffer(
                        command,
                        entry.index(),
                        0,
                        entry.index_type(),
                    );
                    device
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
    device: &Device,
) -> Result<(vk::CommandPool, [vk::CommandBuffer; MAX_FRAMES_IN_FLIGHT]), EngineError> {
    // SAFETY: 池与命令缓冲归 `GpuRenderer` 独占；`Drop` 里销毁池。
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
    use super::destination_update;
    use crate::math::Size;

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
}

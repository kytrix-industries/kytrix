//! 帧编排：上传 → 逐 damage scissor 画进持久 offscreen → **差分 copy** 到 swapchain → present
//!
//! **两条硬约束**：
//! 1. swapchain image 的内容在 present 之后对应用不可信 ⇒ 每张 image 相对 persist 还差哪些矩形
//!    必须**记账**（[`ImageCopyPlan`]，与 D3D12 的 `copy_plan` / Metal 的 `DrawableSyncs` 同构）。
//!    旧实现每帧整面 `cmd_copy_image`（1080p@120Hz ≈ 1.0 GB/s，4K ≈ 4 GB/s 纯带宽），把
//!    damage/scissor 的收益全部抵消（R11-12）。present 的 hint 现在是**实际拷贝区域**，
//!    同时作为 `VK_KHR_incremental_present` 的 region —— 两者必须一致。
//! 2. **差分拷贝与 `UNDEFINED` 互斥**（AR3 §5.2 的 P1）：`oldLayout = UNDEFINED` 的转场声明
//!    「整张 image 的内容允许丢弃」，而差分帧只覆盖 `pending ∪ written` 这些矩形 —— 两者同时
//!    出现时，矩形之外的全是未定义内容（直扫输出 / 压缩 framebuffer 的驱动上整窗花屏，lavapipe
//!    离屏 golden 测不到）。因此布局按拷贝范围选择（[`plan_swapchain_copy`]）：
//!    整面拷贝才用 `UNDEFINED`；差分拷贝用 `PRESENT_SRC_KHR` 作 `oldLayout` 声明**内容保留**
//!    （acquire 之后 image 的真实布局，Chrome / wgpu 的 WSI 标准写法）；没有像素要搬时连布局
//!    都不碰。这条不变量的直接后果：**`CopyPlan::Regions` ⇒ 必然 `PRESENT_SRC_KHR`**，
//!    单测钉死在 `plan_swapchain_copy` 上（[`ImageSyncStep`]）。
//!
//!    差分拷贝本身依赖「WSI 在 present → acquire 之间保留 image 内容」这一前提（D3D12 的 flip
//!    模型、Metal 的 drawable 池是同一前提；`VK_KHR_incremental_present` 的语义就建立在这上面）。
//!    这里把它写清楚，是因为它**不再**被「每帧 `UNDEFINED` 整面拷贝」掩盖：一旦某驱动丢弃内容，
//!    症状是差分帧里没搬的矩形显示旧内容 —— 只有真机肉眼 / 合成器截图能发现（离屏 golden 走不到
//!    present）。彻底不依赖该前提的唯一办法是关掉差分（AR3 §5.2 的选项 b），本波不做（R11-12 的
//!    收益正是差分）。
//! 3. 没有自动 barrier：本文件是 barrier 的**唯一**写点（表见 §6.6）。
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
//! 热路径上的阻塞只有槽 fence（有界 2 s）与 `vkAcquireNextImageKHR`（100 ms 上限）；旧 swapchain
//! 的销毁在 present 路径**零等待**（C9：timeline 完成 + 新链首 present 之后异步 drop）。队列
//! idle 只保留在 teardown（有界 2 s）与 C7 的 surface 重建强制入口。
//!
//! 共享图集的并发契约（多窗口）：**读锁**覆盖「页对齐 → 录制 → 提交」整个区间（页在帧内不会
//! 变）；待上传页与共享页不一致时先释放读锁，在 `SharedGpu::rebuild_atlas_pages` 的**写锁**里
//! 落地页换代，然后重试。多个窗口同时往同一张纹理上传因此是安全的（同一队列按提交顺序执行，
//! 屏障序列不交错）。

use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use ash::vk;
use wayland_client::protocol::wl_surface;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

#[cfg(feature = "test-support")]
use super::readback::{Readback, ReadbackImage};
use super::{
    atlas::AtlasUploads,
    device::{
        DeferredHandles, DescriptorLease, FrameSync, GpuBuffer, SharedGpu, init_error,
        on_gpu_thread,
    },
    geometry::GeometryRegistry,
    memory::MemoryNeed,
    pipeline::{Pipeline, RENDER_FORMAT},
    swapchain::{Acquire, CreateFailure, Present, Surface, Swapchain},
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

/// `SUBOPTIMAL` 的重建上限（**只属于 SUBOPTIMAL 策略**）：持续 SUBOPTIMAL 时每帧重建会退化成
/// 「帧率崩塌 + 句柄/显存抖动」，超过上限就放弃重建、继续用现有链呈现（R11-7）。
///
/// `OUT_OF_DATE` 不共用这个常量：它是 WSI 的正常控制流，按 [`OutOfDateRecovery`] 无限重建 +
/// 退避（用户裁决 C8）。
const MAX_SUBOPTIMAL_REBUILDS: u32 = 4;

/// 任何可恢复 WSI 重试的退避上限（用户裁决：≤50 ms）。
///
/// GPU 循环的 `thread::sleep` 因此最多多等这么久；关窗 `join()` 的尾延迟有同一个上界。
pub(crate) const MAX_RETRY_BACKOFF: Duration = Duration::from_millis(50);

/// 一次 `SURFACE_LOST` 恢复失败后的重试退避序列：3 次有界重试（用户裁决 C8）。
const SURFACE_LOST_BACKOFF: [Duration; 3] = [
    Duration::from_millis(1),
    Duration::from_millis(4),
    Duration::from_millis(16),
];

/// 重建之后至少多少帧不再重建（即使 acquire / present 仍报 `SUBOPTIMAL`）。
///
/// `VK_SUBOPTIMAL_KHR` 可以**持续**返回（extent / transform / colorSpace 与合成器期望不符），
/// 而每次重建都要 `vkCreateSwapchainKHR` + `vkGetSwapchainImagesKHR` + N 个信号量。没有这个门槛
/// 时，持续 SUBOPTIMAL 会退化成每帧重建：帧率崩塌 + 句柄/显存抖动，现象像「卡顿」而不是错误。
const REBUILD_COOLDOWN_FRAMES: u64 = 60;

/// 一次可恢复 WSI 故障之后的等待决策（[`OutOfDateRecovery`] 的输出）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Retry {
    /// 下一次重试之前必须等待的时长（`OUT_OF_DATE` 恒 >0；`SURFACE_LOST` 按预算取 1/4/16 ms）。
    after: Duration,
    /// 本次是否要打一行低频诊断（第 1 / 16 次，之后每 64 次；不刷屏）。
    report: bool,
}

/// `OUT_OF_DATE` / `SURFACE_LOST` 的恢复策略（**纯逻辑**，无 GPU 依赖，可单测）。
///
/// 用户裁决（C8）：
/// - `VK_ERROR_OUT_OF_DATE_KHR` 是 WSI 的**正常控制流**（acquire / present 都会返回它）：
///   无限重建 + 指数退避（1/2/4/… ≤50 ms），**永不**因它产生致命错误或结束事件循环；
///   成功 acquire / present 之后计数与退避复位。对齐 Chromium（OOD = swap 失败后重建）与
///   Qt（`beginFrame` 返回 `FrameOpSwapChainOutOfDate`）的处理方式。
/// - `VK_ERROR_SURFACE_LOST_KHR` 有界重试：最多 3 次退避重试（1/4/16 ms），仍失败才上报
///   「合成器/连接还活着但 surface 不可用」的强类型错误（区别于设备丢失）。
#[derive(Debug, Default)]
struct OutOfDateRecovery {
    /// 连续 `OUT_OF_DATE` 次数（成功 acquire / present 后清零）。
    out_of_date: u32,
    /// 连续 surface 恢复失败次数（成功 acquire / present 后清零）。
    surface_lost_failures: u32,
}

impl OutOfDateRecovery {
    const fn new() -> Self {
        Self {
            out_of_date: 0,
            surface_lost_failures: 0,
        }
    }

    /// 一次 `OUT_OF_DATE`：无限重建，退避 = 1/2/4/8/16/32/50 ms（封顶）。
    fn on_out_of_date(&mut self) -> Retry {
        self.out_of_date += 1;
        Retry {
            after: out_of_date_backoff(self.out_of_date),
            report: out_of_date_diagnostic(self.out_of_date),
        }
    }

    /// 一次成功 acquire / present：计数与退避一起复位。
    ///
    /// 返回上一段连续 OOD 的次数：>1 时才值得打一行「已恢复」诊断（第 1 次 OOD 立刻恢复属于
    /// 正常控制流，不刷屏）。
    fn on_progress(&mut self) -> Option<u32> {
        let recovered = self.out_of_date;
        self.out_of_date = 0;
        self.surface_lost_failures = 0;
        (recovered > 1).then_some(recovered)
    }

    /// 一次 `SURFACE_LOST` 恢复失败：预算内返回本次重试前的退避，耗尽后上报。
    ///
    /// 预算是「3 次重试」：第 1/2/3 次失败分别退避 1/4/16 ms，第 4 次失败才返回错误。
    fn on_surface_lost_failure(&mut self, reason: &'static str) -> Result<Duration, EngineError> {
        self.surface_lost_failures += 1;
        SURFACE_LOST_BACKOFF
            .get(self.surface_lost_failures as usize - 1)
            .copied()
            .ok_or_else(|| EngineError::Swapchain {
                detail: smol_str::format_smolstr!(
                    "compositor/connection is still alive but the swapchain surface stayed lost \
                     after {SURFACE_LOST_BACKOFF_LEN} retries ({reason})"
                ),
            })
    }
}

/// `SURFACE_LOST_BACKOFF` 的条目数（错误文案与预算单测共用同一事实）。
const SURFACE_LOST_BACKOFF_LEN: usize = SURFACE_LOST_BACKOFF.len();

/// `OUT_OF_DATE` 的指数退避：第 n 次连续失败等 `min(2^(n-1) ms, 50 ms)`。
fn out_of_date_backoff(consecutive: u32) -> Duration {
    debug_assert!(consecutive >= 1, "the first out-of-date retry waits 1 ms");
    let exponent = consecutive.saturating_sub(1).min(6);
    Duration::from_millis(1u64 << exponent).min(MAX_RETRY_BACKOFF)
}

/// `OUT_OF_DATE` 诊断行的低频策略：第 1 / 16 次，之后每 64 次。
///
/// 1000 次连续 OOD 只产生 ~17 行；交互 resize 造成的偶发 OOD 不刷屏（用户裁决「低频诊断」）。
fn out_of_date_diagnostic(consecutive: u32) -> bool {
    consecutive == 1 || consecutive == 16 || consecutive.is_multiple_of(64)
}

/// 旧 swapchain 的**非阻塞**延迟销毁台账（C9）；`retired_persists` 也复用同一结构。
///
/// 每条 `(tag, value)` 的 `tag` 是「最后一次使用该资源的提交序号」（`last_sequence`，
/// **不是** `FrameIndex`：UI 帧序号会跳号 / 饱和，R11-1）。timeline 确认 `completed >= tag`
/// 之后才把 value 交给 `drop_fn` 真正释放 —— 与 `RetireQueue`（GPU 资源句柄）同一语义，
/// 但对象类型不同（Swapchain / Persist 持有的是 Vulkan 对象 + 它们的 Drop）。
///
/// **present 热路径零阻塞**：`drain_ready` 只比较已观测到的 timeline 值，不做任何等待。
struct Retired<T> {
    entries: Vec<(u64, T)>,
}

impl<T> Retired<T> {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 入账：`tag` 必须是提交序号（调用方只从 `last_sequence` 取，见 `retire_swapchain`）。
    fn retire(&mut self, tag: u64, value: T) {
        self.entries.push((tag, value));
    }

    /// 非阻塞释放：`tag <= completed` 的条目按入账顺序交给 `drop_fn`。
    fn drain_ready(&mut self, completed: u64, mut drop_fn: impl FnMut(T)) {
        let mut index = 0;
        while index < self.entries.len() {
            if self.entries[index].0 <= completed {
                let (_, value) = self.entries.remove(index);
                drop_fn(value);
            } else {
                index += 1;
            }
        }
    }

    /// 取出全部条目（保持顺序）；调用方负责 drop。teardown / C7 强制入口用。
    fn take_all(&mut self) -> Vec<(u64, T)> {
        std::mem::take(&mut self.entries)
    }

    /// 条数（安全阀 `> MAX_RETIRED_SWAPCHAINS` 与测试用）。
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 只读遍历 value（leak 兜底要把台账里的每个对象都标记成「交给进程级回收」）。
    fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.entries.iter_mut().map(|(_, value)| value)
    }
}

/// 延迟 swapchain 的容量上限（C9 安全阀）：`MAX_FRAMES_IN_FLIGHT + 1`。
///
/// 正常节奏下旧链在下一帧 present 后就被释放（≤2 条）；极端「慢 GPU + 高频 resize」时不允许
/// 链 / 显存无限堆积，超过上限就退回 C7 的强制销毁（一次有界队列 idle）。
const MAX_RETIRED_SWAPCHAINS: usize = MAX_FRAMES_IN_FLIGHT + 1;

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
    /// 本帧没有产生 GPU 工作（acquire 未就绪 / 可恢复的 WSI 故障）：调用方**保留**帧请求，
    /// 按 `after` 有界退避后重试（`None` = 立即重试；GPU 循环的 sleep 上限见
    /// [`MAX_RETRY_BACKOFF`]）。`OUT_OF_DATE` / `SURFACE_LOST` 走这里，**永不**因此退出进程。
    Retry { after: Option<Duration> },
}

/// 已提交、尚未被 timeline 确认完成的帧的**提交序号**（timeline 的 signal 值 = ring 的提交序号）。
///
/// UI 帧序号（`FrameIndex`）不参与完成判据：它只随反馈 / 诊断走（`renderer::SubmittedFrame`），
/// 放在这里会诱使后来者拿它去比 timeline 值 —— 那正是 R11-1 的成因。
type InFlightSequence = u64;

/// 本帧要把 persist 的哪些像素同步进 presentable image，以及**允许丢弃哪些内容**。
///
/// 这是 AR3 §5.2 那条 P1 的落点：`VK_KHR_incremental_present` 的差分拷贝只有在「没搬的像素
/// 仍然是上一次写进去的内容」时才成立，而 `oldLayout = UNDEFINED` 的转场**恰好声明了相反的东西**
/// （整张 image 的内容允许丢弃）。两者不能同时存在，因此：
///
/// - 整面拷贝（每个像素都会被重写）⇒ `UNDEFINED`（省掉一次读，内容本来就要丢）；
/// - 差分拷贝 ⇒ `PRESENT_SRC_KHR` 作 `oldLayout`（**内容保留**：这是 acquire 之后 image 的真实
///   布局，也是 WSI 的标准写法）；
/// - 没有像素要搬 ⇒ 连布局都不碰（image 已经在 `PRESENT_SRC_KHR`，present 照样合法）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageSyncStep {
    /// 整面拷贝：`UNDEFINED → TRANSFER_DST_OPTIMAL` + 一个覆盖整张 image 的 `vkCmdCopyImage`。
    Full,
    /// 差分拷贝：`PRESENT_SRC_KHR → TRANSFER_DST_OPTIMAL`（内容保留）+ 只拷这些矩形
    /// （**已夹到 swapchain extent**，非空）。
    Rects(DamageRects),
    /// 一个像素都不搬（本帧 damage 全部落在 swapchain extent 之外）：不做任何布局转场与拷贝。
    Nothing,
}

/// 本帧 copy 步骤的完整决策（[`plan_swapchain_copy`] 的结果）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SwapchainCopy {
    /// 布局转场 + 搬运范围。
    step: ImageSyncStep,
    /// present 的 hint（`None` = 整面语义 / 不挂 `VkPresentRegionKHR`）。
    hint: Option<DamageRects>,
}

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
    /// 旧 persist 的延迟销毁（I4 / C9 台账）：提交序号完成之后才 drop（`Persist::drop` 销毁句柄）。
    retired_persists: Retired<Persist>,
    /// 旧 swapchain 的延迟销毁（§7.4 / R11-3 / C9）：tag 是最后一次使用它的提交序号；timeline
    /// 确认完成、且当前链已经成功 present 过之后由 [`Self::drain_retired_swapchains`] 非阻塞
    /// 释放（present 热路径零等待）。强制入口 [`Self::destroy_retired_swapchains_now`] 只用于
    /// C7 的 surface 重建与容量安全阀。
    retired_swapchains: Retired<Swapchain>,
    /// 当前这条 swapchain 是否已经成功 present 过至少一次（C9 的「新链首 present 门」）。
    ///
    /// `install_swapchain` 置 `false`、present 成功置 `true`。旧链只有在「新链至少 present 过
    /// 一次」之后才允许销毁 —— 对齐 VVL `ResetPresentWaitSemaphores` 的启发式：旧链的
    /// present-wait semaphore 不再被占用。unextended Vulkan 没有 present fence，这是可达的最佳
    /// 实践（Chromium 用 FenceHelper 延迟清理旧链；Qt 在 releaseSwapChainResources 里 queue
    /// idle）；它让旧链多活一帧，但**永不阻塞**。
    current_presented_once: bool,
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
    /// 本帧 present 的 `VK_KHR_incremental_present` hint（`None` = 整面语义）。
    ///
    /// **事实来源**：`copy_to_swapchain` 写、`present` 读 —— hint 必须与实际搬运的像素一致
    /// （R11-12）。唯一的例外是「本帧没有任何像素要搬」（damage 全落在 swapchain extent 之外）：
    /// 扩展没有「什么都没变」的编码（空 region 数组会被解释成整面），所以沿用上一次的 hint
    /// 而不是声称整张 image 都变了（AR3 §5.3）。
    present_hint: Option<DamageRects>,
    /// 本帧对 presentable image 做的同步动作（`copy_to_swapchain` 写、present 之后消费）。
    ///
    /// 只有 [`ImageSyncStep::Nothing`] 会让 present 之后的 `ImageCopyPlan` 推进走
    /// [`ImageCopyPlan::commit_without_copy`]（那张 image 的欠账必须保留）。
    image_sync: ImageSyncStep,
    /// present 成功后需要重建 swapchain（`SUBOPTIMAL`，§7.3）。
    rebuild_swapchain: bool,
    /// `OUT_OF_DATE` / `SURFACE_LOST` 的恢复策略（C8：无限退避重建 / surface 有界重试；
    /// 失败分类见 `swapchain::CreateFailure`）。
    out_of_date_recovery: OutOfDateRecovery,
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
        let swapchain =
            Swapchain::new(&shared, &vk_surface, size).map_err(CreateFailure::into_error)?;
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
            retired_persists: Retired::new(),
            retired_swapchains: Retired::new(),
            current_presented_once: false,
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
            present_hint: None,
            image_sync: ImageSyncStep::Full,
            rebuild_swapchain: false,
            out_of_date_recovery: OutOfDateRecovery::new(),
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
    /// 进延迟销毁台账（C9：旧链在「最后一次使用它的提交完成 + 新链首 present」之后异步 drop，
    /// 见 [`Self::drain_retired_swapchains`]；旧 persist 等提交序号完成，I4）。
    ///
    /// 渲染目标尺寸的**唯一来源是 swapchain extent**（clamp / `currentExtent` / transform 之后，
    /// R11-9），不是 UI 请求值 —— 旧实现让 persist 用未 clamp 的请求尺寸，于是 `renderArea` 可能
    /// 超出 framebuffer（`VUID-VkRenderPassBeginInfo-*`）、`vkCmdCopyImage` 的 extent 可能超出源图。
    ///
    /// C8：窗口路径的建链失败按 `CreateFailure` 分类 —— `OUT_OF_DATE` / `SURFACE_LOST` 不升级为
    /// 致命错误：链保持 absent、请求尺寸先记账，由下一次 `submit_frame` 顶部的
    /// [`Self::ensure_swapchain`] 退避补建（`SURFACE_LOST` 会先走 C7 的表面重建）。旧实现把任何
    /// 建链失败都变成粘性错误，正常 resize 竞争就能结束事件循环。
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        // 窗口目标的 resize 只能发生在 GPU 线程（§7.3）；离屏目标由门面同步调用。
        debug_assert!(
            self.vk_surface.is_none() || on_gpu_thread(),
            "resizing a window target may only run on the kytrix-vulkan thread"
        );
        let size = (size.0.max(1), size.1.max(1));
        if self.vk_surface.is_some() {
            if size == self.surface && self.swapchain.is_some() {
                return Ok(());
            }
            // 请求尺寸先记账：建链失败后的重试按最新请求尺寸走（swapchain extent 仍由
            // `install_swapchain` 在成功时覆盖，单一真相源不变）。
            self.surface = size;
            return match self.try_replace_swapchain(size) {
                Ok(()) => Ok(()),
                Err(CreateFailure::Fatal(error)) => Err(error),
                // 可恢复的 WSI 状态：链保持 absent，`submit_frame` 顶部按 C8 策略恢复。
                Err(CreateFailure::OutOfDate | CreateFailure::SurfaceLost) => Ok(()),
            };
        }
        // 离屏路径：没有 swapchain，渲染目标尺寸就是请求尺寸。
        if size == self.surface {
            return Ok(());
        }
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
    /// `Submitted::Retry` 表示本帧没有产生 GPU 工作（或遇到可恢复的 WSI 故障），调用方必须
    /// **保留**该请求并按 `after` 退避后重试（跳过它会把 damage 弄丢）。
    pub(crate) fn submit_frame(
        &mut self,
        request: &FrameRequest,
        frame: &mut FramePlan,
    ) -> Result<Submitted, EngineError> {
        debug_assert!(
            on_gpu_thread(),
            "Vulkan 窗口帧只能在 kytrix-vulkan 线程提交"
        );
        if let Some(error) = self.shared.lost_error() {
            return Err(error);
        }
        // C8：上一次可恢复的建链失败（OOD / SURFACE_LOST）可能留下 absent 的链。统一入口先补建：
        // 可恢复故障返回退避（保留 pending 帧），Fatal 才上报。
        if self.swapchain.is_none()
            && let Some(after) = self.ensure_swapchain("rebuilding the missing swapchain")?
        {
            return Ok(Submitted::Retry { after: Some(after) });
        }
        debug_assert!(
            self.swapchain.is_some(),
            "window frames require a swapchain"
        );
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
                // 拿到可呈现的 image = 进展：OOD / surface-lost 的连续计数复位（C8）。
                self.note_progress();
                index
            }
            // 超时：帧请求保留着重试（acquire 自带 100 ms 节拍，不额外退避）。
            Ok(Acquire::NotReady) => return Ok(Submitted::Retry { after: None }),
            Ok(Acquire::OutOfDate) => {
                return self.retry_out_of_date("acquire reported OUT_OF_DATE");
            }
            // R11-11：surface 永久失效 ⇒ 重建 `VkSurfaceKHR` + swapchain（旧实现把 acquire 的
            // `SURFACE_LOST` 混进 `OutOfDate`，于是 present 侧的同类错误直接结束 GPU 线程、窗口黑屏）。
            Ok(Acquire::SurfaceLost) => {
                return self.recover_surface("acquire reported SURFACE_LOST");
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
        // 我们其实已经重画过的像素（浪费）或反之。唯一的例外是「本帧没有像素要搬」（空集）：
        // 那时 hint 沿用上一次的声明（扩展没有「什么都没变」的编码，AR3 §5.3）。
        // 本帧写进 persist 的矩形：**必须在 `frame.presented()` 之前取**（那会清 `fresh`，
        // 而 `fresh` 是「整面重画」的判据；取晚了会把整帧误当成局部帧，让其余 image 少拷像素）。
        let written = frame_written(frame);
        let present_started = Instant::now();
        let presented = {
            // 本帧的拷贝计划（在 `copy_to_swapchain` 里录制，present 时只读同样的事实）。
            let hint = self.present_hint;
            let swapchain = self.swapchain.as_ref().expect("checked above");
            swapchain.present(&self.shared, image_index, hint)
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
                // 成功 present = 进展：连续 OOD / surface-lost 的计数与退避复位（C8）。
                self.note_progress();
                frame.presented();
                self.fresh = false;
                // C9 的「新链首 present 门」：旧链此刻起才允许被非阻塞销毁。
                self.current_presented_once = true;
                // 本 image 已经被这一帧追平；其余 image 累积本帧 damage（R11-12）。
                // 例外：本帧一个像素都没搬（`Nothing`）⇒ 欠账必须原样累积（AR3 §5.3），否则
                // 「persist 里变了、这张 image 上没变」的矩形会被当成已同步。
                match self.image_sync {
                    ImageSyncStep::Nothing => {
                        self.image_copy_plan
                            .commit_without_copy(image_index, written);
                    }
                    ImageSyncStep::Full | ImageSyncStep::Rects(_) => {
                        self.image_copy_plan.commit(image_index, written);
                    }
                }
                // C9：present 热路径**不再**做队列 idle；旧链由 GPU 循环的完成观测点
                // （`drain_completed` → `drain_retired_swapchains`）按 timeline 非阻塞释放。
            }
            Present::OutOfDate => {
                // present 返回 OOD 说明本帧的提交**已经发生**（timeline 会 signal）：先把提交
                // 序号登记进 retire 流水线，再保留 pending 帧、退避后重画。
                //
                // 旧实现把 present-OOD 当 `Presented` 返回，pending 帧被丢弃：resize 之后如果
                // UI 没有新帧，新链永远等不到第一次 present（黑窗），而 OOD 计数继续累计到
                // 旧的「连续 OOD 上限」（4 次）就结束事件循环（C8 的问题本体）。
                self.submitted.push_back(sequence);
                return self.retry_out_of_date("present reported OUT_OF_DATE");
            }
            // R11-11：present 侧的 `SURFACE_LOST_KHR` 走与 acquire 同一条恢复路径。
            Present::SurfaceLost => {
                self.submitted.push_back(sequence);
                return self.recover_surface("present reported SURFACE_LOST");
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
    ///
    /// C9：旧链的释放走 `Retired::drain_ready`（比较 timeline，不等待）+ 新链首 present 门；
    /// 唯一可能阻塞的是超过 [`MAX_RETIRED_SWAPCHAINS`] 的安全阀（退回 C7 的强制入口）。
    /// 链暂时缺席（C8 恢复中）时也要继续推进 ledger —— 旧实现早退会让已完成的 persist 永远
    /// 等不到释放。
    pub(crate) fn drain_completed(&mut self) -> Result<u64, EngineError> {
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
                .drain_ready(self.completed_sequence, drop);
        }
        self.drain_retired_swapchains()?;
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
    /// 之间再复制一次整页（2048² 的图片页是 16 MiB）。`layer` 是图集 texture array 的层号
    /// （字形恒 0、图片 = 图集页索引），`extent` 是该层的页边长；入队时同时记录**当前共享页**
    /// （`SharedGpu::atlas_page`），供录制前区分「App 换代的新页」与「已被别人换代的旧页」。
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        layer: u16,
        extent: u32,
        pixels: Vec<u8>,
        rects: crate::render::atlas::DirtyRects,
    ) {
        debug_assert!(
            self.swapchain.is_none() || on_gpu_thread(),
            "window atlas uploads may only be queued on the kytrix-vulkan thread"
        );
        let queued_page = self.shared.atlas_page(kind);
        self.atlas_uploads
            .queue_upload(kind, layer, extent, queued_page, pixels, rects);
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
        let payload = FramePayload::full(buffer, Vec::new(), Vec::new());
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
        // 防御性默认值：`copy_to_swapchain` 会覆盖它（离屏帧没有 swapchain，也就不会 present，
        // 不读这个值）。`present_hint` 相反，必须**跨帧保留**（空集帧沿用上一次的 hint）。
        self.image_sync = ImageSyncStep::Full;
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
        self.retired_persists.drain_ready(sequence, drop);
        Ok(())
    }

    /// 用 `oldSwapchain` 建新 swapchain（§7.4）：**不等设备空闲**，旧结构进延迟销毁队列。
    ///
    /// C8：建链失败按 [`CreateFailure`] 分类 —— 可恢复故障不在此升级为致命错误（链保持
    /// absent，下一次 `submit_frame` 顶部的 [`Self::ensure_swapchain`] 按策略补建）；Fatal 上报。
    /// SUBOPTIMAL 的「冷却期 + 放弃重建」策略不变（[`Self::request_rebuild`]）。
    fn recreate_swapchain(&mut self) -> Result<(), EngineError> {
        let size = self.surface;
        match self.try_replace_swapchain(size) {
            Ok(()) => Ok(()),
            Err(CreateFailure::Fatal(error)) => Err(error),
            // OUT_OF_DATE / SURFACE_LOST：链已 retired，统一入口会在下一次提交前恢复。
            Err(CreateFailure::OutOfDate | CreateFailure::SurfaceLost) => Ok(()),
        }
    }

    /// 尝试用 `oldSwapchain` 建新链；失败按 [`CreateFailure`] 分类返回，**不在这里**重建 surface。
    ///
    /// 链始终先 `take()` 再建：失败时旧链已按规范 retired（`oldSwapchain` 语义），放进延迟
    /// 销毁队列，由 [`Self::drain_retired_swapchains`] / [`Self::destroy_retired_swapchains_now`]
    /// 在队列空闲后销毁。surface 恢复只在 [`Self::ensure_swapchain`] / [`Self::recover_surface`]
    /// 走 C7 的唯一入口 —— 修正「任何建链失败都重建 surface」的过度归因（audit-c2 §1/§2）。
    fn try_replace_swapchain(&mut self, size: (u32, u32)) -> Result<(), CreateFailure> {
        let old = self.swapchain.take();
        let Some(surface) = self.vk_surface.as_ref() else {
            if let Some(old) = old {
                self.retire_swapchain(old);
            }
            return Err(CreateFailure::Fatal(EngineError::Swapchain {
                detail: smol_str::SmolStr::new_static(
                    "swapchain replacement requires a window surface",
                ),
            }));
        };
        let created = match old.as_ref() {
            Some(old) => Swapchain::replace(&self.shared, surface, size, old.raw()),
            None => Swapchain::new(&self.shared, surface, size),
        };
        match created {
            Ok(swapchain) => self
                .install_swapchain(swapchain, old, size)
                .map_err(CreateFailure::Fatal),
            Err(failure) => {
                if let Some(old) = old {
                    // 规范：传给 `oldSwapchain` 的旧 swapchain 即使创建失败也已 retired，必须销毁。
                    self.retire_swapchain(old);
                }
                Err(failure)
            }
        }
    }

    /// 链不在时的统一补建入口（C8；`submit_frame` 顶部调用）。
    ///
    /// 返回 `Some(delay)` = 本轮没有可用链，调用方必须保留 pending 帧、退避 `delay` 后重试；
    /// `None` = 链已就绪、继续本帧。`OUT_OF_DATE` 无限重建（退避 ≤50 ms）；`SURFACE_LOST`
    /// 先经 C7 的唯一入口重建 surface，再按 1/4/16 ms 有界重试（用尽才 `Err` 上报）。
    fn ensure_swapchain(&mut self, reason: &'static str) -> Result<Option<Duration>, EngineError> {
        if self.swapchain.is_some() {
            return Ok(None);
        }
        match self.try_replace_swapchain(self.surface) {
            Ok(()) => Ok(None),
            Err(CreateFailure::Fatal(error)) => Err(error),
            Err(CreateFailure::OutOfDate) => Ok(Some(self.wait_for_out_of_date(reason))),
            Err(CreateFailure::SurfaceLost) => {
                // C7：先把该 surface 的全部旧链强制销毁，再销毁旧 surface、建新的。
                self.recreate_vk_surface()?;
                // 新 surface 上立刻再试一次：多数合成器重启在这里恢复，不消耗重试预算。
                match self.try_replace_swapchain(self.surface) {
                    Ok(()) => Ok(None),
                    Err(CreateFailure::Fatal(error)) => Err(error),
                    Err(CreateFailure::OutOfDate) => Ok(Some(self.wait_for_out_of_date(reason))),
                    Err(CreateFailure::SurfaceLost) => {
                        Ok(Some(self.wait_for_surface_lost_retry(reason)?))
                    }
                }
            }
        }
    }

    /// 一次 `OUT_OF_DATE` 的可恢复处理（acquire / present 两个入口）。
    ///
    /// 立即尝试重建一次（OOD 不能被 SUBOPTIMAL 冷却期拦住：链已经不能 present），失败留给下一次
    /// `submit_frame` 顶部的统一入口。**永远返回 `Submitted::Retry`**：OOD 是 WSI 的正常控制流，
    /// 不能变成粘性错误 —— 旧实现里「连续 OOD 超过上限（4 次）就返回 EngineError」正是 C8 的
    /// 问题本体。
    fn retry_out_of_date(&mut self, reason: &'static str) -> Result<Submitted, EngineError> {
        self.rebuild_swapchain = false;
        let after = self.wait_for_out_of_date(reason);
        if self.swapchain.is_some() {
            match self.try_replace_swapchain(self.surface) {
                Ok(()) | Err(CreateFailure::OutOfDate | CreateFailure::SurfaceLost) => {}
                Err(CreateFailure::Fatal(error)) => return Err(error),
            }
        }
        Ok(Submitted::Retry { after: Some(after) })
    }

    /// 一次 OOD 的退避决策 + 低频诊断（第 1 / 16 次，之后每 64 次）。
    fn wait_for_out_of_date(&mut self, reason: &'static str) -> Duration {
        let retry = self.out_of_date_recovery.on_out_of_date();
        if retry.report {
            eprintln!(
                "kytrix: vulkan swapchain stayed OUT_OF_DATE ({} consecutive rebuilds, {reason}); \
                 retrying in {} ms (extent={:?})",
                self.out_of_date_recovery.out_of_date,
                retry.after.as_millis(),
                self.swapchain.as_ref().map(Swapchain::extent),
            );
        }
        retry.after
    }

    /// 一次 surface 恢复失败的退避决策 + 诊断（3 次后上报；文案区分「连接还活着」与设备丢失）。
    fn wait_for_surface_lost_retry(
        &mut self,
        reason: &'static str,
    ) -> Result<Duration, EngineError> {
        let attempts = self.out_of_date_recovery.surface_lost_failures + 1;
        let after = self.out_of_date_recovery.on_surface_lost_failure(reason)?;
        eprintln!(
            "kytrix: vulkan compositor/connection is alive but the swapchain surface is still lost \
             (recovery {attempts}/{SURFACE_LOST_BACKOFF_LEN}); retrying in {} ms",
            after.as_millis()
        );
        Ok(after)
    }

    /// 成功 acquire / present 后的复位 + 恢复诊断（C8）。
    fn note_progress(&mut self) {
        if let Some(recovered) = self.out_of_date_recovery.on_progress() {
            eprintln!(
                "kytrix: vulkan swapchain recovered after {recovered} consecutive OUT_OF_DATE rebuilds"
            );
        }
    }

    /// `VK_ERROR_SURFACE_LOST_KHR` 的恢复（R11-11 + C8 有界预算）。
    ///
    /// 顺序（C7）：旧链先取出放进延迟队列 → [`Self::recreate_vk_surface`] 强制销毁它们并重建
    /// surface → 用新 surface 建链。建链仍报 `SURFACE_LOST` 时按 1/4/16 ms 退避重试 3 次，
    /// 用尽才上报；`OUT_OF_DATE` 走正常 OOD 退避（无限重试）。
    fn recover_surface(&mut self, reason: &'static str) -> Result<Submitted, EngineError> {
        if self.vk_surface.is_none() {
            return Err(EngineError::Swapchain {
                detail: smol_str::format_smolstr!("{reason}: no window surface"),
            });
        }
        eprintln!("kytrix: vulkan {reason}: recreating VkSurfaceKHR");
        // 旧 swapchain 必须用掉（它引用旧 surface），随后进延迟销毁队列 —— 由唯一入口在销毁
        // surface **之前**强制排空。
        if let Some(old) = self.swapchain.take() {
            self.retire_swapchain(old);
        }
        self.recreate_vk_surface()?;
        match self.try_replace_swapchain(self.surface) {
            Ok(()) => Ok(Submitted::Retry { after: None }),
            Err(CreateFailure::Fatal(error)) => Err(error),
            Err(CreateFailure::OutOfDate) => Ok(Submitted::Retry {
                after: Some(self.wait_for_out_of_date(reason)),
            }),
            Err(CreateFailure::SurfaceLost) => Ok(Submitted::Retry {
                after: Some(self.wait_for_surface_lost_retry(reason)?),
            }),
        }
    }

    /// 装上新链：旧链进延迟销毁台账，差分拷贝表与描述符记账一起重置。
    ///
    /// 渲染目标的尺寸只有一个来源：**swapchain 的 `imageExtent`**（`currentExtent` / clamp /
    /// transform 之后）。同尺寸重建时它也可能变（合成器改了 `currentExtent`），所以这里比较一次，
    /// 不同就重建 persist —— 否则 `renderArea` 会超出 framebuffer（R11-9）。
    ///
    /// C9：旧链按 `last_sequence` 入账；新链的「首 present 门」置 `false`（旧链要等新链至少
    /// present 一次之后才能被非阻塞销毁）。
    fn install_swapchain(
        &mut self,
        swapchain: Swapchain,
        old: Option<Swapchain>,
        size: (u32, u32),
    ) -> Result<(), EngineError> {
        if let Some(old) = old {
            self.retire_swapchain(old);
        }
        // 新链的 image 索引与旧链无关（R11-12 / R11-9）：差分拷贝表必须整体重置。
        self.image_copy_plan.reset(swapchain.image_count());
        // 每张 image 的 render_finished 是新的；每帧槽的 `image_available` 由 `FrameSync` 持有
        // 且跨重建存活（规范允许），它们的等待值不受影响。
        let extent = swapchain.extent();
        self.swapchain = Some(swapchain);
        self.fresh = true;
        self.surface = size;
        self.current_presented_once = false;
        // 上一次 present 的 hint 属于旧链的 image：新链的 image 内容未定义，第一帧必然整面拷贝
        // （`fresh` ⇒ `Plan::Full`），hint 一并作废。
        self.present_hint = None;
        if self.persist.size != (extent.width, extent.height) {
            let persist = Persist::new(&self.shared, (extent.width, extent.height))?;
            let previous = std::mem::replace(&mut self.persist, persist);
            // 旧 persist 仍可能被在飞命令引用 ⇒ 按提交序号延迟释放（I4 / C9 台账）。
            self.retired_persists.retire(self.last_sequence, previous);
        }
        Ok(())
    }

    /// 旧 swapchain 入账（C9 的 retire 点**唯一**实现）。
    ///
    /// tag = `last_sequence`（最后一次提交的 timeline 值，**不是** `FrameIndex`：UI 帧序号会
    /// 跳号 / 饱和，R11-1）；真正销毁在 timeline 确认完成 + 新链首 present 之后，非阻塞。
    fn retire_swapchain(&mut self, old: Swapchain) {
        self.retired_swapchains.retire(self.last_sequence, old);
    }

    /// 需要重建 swapchain 时的**门槛**入口（R11-7，`SUBOPTIMAL` 专用）。
    ///
    /// `VK_SUBOPTIMAL_KHR` 可以**持续**返回（extent / transform / colorSpace 与合成器期望不符），
    /// 旧实现每收到一次就 `vkCreateSwapchainKHR` + `get_swapchain_images` + N 个新信号量 —— 持续
    /// SUBOPTIMAL 时退化成每帧重建（帧率崩塌 + 句柄/显存抖动，现象像「卡顿」而不是错误）。两道
    /// 门槛：
    /// 1. 冷却期：刚重建过 [`REBUILD_COOLDOWN_FRAMES`] 个提交内不再重建；
    /// 2. 预算：最近一轮里超过 [`MAX_SUBOPTIMAL_REBUILDS`] 次就**放弃重建**，记一次诊断后继续
    ///    用现有链呈现（并由 `swapchain.rs` 的 `log_plan` 打出实际参数）。
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
        if self.suboptimal_rebuilds > MAX_SUBOPTIMAL_REBUILDS {
            self.rebuild_abandoned = true;
            eprintln!(
                "kytrix: vulkan swapchain keeps reporting {reason} after \
                 {MAX_SUBOPTIMAL_REBUILDS} rebuilds; keeping the current swapchain \
                 (extent={:?} format={:?} color_space={:?})",
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

    /// 销毁运行期 `Surface` 的**唯一**入口（C7）。
    ///
    /// 顺序 = 强制销毁该 surface 的全部延迟 swapchain（timeline 等待 + 有界队列 idle）→
    /// 销毁旧 `VkSurfaceKHR`、用同一对 wl 句柄建新的。失败时**不销毁 surface、不清延迟队列**，
    /// 直接返回错误：调用方只能放弃本窗口，交给 `GpuRenderer::drop` 的 `DeferredHandles` /
    /// leak 兜底（否则在未知的队列状态下销毁句柄是二次违规）。
    ///
    /// `recover_surface` / `ensure_swapchain` 的失败分支都只能走这里；禁止直接调
    /// `Surface::recreate`（源码级防回归测试钉住）。
    fn recreate_vk_surface(&mut self) -> Result<(), EngineError> {
        debug_assert!(
            self.swapchain.is_none(),
            "surface recreation requires the current swapchain to be taken out first"
        );
        self.destroy_retired_swapchains_now()?;
        let Some(surface) = self.vk_surface.as_mut() else {
            return Err(EngineError::Swapchain {
                detail: smol_str::SmolStr::new_static(
                    "surface recreation requires a window surface",
                ),
            });
        };
        surface.recreate()
    }

    /// 强制销毁全部延迟 swapchain（C7 的唯一安全出口，也是 C9 安全阀的落点）。
    ///
    /// `vkDestroySwapchainKHR` 要求 presentation engine 已释放该链（VUID-…-01282）、
    /// `vkDestroySemaphore` 要求引用它的批次已完成 —— 二者的共同前置条件是「本窗口 timeline
    /// 已到 + 共享队列 idle」。两个等待都有界（2 s）；**失败时一条链都不销毁**（返回错误，
    /// 由 teardown 的 leak 兜底）。
    fn destroy_retired_swapchains_now(&mut self) -> Result<(), EngineError> {
        if self.retired_swapchains.is_empty() {
            return Ok(());
        }
        self.sync
            .wait_idle()
            .map_err(|error| self.classify(error))?;
        self.shared
            .wait_queue_idle()
            .map_err(|error| self.classify(error))?;
        // 先整队取出再 drop：销毁发生在台账清空之后，持有者的 Drop 不会看到半空台账。
        let retired = self.retired_swapchains.take_all();
        debug_assert!(self.retired_swapchains.is_empty());
        drop(retired);
        Ok(())
    }

    /// 非阻塞释放「已经安全」的旧 swapchain（C9；GPU 循环的完成观测点每轮调用）。
    ///
    /// 安全 = ① 旧链最后一次使用它的提交已完成（timeline 提交序号 `<= completed_sequence`），
    /// ② 当前链已经成功 present 过至少一次（[`Self::current_presented_once`]，对齐 VVL
    /// `ResetPresentWaitSemaphores` 的启发式：旧链的 present-wait semaphore 不再被占用）。
    /// unextended Vulkan 没有 present fence：Chromium 用 FenceHelper 延迟清理旧链、Qt 在
    /// `releaseSwapChainResources` 里 `vkQueueWaitIdle` 后销毁；本实现是可达的最佳实践 ——
    /// 旧链多活一帧，但 present 热路径**零等待**。
    ///
    /// 唯一允许阻塞的出口：台账超过 [`MAX_RETIRED_SWAPCHAINS`] 的安全阀（慢 GPU + 高频 resize
    /// 时链不能无限堆积），退回 C7 的强制销毁（一次有界队列 idle）。
    fn drain_retired_swapchains(&mut self) -> Result<(), EngineError> {
        if self.retired_swapchains.is_empty() {
            return Ok(());
        }
        if self.retired_swapchains.len() > MAX_RETIRED_SWAPCHAINS {
            return self.destroy_retired_swapchains_now();
        }
        if !self.current_presented_once {
            return Ok(());
        }
        let completed = self.completed_sequence;
        self.retired_swapchains.drain_ready(completed, drop);
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
    /// 否则只拷 `pending ∪ written`。**布局也在这一步决定**（[`plan_swapchain_copy`]）：只有整面
    /// 拷贝才允许 `UNDEFINED`（内容可丢弃），差分拷贝必须声明内容保留 —— 否则没搬的像素在
    /// 差分帧里是未定义的（AR3 §5.2 的 P1）。实际搬运的矩形成为 present 的
    /// `VK_KHR_incremental_present` hint（两者必须一致）。
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
        // 差分拷贝计划（R11-12）+ 布局策略（AR3 §5.2 的 P1）：两者必须同时决定 —— 差分拷贝只在
        // 「没搬的像素保持原内容」时才成立，所以差分路径绝不能再用 `UNDEFINED` 转场。
        let copy = plan_swapchain_copy(
            self.image_copy_plan.plan(image_index, written),
            extent,
            self.present_hint,
        );
        self.image_sync = copy.step;
        self.present_hint = copy.hint;
        let copies = match copy.step {
            // 没有像素要搬（damage 全在 swapchain extent 之外，AR3 §5.3）：image 已经在
            // `PRESENT_SRC_KHR`（上一次 present 留下的），连布局转场都不需要 —— present 照样合法，
            // 而 `present_hint` 保持上一次的声明（扩展没有「什么都没变」的编码）。
            ImageSyncStep::Nothing => return Ok(()),
            ImageSyncStep::Full => vec![image_copy(0, 0, extent.width, extent.height)],
            ImageSyncStep::Rects(regions) => regions
                .as_slice()
                .iter()
                .map(|rect| {
                    image_copy(
                        rect.x0 as u32,
                        rect.y0 as u32,
                        rect.width() as u32,
                        rect.height() as u32,
                    )
                })
                .collect(),
        };
        debug_assert!(!copies.is_empty(), "a copy step must move pixels");
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        // ① swapchain image 的 layout 转换必须**先**发生（它的首次使用是 TRANSFER 阶段的
        //    transition + copy）。src/dst 都是 TRANSFER，才能排在 `image_available` 的 acquire
        //    wait（同样在 TRANSFER 阶段）之后；用 `TOP_OF_PIPE` 会与 acquire 信号量没有依赖链，
        //    syncval 报 `SYNC-HAZARD-WRITE-AFTER-READ`（F-168）。
        //
        //    `old_layout` 就是 AR3 §5.2 的修复点：整面拷贝用 `UNDEFINED`（每个像素都会被重写，
        //    旧内容允许丢弃）；差分拷贝必须用 `PRESENT_SRC_KHR` 声明**内容保留**（那是 acquire
        //    之后 image 的真实布局）。`src_access` 恒空：上一次写它的调用已经结束，与 presentation
        //    engine 的读由 acquire 信号量同步（WSI 的标准写法，barrier 不参与那次同步）。
        let discards_content = matches!(copy.step, ImageSyncStep::Full);
        let to_dst = vk::ImageMemoryBarrier::default()
            .old_layout(if discards_content {
                vk::ImageLayout::UNDEFINED
            } else {
                vk::ImageLayout::PRESENT_SRC_KHR
            })
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

    /// teardown 无法证明「GPU 已空闲」时的兜底：把本窗口**全部** Vulkan 句柄标记成「交给进程级
    /// 兜底」（AR3 §5.1 / P2-5）。
    ///
    /// 调用之后各持有者的 `Drop` 变成空操作，句柄由 `SharedGpu` 的 `vkDestroyDevice` 在进程级
    /// 兜底回收。理由：`vkDestroySwapchainKHR`（presentation engine 必须已释放）、
    /// `vkDestroySemaphore` / `vkDestroyFence`（相关提交必须已完成）、`vkDestroyImage` /
    /// `vkFreeMemory` / `vkDestroySurfaceKHR`（引用它们的命令必须已完成、swapchain 必须先销毁）
    /// 的前置条件此刻都无法证明。**描述符段不在其中**：它是纯 CPU 记账，`DescriptorLease` 的
    /// `Drop` 只把段还给共享池，不产生任何 Vulkan 销毁。
    fn leak_gpu_handles(&mut self) {
        if let Some(swapchain) = self.swapchain.as_mut() {
            swapchain.leak();
        }
        if let Some(surface) = self.vk_surface.as_mut() {
            surface.leak();
        }
        self.persist.leak();
        // C9：台账里的**全部**条目同样标记成「交给进程级回收」（否则它们的 Drop 会在
        // 无法证明空闲时逐个 vkDestroy*）。
        for persist in self.retired_persists.iter_mut() {
            persist.leak();
        }
        for swapchain in self.retired_swapchains.iter_mut() {
            swapchain.leak();
        }
        self.sync.leak();
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
        // 两条失败的语义：**设备已经不可用**（或至少无法证明空闲）。此时**不销毁**任何 Vulkan
        // 句柄（旧实现 `let _ =` 吞掉错误照样销毁；随后又把两个延迟销毁队列 `clear()` —— 那正是
        // 逐个 `vkDestroy*`，与注释承诺相反，AR3 §5.1 / P2-5）。见 [`Self::leak_gpu_handles`]。
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
            self.leak_gpu_handles();
            // `Drop` 无法取消字段的 drop，所以「泄漏」只能靠每个持有者的 `handles.leaked()`
            // 守卫（`Swapchain` / `Surface` / `Persist` / `FrameSync`）。这里只剩 CPU 内存要放掉
            // （条目已全部标记泄漏，drop 时不会产生任何 Vulkan 销毁）。
            self.retired_persists.take_all();
            self.retired_swapchains.take_all();
            return;
        }
        // SAFETY: ① 本窗口命令已完成；② 队列已 idle ⇒ 没有在飞批次引用命令池。
        unsafe {
            self.shared
                .raw()
                .destroy_command_pool(self.command_pool, None);
        }
        // 旧 swapchain / 旧 persist：队列 idle 之后 presentation engine 与在飞批次都不再引用它们
        // （C9：它们是台账里剩下的条目，此刻才允许真正 drop）。
        self.retired_persists.take_all();
        self.retired_swapchains.take_all();
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
    /// teardown 的兜底标记（AR3 §5.1 / P2-5）：`leaked()` 为真时 `Drop` 不销毁任何句柄。
    handles: DeferredHandles,
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
            handles: DeferredHandles::default(),
        })
    }

    /// teardown 兜底（AR3 §5.1 / P2-5）：不再销毁 image / view / memory / framebuffer。
    ///
    /// 调用方（`GpuRenderer::drop`）在无法证明 GPU 已空闲时调用：`vkDestroyImage` /
    /// `vkFreeMemory` 都要求引用它们的命令已完成，否则是规范违规。
    fn leak(&mut self) {
        self.handles.leak();
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
        // teardown 无法证明 GPU 已空闲：句柄已交给进程级兜底（AR3 §5.1 / P2-5）。
        if self.handles.leaked() {
            return;
        }
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

/// `[x0, y0, x1, y1]`（物理像素）→ Vulkan scissor 矩形（调用方保证非空）。
fn scissor_rect(rect: [i32; 4]) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D {
            x: rect[0].max(0),
            y: rect[1].max(0),
        },
        extent: vk::Extent2D {
            width: (rect[2] - rect[0]).max(0) as u32,
            height: (rect[3] - rect[1]).max(0) as u32,
        },
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
    // op 的 `bounds` 与实例字节同处**烘焙坐标空间**（不含滚动偏移），而 scissor 是屏幕坐标：
    // 求交前先按本 op 的滚动槽平移，否则滚动帧会把本该绘制的 op 提前剔掉（横向滚动尤其明显）。
    for op in payload.ops() {
        if !op
            .bounds()
            .translated(crate::paint::instance::scroll_offset(payload.scroll(), *op))
            .intersects(scissor)
        {
            continue;
        }
        match *op {
            FrameOp::Instances {
                start, len, scroll, ..
            } => {
                if len == 0 {
                    continue;
                }
                // 实例基址只经 push constant `draw.clip.y` 进 shader，硬件基址恒 0
                // （Vulkan 的 firstInstance 会加到 gl_InstanceIndex 上，与 D3D12 相反；
                //  统一走 uniform 才能三平台一致，contract §6）。
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
                    scissor,
                );
                if let Some(rect) = narrowed {
                    if rect[0] >= rect[2] || rect[1] >= rect[3] {
                        // 完全被容器视口裁掉：本 op 一个像素都不该画。
                        continue;
                    }
                    // SAFETY: 收窄后的矩形是帧 scissor 与该槽屏幕裁剪区的交集（都在目标内）。
                    unsafe {
                        shared
                            .raw()
                            .cmd_set_scissor(command, 0, &[scissor_rect(rect)]);
                    }
                }
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
                if narrowed.is_some() {
                    // SAFETY: 恢复本趟 damage 矩形的 scissor（同一份 `[i32; 4]`）。
                    unsafe {
                        shared
                            .raw()
                            .cmd_set_scissor(command, 0, &[scissor_rect(scissor)]);
                    }
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

/// 本帧 copy 步骤的决策：布局转场、搬运范围、present 的 hint（**纯函数**，可单测）。
///
/// 三条规则（AR3 §5.2 / §5.3）：
/// 1. `CopyPlan::Full` ⇒ 整面拷贝 ⇒ `oldLayout` 可以用 `UNDEFINED`：这一帧每个像素都会被重写，
///    旧内容允许丢弃（也省掉一次整图读）。
/// 2. `CopyPlan::Regions` 夹到 extent 后**非空** ⇒ 差分拷贝 ⇒ `oldLayout` 必须是
///    `PRESENT_SRC_KHR`（内容保留）：没搬的像素仍要显示上一次写进去的内容，而 `UNDEFINED` 转场
///    会把它们变成未定义。
/// 3. `CopyPlan::Regions` 夹到 extent 后**为空**（damage 全在可见区域之外）⇒ 一个像素都不搬，
///    连布局都不碰：image 已经在 `PRESENT_SRC_KHR`（上一次 present 留下的）。此时 hint 沿用
///    `previous_hint` —— `VK_KHR_incremental_present` 没有「什么都没变」的编码（空 region 数组
///    = 整面），声称整张 image 都变了只会让合成器白重合成一遍。
fn plan_swapchain_copy(
    plan: CopyPlan,
    extent: vk::Extent2D,
    previous_hint: Option<DamageRects>,
) -> SwapchainCopy {
    let Some(regions) = plan.regions() else {
        return SwapchainCopy {
            step: ImageSyncStep::Full,
            hint: None,
        };
    };
    // R11-16：damage 是相对 UI 物理 surface 算的，swapchain 的 `imageExtent` 可能更小
    // （`currentExtent` / clamp / transform，R11-9）。**夹紧只有一份实现**：这里夹出来的集合
    // 既是 `vkCmdCopyImage` 的矩形，也是 present 的 `VkPresentRegionKHR` —— 两者逐字一致，
    // 合成器不会把没写的像素当成新的（R11-12）。
    let clamped = regions.clamped(extent.width, extent.height);
    if clamped.is_empty() {
        return SwapchainCopy {
            step: ImageSyncStep::Nothing,
            hint: previous_hint,
        };
    }
    SwapchainCopy {
        step: ImageSyncStep::Rects(clamped),
        hint: Some(clamped),
    }
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
    use std::{sync::Arc, time::Duration};

    use ash::vk;

    use super::{
        GpuRenderer, ImageSyncStep, MAX_RETIRED_SWAPCHAINS, MAX_RETRY_BACKOFF, OutOfDateRecovery,
        Retired, destination_update, out_of_date_backoff, out_of_date_diagnostic,
        plan_swapchain_copy,
    };
    use crate::{
        math::{PhysicalRect, Size},
        paint::atlas::AtlasKind,
        render::{
            DirtyRects,
            compositor::{CopyPlan, DamageRects, FramePlan, Plan},
            frame::FramePayload,
            linux::device::{SharedGpu, gpu_test_lock},
            timer::FrameTimings,
        },
        window::FrameIndex,
    };

    /// 建一组矩形（容量内）。
    fn rects(list: &[PhysicalRect]) -> DamageRects {
        let mut rects = DamageRects::new();
        for rect in list {
            assert!(rects.push(*rect));
        }
        rects
    }

    /// AR3 §5.2 的 P1：布局策略必须跟着拷贝范围走 —— **只有整面拷贝才允许丢弃 image 内容**。
    ///
    /// 差分帧只覆盖 `pending ∪ written`，若此时仍用 `oldLayout = UNDEFINED`，矩形之外的像素就是
    /// 未定义内容（离屏 golden 走不到 present，测不出来）。
    #[test]
    fn only_a_full_surface_copy_may_discard_the_image_content() {
        let extent = vk::Extent2D {
            width: 64,
            height: 64,
        };
        let region = rects(&[PhysicalRect::new(4, 8, 12, 16)]);
        // 整面拷贝：每个像素都会被重写 ⇒ `UNDEFINED` 合法（也省掉一次整图读）。
        let full = plan_swapchain_copy(CopyPlan::Full, extent, Some(region));
        assert_eq!(full.step, ImageSyncStep::Full);
        assert_eq!(full.hint, None, "整面拷贝不挂 region（整面语义）");
        // 差分拷贝：必须声明内容保留，否则没搬的像素是未定义内容。
        let partial = plan_swapchain_copy(CopyPlan::Regions(region), extent, None);
        assert_eq!(
            partial.step,
            ImageSyncStep::Rects(region),
            "差分帧只允许 `PRESENT_SRC_KHR`（内容保留）作 oldLayout"
        );
        assert_eq!(partial.hint, Some(region), "hint 与实际拷贝的矩形逐字一致");
    }

    /// AR3 §5.3：damage 全部落在 swapchain extent 之外 ⇒ 一个像素都不搬、连布局都不碰，
    /// present 的 hint 沿用上一次（`VK_KHR_incremental_present` 没有「什么都没变」的编码）。
    #[test]
    fn damage_entirely_outside_the_extent_copies_nothing_and_keeps_the_previous_hint() {
        let extent = vk::Extent2D {
            width: 32,
            height: 32,
        };
        let outside = rects(&[PhysicalRect::new(40, 0, 60, 20)]);
        let previous = rects(&[PhysicalRect::new(0, 0, 8, 8)]);
        let step = plan_swapchain_copy(CopyPlan::Regions(outside), extent, Some(previous));
        assert_eq!(step.step, ImageSyncStep::Nothing, "没有像素要搬，不碰布局");
        assert_eq!(
            step.hint,
            Some(previous),
            "沿用上一次的 hint，而不是声称整张 image 都变了"
        );
        // 没有上一次的 hint 时只让 present 走「不挂 region」这条（比整面声明更保守的那一边）。
        let step = plan_swapchain_copy(CopyPlan::Regions(outside), extent, None);
        assert_eq!(step.step, ImageSyncStep::Nothing);
        assert_eq!(step.hint, None);
        // 部分在外的矩形：只搬夹进来的那一块，hint 必须是夹过之后的同一份集合（R11-16）。
        let straddling = rects(&[PhysicalRect::new(28, 0, 40, 8)]);
        let clamped = rects(&[PhysicalRect::new(28, 0, 32, 8)]);
        let step = plan_swapchain_copy(CopyPlan::Regions(straddling), extent, None);
        assert_eq!(step.step, ImageSyncStep::Rects(clamped));
        assert_eq!(step.hint, Some(clamped));
    }

    /// AR3 §5.6 / R11-4 的 CI 门禁：验证层在跑时，一帧 headless 渲染不得产生 **ERROR** 级消息。
    ///
    /// 只能比较**增量**（同一进程里别的 GPU 测试可能先触发过消息）。真机门禁命令是
    /// `KYTRIX_VK_VALIDATION=strict cargo test --all-features`：`strict` 模式下 ERROR 直接 panic，
    /// 这条断言是同一判据的非 panic 版本（也覆盖 `on` 模式 —— 那里错误只打日志、不会 fail）。
    #[test]
    fn a_headless_frame_produces_no_validation_errors() {
        let _guard = gpu_test_lock();
        let mut renderer = GpuRenderer::new_offscreen((32, 32)).expect("offscreen renderer");
        if !renderer.shared.caps().validation {
            eprintln!(
                "skip: Vulkan validation layer is not enabled \
                 (release build or KYTRIX_VK_VALIDATION=off)"
            );
            return;
        }
        let (errors_before, _) = crate::render::linux::instance::validation_counts();
        let mut frame = FramePlan::new(Plan::Full, true);
        let mut timings = FrameTimings::new();
        renderer
            .submit_snapshot(
                FrameIndex::from_raw(1),
                &FramePayload::empty(),
                &mut frame,
                &mut timings,
            )
            .expect("headless frame");
        let (errors_after, _) = crate::render::linux::instance::validation_counts();
        assert_eq!(
            errors_after, errors_before,
            "验证层报了 ERROR 级消息（VUID 见 stderr；strict 模式会直接 panic）"
        );
    }

    /// AR3 §5.1 / P2-5 的静态防回归：teardown 的「无法证明 GPU 空闲」分支必须**先**把句柄标记成
    /// 「交给进程级兜底」，再释放延迟销毁台账 —— 反过来（旧实现）`clear()` 就是逐个 `vkDestroy*`，
    /// 销毁的正是可能仍被在飞命令 / presentation engine 引用的对象。
    ///
    /// 该分支只能在真机「设备健康但挂起」时进入，普通 GPU 测试覆盖不到，因此用源码级断言钉住
    /// 顺序（与 D3D12 `swapchain.rs` 的 `include_str!` 防回归测试同一手法）。
    #[test]
    fn an_unsafe_teardown_marks_the_handles_leaked_before_releasing_the_ledgers() {
        let source = include_str!("compositor.rs");
        let leak = source
            .find("self.leak_gpu_handles();")
            .expect("不安全 teardown 必须调用 leak_gpu_handles()");
        let release = source
            .find("self.retired_persists.take_all();")
            .expect("延迟销毁台账仍必须被释放（只放 CPU 内存）");
        assert!(
            leak < release,
            "必须先标记泄漏再释放台账：否则 take_all 的 drop 会在泄漏标记之前销毁在飞句柄"
        );
        // 每个持有者的 `Drop` 都必须看这个标记（否则「泄漏」只是注释里的话）。
        for (file, source) in [
            ("compositor.rs", source),
            ("swapchain.rs", include_str!("swapchain.rs")),
            ("device.rs", include_str!("device.rs")),
        ] {
            assert!(
                source.contains("handles.leaked()"),
                "{file}: 持有 Vulkan 句柄的 Drop 必须有 leaked() 守卫"
            );
        }
    }

    /// C9 台账边界：`completed == tag` 才释放；`tag - 1` 时条目必须仍在台账里（在飞提交仍可能
    /// 引用该资源 —— 提前释放就是 UAF）。重复 drain 不得重复释放。
    #[test]
    fn retired_entries_are_released_only_after_their_own_submission_completed() {
        let mut retired: Retired<u32> = Retired::new();
        assert!(retired.is_empty());
        assert_eq!(retired.len(), 0);
        retired.retire(7, 70);
        let mut released = Vec::new();
        retired.drain_ready(6, |value| released.push(value));
        assert_eq!(
            released,
            Vec::<u32>::new(),
            "tag-1：提交 7 仍在飞，不得释放"
        );
        assert_eq!(retired.len(), 1, "未完成的条目必须留在台账里（否则 UAF）");
        retired.drain_ready(7, |value| released.push(value));
        assert_eq!(released, vec![70], "completed == tag 才释放");
        assert!(retired.is_empty());
        // 重复 drain：没有条目可释放，回调不得再被调用（不得双重 drop）。
        retired.drain_ready(7, |value| released.push(value));
        assert_eq!(released, vec![70]);
    }

    /// C9 台账 FIFO：多个条目按入账顺序释放；只释放已完成的，未完成的保持原顺序。
    #[test]
    fn multiple_retired_entries_are_released_in_fifo_order() {
        let mut retired: Retired<u32> = Retired::new();
        for (tag, value) in [(3, 30), (5, 50), (9, 90)] {
            retired.retire(tag, value);
        }
        let mut released = Vec::new();
        retired.drain_ready(5, |value| released.push(value));
        assert_eq!(released, vec![30, 50], "已完成的条目按入账顺序释放");
        assert_eq!(retired.len(), 1);
        released.clear();
        retired.drain_ready(8, |value| released.push(value));
        assert!(released.is_empty(), "completed=8 不覆盖 tag=9");
        retired.drain_ready(9, |value| released.push(value));
        assert_eq!(released, vec![90]);
        assert!(retired.is_empty());
    }

    /// C9：`take_all` 取出全部条目（保持顺序）并清空台账；空台账是 no-op。
    #[test]
    fn take_all_returns_every_entry_in_order_and_empties_the_ledger() {
        let mut retired: Retired<u32> = Retired::new();
        assert!(retired.take_all().is_empty(), "空台账是 no-op");
        retired.retire(1, 10);
        retired.retire(4, 40);
        retired.retire(2, 20);
        let entries = retired.take_all();
        assert_eq!(entries, vec![(1, 10), (4, 40), (2, 20)]);
        assert!(retired.is_empty());
        assert_eq!(retired.len(), 0);
        assert!(retired.take_all().is_empty(), "再次调用仍是 no-op");
    }

    /// C9：`iter_mut` 覆盖**全部**台账条目（teardown 的 leak 兜底必须标记每一个对象）。
    #[test]
    fn the_leak_path_visits_every_retired_entry() {
        let mut retired: Retired<u32> = Retired::new();
        retired.retire(1, 10);
        retired.retire(2, 20);
        for value in retired.iter_mut() {
            *value += 1;
        }
        assert_eq!(retired.take_all(), vec![(1, 11), (2, 21)]);
    }

    /// C7 的源码级防回归：`Surface::recreate` 只能有一个调用点（`recreate_vk_surface`），且该
    /// 入口必须先强制销毁该 surface 的全部 swapchain（VUID-vkDestroySurfaceKHR-surface-01266）。
    ///
    /// 旧实现的注释把「从 `Option` 取出 swapchain」当成「已销毁」，于是恢复路径在旧链仍存活时
    /// 销毁了 surface；这条测试把顺序钉在生产代码文本上（与 teardown 的 `include_str!` 先例
    /// 同一手法），真实 SURFACE_LOST 路径由 [`super::LiveSwapchains`] 的 debug 断言与手工
    /// nested-Wayland 探针兜底。
    #[test]
    fn the_surface_is_recreated_only_after_its_swapchains_are_force_destroyed() {
        let source = include_str!("compositor.rs");
        let production = source
            .split_once("#[cfg(test)]")
            .expect("production code precedes the test module")
            .0;
        assert_eq!(
            production.matches("surface.recreate()").count(),
            1,
            "运行期 surface 重建必须只有一个入口（GpuRenderer::recreate_vk_surface）"
        );
        let entry = production
            .find("fn recreate_vk_surface(&mut self)")
            .expect("唯一入口必须存在");
        let entry = &production[entry..];
        assert!(
            entry[..entry
                .find("self.destroy_retired_swapchains_now()")
                .expect("先强制排空")]
                .contains("self.swapchain.is_none()"),
            "入口必须断言当前 swapchain 已被取出（没有第三份所有权）"
        );
        let forced_drain = entry
            .find("self.destroy_retired_swapchains_now()")
            .expect("重建 surface 之前必须先强制排空该 surface 的 swapchain");
        let recreate = entry
            .find("surface.recreate()")
            .expect("唯一调用点必须在入口里");
        assert!(
            forced_drain < recreate,
            "顺序违反 VUID-vkDestroySurfaceKHR-surface-01266：必须先销毁 swapchain 再销毁 surface"
        );
        // 强制排空本身必须「先证明空闲，再销毁」。
        let drain_entry = production
            .find("fn destroy_retired_swapchains_now(&mut self)")
            .expect("强制排空函数必须存在");
        let drain = &production[drain_entry..];
        let drain_end = drain.find("\n    fn ").unwrap_or(drain.len());
        let drain = &drain[..drain_end];
        let timeline = drain.find(".wait_idle(").expect("timeline 有界等待");
        let queue = drain.find(".wait_queue_idle(").expect("队列 idle 有界等待");
        let take = drain.find("take_all").expect("取出全部条目");
        assert!(
            timeline < take && queue < take,
            "必须先证明队列空闲（有界）再销毁链，否则是 R11-3 的同步违规"
        );
        // teardown 的字段声明顺序就是销毁顺序：swapchain 必须先于 surface 销毁（I7-c）。
        let swapchain_field = production
            .find("\n    swapchain: Option<Swapchain>,\n")
            .expect("GpuRenderer 必须按声明顺序先销毁 swapchain");
        let surface_field = production
            .find("\n    vk_surface: Option<Surface>,\n")
            .expect("GpuRenderer 必须按声明顺序后销毁 surface");
        assert!(
            swapchain_field < surface_field,
            "字段顺序即销毁顺序：swapchain 字段必须声明在 vk_surface 之前"
        );
    }

    /// 生产代码文本（不含测试模块自身，避免断言字符串自匹配）。
    fn production_source() -> &'static str {
        include_str!("compositor.rs")
            .split_once("#[cfg(test)]")
            .expect("production code precedes the test module")
            .0
    }

    /// 取以 `signature` 开头的函数的源码片段（到下一个 `\n    fn ` 为止）。
    fn function_body<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} must exist"));
        let body = &source[start..];
        let end = body.find("\n    fn ").unwrap_or(body.len());
        &body[..end]
    }

    /// C9-I9-a：present 成功分支**零阻塞等待**，且不再调用任何 drain（旧实现在这里
    /// `drain_retired_swapchains` → `wait_queue_idle`，把流水线深度强行拉回 1）。
    ///
    /// `drain_ready` 内部同样只比较已观测到的 timeline 值：旧链的销毁不等待 presentation
    /// engine（unextended Vulkan 没有 present fence，唯一允许的阻塞点是 C7 强制入口与 teardown）。
    #[test]
    fn the_present_hot_path_and_the_ledger_drain_never_wait_for_the_queue() {
        let production = production_source();
        let presented = production
            .find("Present::Presented | Present::Suboptimal =>")
            .expect("present 成功分支必须存在");
        let out_of_date = production
            .find("Present::OutOfDate =>")
            .expect("present OOD 分支必须存在");
        let hot_path = &production[presented..out_of_date];
        for forbidden in [
            "wait_idle(",
            "wait_queue_idle(",
            "drain_retired_swapchains(",
            "destroy_retired_swapchains_now(",
        ] {
            assert!(
                !hot_path.contains(forbidden),
                "present 热路径不得出现 `{forbidden}`（C9：旧链销毁必须非阻塞）"
            );
        }
        let drain_ready = function_body(production, "fn drain_ready(&mut self");
        assert!(
            !drain_ready.contains("wait_idle") && !drain_ready.contains("wait_queue_idle"),
            "drain_ready 只做非阻塞的 timeline 比较：{drain_ready}"
        );
        assert!(
            drain_ready.contains("<= completed"),
            "释放判据必须是 `tag <= completed`（改 `<` 会泄漏、无条件释放是 UAF）"
        );
    }

    /// C9：延迟销毁的 tag 必须是**提交序号**（`last_sequence`），不能用 `FrameIndex`
    /// （UI 帧序号会跳号 / 饱和，R11-1）；且所有入账点都走唯一的 `retire_swapchain`。
    #[test]
    fn retired_swapchains_are_tagged_with_the_submission_sequence_not_a_frame_index() {
        let production = production_source();
        let retire = function_body(production, "fn retire_swapchain(&mut self");
        assert!(
            retire.contains("self.last_sequence"),
            "retire tag 必须取提交序号：{retire}"
        );
        assert!(
            !retire.contains("FrameIndex"),
            "禁止用 FrameIndex 当延迟销毁的 tag（R11-1）"
        );
        let install = function_body(production, "fn install_swapchain(");
        assert!(
            install.contains("self.retire_swapchain(old)"),
            "install_swapchain 必须经唯一 retire 点入账"
        );
        let replace = function_body(production, "fn try_replace_swapchain(");
        assert_eq!(
            replace.matches("self.retire_swapchain(old)").count(),
            2,
            "建链失败时旧链已 retired，两处都必须入账（R11-3）"
        );
    }

    /// C9-I9-d：台账容量必须有硬上限，且超限时退回 C7 的强制入口（一次有界队列 idle）——
    /// 「慢 GPU + 高频 resize」不允许链 / 显存无限堆积。
    #[test]
    fn the_retired_swapchain_ledger_has_a_hard_capacity_cap() {
        let production = production_source();
        assert_eq!(
            MAX_RETIRED_SWAPCHAINS,
            crate::render::frame::MAX_FRAMES_IN_FLIGHT + 1,
            "安全阀阈值 = MAX_FRAMES_IN_FLIGHT + 1"
        );
        let drain = function_body(production, "fn drain_retired_swapchains(");
        let gate = drain
            .find("if !self.current_presented_once")
            .expect("新链首 present 门必须存在");
        let valve = drain
            .find("MAX_RETIRED_SWAPCHAINS")
            .expect("容量安全阀必须存在");
        let forced = drain
            .find("destroy_retired_swapchains_now(")
            .expect("超限必须走 C7 的强制入口");
        assert!(valve < forced, "安全阀必须调用强制入口：{drain}");
        assert!(
            valve < gate,
            "先检查容量安全阀，再看新链首 present 门：{drain}"
        );
        // teardown 的 leak 兜底必须覆盖台账全部条目。
        let leak = function_body(production, "fn leak_gpu_handles(");
        for ledger in [
            "self.retired_persists.iter_mut()",
            "self.retired_swapchains.iter_mut()",
        ] {
            assert!(
                leak.contains(ledger),
                "leak 兜底必须遍历 `{ledger}`（否则无法证明空闲时会逐个 vkDestroy*）"
            );
        }
    }

    /// C8 的核心不变量：任意次数（1000）连续 `OUT_OF_DATE` 都只产生「退避后重试」，不存在
    /// 升级成 `EngineError` 的出口 —— `on_out_of_date` 的返回类型就是 `Retry`（没有 Err 分支），
    /// 结构性证明「OOD 永不结束事件循环」。
    #[test]
    fn a_thousand_consecutive_out_of_date_events_never_escalate_to_a_fatal_error() {
        let mut recovery = OutOfDateRecovery::new();
        for _ in 0..1000 {
            let retry = recovery.on_out_of_date();
            assert!(
                retry.after > Duration::ZERO && retry.after <= MAX_RETRY_BACKOFF,
                "每次 OOD 都必须退避且不超过上限：{retry:?}"
            );
        }
        assert_eq!(recovery.out_of_date, 1000, "计数只用于诊断，不是错误阈值");
        // 第 1001 次仍然是普通重试。
        let retry = recovery.on_out_of_date();
        assert!(retry.after <= MAX_RETRY_BACKOFF);
    }

    /// C8：退避序列 1/2/4/8/16/32/50 ms，之后封顶（用户裁决「上限 ≤50 ms」）。
    #[test]
    fn the_out_of_date_backoff_doubles_up_to_the_fifty_millisecond_cap() {
        let expected = [1u64, 2, 4, 8, 16, 32, 50, 50, 50];
        for (offset, millis) in expected.into_iter().enumerate() {
            let consecutive = offset as u32 + 1;
            assert_eq!(
                out_of_date_backoff(consecutive),
                Duration::from_millis(millis),
                "第 {consecutive} 次连续 OOD"
            );
        }
        assert_eq!(MAX_RETRY_BACKOFF, Duration::from_millis(50));
        // 任意次数的退避都落在 (0, 上限] 内：GPU 循环的 sleep 因此有界。
        for consecutive in 1..=u32::from(u16::MAX) {
            let after = out_of_date_backoff(consecutive);
            assert!(after > Duration::ZERO && after <= MAX_RETRY_BACKOFF);
        }
    }

    /// C8：成功 acquire / present（progress）之后 OOD 计数与 surface-lost 预算一起复位；
    /// >1 次连续 OOD 才打一行「已恢复」诊断。
    #[test]
    fn progress_resets_both_the_out_of_date_and_surface_lost_budgets() {
        let mut recovery = OutOfDateRecovery::new();
        assert_eq!(recovery.on_progress(), None, "没有 OOD 时没有恢复诊断");
        for _ in 0..5 {
            recovery.on_out_of_date();
        }
        assert_eq!(
            recovery.on_progress(),
            Some(5),
            "连续 5 次 OOD 后恢复：打一行低频诊断"
        );
        assert_eq!(recovery.out_of_date, 0);
        // 复位之后退避从 1 ms 重新开始（不是接着上一次的封顶值）。
        assert_eq!(recovery.on_out_of_date().after, Duration::from_millis(1));
        // surface-lost 的预算也一起复位：复位后重新可用 3 次。
        for expected in [1u64, 4, 16] {
            assert_eq!(
                recovery
                    .on_surface_lost_failure("test")
                    .expect("预算内的恢复失败必须返回退避"),
                Duration::from_millis(expected)
            );
        }
        recovery.on_progress();
        assert_eq!(
            recovery
                .on_surface_lost_failure("test")
                .expect("progress 之后预算重新开始"),
            Duration::from_millis(1),
        );
    }

    /// C8：`SURFACE_LOST` 有界重试 —— 只有预算（3 次退避重试）耗尽之后才上报，且错误文案
    /// 必须区分「合成器/连接还活着但 surface 不可用」与设备丢失。
    #[test]
    fn the_surface_lost_budget_reports_only_after_three_failed_retries() {
        let mut recovery = OutOfDateRecovery::new();
        for expected in [1u64, 4, 16] {
            assert_eq!(
                recovery
                    .on_surface_lost_failure("acquire reported SURFACE_LOST")
                    .expect("预算内的恢复失败必须返回退避"),
                Duration::from_millis(expected)
            );
        }
        let error = recovery
            .on_surface_lost_failure("acquire reported SURFACE_LOST")
            .expect_err("3 次重试之后必须上报");
        match error {
            crate::error::EngineError::Swapchain { detail } => {
                assert!(
                    detail.contains("still alive"),
                    "文案必须区分「连接还活着但 surface 不可用」：{detail}"
                );
                assert!(detail.contains("3 retries"), "{detail}");
                assert!(detail.contains("SURFACE_LOST"), "{detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// C8：诊断低频 —— 1000 次连续 OOD 只打 ~17 行（第 1 / 16 次，之后每 64 次）。
    #[test]
    fn out_of_date_diagnostics_are_low_frequency() {
        let reported: Vec<u32> = (1..=200)
            .filter(|count| out_of_date_diagnostic(*count))
            .collect();
        assert_eq!(
            reported,
            vec![1, 16, 64, 128, 192],
            "第 1 次、第 16 次，之后每 64 次"
        );
        let lines = (1..=1000)
            .filter(|count| out_of_date_diagnostic(*count))
            .count();
        assert!(
            (16..=17).contains(&lines),
            "1000 次连续 OOD 的诊断行数应当 ≈17，实际 {lines}"
        );
    }

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
            let rects = DirtyRects::full();
            renderer.queue_atlas_upload(
                AtlasKind::Glyph,
                0,
                64,
                vec![0u8; (64 * 64) as usize],
                rects,
            );
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
                let rects = crate::render::DirtyRects::full();
                renderer.queue_atlas_upload(AtlasKind::Glyph, 0, 64, vec![0x40u8; 64 * 64], rects);
                let scene = red_quad_scene();
                let mut encoder = InstanceEncoder::new();
                let buffer = encoder
                    .encode(
                        &scene,
                        Scale::ONE,
                        ColorResolver,
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

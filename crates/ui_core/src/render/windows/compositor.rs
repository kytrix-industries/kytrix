//! 帧编排：上传 → 绘制（直绘 back buffer / 持久目标）→ 按需 copy → `Present1`
//! （`design/phase-08-d3d12.md` §5.6 / §6、`design/async-render-pipeline.md` §6.3）。
//!
//! 结构要点（R08-1 / R08-8 / R08-11 之后）：
//!
//! - **两种绘制目标**：全量帧（以及 `Fresh` / persist 过期时的帧）**直接画进 swapchain 的
//!   back buffer**（零拷贝，flip 模型的本意）；只有增量帧才画进持久 offscreen 再拷矩形过去。
//!   旧实现对所有帧都「画进 persist + 整块拷贝」，把 flip 模型省掉的那次拷贝又加了回来。
//! - **`buffer` / `content` 分离**：交换链与持久目标按 [`buffer_size`] 的桶尺寸创建
//!   （内容 + 64 余量、256 对齐），[`GpuRenderer::surface`] 是当前内容尺寸。绘制 / viewport /
//!   scissor / root uniform / copy / present dirty 全部按内容子矩形 —— 桶只影响资源创建。
//!   窗口 resize 在桶内是**纯 CPU 记账**；越过桶时走 [`GpuRenderer::resize_target`]，即
//!   `wait_idle` + `reset_in_flight_lists` + 重建持久目标 + **`ResizeBuffers`**（R08-1：
//!   不再整条替换交换链、不再 prime、不再 `SetContent`/`Commit`、不再新建 DXGI factory；
//!   因此也不会闪一帧背景色）。`ResizeBuffers` 被驱动拒绝时才走回退开关（整链替换）。
//! - **唯一状态源**：`TargetState`（`Fresh` / `PersistValid` / `PersistInvalid`），见
//!   [`plan`]。旧实现有三个互相独立的「必须全量」状态（`Swapchain::first_present` /
//!   `GpuRenderer::fresh` / `FramePlan::fresh`），漏设任一处就是只在重建首帧出现的静态错帧。
//! - **每 buffer 记账**：`FLIP_SEQUENTIAL` 的 back buffer 跨帧内容在规范上没有保证，增量帧
//!   拷贝前按 [`plan::BufferSyncs`] 补齐「这张 buffer 错过的 damage」。
//!
//! 窗口路径另有**一条 1×1 的组合交换链**作为背景层（`background_swapchain`）：它被 prime 成
//! 窗口背景色，由 DComp 的 scale 变换铺满客户区，所以窗口放大时露出的新区域是背景色，而不是
//! 桌面、也不是被拉伸的旧内容（Chrome 语义）。背景层**没有**自己的持久目标：prime 直接 clear
//! 它的 back buffer 并 present（[`record::prime_detached`]）。
//!
//! 所有权（`design/async-render-pipeline.md` §3.1）：本模块的 [`GpuRenderer`] 是**GPU 线程
//! 独占**的每窗口对象，构造发生在窗口创建时（UI 线程），构造完成后整体移交给
//! `kytrix-d3d12` 线程（[`super::renderer`]）。device / direct queue / 根签名 / PSO / 图集纹理
//! 是进程级共享的（[`SharedGpu`]，见 `device.rs`）；离屏 / 快照路径不走线程。
//!
//! 共享图集的写入规则：一帧的「对齐待上传页 → 录制 → 提交」全程持有图集**读锁**（共享页在帧内
//! 不会变）；待上传页与共享页不一致时先释放读锁，在写锁里重建页并等各窗口的**提交水位**
//! （[`SharedGpu::rebuild_atlas_pages`]），然后重试。多个窗口同时上传同一张纹理因此是安全的。
//!
//! 提交是异步的（I2）：`ExecuteCommandLists` 后立即返回；节奏来自 DXGI 的帧延迟 waitable
//! （由 GPU 循环在每帧录制前等一次），完成通知只通过 fence 观察。

use std::{
    cell::Cell,
    sync::{Arc, Mutex},
};

use persist::rtv_handle;
use windows::Win32::{
    Foundation::{HANDLE, HWND},
    Graphics::{Direct3D12::*, Dwm::*},
    UI::WindowsAndMessaging::{IsIconic, IsWindowVisible},
};

#[cfg(any(test, feature = "test-support"))]
use crate::paint::instance::InstanceBuffer;
/// `Plan` 只在快照/测试路径（`render_snapshot`）里直接构造完整帧计划。
#[cfg(any(test, feature = "test-support"))]
use crate::render::compositor::Plan;
#[cfg(any(test, feature = "test-support"))]
use crate::render::windows::readback::{Readback, ReadbackImage};
use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    platform::windows::composition::{self, Composition, buffer_size},
    render::{
        RenderStats,
        compositor::{DamageRects, FramePlan},
        frame::{FramePayload, FrameRequest, FrameSlot, InstanceMirror, MAX_FRAMES_IN_FLIGHT},
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample},
        windows::{
            atlas::{AtlasUploads, SharedAtlas},
            device::{DescriptorLease, SharedGpu, init_error_from},
            pipeline::{GeometryRegistry, Pipeline},
            queue::{CommandStream, WaitOutcome},
            swapchain::Swapchain,
            timer::GpuTimer,
        },
    },
    window::FrameIndex,
};

mod persist;
mod plan;
mod record;
mod timings;
mod upload;

#[cfg(test)]
mod tests;

use plan::BufferSyncs;
pub(crate) use plan::TargetState;
use record::OpBuckets;
pub(crate) use timings::FrameSubmitError;

/// 测试构建的图集页（2048² 在 WARP 上代价过高；页尺寸随上传显式给出，S-153）。
#[cfg(test)]
const TEST_ATLAS_PAGE: u32 = 64;

thread_local! {
    /// 当前线程是否是 `kytrix-d3d12` GPU 线程（§10.1 的线程归属断言）。
    static ON_GPU_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// 标记当前线程为 GPU 线程（`render/windows/renderer.rs` 的循环在入口调用一次）。
pub(crate) fn enter_gpu_thread() {
    ON_GPU_THREAD.set(true);
}

/// 所有 GPU / DXGI 调用都必须在 GPU 线程上（对象构造阶段除外，§3.1）。
fn on_gpu_thread() -> bool {
    ON_GPU_THREAD.get()
}

/// 一个窗口的 GPU 渲染器（**GPU 线程独占**；离屏路径由门面同步调用）。
///
/// 共享与每窗口的边界见 `windows.rs` 与 [`SharedGpu`]：device / direct queue / 根签名 / PSO /
/// 图集纹理是进程级共享的；这里只持有**每窗口**的 swapchain / DirectComposition 对象 / persist
/// 目标 / 提交流（allocator + list + fence + retire）/ 实例上传缓冲 / 几何缓冲。
pub(crate) struct GpuRenderer {
    /// 进程级共享 GPU（设备 / 队列 / PSO / 图集纹理）。
    shared: Arc<SharedGpu>,
    /// 本窗口在共享队列上的提交流（allocator / list / fence / 帧槽 / retire / 提交水位）。
    stream: CommandStream,
    /// 本窗口在共享 SRV 堆里的段（[instances, clips] × in-flight 槽）。
    descriptors: DescriptorLease,
    /// 本窗口的图集上传路径（每槽 staging + 待上传页；纹理本身在 `shared` 里）。
    atlas_uploads: AtlasUploads,
    persist: Persist,
    upload: UploadArena,
    /// 实例缓冲的持久镜像（增量写：只写变化区间，见 `render/frame.rs` 的 `InstanceMirror`）。
    mirror: InstanceMirror,
    geometry: GeometryRegistry,
    timer: Option<GpuTimer>,
    /// 内容层组合交换链（窗口路径）。尺寸变化走 `ResizeBuffers`（R08-1）。
    swapchain: Option<Swapchain>,
    /// 背景层（窗口路径）：**1×1** 的组合交换链，内容恒为窗口背景色（`clear_color`）。
    ///
    /// DComp 把它按客户区缩放后铺满内容之外的区域；背景色变化时整条替换
    /// （[`GpuRenderer::refresh_background`]），内容交换链与内容尺寸都不受影响。
    background_swapchain: Option<Swapchain>,
    /// 背景色已改、背景层还没重建（由 [`GpuRenderer::set_background_color`] 置位，
    /// [`GpuRenderer::refresh_background`] 消费）。
    background_dirty: bool,
    /// 合成目标（窗口路径）：GPU 线程在 `ResizeBuffers` 失败回退与背景层刷新时持锁做一次
    /// `SetContent` + `Commit`。
    composition: Option<Arc<Mutex<Composition>>>,
    /// 主持久目标的 RTV 堆（**1** 个描述符：全量帧直绘 back buffer 之后，persist 只服务增量帧）。
    rtv_heap: ID3D12DescriptorHeap,
    clear_color: [f32; 4],
    /// 合成面尺寸（持久目标 + 组合交换链的桶，`buffer_size(content)`）。
    buffer: (u32, u32),
    /// 当前**内容**尺寸（绘制 / copy / present 的范围；窗口客户区变化时只改这个字段）。
    surface: (u32, u32),
    /// **唯一**的「必须全量」状态源（R08-11）。
    target: TargetState,
    /// 每个 back buffer 与 persist 的同步记账（R08-8）。
    buffer_syncs: BufferSyncs,
    /// 局部帧的 op 分桶缓存（R08-13；跨帧复用 `Vec` 容量）。
    op_buckets: OpBuckets,
    /// 最后一次**真正提交**的提交序号（fence 的 signal 值；重建资源入 retire 队列时的 tag，
    /// §4.3）。**不是** `FrameIndex`（R06-1）：帧序号会饱和，后端的 u64 序号不会。
    last_sequence: u64,
    /// 窗口 DPI（`frame.params.x`）。平台层在 DPI 变化时写入；只影响 geometry 通道的
    /// 逻辑→物理换算，实例通道的 bounds 已是物理像素。
    scale: f32,
    #[cfg(any(test, feature = "test-support"))]
    readback: Readback,
}

// SAFETY: `GpuRenderer` 在构造完成后只整体移动一次到 `kytrix-d3d12` 线程，此后 UI 线程不再
// 访问其中任何对象；D3D12/DXGI 对象本身允许跨线程使用，这里保证的是「单线程独占」而不是共享。
// `CommandStream` 里的 fence 事件句柄（`HANDLE`）与 swapchain 的 waitable 句柄只在该线程上使用；
// `Arc<SharedGpu>` 的共享语义由 `SharedGpu` 自己的 `Send + Sync` 契约负责。
unsafe impl Send for GpuRenderer {}

impl GpuRenderer {
    /// 产品路径：进程级共享 GPU + 离屏目标（P09 在此之后 attach 原生窗口）。
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Self::with_shared(SharedGpu::process()?, size)
    }

    /// 测试用的 WARP 渲染器（进程级共享 WARP 设备；与产品路径同构）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_warp_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Self::with_shared(SharedGpu::process_warp()?, size)
    }

    /// 组合交换链路径（Windows 窗口）。
    ///
    /// `content` 是当前客户区（物理像素）；`buffer` 由 [`buffer_size`] 给出（内容 + 余量、256
    /// 对齐）。本方法在 UI 线程、GPU 线程启动之前运行。
    ///
    /// 顺序（P1-B）：先创建主交换链与 **1×1 背景交换链**，各 prime 一帧背景色（第一次 present，
    /// 必须全量；客户区在第一次 present 之前是**完全透明**的），**再**把两者挂到 [`Composition`]
    /// 的 visual 上并 `Commit`。已 attach 但从未 present 的交换链在合成器里是透明的，反过来做
    /// 会让 `ShowWindow` 之前的一帧露出桌面。
    pub(crate) fn new_for_composition(
        composition: Arc<Mutex<Composition>>,
        content: (u32, u32),
        background: [f32; 4],
    ) -> Result<Self, EngineError> {
        Self::build_window(SharedGpu::process()?, composition, content, background)
    }

    /// 窗口目标的构造（首建与设备恢复共用，R08-4）。
    fn build_window(
        shared: Arc<SharedGpu>,
        composition: Arc<Mutex<Composition>>,
        content: (u32, u32),
        background: [f32; 4],
    ) -> Result<Self, EngineError> {
        let content = (content.0.max(1), content.1.max(1));
        let buffer = buffer_size(content)?;
        let mut renderer = Self::with_shared(shared, buffer)?;
        renderer.clear_color = background;
        let mut swapchain =
            Swapchain::for_composition(renderer.shared.raw(), renderer.shared.queue(), buffer)?;
        // 背景层是 1×1 的纯色交换链，由 UI 线程的 DComp scale 变换铺满客户区。
        let mut background_swapchain =
            Swapchain::for_composition(renderer.shared.raw(), renderer.shared.queue(), (1, 1))?;
        // prime 用显式的交换链；此刻两条 visual 都还没有内容。
        renderer.prime_detached(&mut background_swapchain)?;
        renderer.prime_detached(&mut swapchain)?;
        {
            let target = composition::lock(&composition);
            target.set_background_content(background_swapchain.raw())?;
            target.set_content(swapchain.raw())?;
            target.commit()?;
        }
        renderer.swapchain = Some(swapchain);
        renderer.background_swapchain = Some(background_swapchain);
        renderer.surface = content;
        renderer.composition = Some(composition);
        // prime 已经做过第一次 present（back buffer 内容有定义），但 persist 还是空纹理：
        // 下一帧必须整帧重画 ⇒ `PersistInvalid`（不是 `Fresh`：整面 present 的约束已经满足）。
        renderer.target = TargetState::PersistInvalid;
        Ok(renderer)
    }

    /// **设备丢失后的进程内恢复**（R08-4）：释放本窗口的全部 GPU 对象 → 重建进程级设备 →
    /// 重建交换链 / 持久目标 / 提交流并重新 prime + `SetContent`。
    ///
    /// 图集内容由 `SharedGpu::recover` 按 CPU 影子页重传，因此恢复后的窗口仍有字形与图片。
    /// 失败时把新的 `DeviceLost` 交回调用方（GPU 循环据此上报粘性错误）。
    pub(crate) fn recover(self) -> Result<Self, EngineError> {
        let Some(composition) = self.composition.clone() else {
            return Err(EngineError::AppInit {
                detail: smol_str::SmolStr::new_static(
                    "device recovery requires a composition target",
                ),
            });
        };
        let content = self.surface;
        let background = self.clear_color;
        let stale = Arc::clone(&self.shared);
        // 设备已经丢了：`Drop` 不等待、只把句柄归还给（即将销毁的）句柄表。
        drop(self);
        let shared = SharedGpu::recover(&stale)?;
        Self::build_window(shared, composition, content, background)
    }

    /// 每窗口 GPU 对象：提交流 / 共享 SRV 堆的窗口段 / 图集上传路径 / 持久目标 / RTV 堆 / 计时器。
    ///
    /// 设备 / 队列 / PSO / 图集纹理来自进程级 `SharedGpu`；本函数只创建每窗口对象。
    fn with_shared(shared: Arc<SharedGpu>, size: (u32, u32)) -> Result<Self, EngineError> {
        let stream = CommandStream::new(&shared)?;
        let descriptors = shared.window_descriptors()?;
        // SAFETY: 设备句柄由进程级 `SharedGpu` 持有，调用期间有效；错误按 HRESULT 返回。
        let rtv_heap: ID3D12DescriptorHeap = unsafe {
            shared
                .raw()
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    // 0 = 持久目标（back buffer 的 RTV 由交换链自己持有）。
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })
        }
        .map_err(|error| init_error_from("CreateDescriptorHeap(RTV)", error))?;
        // SAFETY: 描述符堆由本结构持有，句柄在堆生命周期内有效。
        let rtv = rtv_handle(shared.raw(), &rtv_heap, 0);
        let size = (size.0.max(1), size.1.max(1));
        let persist = Persist::new(&shared, size, rtv)?;
        let upload = UploadArena::new(&shared)?;
        let mirror = InstanceMirror::new(MAX_FRAMES_IN_FLIGHT);
        let timer = GpuTimer::new(&shared, &stream)?;

        Ok(Self {
            shared,
            stream,
            descriptors,
            atlas_uploads: AtlasUploads::new(),
            persist,
            upload,
            mirror,
            geometry: GeometryRegistry::new(),
            timer,
            swapchain: None,
            background_swapchain: None,
            background_dirty: false,
            composition: None,
            rtv_heap,
            clear_color: [0.0, 0.0, 0.0, 0.0],
            buffer: size,
            surface: size,
            target: TargetState::Fresh,
            buffer_syncs: BufferSyncs::new(),
            op_buckets: OpBuckets::new(),
            last_sequence: 0,
            scale: 1.0,
            #[cfg(any(test, feature = "test-support"))]
            readback: Readback::new(),
        })
    }

    /// 写入窗口 DPI（平台层在 `set_scale` 之后调用；只影响 geometry 通道）。
    pub(crate) fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
    }

    /// 当前**内容**尺寸（GPU 线程侧的真值；窗口 resize 只改这个字段）。
    pub(crate) fn surface(&self) -> (u32, u32) {
        self.surface
    }

    /// 当前的合成面尺寸（测试：内容可以小于它）。
    #[cfg(test)]
    pub(crate) fn buffer(&self) -> (u32, u32) {
        self.buffer
    }

    /// 唯一状态源的快照（测试 / 诊断）。
    #[cfg(test)]
    pub(crate) fn target_state(&self) -> TargetState {
        self.target
    }

    /// 本帧是否必须整面（`FramePlan::fresh` 的唯一来源，R08-11）。
    pub(crate) fn is_fresh(&self) -> bool {
        self.target.is_fresh()
    }

    /// 帧延迟 waitable（窗口路径的节奏原语，只有 GPU 线程等它）。
    pub(crate) fn waitable(&self) -> Option<HANDLE> {
        debug_assert!(
            on_gpu_thread(),
            "the frame-latency waitable may only be waited on the kytrix-d3d12 thread"
        );
        self.swapchain.as_ref().map(Swapchain::waitable)
    }

    /// 取本帧的 ring 槽（**按提交次数轮转**；提交成功后才推进 fence 记账）。
    pub(crate) fn acquire_slot(&mut self) -> FrameSlot {
        debug_assert!(
            on_gpu_thread(),
            "window frame slots may only be acquired on the kytrix-d3d12 thread"
        );
        self.stream.acquire_slot()
    }

    /// 等本帧槽上一次提交完成（复用 allocator / timestamp 读回缓冲的前置条件）。
    ///
    /// 超时返回 [`WaitOutcome::TimedOut`]（设备健康，R08-3）：调用方必须**换一个槽**或跳过本帧，
    /// 绝不能复用这个 allocator。
    pub(crate) fn wait_slot_fence(&mut self, slot: FrameSlot) -> Result<WaitOutcome, EngineError> {
        debug_assert!(
            on_gpu_thread(),
            "window slot fences may only be waited on the kytrix-d3d12 thread"
        );
        self.stream
            .wait_slot_fence(slot.wait_value)
            .map_err(|error| self.classify(error))
    }

    /// 请求 `ID3D12Fence::SetEventOnCompletion`：该帧 GPU 工作完成时置位 `event`（不轮询）。
    pub(crate) fn set_completion_event(
        &mut self,
        signal_value: u64,
        event: HANDLE,
    ) -> Result<(), EngineError> {
        debug_assert!(
            on_gpu_thread(),
            "completion events may only be armed on the kytrix-d3d12 thread"
        );
        self.stream
            .set_completion_event(signal_value, event)
            .map_err(|error| self.classify(error))
    }

    /// 队列 fence 的已完成值（非阻塞；完成通知的唯一判据）。
    pub(crate) fn completed_value(&self) -> u64 {
        debug_assert!(
            on_gpu_thread(),
            "the queue fence may only be observed on the kytrix-d3d12 thread"
        );
        self.stream.completed_value()
    }

    /// 实现在 `persist::set_content_size`（薄委托）。
    pub(crate) fn set_content_size(&mut self, content: (u32, u32)) -> Result<(), EngineError> {
        persist::set_content_size(self, content)
    }

    /// 实现在 `persist::resize_target`（薄委托）：把合成面调整到当前内容所需的桶。
    ///
    /// 消费者：GPU 循环在 `Present1` 报告交换链 out of date 时重试（R08-7）。
    pub(crate) fn resize_target(&mut self) -> Result<(), EngineError> {
        persist::resize_target(self, self.surface)
    }

    /// 实现在 `persist::rebuild_target`（薄委托）：显示 / 电源恢复之后的**整链重建**（R07-14）。
    ///
    /// 与 [`GpuRenderer::resize_target`] 的差别是「即使尺寸没变也重建」：睡眠唤醒 / 换屏之后
    /// 交换链的显示器关联与持久目标内容都不可信，只 `ResizeBuffers`（同尺寸时是 no-op）
    /// 修不好。返回 `Ok(false)` = 设备健康但还有在飞工作，调用方下一轮重试。
    pub(crate) fn rebuild_target(&mut self) -> Result<bool, EngineError> {
        persist::rebuild_target(self)
    }

    /// 实现在 `persist::resize_offscreen`（薄委托）。
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        persist::resize_offscreen(self, size)
    }

    /// 改窗口背景色（GPU 线程；离屏目标不使用它）。
    ///
    /// 组合交换链没有可用的 `SetBackgroundColor`（probe：返回 OK 但惰性），背景色只作为持久
    /// 目标的 clear color（linear，`render::color::background_linear`）和背景层的 prime 颜色：
    /// 下一次全量帧的 clear 生效，背景层由 [`GpuRenderer::refresh_background`] 重建。
    /// 窗口的 `set_background_color` 已经标了全量 damage，这里**不**碰 `TargetState`（那会让
    /// 整块 buffer 全量 present）。
    pub(crate) fn set_background_color(&mut self, rgba: [f32; 4]) {
        debug_assert!(
            self.swapchain.is_none() || on_gpu_thread(),
            "window background changes may only run on the kytrix-d3d12 thread"
        );
        self.clear_color = rgba;
        self.background_dirty = true;
    }

    /// 实现在 `persist::refresh_background`（薄委托）。
    pub(crate) fn refresh_background(&mut self) -> Result<(), EngineError> {
        persist::refresh_background(self)
    }

    /// 实现在 `record::prime_detached`（薄委托）。
    fn prime_detached(&mut self, swapchain: &mut Swapchain) -> Result<(), EngineError> {
        record::prime_detached(self, swapchain)
    }

    /// 离屏 / 快照路径：提交一份快照并**同步**等完成（§4.5 的下半段）。
    ///
    /// `frame` 由调用方构造（`Fresh` 的判定权在资源所有者手上，§4.1）；窗口路径的常规帧用
    /// [`GpuRenderer::submit_frame`]（异步，完成状态由 GPU 循环按 fence 推进）。
    pub(crate) fn submit_snapshot(
        &mut self,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
    ) -> Result<RenderStats, EngineError> {
        let slot = self.stream.acquire_slot();
        if !self
            .stream
            .wait_slot_fence(slot.wait_value)
            .map_err(|error| self.classify(error))?
            .is_signaled()
        {
            return Err(EngineError::AppInit {
                detail: smol_str::SmolStr::new_static(
                    "synchronous submit needs an idle frame slot",
                ),
            });
        }
        let (recorded, slot) = self.submit_recorded(slot, index, payload, frame, timings)?;
        self.finish_sync(slot, timings)?;
        Ok(recorded.stats)
    }

    /// 测试 / 快照的便捷入口：把 `InstanceBuffer` 复制成快照后同步提交。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn submit_buffer(
        &mut self,
        index: FrameIndex,
        buffer: &InstanceBuffer,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
    ) -> Result<RenderStats, EngineError> {
        let payload = FramePayload::full(buffer, Vec::new(), Vec::new());
        self.submit_snapshot(index, &payload, frame, timings)
    }

    /// 实现在 `timings::finish_sync`（薄委托）。
    fn finish_sync(
        &mut self,
        slot: FrameSlot,
        timings: &mut FrameTimings,
    ) -> Result<(), EngineError> {
        timings::finish_sync(self, slot, timings)
    }

    /// 实现在 `timings::submit_frame`（薄委托）。
    ///
    /// `replace` = 已经有更新的帧待画（UI 又发布了一帧）⇒ `Present1(sync = 0)` 替换队列里的帧。
    pub(crate) fn submit_frame(
        &mut self,
        request: &FrameRequest,
        frame: &mut FramePlan,
        slot: FrameSlot,
        replace: bool,
    ) -> Result<(RenderStats, Vec<GpuTimerSample>), timings::FrameSubmitError> {
        timings::submit_frame(self, request, frame, slot, replace)
    }

    /// 观察完成之后读本槽的 GPU 时间（`timestamp` 读回缓冲此时不再被 GPU 写）。
    pub(crate) fn resolve_frame_timer(&self, slot: usize) -> Option<GpuTimerSample> {
        debug_assert!(
            on_gpu_thread(),
            "GPU timestamps may only be resolved on the kytrix-d3d12 thread"
        );
        self.timer.as_ref()?.resolve(slot)
    }

    /// **非阻塞**推进 retire：fence 已到达的帧（含被跳过的序号，§4.3）及更早的句柄交回
    /// `device` 真正释放。只在观察到完成之后调用（I4）。
    pub(crate) fn release_completed(&mut self) {
        debug_assert!(
            on_gpu_thread(),
            "the retire queue may only be advanced on the kytrix-d3d12 thread"
        );
        self.stream.release_completed();
    }

    /// 实现在 `record::submit_recorded`（薄委托）。
    fn submit_recorded(
        &mut self,
        slot: FrameSlot,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
    ) -> Result<(RecordedFrame, FrameSlot), EngineError> {
        record::submit_recorded(self, slot, index, payload, frame, timings)
    }

    /// 实现在 `upload::queue_atlas_upload`（薄委托）。
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        page: u32,
        pixels: Vec<u8>,
        rows: crate::render::atlas::DirtyRows,
    ) {
        upload::queue_atlas_upload(self, kind, page, pixels, rows)
    }

    /// 该图集是否有一笔上传已入队、还没被命令列表消费（离屏目标的空闲判据）。
    pub(crate) fn atlas_pending(&self, kind: AtlasKind) -> bool {
        self.atlas_uploads.pending(kind)
    }

    /// 读回持久 offscreen（快照；阻塞到 GPU 完成）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn readback(&mut self) -> Result<ReadbackImage, EngineError> {
        if !self.stream.wait_idle()?.is_signaled() {
            return Err(EngineError::AppInit {
                detail: smol_str::SmolStr::new_static("readback needs an idle command stream"),
            });
        }
        self.readback.capture(
            &self.shared,
            &mut self.stream,
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
        self.submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)?;
        self.readback()
    }

    /// 测试用的静态几何注册入口：直接放进 registry（生产路径是编码器随帧带描述、
    /// `draw_ops` 按需建缓冲）。
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
        let desc = crate::paint::geometry::GeometryDesc {
            vertex_bytes: bytes::Bytes::copy_from_slice(vertices),
            index_bytes: bytes::Bytes::copy_from_slice(indices),
            vertex_stride: pipeline.vertex_stride(),
            index_type,
            topology,
            bounds: crate::math::Rect::new(0.0, 0.0, 1.0, 1.0),
            pipeline,
        };
        let mut retired = Vec::new();
        let _ = self
            .geometry
            .ensure(&self.shared, id, &desc, &mut retired)?;
        for handle in retired {
            self.stream.retire(self.last_sequence, handle);
        }
        Ok(())
    }

    /// 把底层错误归类为设备丢失（粘性）或原样返回。
    fn classify(&mut self, error: EngineError) -> EngineError {
        self.shared.note_failure().unwrap_or(error)
    }

    /// 用 `GetDeviceRemovedReason()` 裁决「是不是真的丢了设备」（R08-3）。
    ///
    /// 返回 `Some` **只**代表真正的移除 / 挂死 / 复位 / 驱动内部错误；设备健康时恒 `None`
    /// （超时、遮挡、模式变化都不是设备丢失）。
    pub(crate) fn check_device_lost(&self) -> Option<EngineError> {
        self.shared.note_failure()
    }

    /// 窗口当前是否被遮挡 / 最小化（R08-7）。
    ///
    /// 由 GPU 线程自己判定，平台层不需要额外接线（`Composition` 已经持有 HWND）：
    /// - `IsIconic`：最小化；
    /// - `DWMWA_CLOAKED`：DWM 把窗口标成不可见（锁屏 / 切到其它虚拟桌面 / UWP 挂起）；
    ///   **这是「被完全遮挡」唯一可靠的信号** —— 普通被别的窗口盖住时
    ///   `IsWindowVisible` 仍为 true，DWM 也不会停止合成，只是丢弃不必要的工作；
    /// - `!IsWindowVisible`：`ShowWindow(SW_HIDE)`。
    ///
    /// 遮挡期间 GPU 循环不取帧、不提交、不 present（帧留在最新帧槽里继续合并 damage）：
    /// 长时间挂机（切走 / 锁屏 / 最小化）因此不再持续空烧 GPU 与电量。
    pub(crate) fn window_occluded(&self) -> bool {
        let Some(composition) = self.composition.as_ref() else {
            return false;
        };
        let hwnd = composition::lock(composition).hwnd_raw();
        if hwnd == 0 {
            return false;
        }
        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
        // SAFETY: HWND 由 UI 线程的 `NativeWindow` 持有且晚于 GPU 线程释放（关闭握手）；三个查询
        // 都只读窗口状态（句柄失效时 `IsIconic` / `IsWindowVisible` 返回 0、DWM 返回错误码）。
        unsafe {
            let iconic = IsIconic(hwnd).as_bool();
            let visible = IsWindowVisible(hwnd).as_bool();
            let mut cloaked = 0u32;
            let cloaked_ok = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                std::ptr::from_mut(&mut cloaked).cast(),
                std::mem::size_of::<u32>() as u32,
            )
            .is_ok();
            occluded_from(iconic, visible, cloaked_ok && cloaked != 0)
        }
    }
}

impl Drop for GpuRenderer {
    fn drop(&mut self) {
        // 退出路径：等本窗口 GPU 完成，并丢掉命令列表对 back buffer 的引用，之后才销毁
        // swapchain（§6.7 的关闭握手由门面负责线程 join）。
        let lost = self.shared.lost_error().is_some();
        if !lost {
            let idle = self.stream.wait_idle().is_ok_and(WaitOutcome::is_signaled);
            if !idle {
                // R08-24：`wait_idle` 超时（设备**健康**，只是没回话）时**不能**释放句柄 ——
                // 命令列表可能仍在引用 persist / staging。句柄留在共享设备表里（随进程释放），
                // 代价是一次显存泄漏，好过在设备健康时把在飞帧的资源抽走。
                eprintln!(
                    "kytrix: d3d12 window teardown timed out while the device is still present; keeping its GPU resources registered"
                );
                return;
            }
            let _ = self.stream.reset_in_flight_lists();
        }
        // 设备丢失时句柄表随旧设备一起释放，这里照常归还（不会触碰已失效的 D3D12 对象）。
        let _ = self.stream.release_completed();
        self.stream.drain_retire();
        self.upload.release(&self.shared);
        self.geometry.release(&self.shared);
        self.atlas_uploads.release(&self.shared);
        let _ = self.shared.release(self.persist.handle);
    }
}

/// 一次录制的结果：统计 + 本帧 `Present1` 的 dirty 集。
///
/// 定在 `compositor` 模块（而不是 `record` 子模块）：字段保持私有、只有本模块树可见
/// （clippy 的 `field_scoped_visibility_modifiers` 不允许对字段写 `pub(super)`）。
struct RecordedFrame {
    stats: RenderStats,
    /// `Some` = 只呈现这些矩形；`None` = 整面（`DirtyRectsCount = 0`）。
    ///
    /// 矩形已经夹紧到当前内容（R08-10），`Present1` 因此不可能收到越界坐标。
    present: Option<DamageRects>,
}

/// 一帧录制所需的 GPU 对象（借用拆分：`GpuRenderer` 的字段在这里按用途分组）。
struct FrameRecorder<'a> {
    shared: &'a SharedGpu,
    stream: &'a mut CommandStream,
    pipeline: &'a Pipeline,
    descriptors: &'a DescriptorLease,
    /// 调用方持有的图集读锁（共享页在录制 + 提交期间不会变）。
    atlas: &'a SharedAtlas,
    atlas_uploads: &'a mut AtlasUploads,
    persist: &'a Persist,
    upload: &'a mut UploadArena,
    /// GPU 线程侧的持久实例镜像（本帧只写变化区间）。
    mirror: &'a mut InstanceMirror,
    geometry: &'a mut GeometryRegistry,
    timer: Option<&'a GpuTimer>,
    /// 内容层交换链（离屏目标为 `None`：既不能直绘，也没有 present）。
    swapchain: Option<&'a Swapchain>,
    clear_color: [f32; 4],
    /// 当前内容尺寸（viewport / scissor / root uniform / copy / present 的范围）。
    content: (u32, u32),
    /// 窗口 DPI（`frame.params.x`；geometry 顶点是逻辑像素，shader 用它换物理）。
    scale: f32,
    /// 唯一状态源（录制结束时按绘制路径推进）。
    target: &'a mut TargetState,
    /// 每 back buffer 的同步记账（增量帧的拷贝范围）。
    syncs: &'a mut BufferSyncs,
    /// 局部帧的 op 分桶缓存。
    buckets: &'a mut OpBuckets,
    /// prime 帧：只 clear + present，不写上传缓冲、不消费图集上传路径（见 `record::prime_detached`）。
    prime: bool,
}

impl FrameRecorder<'_> {
    /// 实现在 `record::record`（薄委托）。
    fn record(
        &mut self,
        slot: FrameSlot,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
        frame_started: std::time::Instant,
    ) -> Result<RecordedFrame, EngineError> {
        record::record(self, slot, payload, frame, timings, frame_started)
    }
}

/// 持久 offscreen（增量帧的绘制目标与拷贝源；资源格式 UNORM + RTV 视图 sRGB，DEVIATIONS F-80）。
struct Persist {
    texture: ID3D12Resource,
    handle: GpuResourceHandle,
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    /// 资源尺寸（= 合成面桶；整块拷贝的范围）。
    size: (u32, u32),
}

impl Persist {
    /// 实现在 `persist::create_persist`（薄委托）。
    fn new(
        shared: &SharedGpu,
        size: (u32, u32),
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Result<Self, EngineError> {
        persist::create_persist(shared, size, rtv)
    }

    /// 实现在 `persist::recreate_persist`（薄委托）。
    fn recreate(
        &mut self,
        shared: &SharedGpu,
        size: (u32, u32),
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Result<GpuResourceHandle, EngineError> {
        persist::recreate_persist(self, shared, size, rtv)
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
    /// 持久映射的写入指针（`Map` 一次、整个生命周期不再 `Unmap`；DEVIATIONS F-210）。
    instance_ptr: *mut u8,
    clips: ID3D12Resource,
    clip_handle: GpuResourceHandle,
    clip_capacity: u32,
    clip_ptr: *mut u8,
}

// SAFETY: 指针只用于 GPU 线程独占的 CPU 写入（`UploadArena` 只在 `kytrix-d3d12` 线程上使用），
// 与 D3D12 的「Map 一次、长期写」用法一致。
unsafe impl Send for UploadSlot {}

impl UploadArena {
    /// 实现在 `upload::create_arena`（薄委托）。
    fn new(shared: &SharedGpu) -> Result<Self, EngineError> {
        upload::create_arena(shared)
    }

    fn slot(&self, slot: usize) -> &UploadSlot {
        &self.slots[slot]
    }

    /// teardown：把本窗口实例 / 裁剪上传缓冲的句柄交回共享设备（GPU 线程已 idle）。
    fn release(&self, shared: &SharedGpu) {
        for slot in &self.slots {
            let _ = shared.release(slot.instance_handle);
            let _ = shared.release(slot.clip_handle);
        }
    }

    /// 实现在 `upload::write_uploads`（薄委托）：实例走增量写（持久镜像），裁剪表全量写。
    fn write(
        &mut self,
        mirror: &mut InstanceMirror,
        shared: &SharedGpu,
        stream: &mut CommandStream,
        sequence: u64,
        slot: usize,
        payload: &FramePayload,
    ) -> Result<(u32, u32), EngineError> {
        upload::write_uploads(self, mirror, shared, stream, sequence, slot, payload)
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

/// 三个 Win32 查询的结果 → 是否遮挡（纯函数：遮挡状态机的单测入口）。
fn occluded_from(iconic: bool, visible: bool, cloaked: bool) -> bool {
    iconic || !visible || cloaked
}

#[cfg(test)]
mod occlusion_tests {
    use super::occluded_from;

    /// R08-7 的遮挡状态机真值表：三个判据任一成立就必须停帧。
    #[test]
    fn any_occlusion_signal_stops_frame_production() {
        // 可见、未最小化、未 cloak：正常出帧。
        assert!(!occluded_from(false, true, false));
        // 最小化（IsIconic）。
        assert!(occluded_from(true, true, false));
        // 锁屏 / 其它虚拟桌面（DWMWA_CLOAKED）。
        assert!(occluded_from(false, true, true));
        // ShowWindow(SW_HIDE)。
        assert!(occluded_from(false, false, false));
        // 多判据同时成立。
        assert!(occluded_from(true, false, true));
        // 「被别的窗口完全盖住」不在判据里（`IsWindowVisible` 仍为 true、DWM 继续合成）：
        // 这一档靠 R08-9 的呈现延迟测量与 `sync = 0` 处理，不靠遮挡停帧。
        assert!(!occluded_from(false, true, false));
    }
}

//! 帧编排：上传 → 逐 damage scissor 画进持久 offscreen → copy 到 back buffer → `Present1`
//! （`design/phase-08-d3d12.md` §5.6 / §6、`design/async-render-pipeline.md` §6.3）。
//!
//! 结构要点（DEVIATIONS F-71）：**只有持久 offscreen 是绘制目标**；swapchain 的 back buffer
//! 只作 `COPY_DEST`（不建 RTV、不挂 scissor 绘制）。
//!
//! **buffer / content 分离 + 内容取桶**（`design/DEVIATIONS.md` §36 F-199）：组合交换链在第一次 present
//! 之后不能 `ResizeBuffers`（probe `resize.log`），所以持久目标与交换链按
//! [`buffer_size`] 的桶尺寸创建（内容 + 64 余量、256 对齐），而 [`GpuRenderer::surface`] 是
//! **当前内容**尺寸 —— 绘制、viewport、scissor、root uniform、copy、present dirty 全部按内容
//! 子矩形，只有资源创建与 clear 用 buffer。窗口 resize 因此不需要任何 GPU 等待或资源重建；
//! 只有内容**越过** buffer 时才走 [`GpuRenderer::grow`]：等 idle → 新建持久目标 + 交换链 →
//! prime 新交换链一帧背景色（等 GPU fence）→ 把 visual 切过去并 `Commit` → 丢弃旧对象
//! （越过边界时会闪一帧背景，见 `grow` 的注释）。内容 visual 恒 1:1（不随 resize 缩放）；
//! UI 线程只更新内容裁剪与背景层缩放（`platform/windows/composition.rs`）。
//!
//! 窗口路径另有**一条 1×1 的组合交换链**作为背景层（`background_swapchain`）：它被 prime 成
//! 窗口背景色，由 DComp 的 scale 变换铺满客户区，所以窗口放大时露出的新区域是背景色，而不是
//! 桌面、也不是被拉伸的旧内容（Chrome 语义）。背景层有**独立的 1×1 持久目标**
//! （`background_persist`，RTV 堆的第二个描述符），改色只 clear / copy 这块 1×1，不再借主
//! 持久目标；[`GpuRenderer::refresh_background`] 换一条新的 1×1 交换链（1×1，代价可忽略）。
//!
//! **高水位缩容**（[`GpuRenderer::maybe_shrink`]）：grow-only 桶在内容静止
//! [`SHRINK_IDLE`] 之后缩回**当前内容**的桶。组合交换链不能 `ResizeBuffers`，所以整条替换；
//! 与 `grow` 的差别是内容不能丢：新持久目标先 clear，再从旧持久目标把 `(0, 0, content)` 拷到
//! 左上角，然后才 prime / 切 visual —— 缩容那一帧可见像素完全不变，不会闪一帧背景色。
//!
//! 所有权（`design/async-render-pipeline.md` §3.1）：本模块的 [`GpuRenderer`] 是**GPU 线程独占**
//! 的每窗口对象（swapchain / DirectComposition / persist / 提交流 / 实例上传缓冲 / 计时），构造
//! 发生在窗口创建时（UI 线程），构造完成后整体移交给 `kytrix-d3d12` 线程（[`super::renderer`]）。
//! device / direct queue / 根签名 / PSO / 图集纹理是进程级共享的（[`SharedGpu`]，见 `device.rs`）；
//! 离屏 / 快照路径不走线程：`GpuRenderer` 由门面同步调用，语义与 golden 不变（§4.5 的下半段）。
//!
//! 共享图集的写入规则：一帧的「对齐待上传页 → 录制 → 提交」全程持有图集**读锁**（共享页在帧内
//! 不会变）；待上传页与共享页不一致时先释放读锁，在写锁里重建页并等共享队列 idle
//! （[`SharedGpu::rebuild_atlas_pages`]），然后重试。多个窗口同时上传同一张纹理因此是安全的：
//! 每窗口有自己的 staging，同一条队列上的命令列表按提交顺序整段执行。
//!
//! 提交是异步的（I2）：`ExecuteCommandLists` 后立即返回；节奏来自 DXGI 的帧延迟 waitable
//! （由 GPU 循环在每帧录制前等一次），完成通知只通过 fence 观察。

use std::{
    cell::Cell,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use persist::{rtv_handle, shrink_target};
use windows::Win32::{Foundation::HANDLE, Graphics::Direct3D12::*};

#[cfg(any(test, feature = "test-support"))]
use crate::paint::instance::InstanceBuffer;
/// `Plan` 只在快照/测试路径（`render_snapshot`）里直接构造完整帧计划。
#[cfg(any(test, feature = "test-support"))]
use crate::render::compositor::Plan;
#[cfg(any(test, feature = "test-support"))]
use crate::render::windows::readback::{Readback, ReadbackImage};
use crate::{
    error::EngineError,
    paint::{atlas::AtlasKind, instance::GpuClipEntry},
    platform::windows::composition::{self, Composition, buffer_size},
    render::{
        RenderStats,
        compositor::FramePlan,
        frame::{FramePayload, FrameRequest, FrameSlot},
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample},
        windows::{
            atlas::{AtlasUploads, SharedAtlas},
            device::{DescriptorLease, SharedGpu, init_error_from},
            pipeline::{GeometryRegistry, Pipeline},
            queue::CommandStream,
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

/// 测试构建的图集页（2048² 在 WARP 上代价过高；页尺寸随上传显式给出，S-153）。
#[cfg(test)]
const TEST_ATLAS_PAGE: u32 = 64;

/// 内容尺寸静止多久之后才把 grow-only 高水位缩回当前内容的桶。
///
/// 拖动是连续的尺寸变化：`content_since` 每帧刷新，所以拖动中永远不会缩容（也不会在
/// 「拖大 → 拖小」时反复重建）。静止满 2 s 之后，GPU 循环在没有待画帧时尝试一次
/// [`GpuRenderer::maybe_shrink`]；缩容 prime 会保留旧内容，可见像素不变。
const SHRINK_IDLE: Duration = Duration::from_secs(2);

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
    /// 本窗口在共享队列上的提交流（allocator / list / fence / 帧槽 / retire）。
    stream: CommandStream,
    /// 本窗口在共享 SRV 堆里的段（[instances, clips] × in-flight 槽）。
    descriptors: DescriptorLease,
    /// 本窗口的图集上传路径（每槽 staging + 待上传页；纹理本身在 `shared` 里）。
    atlas_uploads: AtlasUploads,
    persist: Persist,
    upload: UploadArena,
    geometry: GeometryRegistry,
    timer: Option<GpuTimer>,
    /// 组合交换链（窗口路径）；内容越过 buffer 时整条替换（`grow` / `maybe_shrink`）。
    swapchain: Option<Swapchain>,
    /// 背景层（窗口路径）：**1×1** 的组合交换链，内容恒为窗口背景色（`clear_color`）。
    ///
    /// DComp 把它按客户区缩放后铺满内容之外的区域；背景色变化时整条替换
    /// （[`GpuRenderer::refresh_background`]），内容交换链与内容尺寸都不受影响。
    background_swapchain: Option<Swapchain>,
    /// 背景层自己的 **1×1** 持久目标（窗口路径；离屏目标是 `None`）。
    ///
    /// 背景层 prime 只碰这一块（RTV 堆的第二个描述符）：改色不再借主持久目标，也就不会把
    /// 内容整块清成背景色。生命周期与渲染器一致（`grow` / `maybe_shrink` 只换主持久目标）。
    background_persist: Option<Persist>,
    /// 背景色已改、背景层还没重建（由 [`GpuRenderer::set_background_color`] 置位，
    /// [`GpuRenderer::refresh_background`] 消费）。
    background_dirty: bool,
    /// 合成目标（窗口路径）：GPU 线程在 `grow` / `maybe_shrink` 时把内容 visual 切到新交换链。
    ///
    /// 与 `NativeWindow` 共享（`Arc<Mutex<_>>`）：所有 DComp 调用由互斥锁串行化，GPU 线程只在
    /// `grow` / `maybe_shrink` / `refresh_background` 里持锁做一次 `SetContent` + `Commit`（见
    /// `platform/windows/composition.rs`）。
    composition: Option<Arc<Mutex<Composition>>>,
    rtv_heap: ID3D12DescriptorHeap,
    clear_color: [f32; 4],
    /// 合成面尺寸（持久目标 + 组合交换链的高水位，见 `grown_buffer`）。
    ///
    /// 初值来自 [`buffer_size`]（内容 + 64 余量、256 对齐）；只在内容越过它时按
    /// [`GpuRenderer::grow`] 逐轴取大重建。内容静止 [`SHRINK_IDLE`] 之后
    /// [`GpuRenderer::maybe_shrink`] 把它缩回当前内容的桶（组合交换链不能 `ResizeBuffers`，
    /// 只能整条替换）。
    buffer: (u32, u32),
    /// 当前**内容**尺寸（绘制 / copy / present 的范围；窗口客户区变化时只改这个字段）。
    surface: (u32, u32),
    /// `surface` 最后一次变化的时刻（[`GpuRenderer::maybe_shrink`] 的空闲策略输入）。
    content_since: Instant,
    /// 下一次 present 是否必须全量：组合交换链刚建好之后为 `true`，第一次 present 成功后清除
    /// （首次 present 与首次 copy 都必须覆盖整块 buffer，见 `copy_to_back` / `present1`）。
    ///
    /// 离屏目标没有 present，因此它恒为 `true` —— 与移交给 GPU 线程之前的离屏语义一致。
    fresh: bool,
    /// 最后一次**真正提交**的帧序号（重建资源入 retire 队列时的 tag，§4.3）。
    last_index: FrameIndex,
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

/// `submit_recorded` 的呈现目标。
enum PresentTarget<'a> {
    /// 常规帧：`GpuRenderer` 自己的交换链 + 当前 `buffer` / `surface`。
    Own,
    /// prime 帧：调用方传入的、**还没挂到 visual 上**的新交换链（显式 buffer / content）。
    ///
    /// prime 不写 GPU timer：它会复用某个 ring 槽，若在槽的 timestamp 读回缓冲里录制新样本，
    /// 该槽上一帧尚未 `drain_completed` 的 GPU 采样就会被覆盖（错帧；drain 在 grow 之前，
    /// 但 `grow` 内部的 `wait_idle` 仍可能让一帧在 drain 之后才完成）。
    Detached {
        swapchain: &'a Swapchain,
        buffer: (u32, u32),
        content: (u32, u32),
        /// 这次 prime 录进哪块持久目标。
        persist: PrimePersist,
        /// 内容来源：缩容 prime 在切 visual 之前把**旧**持久目标的内容拷进新目标；其余为
        /// `None`（新建的交换链只 prime 背景色）。
        source: Option<&'a Persist>,
    },
}

/// prime 帧的持久目标选择（`FrameRecorder::persist` 的来源）。
///
/// 用选择器而不是 `&Persist`：prime 需要 `&mut GpuRenderer` 录制，同时又要借一个字段里的
/// 持久目标；在 `submit_recorded` 内部按字段借用就是两次不相交的借用，直接传 `&self.persist`
/// 会和 `&mut self` 冲突。
#[derive(Clone, Copy)]
enum PrimePersist {
    /// 主持久目标（`self.persist`）：grow / 缩容 prime。
    Main,
    /// 1×1 背景持久目标（`self.background_persist`）：背景层 prime。
    Background,
}

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
    /// 对齐），此后窗口 resize 只改内容尺寸（[`GpuRenderer::set_content_size`]），只有内容越过
    /// buffer 时才重建资源（[`GpuRenderer::grow`]）；内容静止 [`SHRINK_IDLE`] 之后
    /// [`GpuRenderer::maybe_shrink`] 把高水位缩回当前内容的桶。
    ///
    /// 本方法在 UI 线程、GPU 线程启动之前运行。顺序（P1-B）：先创建主交换链、**1×1 背景
    /// 交换链**与它自己的 **1×1 背景持久目标**，再各 prime 一帧背景色（第一次 present，必须
    /// 全量；客户区在第一次 present 之前是**完全透明**的，probe `dpi.log` 03/04），**再**把两者
    /// 挂到 [`Composition`] 的 visual 上并 `Commit`。已 attach 但从未 present 的交换链在合成器
    /// 里是透明的，反过来做会让 `ShowWindow` 之前的一帧露出桌面。背景层先 prime、内容层后
    /// prime：两者用同一个 clear color，顺序不影响首次可见像素，这里只是保持「内容最后」的
    /// 既有次序。
    pub(crate) fn new_for_composition(
        composition: Arc<Mutex<Composition>>,
        content: (u32, u32),
        background: [f32; 4],
    ) -> Result<Self, EngineError> {
        let content = (content.0.max(1), content.1.max(1));
        let buffer = buffer_size(content)?;
        let mut renderer = Self::with_shared(SharedGpu::process()?, buffer)?;
        renderer.clear_color = background;
        let mut swapchain = Swapchain::for_composition(renderer.shared.queue(), buffer)?;
        // 背景层是 1×1 的纯色交换链，由 UI 线程的 DComp scale 变换铺满客户区。
        let mut background_swapchain = Swapchain::for_composition(renderer.shared.queue(), (1, 1))?;
        // 背景层有自己的 1×1 持久目标（RTV 堆的第二个描述符）：改色只 clear / copy 这一块，
        // 不再顺带清掉整块主内容目标（旧实现的 P2）。必须在 prime 之前建好。
        let background_rtv = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 1);
        let background_persist = Persist::new(&renderer.shared, (1, 1), background_rtv)?;
        renderer.background_persist = Some(background_persist);
        // prime 用显式的 swapchain / buffer / content / persist；此刻两条 visual 都还没有内容。
        renderer.prime_detached(
            &mut background_swapchain,
            (1, 1),
            (1, 1),
            PrimePersist::Background,
            None,
        )?;
        renderer.prime_detached(&mut swapchain, buffer, content, PrimePersist::Main, None)?;
        {
            let target = composition::lock(&composition);
            target.set_background_content(background_swapchain.raw())?;
            target.set_content(swapchain.raw())?;
            target.commit()?;
        }
        renderer.swapchain = Some(swapchain);
        renderer.background_swapchain = Some(background_swapchain);
        renderer.surface = content;
        // prime 已经做过新交换链的第一次 present，后续帧不再是 `fresh`。
        renderer.fresh = false;
        renderer.composition = Some(composition);
        Ok(renderer)
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
                    // 0 = 内容持久目标；1 = 1×1 背景持久目标（只有窗口路径会创建它）。
                    NumDescriptors: 2,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })
        }
        .map_err(|error| init_error_from("CreateDescriptorHeap(RTV)", error))?;
        // SAFETY: 描述符堆由本结构持有，句柄在堆生命周期内有效。
        let rtv = rtv_handle(shared.raw(), &rtv_heap, 0);
        let persist = Persist::new(&shared, size, rtv)?;
        let upload = UploadArena::new(&shared)?;
        let timer = GpuTimer::new(&shared, &stream)?;

        Ok(Self {
            shared,
            stream,
            descriptors,
            atlas_uploads: AtlasUploads::new(),
            persist,
            upload,
            geometry: GeometryRegistry::new(),
            timer,
            swapchain: None,
            background_swapchain: None,
            background_persist: None,
            background_dirty: false,
            composition: None,
            rtv_heap,
            clear_color: [0.0, 0.0, 0.0, 0.0],
            buffer: (size.0.max(1), size.1.max(1)),
            surface: (size.0.max(1), size.1.max(1)),
            content_since: Instant::now(),
            fresh: true,
            last_index: FrameIndex::default(),
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

    /// 当前的合成面尺寸（测试：内容可以小于它；越过它时 `grow` 按新内容重建一个桶）。
    #[cfg(test)]
    pub(crate) fn buffer(&self) -> (u32, u32) {
        self.buffer
    }

    /// 下一次 present 是否必须全量（组合交换链刚建好时为 `true`，首帧 present 后清除）。
    pub(crate) fn is_fresh(&self) -> bool {
        self.fresh
    }

    /// 帧延迟 waitable（窗口路径的节奏原语，只有 GPU 线程等它）。
    pub(crate) fn waitable(&self) -> Option<windows::Win32::Foundation::HANDLE> {
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
    /// 调用方紧接着要 [`GpuRenderer::resolve_frame_timer`] 读上一帧的 timestamp，因此这里等完
    /// 之后必须先把上一帧的反馈发布掉，再录当前帧（否则 `ResolveQueryData` 会覆盖旧样本）。
    pub(crate) fn wait_slot_fence(&mut self, slot: FrameSlot) -> Result<(), EngineError> {
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

    /// 实现在 `persist::grow`（薄委托）。
    fn grow(&mut self, content: (u32, u32)) -> Result<(), EngineError> {
        persist::grow(self, content)
    }

    /// grow-only 高水位还有缩小的空间、且内容已经静止 [`SHRINK_IDLE`]。
    ///
    /// GPU 循环在**没有待画帧**的空闲路径与**取到帧但尚未录制**的路径各查一次：前者让静止
    /// 窗口也还显存，后者保证持续出帧（HUD / 动画）时缩容不会被「总有一帧待画」饿死。
    pub(crate) fn shrink_due(&self) -> bool {
        self.shrink_candidate().is_some() && self.content_since.elapsed() >= SHRINK_IDLE
    }

    /// 空闲等待的定时唤醒：还有缩小空间时，等到内容静止满 [`SHRINK_IDLE`] 再醒一次。
    ///
    /// 已经是最小桶（或没有合成目标）时返回 `None`：空闲等待保持无限等，不引入周期性唤醒。
    pub(crate) fn shrink_in(&self) -> Option<Duration> {
        self.shrink_candidate()?;
        Some(SHRINK_IDLE.saturating_sub(self.content_since.elapsed()))
    }

    /// 当前内容对应的、可以缩回去的桶（`None` = 没有合成目标 / 已经是最小桶）。
    fn shrink_candidate(&self) -> Option<(u32, u32)> {
        shrink_target(self.buffer, self.surface, self.composition.is_some())
            .ok()
            .flatten()
    }

    /// 实现在 `persist::maybe_shrink`（薄委托）。
    pub(crate) fn maybe_shrink(&mut self) -> Result<(), EngineError> {
        persist::maybe_shrink(self)
    }

    /// 实现在 `persist::resize`（薄委托）。
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        persist::resize(self, size)
    }

    /// 改窗口背景色（GPU 线程；离屏目标不使用它）。
    ///
    /// 组合交换链没有可用的 `SetBackgroundColor`（probe：返回 OK 但惰性），背景色只作为持久
    /// 目标的 clear color（linear，`render::color::background_linear`）和背景层的 prime 颜色：
    /// 下一次 `Plan::Full` 的 clear 生效，背景层由 [`GpuRenderer::refresh_background`] 重建。
    /// 窗口的 `set_background_color` 已经标了全量 damage，这里**不**碰 `fresh`（那会让整块
    /// buffer 全量 copy + present）。
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
    fn prime_detached(
        &mut self,
        swapchain: &mut Swapchain,
        buffer: (u32, u32),
        content: (u32, u32),
        persist: PrimePersist,
        source: Option<&Persist>,
    ) -> Result<(), EngineError> {
        record::prime_detached(self, swapchain, buffer, content, persist, source)
    }

    /// 离屏 / 快照路径（以及窗口的首次 present）：提交一份快照并**同步**等完成（§4.5 的下半段）。
    ///
    /// `frame` 由调用方构造（`fresh` 的判定权在资源所有者手上，§4.1）；窗口路径的常规帧用
    /// [`GpuRenderer::submit_frame`]（异步，完成状态由 GPU 循环按 fence 推进）。
    pub(crate) fn submit_snapshot(
        &mut self,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
    ) -> Result<RenderStats, EngineError> {
        let slot = self.stream.acquire_slot();
        self.stream
            .wait_slot_fence(slot.wait_value)
            .map_err(|error| self.classify(error))?;
        let (stats, slot) =
            self.submit_recorded(slot, index, payload, frame, timings, PresentTarget::Own)?;
        self.finish_sync(slot, timings)?;
        Ok(stats)
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
        let payload = FramePayload::from_buffer(buffer);
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
    pub(crate) fn submit_frame(
        &mut self,
        request: &FrameRequest,
        frame: &mut FramePlan,
        slot: FrameSlot,
    ) -> Result<(RenderStats, Vec<GpuTimerSample>), EngineError> {
        timings::submit_frame(self, request, frame, slot)
    }

    /// 观察完成之后读本槽的 GPU 时间（`timestamp` 读回缓冲此时不再被 GPU 写）。
    ///
    /// 调用前置条件：该槽的 fence 已完成（[`GpuRenderer::wait_slot_fence`] 或
    /// [`GpuRenderer::completed_value`] 越过其 signal 值）。
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
        target: PresentTarget<'_>,
    ) -> Result<(RenderStats, FrameSlot), EngineError> {
        record::submit_recorded(self, slot, index, payload, frame, timings, target)
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
        self.stream.wait_idle()?;
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
            self.stream.retire(self.last_index, handle);
        }
        Ok(())
    }

    /// 把底层错误归类为设备丢失（粘性）或原样返回。
    fn classify(&mut self, error: EngineError) -> EngineError {
        self.shared.note_failure().unwrap_or(error)
    }
}

impl Drop for GpuRenderer {
    fn drop(&mut self) {
        // 退出路径：等 GPU 完成所有在飞提交，并丢掉命令列表对 back buffer 的引用，
        // 之后才销毁 swapchain（§6.7 的关闭握手由门面负责线程 join）。
        let _ = self.stream.wait_idle();
        let _ = self.stream.reset_in_flight_lists();
        // 进程级设备不会随窗口销毁：本窗口的常驻资源必须在这里从共享句柄表里注销，
        // 否则每次关窗都会把 persist / 上传缓冲 / staging / 几何缓冲留在进程里。
        // GPU 已 idle、命令列表已重置，因此没有在飞引用；retire 队列里残留的句柄（包括
        // 录制失败、从未提交的帧）也在这里一次性释放。
        let _ = self.stream.release_completed();
        self.stream.drain_retire();
        self.upload.release(&self.shared);
        self.geometry.release(&self.shared);
        self.atlas_uploads.release(&self.shared);
        let _ = self.shared.release(self.persist.handle);
        if let Some(background) = self.background_persist.as_ref() {
            let _ = self.shared.release(background.handle);
        }
    }
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
    geometry: &'a mut GeometryRegistry,
    timer: Option<&'a GpuTimer>,
    swapchain: Option<&'a Swapchain>,
    /// 缩容 prime 的内容来源（旧主持久目标）；其余帧为 `None`。
    source: Option<&'a Persist>,
    clear_color: [f32; 4],
    /// 高水位的合成面尺寸（只有资源创建与 clear 用它）。
    buffer: (u32, u32),
    /// 当前内容尺寸（viewport / scissor / root uniform / copy / present 的范围）。
    content: (u32, u32),
    /// 窗口 DPI（`frame.params.x`；geometry 顶点是逻辑像素，shader 用它换物理）。
    scale: f32,
}

impl FrameRecorder<'_> {
    /// 实现在 `record::record`（薄委托）。
    fn record(
        &mut self,
        slot: FrameSlot,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
        frame_started: Instant,
    ) -> Result<RenderStats, EngineError> {
        record::record(self, slot, index, payload, frame, timings, frame_started)
    }
}

/// 持久 offscreen（唯一绘制目标；资源格式 UNORM + RTV 视图 sRGB，DEVIATIONS F-80）。
struct Persist {
    texture: ID3D12Resource,
    handle: GpuResourceHandle,
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
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

    /// 实现在 `upload::write_uploads`（薄委托）。
    fn write(
        &mut self,
        shared: &SharedGpu,
        stream: &mut CommandStream,
        index: FrameIndex,
        slot: usize,
        instances: &[u8],
        clips: &[GpuClipEntry],
    ) -> Result<(u32, u32), EngineError> {
        upload::write_uploads(self, shared, stream, index, slot, instances, clips)
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

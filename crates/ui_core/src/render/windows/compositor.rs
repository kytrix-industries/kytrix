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

use windows::Win32::{
    Foundation::{HANDLE, RECT},
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
use crate::paint::instance::InstanceBuffer;
#[cfg(any(test, feature = "test-support"))]
use crate::render::windows::readback::{Readback, ReadbackImage};
use crate::{
    error::EngineError,
    paint::{
        atlas::AtlasKind,
        instance::{FrameOp, GeometryDrawGpu, GpuClipEntry},
    },
    platform::windows::composition::{self, Composition, buffer_size},
    render::{
        RenderStats,
        compositor::{FramePlan, Plan},
        frame::{FramePayload, FrameRequest, FrameSlot, MAX_FRAMES_IN_FLIGHT},
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample, Section},
        uniform::{ROOT_UNIFORM_DWORDS, RootUniform},
        windows::{
            atlas::{AtlasUploads, SharedAtlas},
            device::{DescriptorLease, SharedGpu, init_error_from},
            pipeline::{GeometryRegistry, Pipeline, SrvHeap},
            queue::CommandStream,
            swapchain::Swapchain,
            timer::GpuTimer,
        },
    },
    window::FrameIndex,
};

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

    /// 内容尺寸变化（窗口路径）：内容仍在 buffer 内时是**纯 CPU 记账**；越过 buffer 时走
    /// [`GpuRenderer::grow`] 重建持久目标 + 组合交换链（grow-only 桶）。
    ///
    /// 窗口的 resize 已经把整帧标成全量 damage（`Window::set_surface`），下一帧的
    /// `FramePlan` 是 `Full`，不需要在这里碰 `fresh`。每次真实变化都刷新 `content_since`：
    /// 缩容策略只在内容静止 [`SHRINK_IDLE`] 之后才动手（拖动中永远不缩）。
    pub(crate) fn set_content_size(&mut self, content: (u32, u32)) -> Result<(), EngineError> {
        let content = (content.0.max(1), content.1.max(1));
        if content == self.surface {
            return Ok(());
        }
        self.content_since = Instant::now();
        if content.0 <= self.buffer.0 && content.1 <= self.buffer.1 {
            self.surface = content;
            return Ok(());
        }
        self.grow(content)
    }

    /// 内容越过 buffer：重建持久目标 + 组合交换链，并把 visual 切到新交换链。
    ///
    /// 组合交换链在第一次 present 之后不能 `ResizeBuffers`，所以只能整条替换。顺序：
    /// 1. `wait_idle` + 重置全部 in-flight 命令列表：释放旧 back buffer 的引用，之后旧交换链
    ///    才能随字段替换真正释放（否则三个 buffer 被命令列表钉住）。
    /// 2. 在旧对象仍完好时创建新的持久目标与交换链（创建失败 ⇒ 原状态不变，错误冒泡）。
    /// 3. **先**用新的 buffer / content prime 新交换链（全量背景 + 等 GPU fence），**然后**才
    ///    `SetContent(新) + Commit`（P1-B：已 attach 但从未 present 的交换链在合成器里是透明的，
    ///    反过来做会在 Commit 与 prime 之间露出一帧桌面）。
    /// 4. 视觉切换**之后**才替换字段（旧 swapchain / 旧 persist 在此 drop；旧 persist 的纹理
    ///    句柄按最后一帧序号进 retire 队列）。
    /// 5. `fresh = false`（prime 已经做了新交换链的第一次全量 present），下一帧按内容子矩形 copy。
    ///
    /// 代价：越过 buffer 边界的那一帧只呈现背景色（在 UI 线程把内容 clip 更新到新尺寸之前会
    /// 看到一帧纯背景，而不是桌面或未定义像素）。这是「第一次 present 之后不能 `ResizeBuffers`」
    /// 的必然结果，比整窗黑一帧好。内容 visual 始终 1:1，`grow` 不改变它的缩放。
    fn grow(&mut self, content: (u32, u32)) -> Result<(), EngineError> {
        // grow-only 高水位：新桶 = 逐轴取「按内容算出的桶」与「当前 buffer」的最大值（见
        // [grown_buffer]）。上限检查在 max 之后：`buffer_size` 已经拒绝 > 16384 的内容，
        // 而当前 buffer 永远 ≤ 上限。
        let buffer = grown_buffer(self.buffer, content)?;
        let Some(composition) = self.composition.clone() else {
            return Err(EngineError::Composition {
                detail: smol_str::SmolStr::new_static(
                    "content exceeds the buffer on a renderer without a composition target",
                ),
            });
        };
        // 1. GPU 完成 + 丢掉命令列表对旧 back buffer 的引用。
        self.stream
            .wait_idle()
            .map_err(|error| self.classify(error))?;
        self.stream
            .reset_in_flight_lists()
            .map_err(|error| self.classify(error))?;
        // 2. 新对象在旧对象销毁之前创建（失败时旧状态仍然完好）。
        let rtv = rtv_handle(self.shared.raw(), &self.rtv_heap, 0);
        let persist =
            Persist::new(&self.shared, buffer, rtv).map_err(|error| self.classify(error))?;
        let mut swapchain = Swapchain::for_composition(self.shared.queue(), buffer)?;
        // 3. **先**把新交换链 prime 一次（全量背景 + 等 GPU fence），**再**挂到 visual 上：
        //    已 attach 但从未 present 的交换链在合成器里是透明的（P1-B）。prime 用显式的新
        //    `swapchain` / `buffer` / `content` 记账，旧 visual 在这期间继续显示旧内容。
        //    旧 persist 先换下来、但保持存活到视觉切换之后；prime 用的是新 persist。
        let retired_index = self.last_index;
        let old_persist = std::mem::replace(&mut self.persist, persist);
        self.prime_detached(&mut swapchain, buffer, content, PrimePersist::Main, None)?;
        {
            let target = composition::lock(&composition);
            target.set_content(swapchain.raw())?;
            target.commit()?;
        }
        // 4. 视觉切换**之后**才替换 / 丢弃旧对象（旧 swapchain 的已 present 内容可能仍被
        //    DWM 引用到下一次 `Commit`，本地变量保证它活到 `Commit` 之后）。
        let _old_swapchain = self.swapchain.replace(swapchain);
        self.stream.retire(retired_index, old_persist.handle);
        self.buffer = buffer;
        self.surface = content;
        // prime 已经做过新交换链的第一次 present，后续帧不再是 `fresh`。
        self.fresh = false;
        self.content_since = Instant::now();
        Ok(())
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

    /// 把 grow-only 高水位缩回**当前内容**所需的桶（内容保留，视觉切换没有闪帧）。
    ///
    /// 组合交换链不能 `ResizeBuffers`，所以与 `grow` 一样整条替换；差别是内容不能丢：
    /// 1. 决策：`buffer_size(surface)` 不比当前 buffer 小 ⇒ 已经是最小桶，no-op。
    /// 2. `wait_idle` + 重置全部 in-flight 命令列表（旧 back buffer / 旧持久目标的引用）。
    /// 3. 旧对象仍完好时创建新持久目标 + 新交换链。
    /// 4. 新持久目标换进来、旧目标留在栈上；prime 先整块 clear 成 `clear_color`，再把旧目标的
    ///    `(0, 0, content)` 拷进左上角，然后全量 copy / present 并等队列 fence。
    /// 5. `SetContent(新) + Commit` 之后才替换 / 丢弃旧对象（旧交换链活到 `Commit` 之后）。
    /// 6. `buffer = 新桶`、`fresh = false`、`content_since = now`。
    ///
    /// 可见像素在整条路径上不变：旧内容原样拷进新持久目标，prime 的是它，切 visual 时内容已经
    /// 就绪，不需要「先露一帧背景色」（那是 `grow` 越过 buffer 边界时的代价）。
    ///
    /// 旧持久目标的纹理在第二次 `reset_in_flight_lists` 之后直接交回 `device`：prime 的命令
    /// 列表刚刚引用过它，不重置的话纹理要等到下一次同槽录制才真正释放（静止窗口可能永远不释放）。
    pub(crate) fn maybe_shrink(&mut self) -> Result<(), EngineError> {
        let Some(buffer) = shrink_target(self.buffer, self.surface, self.composition.is_some())?
        else {
            // 已经是最小桶（或没有合成目标）：推迟下一次检查，避免空闲路径立刻重试。
            self.content_since = Instant::now();
            return Ok(());
        };
        let Some(composition) = self.composition.clone() else {
            // `shrink_target` 已经把「没有合成目标」判成 `None`；这里只是不构造一个假的 Arc。
            return Ok(());
        };
        debug_assert!(
            on_gpu_thread(),
            "the high-water bucket may only shrink on the kytrix-d3d12 thread"
        );
        // 1. GPU 完成 + 丢掉命令列表对旧 back buffer / 旧持久目标的引用。
        self.stream
            .wait_idle()
            .map_err(|error| self.classify(error))?;
        self.stream
            .reset_in_flight_lists()
            .map_err(|error| self.classify(error))?;
        // 2. 新对象在旧对象销毁之前创建（失败时旧状态仍然完好）。
        let rtv = rtv_handle(self.shared.raw(), &self.rtv_heap, 0);
        let persist =
            Persist::new(&self.shared, buffer, rtv).map_err(|error| self.classify(error))?;
        let mut swapchain = Swapchain::for_composition(self.shared.queue(), buffer)?;
        // 3. prime 从旧持久目标拷内容，所以先把新目标换进来、旧目标留在栈上保持存活。
        let old_persist = std::mem::replace(&mut self.persist, persist);
        self.prime_detached(
            &mut swapchain,
            buffer,
            self.surface,
            PrimePersist::Main,
            Some(&old_persist),
        )?;
        {
            let target = composition::lock(&composition);
            target.set_content(swapchain.raw())?;
            target.commit()?;
        }
        // 4. 视觉切换之后才替换 / 丢弃旧对象（旧交换链的已 present 内容可能仍被 DWM 引用到
        //    本次 `Commit` 之后）。
        let _old_swapchain = self.swapchain.replace(swapchain);
        // prime 的命令列表刚引用过旧持久目标：重置列表把引用丢掉，再把纹理直接交回 device。
        self.stream
            .reset_in_flight_lists()
            .map_err(|error| self.classify(error))?;
        let _ = self.shared.release(old_persist.handle);
        drop(old_persist);
        self.buffer = buffer;
        // prime 已经做过新交换链的第一次全量 present，后续帧按内容子矩形 copy。
        self.fresh = false;
        self.content_since = Instant::now();
        Ok(())
    }

    /// 离屏 / 快照路径的尺寸变化：重建持久目标（**没有** swapchain）。
    ///
    /// 窗口路径不经过这里：内容尺寸由 [`GpuRenderer::set_content_size`] 记账，buffer 由
    /// [`GpuRenderer::grow`] 重建（组合交换链不能 `ResizeBuffers`）。
    ///
    /// 每一步失败都走 [`GpuRenderer::classify`]（设备丢失优先归类）；调用方收到任何错误都会
    /// 直接冒泡（§6.6：不重试）。
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        debug_assert!(
            self.swapchain.is_none(),
            "window targets never resize their composition swapchain"
        );
        let size = (size.0.max(1), size.1.max(1));
        if size == self.surface {
            return Ok(());
        }
        self.stream
            .wait_idle()
            .map_err(|error| self.classify(error))?;
        // RTV 句柄由堆派生（堆是所有者；`Persist` 只缓存句柄本身）。
        let rtv = rtv_handle(self.shared.raw(), &self.rtv_heap, 0);
        let old = self
            .persist
            .recreate(&self.shared, size, rtv)
            .map_err(|error| self.classify(error))?;
        self.stream.retire(self.last_index, old);
        self.buffer = size;
        self.surface = size;
        self.content_since = Instant::now();
        self.fresh = true;
        Ok(())
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

    /// 把新的背景色落到独立的 **1×1 背景层**（GPU 线程；颜色没变是 no-op）。
    ///
    /// 背景层是单独一条交换链 + 自己的 **1×1 持久目标**（`background_persist`），已经挂到
    /// visual 上的内容不会因为 `clear_color` 改变而变化，所以换色必须：新建一条 1×1 组合
    /// 交换链 → 用 1×1 持久目标 prime 新背景色（全量 + 等 GPU fence）→ `SetContent` + `Commit`
    /// → 丢弃旧交换链。顺序与 `grow` 相同（prime 先于 attach，P1-B）。1×1 纹理 + 一次 1×1
    /// copy，代价可忽略；即使当前没有内容帧，背景色也会立刻生效。
    ///
    /// 主持久目标完全不受影响：旧实现借主 persist prime 1×1，每次改色都会顺带把整块内容目标
    /// 清成背景色（罕见但错误，且对大 buffer 是整块 clear 带宽）。
    ///
    /// 离屏目标没有合成树：只留下新的 `clear_color`，标记直接消费掉。
    pub(crate) fn refresh_background(&mut self) -> Result<(), EngineError> {
        if !self.background_dirty {
            return Ok(());
        }
        let Some(composition) = self.composition.clone() else {
            self.background_dirty = false;
            return Ok(());
        };
        debug_assert!(
            on_gpu_thread(),
            "background layers may only be replaced on the kytrix-d3d12 thread"
        );
        let mut swapchain = Swapchain::for_composition(self.shared.queue(), (1, 1))?;
        self.prime_detached(
            &mut swapchain,
            (1, 1),
            (1, 1),
            PrimePersist::Background,
            None,
        )?;
        {
            let target = composition::lock(&composition);
            target.set_background_content(swapchain.raw())?;
            target.commit()?;
        }
        // 视觉切换之后才丢弃旧背景交换链（DWM 可能仍引用它到本次 `Commit` 之后）。
        let _old_background = self.background_swapchain.replace(swapchain);
        self.background_dirty = false;
        Ok(())
    }

    /// 把一帧背景色 prime 进 `swapchain`（全量 clear + 等 GPU fence），**不碰 visual**。
    ///
    /// 用于新交换链的第一次 present：组合交换链的第一次 present 必须是全量
    /// （`DirtyRectsCount = 0`），而 `WS_EX_NOREDIRECTIONBITMAP` 的客户区在第一次 present 之前
    /// 完全透明。`new_for_composition` / `grow` / `maybe_shrink` / `refresh_background` 必须在
    /// `SetContent` **之前**调用本方法：已 attach 但从未 present 的交换链在合成器里是透明的，
    /// 若先 `SetContent` + `Commit` 再 prime，中间一次 DWM 合成就会露出桌面（P1-B）。这里用显式
    /// 的 `swapchain` / `persist` / `buffer` / `content`，而不是 `self.swapchain`（它可能还是旧的）。
    ///
    /// 背景层的 1×1 交换链也走这里（`persist = PrimePersist::Background`、
    /// `buffer = content = (1, 1)`）：prime 只 clear 那块独立的 1×1 持久目标，再 copy 左上角
    /// 1×1 —— 拷到的必然是背景色，主持久目标一个像素都不碰。
    ///
    /// `source` 是缩容 prime 的内容来源（旧主持久目标），在 clear 之后、绘制之前把
    /// `(0, 0, content)` 拷进新目标的左上角；其余 prime 为 `None`。
    ///
    /// `sync = 0`：prime 不是用户可见帧，不值得等 vsync —— 但**必须**等这一帧的队列 fence
    /// （P2-1）：`Present1` 返回只是 CPU 侧入队，`ShowWindow` / 后续帧的 waitable 之前必须有
    /// 已完成的合成内容。
    fn prime_detached(
        &mut self,
        swapchain: &mut Swapchain,
        buffer: (u32, u32),
        content: (u32, u32),
        persist: PrimePersist,
        source: Option<&Persist>,
    ) -> Result<(), EngineError> {
        let slot = self.stream.acquire_slot();
        self.stream
            .wait_slot_fence(slot.wait_value)
            .map_err(|error| self.classify(error))?;
        let payload = FramePayload::empty();
        let mut frame = FramePlan::new(Plan::Full, true);
        let mut timings = FrameTimings::new();
        // prime 不是内容帧：不要让它把 `last_index`（retire 的 tag）推回 `FrameIndex::default()`。
        // `grow` / `maybe_shrink` 已经在 prime 之前取好 `retired_index`；背景层刷新则发生在帧
        // 之间，两者都不该改变「最后一次内容帧」的记账。
        let last_index = self.last_index;
        self.submit_recorded(
            slot,
            FrameIndex::default(),
            &payload,
            &mut frame,
            &mut timings,
            PresentTarget::Detached {
                swapchain,
                buffer,
                content,
                persist,
                source,
            },
        )?;
        self.last_index = last_index;
        swapchain
            .present1(&mut frame, 0, content)
            .map_err(|error| self.classify(error))?;
        // 等到本帧的 GPU 工作完成（`FrameSlot::signal_value`），不是 `Present1` 的 CPU 返回。
        self.stream
            .wait_slot_fence(slot.signal_value)
            .map_err(|error| self.classify(error))?;
        Ok(())
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

    /// 同步路径的收尾：等本帧完成 → 读本帧 GPU 时间 → 推进 retire（I4）。
    fn finish_sync(
        &mut self,
        slot: FrameSlot,
        timings: &mut FrameTimings,
    ) -> Result<(), EngineError> {
        // 同步语义：本帧命令必须完成，GPU 时间与随后的读回才有意义。
        self.stream
            .wait_slot_fence(slot.signal_value)
            .map_err(|error| self.classify(error))?;
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
        self.stream.release_completed();
        Ok(())
    }

    /// 提交一份跨线程帧快照（窗口路径：**不等** GPU 完成，I2）。
    ///
    /// 调用方（GPU 循环）已经等过 DXGI 的帧延迟 waitable（I3）、并用
    /// [`GpuRenderer::set_content_size`] 把内容尺寸对齐到本帧；`slot` 是调用方已等到上一帧完成、
    /// 尚未复位的 ring 槽（[`GpuRenderer::acquire_slot`] + [`GpuRenderer::wait_slot_fence`]）。
    ///
    /// 本方法**不**读 timestamp、不推进 retire：这两件事只允许在观察到该帧 fence 完成之后做
    /// （[`GpuRenderer::resolve_frame_timer`] / [`GpuRenderer::release_completed`]，§4.1 / §6.5）。
    pub(crate) fn submit_frame(
        &mut self,
        request: &FrameRequest,
        frame: &mut FramePlan,
        slot: FrameSlot,
    ) -> Result<(RenderStats, Vec<GpuTimerSample>), EngineError> {
        debug_assert!(
            on_gpu_thread(),
            "D3D12 window frames may only be submitted on the kytrix-d3d12 thread"
        );
        let mut timings = FrameTimings::new();
        let (stats, _) = self.submit_recorded(
            slot,
            request.index,
            &request.payload,
            frame,
            &mut timings,
            PresentTarget::Own,
        )?;

        let present_started = Instant::now();
        let presented = match self.swapchain.as_mut() {
            // §6.3：`Present1(1, dirty)` —— sync = 1 保持 vsync。
            Some(swapchain) => swapchain.present1(frame, 1, self.surface),
            None => Ok(()),
        };
        if let Err(error) = presented {
            return Err(self.classify(error));
        }
        self.fresh = false;
        timings.push(GpuTimerSample {
            section: Section::Present,
            cpu_ns: elapsed_ns(present_started),
            gpu_ns: 0,
        });
        Ok((stats, timings.samples().to_vec()))
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

    /// 录制 + 提交一帧（不呈现、不等完成）。调用方先取槽并等该槽上一帧完成（§6.5）。
    ///
    /// `target` 决定录制的呈现目标：常规帧用自己的交换链；`prime_detached` 用**还没挂到
    /// visual 上**的新交换链 + 显式持久目标（grow / 缩容 prime 用主持久目标，背景层 prime 用
    /// 1×1 背景持久目标）。
    ///
    /// 共享图集的页对齐在录制之前完成：**图集读锁**在整个录制 + 提交区间内持有（共享页在帧内
    /// 不会变；别的窗口的页重建是写锁，会等本帧提交完）。发现待上传页与共享页不一致时先释放
    /// 读锁、在写锁里重建（[`SharedGpu::rebuild_atlas_pages`]），然后重试。
    fn submit_recorded(
        &mut self,
        slot: FrameSlot,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
        target: PresentTarget<'_>,
    ) -> Result<(RenderStats, FrameSlot), EngineError> {
        if let Some(error) = self.shared.lost_error() {
            return Err(error);
        }
        timings.reset(index);
        debug_assert_eq!(
            timings.frame(),
            index,
            "timings must be reset for this frame"
        );
        let frame_started = Instant::now();
        let shared = Arc::clone(&self.shared);
        loop {
            // 图集读锁：持有到本帧提交完成（页重建是写锁，会等这里释放）。
            let atlas = shared.atlas();
            // 待上传页 ≠ 共享页 ⇒ 先落地页换代：全页上传触发重建，过期增量直接丢弃。
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
            // staging 对齐到共享页（第一次上传 / 页换代之后整组重建本窗口的 staging）。
            self.atlas_uploads
                .sync_staging(&shared, &atlas)
                .map_err(|error| self.classify(error))?;

            self.stream
                .reset_slot(slot)
                .map_err(|error| self.classify(error))?;

            let stats = {
                let (swapchain, buffer, content, timer, persist, source) = match target {
                    PresentTarget::Own => (
                        self.swapchain.as_ref(),
                        self.buffer,
                        self.surface,
                        self.timer.as_ref(),
                        &self.persist,
                        None,
                    ),
                    PresentTarget::Detached {
                        swapchain,
                        buffer,
                        content,
                        persist,
                        source,
                    } => {
                        let persist = match persist {
                            PrimePersist::Main => &self.persist,
                            PrimePersist::Background => self
                                .background_persist
                                .as_ref()
                                .ok_or_else(|| EngineError::Composition {
                                    detail: smol_str::SmolStr::new_static(
                                        "background prime without a 1x1 background persist",
                                    ),
                                })?,
                        };
                        (Some(swapchain), buffer, content, None, persist, source)
                    }
                };
                // 借用拆分：录制只碰这些字段，`buffer` / `surface` / `fresh` / `last_index` 留在外层。
                let mut recorder = FrameRecorder {
                    shared: &shared,
                    stream: &mut self.stream,
                    pipeline: shared.pipeline(),
                    descriptors: &self.descriptors,
                    atlas: &atlas,
                    atlas_uploads: &mut self.atlas_uploads,
                    persist,
                    upload: &mut self.upload,
                    geometry: &mut self.geometry,
                    timer,
                    swapchain,
                    source,
                    clear_color: self.clear_color,
                    buffer,
                    content,
                    scale: self.scale,
                };
                recorder.record(slot, index, payload, frame, timings, frame_started)
            }
            .map_err(|error| self.classify(error))?;

            self.stream
                .submit(index, slot)
                .map_err(|error| self.classify(error))?;
            self.last_index = index;
            return Ok((stats, slot));
        }
    }

    /// 图集上传入口：字形页与图片页共用（GPU 线程消费）。
    ///
    /// `pixels` 是 `rows` 顺序紧密打包的行（整页上传时就是整页），按**所有权**移动进待上传槽，
    /// 避免槽 → 纹理之间再复制一次（DEVIATIONS F-211）。入队时同时记录**当前共享页**
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
            "window atlas uploads may only be queued on the kytrix-d3d12 thread"
        );
        let queued_page = self.shared.atlas_page(kind);
        self.atlas_uploads
            .queue_upload(kind, page, queued_page, pixels, rows);
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

/// 增长后的 buffer：逐轴取「当前 buffer」与「按内容算出的桶」的较大值（grow-only 高水位）。
///
/// 只按内容算时，corner drag（一轴变大、另一轴变小）会把已经开好的另一轴换小；而视觉切换那一
/// 帧的旧 clip 仍指向旧内容，就会采样到新交换链之外（未定义像素 / 露出桌面）。逐轴取大保证
/// 「旧 presented 内容 ≤ 旧 buffer ≤ 新 buffer」，P1-B 的中间帧因此始终有定义。
fn grown_buffer(current: (u32, u32), content: (u32, u32)) -> Result<(u32, u32), EngineError> {
    let required = buffer_size(content)?;
    Ok((required.0.max(current.0), required.1.max(current.1)))
}

/// 缩容决策：返回新的 buffer，`None` = 保持当前高水位。
///
/// - 没有合成目标（离屏 / 快照）：永不缩容（离屏由 [`GpuRenderer::resize`] 直接重建）。
/// - 当前内容所需的桶已经覆盖当前 buffer（两轴都 ≥）：已经是最小桶。这也覆盖 `None` 的另一半
///   语义 —— 内容 > buffer 的 grow-only 不变量被破坏时不得「缩」成更大的桶。
/// - 否则返回 `buffer_size(content)`：内容 ≤ surface ≤ buffer，所以它逐轴 ≤ 当前 buffer。
fn shrink_target(
    buffer: (u32, u32),
    content: (u32, u32),
    has_composition: bool,
) -> Result<Option<(u32, u32)>, EngineError> {
    if !has_composition {
        return Ok(None);
    }
    let target = buffer_size(content)?;
    if target == buffer || target.0 > buffer.0 || target.1 > buffer.1 {
        return Ok(None);
    }
    Ok(Some(target))
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
    /// 上传 + 图集 + clear/scissor 绘制 + copy 到 back buffer + 计时（不提交）。
    fn record(
        &mut self,
        slot: FrameSlot,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
        frame_started: Instant,
    ) -> Result<RenderStats, EngineError> {
        let (instance_count, clip_count) = self.upload.write(
            self.shared,
            self.stream,
            index,
            slot.index,
            payload.instances(),
            payload.clips(),
        )?;
        let srv_heap = self.shared.srv_heap();
        srv_heap.write_slot(
            self.shared.raw(),
            self.descriptors.base(),
            slot.index,
            self.upload.slot(slot.index).instances(),
            instance_count,
            self.upload.slot(slot.index).clips(),
            clip_count,
        );
        timings.push(GpuTimerSample {
            section: Section::Upload,
            cpu_ns: elapsed_ns(frame_started),
            gpu_ns: 0,
        });

        let root = root_uniform(self.content.0 as f32, self.content.1 as f32, self.scale);
        let full = frame.is_full();
        let scissor_whole = RECT {
            left: 0,
            top: 0,
            right: self.content.0 as i32,
            bottom: self.content.1 as i32,
        };
        // 几何绘制的下标表与 ops 同帧：`FrameOp::Geometry.index` 只有配合同一帧的表才有意义。
        let mut draw_calls = 0u32;
        // staging 扩容造成的旧句柄：录制块结束后再 retire（`list` 借用 `self.stream`）。
        let atlas_retired: Vec<GpuResourceHandle>;
        // 几何换代（槽位复用）产生的旧缓冲句柄，同样在录制块结束后 retire。
        let mut geometry_retired: Vec<GpuResourceHandle> = Vec::new();
        {
            let list = self.stream.list(slot);
            if let Some(timer) = self.timer {
                timer.begin(list, slot.index);
            }
            atlas_retired =
                self.atlas_uploads
                    .record_uploads(self.shared, self.atlas, list, slot.index)?;
            if full {
                // `fresh`（交换链刚建好 / prime）必须清整块 buffer：prime 之后、UI 把内容 clip
                // 更新到新尺寸之前，visual 的 clip 可能仍指向旧内容（P1-A 保证它 ≤ 新 buffer），
                // 只有整块背景色能保证那一帧既不是桌面、也不是未定义像素（P1-B）。
                // 普通全量帧只清内容矩形：buffer 的其余部分永远不可见（copy / present dirty 都
                // 夹在内容子矩形内），省掉大 buffer 的全量 clear 带宽。
                let content_rect = [RECT {
                    left: 0,
                    top: 0,
                    right: self.content.0 as i32,
                    bottom: self.content.1 as i32,
                }];
                let rects = if frame.is_fresh() {
                    None
                } else {
                    Some(content_rect.as_slice())
                };
                // SAFETY: 目标 RT 处于 `RENDER_TARGET`（本帧未拷贝到 swapchain 时恒如此）；
                // `rects` 在调用期间存活且落在 buffer 内（内容 ≤ buffer）。
                unsafe { list.ClearRenderTargetView(self.persist.rtv, &self.clear_color, rects) };
            }
            // 缩容 prime：新目标整块已经 clear 成背景色（内容之外不可见），把旧目标的内容原样
            // 拷进左上角；此后这一帧的绘制 / copy 都发生在新目标上。源目标仍是 `RENDER_TARGET`。
            if let Some(source) = self.source {
                debug_assert!(
                    full && frame.is_fresh(),
                    "a source-copy prime is a fresh full frame"
                );
                copy_content(list, source, self.persist, self.content);
            }
            bind_frame(
                list,
                self.pipeline,
                srv_heap,
                self.descriptors.base(),
                self.persist,
                slot.index,
                root,
                self.content,
            );
            match full {
                true => {
                    draw_calls = draw_ops(
                        list,
                        self.pipeline,
                        payload.ops(),
                        self.geometry,
                        self.shared,
                        &mut geometry_retired,
                        payload,
                        root,
                        scissor_whole,
                    )?;
                }
                false => {
                    if let Plan::Partial(damage) = frame.plan() {
                        for rect in damage.as_slice() {
                            let scissor = RECT {
                                left: rect.x0,
                                top: rect.y0,
                                right: rect.x1,
                                bottom: rect.y1,
                            };
                            // SAFETY: RTV / 视口 / 矩形由本帧持有，调用期间有效。
                            unsafe { list.RSSetScissorRects(&[scissor]) };
                            draw_calls = draw_calls.saturating_add(draw_ops(
                                list,
                                self.pipeline,
                                payload.ops(),
                                self.geometry,
                                self.shared,
                                &mut geometry_retired,
                                payload,
                                root,
                                scissor,
                            )?);
                        }
                    }
                }
            }
            if let Some(swapchain) = self.swapchain {
                copy_to_back(
                    list,
                    self.persist,
                    swapchain,
                    *frame,
                    self.buffer,
                    self.content,
                )?;
            }
            if let Some(timer) = self.timer {
                timer.end(list, slot.index);
            }
        }
        for handle in atlas_retired {
            self.stream.retire(index, handle);
        }
        for handle in geometry_retired {
            self.stream.retire(index, handle);
        }

        Ok(RenderStats {
            draw_calls,
            instances: instance_count,
        })
    }
}

/// 持久 offscreen（唯一绘制目标；资源格式 UNORM + RTV 视图 sRGB，DEVIATIONS F-80）。
struct Persist {
    texture: ID3D12Resource,
    handle: GpuResourceHandle,
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
}

impl Persist {
    fn new(
        shared: &SharedGpu,
        size: (u32, u32),
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Result<Self, EngineError> {
        let texture = create_target(shared.raw(), size)?;
        let handle = shared.allocate(texture.clone());
        create_render_target_view(shared.raw(), &texture, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, rtv);
        Ok(Self {
            texture,
            handle,
            rtv,
        })
    }

    /// 重建；返回旧纹理的句柄（调用方决定 retire 还是立即释放，I4）。
    fn recreate(
        &mut self,
        shared: &SharedGpu,
        size: (u32, u32),
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Result<GpuResourceHandle, EngineError> {
        let texture = create_target(shared.raw(), size)?;
        let handle = shared.allocate(texture.clone());
        create_render_target_view(shared.raw(), &texture, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, rtv);
        self.rtv = rtv;
        let old = self.handle;
        self.texture = texture;
        self.handle = handle;
        Ok(old)
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
    fn new(shared: &SharedGpu) -> Result<Self, EngineError> {
        let mut slots = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let instances = create_upload(shared.raw(), 256 * 128)?;
            let instance_handle = shared.allocate(instances.clone());
            let instance_ptr = map_upload(&instances)?;
            let clips = create_upload(shared.raw(), 8 * 48)?;
            let clip_handle = shared.allocate(clips.clone());
            let clip_ptr = map_upload(&clips)?;
            slots.push(UploadSlot {
                instances,
                instance_handle,
                instance_capacity: 256,
                instance_ptr,
                clips,
                clip_handle,
                clip_capacity: 8,
                clip_ptr,
            });
        }
        Ok(Self { slots })
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

    /// 写入本帧的实例/裁剪字节；容量不够时扩容（旧缓冲交 retire 队列）。返回 `(实例数, 裁剪层数)`。
    fn write(
        &mut self,
        shared: &SharedGpu,
        stream: &mut CommandStream,
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
            let resource = create_upload(shared.raw(), u64::from(capacity) * 128)?;
            let handle = shared.allocate(resource.clone());
            let pointer = map_upload(&resource)?;
            let old = self.slots[slot].instance_handle;
            self.slots[slot].instances = resource;
            self.slots[slot].instance_handle = handle;
            self.slots[slot].instance_capacity = capacity;
            self.slots[slot].instance_ptr = pointer;
            stream.retire(index, old);
        }
        let needs_clips = clip_count > self.slots[slot].clip_capacity;
        if needs_clips {
            let capacity = clip_count.next_power_of_two().max(8);
            let resource = create_upload(shared.raw(), u64::from(capacity) * 48)?;
            let handle = shared.allocate(resource.clone());
            let pointer = map_upload(&resource)?;
            let old = self.slots[slot].clip_handle;
            self.slots[slot].clips = resource;
            self.slots[slot].clip_handle = handle;
            self.slots[slot].clip_capacity = capacity;
            self.slots[slot].clip_ptr = pointer;
            stream.retire(index, old);
        }

        // SAFETY: 槽缓冲的容量已按本次字节数扩容；指针是创建时的持久映射，写入区间在容量内
        // （D3D12 约定：UPLOAD 堆的 Map 只需一次，之后一直可写）。
        unsafe {
            std::ptr::copy_nonoverlapping(
                instances.as_ptr(),
                self.slots[slot].instance_ptr,
                instances.len(),
            );
            std::ptr::copy_nonoverlapping(
                bytemuck::cast_slice::<GpuClipEntry, u8>(clips).as_ptr(),
                self.slots[slot].clip_ptr,
                clip_count as usize * 48,
            );
        }
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

/// 把一块 UPLOAD 堆资源持久映射一次，返回长期可写的 CPU 指针（DEVIATIONS F-210）。
///
/// D3D12 允许对 UPLOAD 堆 `Map` 一次后长期保留指针（`Unmap` 只表示 CPU 写入结束，GPU 侧
/// 不受影响）；资源释放时映射随之失效，不需要显式 `Unmap`。
fn map_upload(resource: &ID3D12Resource) -> Result<*mut u8, EngineError> {
    let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: 资源是 `D3D12_HEAP_TYPE_UPLOAD`，整个生命周期只由本函数映射一次。
    unsafe { resource.Map(0, None, Some(&mut pointer)) }
        .map_err(|error| init_error_from("Map(upload)", error))?;
    Ok(pointer.cast::<u8>())
}

fn bind_frame(
    list: &ID3D12GraphicsCommandList,
    pipeline: &Pipeline,
    srv_heap: &SrvHeap,
    descriptor_base: u32,
    persist: &Persist,
    slot: usize,
    root: RootUniform,
    content: (u32, u32),
) {
    // SAFETY: RTV / 视口 / 矩形由本帧持有，调用期间有效；共享描述符堆由调用方持有。
    unsafe {
        list.SetDescriptorHeaps(&[Some(srv_heap.heap().clone())]);
        list.SetGraphicsRootSignature(pipeline.root_signature());
        list.SetPipelineState(pipeline.quad());
        list.SetGraphicsRoot32BitConstants(
            0,
            ROOT_UNIFORM_DWORDS as u32,
            root.words().as_ptr().cast(),
            0,
        );
        list.SetGraphicsRootDescriptorTable(1, srv_heap.slot_table(descriptor_base, slot));
        list.SetGraphicsRootDescriptorTable(2, srv_heap.atlas_table());
        list.OMSetRenderTargets(1, Some(&persist.rtv), false, None);
        // 视口 = **内容**尺寸（不是 buffer）：Ndc 坐标由 root uniform 的内容尺寸给出，
        // 两者必须是同一个矩形，否则内容会被拉伸到整块 buffer 上。
        list.RSSetViewports(&[D3D12_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: content.0 as f32,
            Height: content.1 as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }]);
    }
}

fn draw_ops(
    list: &ID3D12GraphicsCommandList,
    pipeline: &Pipeline,
    ops: &[FrameOp],
    geometry: &mut GeometryRegistry,
    shared: &SharedGpu,
    retired: &mut Vec<GpuResourceHandle>,
    payload: &FramePayload,
    root: RootUniform,
    scissor: RECT,
) -> Result<u32, EngineError> {
    let mut draw_calls = 0u32;
    // 局部刷新时跳过与 damage 不相交的 op（P1-2.1）：旧实现对每个 damage 矩形重放**整份**
    // op 列表，实测使增量帧比全量帧更慢（288 µs vs 222 µs）。
    let scissor_extent = [scissor.left, scissor.top, scissor.right, scissor.bottom];
    let mut skipped = 0u32;
    for op in ops {
        if !op.bounds().intersects(scissor_extent) {
            skipped = skipped.saturating_add(1);
            continue;
        }
        match *op {
            FrameOp::Instances { start, len, .. } => {
                if len == 0 {
                    continue;
                }
                // 实例基址只经 root constant `draw.clip.y` 进 shader，硬件基址恒 0
                // （D3D12 的 StartInstanceLocation **不**加到 SV_InstanceID 上，contract §6）。
                let uniform = root.with_instance_base(start);
                // SAFETY: PSO / 根常量 / 拓扑 / scissor 都属于同一条已打开的 list。
                unsafe {
                    list.SetPipelineState(pipeline.quad());
                    list.SetGraphicsRoot32BitConstants(
                        0,
                        ROOT_UNIFORM_DWORDS as u32,
                        uniform.words().as_ptr().cast(),
                        0,
                    );
                    list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
                    list.RSSetScissorRects(&[scissor]);
                }
                // SAFETY: 4 顶点 TriangleStrip + 实例区间 [0, len) 已在编码期校验；
                // 段起点由 shader 侧的 `draw.clip.y` 承担。
                unsafe { list.DrawInstanced(4, len, 0, 0) };
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
                // 首次见到该 id（或槽位换代）时现场建缓冲；`desc` 随帧过线。
                let entry = geometry.ensure(shared, draw.id, &draw.desc, retired)?;
                let uniform = geometry_root(root, draw.clone(), clip_index, clip_rect);
                // SAFETY: 顶点/索引缓冲由 `GeometryRegistry` 持有到本帧结束；
                // `entry.topology()` 与注册时的图元类型一致。
                unsafe {
                    list.SetPipelineState(pipeline.geometry(entry.pipeline()));
                    list.SetGraphicsRoot32BitConstants(
                        0,
                        ROOT_UNIFORM_DWORDS as u32,
                        uniform.words().as_ptr().cast(),
                        0,
                    );
                    list.IASetPrimitiveTopology(entry.topology());
                    list.IASetVertexBuffers(0, Some(&[entry.vertex()]));
                    list.IASetIndexBuffer(Some(&entry.index()));
                    list.RSSetScissorRects(&[scissor]);
                    list.DrawIndexedInstanced(entry.index_count(), 1, 0, 0, 0);
                }
                draw_calls = draw_calls.saturating_add(1);
            }
        }
    }
    Ok(draw_calls)
}

/// copy 到 back buffer 的范围：`fresh`（交换链刚建好）时强制整块 buffer。
///
/// 只有 D3D12 后端需要这个规则（F-121：Metal / Vulkan 的 present 目标跨帧内容无保证，两平台
/// 每帧整面写入）。`present_dirty() == None` 恰好同时覆盖 `fresh` 与 `Plan::Full`；`Idle` 必须
/// 单独先判，否则会被当成全量。
fn copy_plan(frame: FramePlan) -> Plan {
    if frame.is_idle() {
        return Plan::Idle;
    }
    match frame.present_dirty() {
        Some(damage) => Plan::Partial(damage),
        None => Plan::Full,
    }
}

/// 全量帧要 copy 的物理范围：`fresh`（交换链刚建好）⇒ 整块 buffer（back buffer 的内容未定义），
/// 其余全量帧 ⇒ 只有**内容**子矩形（buffer 的其余部分永远不可见）。
fn copy_extent(frame: FramePlan, buffer: (u32, u32), content: (u32, u32)) -> (u32, u32) {
    if frame.is_fresh() { buffer } else { content }
}

/// copy 持久目标 → back buffer。
///
/// `buffer` / `content` 都是物理像素：只有 `fresh`（组合交换链刚建好、第一次 present 必须整面）
/// 才拷整块 buffer；其余全量帧只拷内容子矩形，局部帧逐 damage 矩形（永远落在内容之内）。
fn copy_to_back(
    list: &ID3D12GraphicsCommandList,
    persist: &Persist,
    swapchain: &Swapchain,
    frame: FramePlan,
    buffer: (u32, u32),
    content: (u32, u32),
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
    match copy_plan(frame) {
        // 全帧也用 `CopyTextureRegion`（与局部同一路径，逐矩形）：
        // swapchain back buffer 上 `CopyResource` 在 flip model 下会被拒绝（本机实测 DEVICE_REMOVED）。
        Plan::Full => {
            let whole = copy_extent(frame, buffer, content);
            let whole = crate::math::PhysicalRect::new(0, 0, whole.0 as i32, whole.1 as i32);
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

/// 缩容 prime 的内容保留：把 `source` 的 `(0, 0, content)` 拷到 `destination` 的左上角。
///
/// 两块持久目标在这一步之前都处于 `RENDER_TARGET`（旧目标刚被 wait_idle，新目标刚 clear），
/// 这里临时过渡到拷贝状态并在返回前转回：后续 `bind_frame` / `copy_to_back` 仍按「持久目标
/// 处于 RENDER_TARGET」记账。`destination` 的其余像素保持 clear 出来的背景色，永远不可见
/// （visual 的 clip 只到内容）。
fn copy_content(
    list: &ID3D12GraphicsCommandList,
    source: &Persist,
    destination: &Persist,
    content: (u32, u32),
) {
    transition(
        list,
        &source.texture,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    transition(
        list,
        &destination.texture,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    let rect = crate::math::PhysicalRect::new(0, 0, content.0 as i32, content.1 as i32);
    copy_rect(list, &source.texture, &destination.texture, &rect);
    transition(
        list,
        &destination.texture,
        D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
    );
    transition(
        list,
        &source.texture,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
    );
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
    // 被 barrier 过的资源（尤其是 swapchain back buffer）会被永久钉住（back buffer 全部泄漏，
    // 退出时 swapchain 也带着引用被销毁，DEVIATIONS F-86）。记录已经提交，这里显式释放。
    unsafe {
        let transition: &mut D3D12_RESOURCE_TRANSITION_BARRIER = &mut barrier.Anonymous.Transition;
        core::mem::ManuallyDrop::drop(&mut transition.pResource);
    }
}

/// `RootUniform`（112 B）：`frame @0`、`draw @32`（P07 §2.7）。
///
/// 布局由 `render::uniform::RootUniform` 唯一实现；本函数只补 Windows 的 DPI（`scale` 来自
/// 窗口，经 `GpuRenderer::scale` 传入）。
fn root_uniform(width: f32, height: f32, scale: f32) -> RootUniform {
    RootUniform::frame(width, height, scale)
}

/// 逐 geometry 的 draw uniform：在帧级常量之上覆盖 draw 块。
///
/// 布局由 `render::uniform::RootUniform` 唯一实现（contract §6）；本函数只负责把编码期
/// 的 clip 前缀换算成契约要求的 `(x, y, w, h)`。颜色与变换已经在 UI 线程的编码器里解析成
/// GPU 可直接消费的形式（`GeometryDrawGpu`），GPU 线程不做色彩空间运算。
fn geometry_root(
    frame: RootUniform,
    draw: GeometryDrawGpu,
    clip_index: u32,
    clip_rect: [u32; 4],
) -> RootUniform {
    // draw.clip_rect @20 = (x, y, w, h) 物理像素。无裁剪时用视口，避免 inf 进根常量；
    // 有裁剪时把编码器给的 AABB `(x0, y0, x1, y1)` 换成 `(x, y, w, h)`。
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

fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn create_target(device: &ID3D12Device, size: (u32, u32)) -> Result<ID3D12Resource, EngineError> {
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
        device.CreateCommittedResource(
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

fn create_upload(device: &ID3D12Device, bytes: u64) -> Result<ID3D12Resource, EngineError> {
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
        device.CreateCommittedResource(
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

/// RTV 堆里第 `index` 个描述符的 CPU 句柄（0 = 内容，1 = 1×1 背景）。
fn rtv_handle(
    device: &ID3D12Device,
    heap: &ID3D12DescriptorHeap,
    index: u32,
) -> D3D12_CPU_DESCRIPTOR_HANDLE {
    // SAFETY: 设备 / 堆句柄由调用方持有，调用期间有效；两个查询都无副作用。
    let size = unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) };
    // SAFETY: 同上。
    let start = unsafe { heap.GetCPUDescriptorHandleForHeapStart() };
    D3D12_CPU_DESCRIPTOR_HANDLE {
        ptr: start.ptr + index as usize * size as usize,
    }
}

/// 建一个 RTV（**视图格式**决定是否做 sRGB 编码，DEVIATIONS F-80）。
fn create_render_target_view(
    device: &ID3D12Device,
    resource: &ID3D12Resource,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let desc = D3D12_RENDER_TARGET_VIEW_DESC {
        Format: format,
        ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
        Anonymous: D3D12_RENDER_TARGET_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_RTV {
                MipSlice: 0,
                PlaneSlice: 0,
            },
        },
    };
    // SAFETY: 设备句柄由进程级 `SharedGpu` 持有；资源与视图描述在调用期间有效。
    unsafe { device.CreateRenderTargetView(resource, Some(&desc), handle) };
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::Direct3D12::D3D12_DESCRIPTOR_HEAP_TYPE_RTV;

    use super::{
        GpuRenderer, Persist, copy_content, copy_extent, copy_plan, geometry_root, grown_buffer,
        root_uniform, rtv_handle, shrink_target,
    };
    use crate::{
        error::EngineError,
        math::{PhysicalRect, Point, Rect, Scale, Size, Transform},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            color::{Color, ColorResolver, Interpolation},
            geometry::{GeometryId, GeometryPipeline, IndexType, Topology},
            instance::{FrameOp, GeometryDrawGpu, InstanceEncoder},
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
    fn warp_renderer(size: (u32, u32)) -> Option<GpuRenderer> {
        match GpuRenderer::new_warp_offscreen(size) {
            Ok(renderer) => Some(renderer),
            Err(error) => {
                eprintln!("skip: WARP renderer unavailable: {error}");
                None
            }
        }
    }

    /// 关窗不泄漏：渲染器销毁后，它注册进共享设备句柄表的资源必须全部注销。
    ///
    /// 进程级设备不会随窗口销毁，所以 persist / 上传缓冲 / staging / 几何缓冲的句柄必须在
    /// `GpuRenderer::drop` 里主动交回（否则每关一个窗口就泄漏一份显存）。
    #[test]
    fn dropping_a_renderer_releases_its_shared_device_resources() {
        let _guard = lock();
        let shared =
            crate::render::windows::device::SharedGpu::new_warp().expect("WARP shared gpu");
        assert_eq!(shared.live_resources(), 0, "共享 GPU 创建时不注册资源");
        {
            let mut renderer = GpuRenderer::with_shared(std::sync::Arc::clone(&shared), (16, 16))
                .expect("renderer");
            let mut rows = crate::render::DirtyRows::default();
            rows.mark(0, u32::MAX);
            renderer.queue_atlas_upload(
                AtlasKind::Glyph,
                super::TEST_ATLAS_PAGE,
                vec![0u8; (super::TEST_ATLAS_PAGE * super::TEST_ATLAS_PAGE) as usize],
                rows,
            );
            let scene = Scene::empty();
            let mut encoder = InstanceEncoder::new();
            let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
            let mut frame = full_plan((16, 16));
            let mut timings = FrameTimings::new();
            renderer
                .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
                .expect("submit");
            assert!(
                shared.live_resources() > 0,
                "渲染器必须持有 persist / upload / staging 资源"
            );
        }
        assert_eq!(
            shared.live_resources(),
            0,
            "关窗后渲染器注册的所有资源句柄必须注销"
        );
    }

    /// 多窗口共享契约：两个渲染器共享同一个进程级 `SharedGpu`，但在共享 SRV 堆里各有独立段。
    #[test]
    fn offscreen_renderers_share_the_process_gpu() {
        let _guard = lock();
        let Some(first) = warp_renderer((8, 8)) else {
            return;
        };
        let Some(second) = warp_renderer((8, 8)) else {
            return;
        };
        assert!(
            std::sync::Arc::ptr_eq(&first.shared, &second.shared),
            "两个窗口必须共享同一个 SharedGpu（device / queue / PSO / 图集纹理）"
        );
        assert_ne!(
            first.descriptors.base(),
            second.descriptors.base(),
            "每个窗口在共享 SRV 堆里必须有各自的段"
        );
    }

    /// 两个窗口并发录制 + 提交到同一条进程级队列：各自的 persist / readback 互不干扰。
    ///
    /// 两张图集纹理是共享的，所以两个线程上传**同一页的相同内容**：共享纹理的最终内容因此与
    /// 两个线程的提交顺序无关（跨窗口图集并发上传的正确性论证见 `windows/atlas.rs` 模块文档）。
    #[test]
    fn two_renderers_render_concurrently_on_one_shared_gpu() {
        let _guard = lock();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let page = super::TEST_ATLAS_PAGE;
        let mut threads = Vec::with_capacity(2);
        for _ in 0..2 {
            let barrier = std::sync::Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                let mut renderer =
                    GpuRenderer::new_warp_offscreen((8, 8)).expect("shared WARP renderer");
                let mut pixels = vec![0u8; (page * page) as usize];
                for row in pixels.chunks_mut(page as usize) {
                    row.fill(0x40);
                }
                let mut rows = crate::render::DirtyRows::default();
                rows.mark(0, u32::MAX);
                renderer.queue_atlas_upload(AtlasKind::Glyph, page, pixels, rows);

                let text_free = one_lane_scene(quad(
                    Rect::new(2.0, 2.0, 2.0, 2.0),
                    Color::srgb(1.0, 0.0, 0.0, 1.0),
                ));
                let mut encoder = InstanceEncoder::new();
                let buffer = encoder.encode(&text_free, Scale::ONE, &resolver());
                // 尽量让两个线程的录制 / 提交重叠（真正的并发点）。
                barrier.wait();
                let mut timings = FrameTimings::new();
                for frame in 1..=6u32 {
                    let mut plan = full_plan((8, 8));
                    renderer
                        .submit_buffer(FrameIndex::from_raw(frame), buffer, &mut plan, &mut timings)
                        .expect("concurrent submit");
                }
                let image = renderer.readback().expect("readback");
                assert_eq!(pixel(&image, 8, 3, 3), [255, 0, 0, 255]);
                assert_eq!(pixel(&image, 8, 0, 0), [0, 0, 0, 0]);
            }));
        }
        for thread in threads {
            thread.join().expect("concurrent gpu thread must not panic");
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
        let uniform = root_uniform(800.0, 600.0, 1.25);
        let words = uniform.words();
        assert_eq!(words[0], 800.0);
        assert_eq!(words[1], 600.0);
        assert_eq!(words[2], 2.0 / 800.0);
        assert_eq!(words[3], 2.0 / 600.0);
        assert_eq!(words[4], 1.25, "frame.params.x 是 DPI scale");
        assert_eq!(words[8], 1.0);
        assert_eq!(words[11], 1.0);
        assert_eq!(words.len(), 28);
    }

    /// P0-1.1 回归：实例段起点只经 `draw.clip.y` 进入 shader，`DrawInstanced` 的
    /// `StartInstanceLocation` 恒 0（D3D12 不会把它加到 `SV_InstanceID` 上）。
    ///
    /// 这里扫描本文件源码做防回归：断言字面量用拼接构造，避免断言本身成为「禁止串」的来源。
    #[test]
    fn instance_segments_use_the_uniform_base_not_the_hardware_base() {
        let uniform = root_uniform(64.0, 64.0, 1.0).with_instance_base(7);
        assert_eq!(uniform.words()[25], 7.0, "draw.clip.y = instance_base");

        let source = include_str!("compositor.rs");
        let call = ["Draw", "Instanced(4, len, 0, "].concat();
        assert!(
            source.contains(&format!("{call}0)")),
            "硬件实例基址必须恒 0（D3D12 的 StartInstanceLocation 不加到 SV_InstanceID）"
        );
        assert!(
            !source.contains(&format!("{call}start)")),
            "回退到硬件基址会让第二段之后的实例全部错位"
        );
    }

    /// D3D12 的 copy 范围：`fresh` 时全量，`presented()` 之后恢复局部（等价覆盖原共享层单测）。
    #[test]
    fn a_fresh_swapchain_forces_a_full_copy_until_presented() {
        let rect = PhysicalRect::new(0, 0, 10, 10);
        let partial = partial_plan(&[rect], (100, 50)).plan();
        assert!(matches!(partial, Plan::Partial(_)));
        let mut frame = FramePlan::new(partial, true);
        assert_eq!(copy_plan(frame), Plan::Full, "fresh ⇒ 全量 copy");
        frame.presented();
        assert_eq!(copy_plan(frame), partial, "present 成功后恢复局部 copy");
        assert_eq!(copy_plan(FramePlan::new(Plan::Full, false)), Plan::Full);
        assert_eq!(copy_plan(FramePlan::new(Plan::Idle, false)), Plan::Idle);
        assert_eq!(
            copy_plan(FramePlan::new(Plan::Idle, true)),
            Plan::Full,
            "fresh 的空帧仍然要全量 copy"
        );
    }

    /// 测试用的最小几何描述（`draw_ops` 只读 `id`/`transform`/`color`，描述由 registry 提供）。
    fn test_desc() -> crate::paint::geometry::GeometryDesc {
        crate::paint::geometry::GeometryDesc {
            vertex_bytes: bytes::Bytes::new(),
            index_bytes: bytes::Bytes::new(),
            vertex_stride: 8,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: Rect::new(0.0, 0.0, 1.0, 1.0),
            pipeline: GeometryPipeline::Ui,
        }
    }

    /// 逐 dword 对照 `common.hlsl` 的 `GeometryDrawUniform`（B1）。
    #[test]
    fn geometry_root_matches_common_hlsl_layout() {
        let frame = root_uniform(800.0, 600.0, 1.0);
        let clip_rect = [4.0f32, 5.0, 100.0, 120.0].map(f32::to_bits);
        let draw = GeometryDrawGpu {
            id: GeometryId::for_test(0, 1),
            transform: Transform::translate(11.0, 13.0),
            color: resolver().resolve(Color::WHITE).to_array(),
            desc: std::sync::Arc::new(test_desc()),
        };
        let uniform = geometry_root(frame, draw, 3, clip_rect);
        let words = uniform.words();
        assert_eq!(
            &words[..8],
            &frame.words()[..8],
            "frame block is copied verbatim"
        );
        // draw.basis @8：translate 的线性部分是单位矩阵。
        assert_eq!(&words[8..12], &[1.0, 0.0, 0.0, 1.0]);
        // draw.translation @12
        assert_eq!(&words[12..14], &[11.0, 13.0]);
        // draw.color0 @16 = premultiplied white
        assert_eq!(&words[16..20], &[1.0, 1.0, 1.0, 1.0]);
        // draw.clip_rect @20 = (x, y, w, h)
        assert_eq!(&words[20..24], &[4.0, 5.0, 96.0, 115.0]);
        // draw.clip @24 = clip_index（数值编码，不是位重解释）
        assert_eq!(words[24], 3.0);
        // draw.clip.y @25 = instance_base（几何绘制恒 0）
        assert_eq!(words[25], 0.0);
        assert_eq!(words[26..].iter().copied().sum::<f32>(), 0.0);
    }

    /// P0-1.2 回归：几何绘制必须消费 `Path.fill` / `GeometryDraw.color` 与 `transform`。
    #[test]
    fn geometry_root_consumes_transform_and_color() {
        let frame = root_uniform(64.0, 32.0, 2.0);
        let draw = GeometryDrawGpu {
            id: GeometryId::for_test(0, 1),
            transform: Transform::scale(3.0, 4.0),
            color: resolver()
                .resolve(Color::srgb(1.0, 0.0, 0.0, 1.0))
                .to_array(),
            desc: std::sync::Arc::new(test_desc()),
        };
        let uniform = geometry_root(frame, draw, 0, [0; 4]);
        let words = uniform.words();
        assert_eq!(&words[8..12], &[3.0, 0.0, 0.0, 4.0], "transform 进 basis");
        assert!(
            words[16] > 0.0 && words[17] == 0.0 && words[18] == 0.0,
            "红色填充必须进 color0（linear premultiplied）"
        );
        assert_eq!(words[19], 1.0, "不透明 alpha");
        assert_eq!(words[4], 2.0, "DPI scale 进 frame.params.x");
    }

    #[test]
    fn geometry_root_without_clip_uses_the_viewport() {
        let frame = root_uniform(64.0, 32.0, 1.0);
        let draw = GeometryDrawGpu {
            id: GeometryId::for_test(0, 1),
            transform: Transform::IDENTITY,
            color: resolver().resolve(Color::WHITE).to_array(),
            desc: std::sync::Arc::new(test_desc()),
        };
        let uniform = geometry_root(frame, draw, 0, [0; 4]);
        let words = uniform.words();
        assert_eq!(&words[20..24], &[0.0, 0.0, 64.0, 32.0]);
        assert_eq!(words[24], 0.0);
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
            FrameOp::Instances {
                start: 0,
                len: 1,
                ..
            }
        ));
    }

    #[test]
    fn clear_half_encodes_once_to_srgb() {
        let _guard = lock();
        let Some(mut renderer) = warp_renderer((8, 8)) else {
            return;
        };
        renderer.set_background_color([0.5, 0.5, 0.5, 0.5]);
        let scene = Scene::empty();
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut frame = full_plan((8, 8));
        let mut timings = FrameTimings::new();
        renderer
            .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
            .expect("submit");
        let image = renderer.readback().expect("readback");
        // probe：persist 资源 UNORM + RTV 视图 SRGB ⇒ clear(0.5) 的 RGB 只编码一次 = 188；
        // alpha 不参与 sRGB 变换（0.5 → 128）。
        assert_eq!(pixel(&image, 8, 4, 4), [188, 188, 188, 128]);
    }

    /// 背景色改动只标脏；离屏目标没有背景层，`refresh_background` 必须消费标记（不能永真）。
    #[test]
    fn a_background_change_marks_the_layer_dirty_until_the_refresh() {
        let _guard = lock();
        let Some(mut renderer) = warp_renderer((16, 16)) else {
            return;
        };
        assert!(!renderer.background_dirty);
        renderer.set_background_color([0.1, 0.2, 0.3, 1.0]);
        assert!(renderer.background_dirty, "改色必须请求背景层重建");
        renderer
            .refresh_background()
            .expect("an offscreen renderer has no background layer to rebuild");
        assert!(
            !renderer.background_dirty,
            "没有背景层的目标也必须消费标记，否则每帧都会尝试重建"
        );
        // 同值 no-op：没有新改动时 refresh 不产生任何工作。
        renderer
            .refresh_background()
            .expect("a clean background layer is a no-op");
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
        let stats = renderer
            .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
            .expect("submit");
        assert_eq!(stats.draw_calls, 1, "one non-empty instance op = one draw");
        assert_eq!(stats.instances, 1);
        let image = renderer.readback().expect("readback");
        assert_eq!(pixel(&image, 64, 24, 24), [255, 0, 0, 255]);
        assert_eq!(pixel(&image, 64, 4, 4), [0, 0, 0, 0]);
    }

    /// 白底 + 红 damage：两帧都是非空场景，damage 之外的蓝块必须原样保留（B5）。
    #[test]
    fn draw_calls_follow_full_and_partial_scissor_passes() {
        let _guard = lock();
        let Some(mut renderer) = warp_renderer((64, 64)) else {
            return;
        };
        let scene = one_lane_scene(quad(
            Rect::new(0.0, 0.0, 64.0, 64.0),
            Color::srgb(1.0, 0.0, 0.0, 1.0),
        ));
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut timings = FrameTimings::new();

        let mut first = full_plan((64, 64));
        let stats = renderer
            .submit_buffer(FrameIndex::from_raw(1), buffer, &mut first, &mut timings)
            .expect("full frame");
        assert_eq!(stats.draw_calls, 1, "full frame has one scissor pass");

        let damage = [
            PhysicalRect::new(0, 0, 32, 64),
            PhysicalRect::new(32, 0, 64, 64),
        ];
        let mut second = partial_plan(&damage, (64, 64));
        let stats = renderer
            .submit_buffer(FrameIndex::from_raw(2), buffer, &mut second, &mut timings)
            .expect("partial frame");
        assert_eq!(
            stats.draw_calls, 2,
            "op 覆盖整个窗口 ⇒ 两个 damage 都与它相交，各画一次（裁剪不适用）"
        );
    }

    /// P1-2.1 回归：与 damage 不相交的 op 必须被跳过。
    ///
    /// 旧实现对每个 damage 矩形重放**整份** op 列表 —— 这正是实测中
    /// `frame_incremental_512`（288 µs）比 `frame_full_512`（222 µs）更慢的原因。
    ///
    /// op 的边界是 **run**（连续实例段），不是单个图元：同 clip 链的相邻图元会合并进同一 op。
    /// 这里用一个真实的几何绘制打断 run，让左右两个 quad 落在两个独立的 `Instances` op 里，
    /// 才能观察到裁剪（否则两个 quad 共用一个 op，其并集包围盒覆盖整个窗口，无从裁起）。
    #[test]
    fn ops_outside_the_damage_are_skipped() {
        let _guard = lock();
        let Some(mut renderer) = warp_renderer((64, 64)) else {
            return;
        };
        let id = GeometryId::for_test(0, 0);
        #[rustfmt::skip]
        let vertices: [f32; 6] = [0.0, 0.0, 8.0, 0.0, 0.0, 8.0];
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

        let mut scene = Scene::empty();
        let lane = scene.new_lane();
        assert!(scene.push(
            lane,
            quad(
                Rect::new(0.0, 0.0, 32.0, 64.0),
                Color::srgb(1.0, 0.0, 0.0, 1.0),
            )
        ));
        // 几何 op 会 flush 当前实例段 ⇒ 后面那个 quad 落进独立的 op。
        assert!(scene.push(
            lane,
            Primitive::Geometry(GeometryDraw {
                id,
                transform: Transform::IDENTITY,
                color: Color::WHITE,
            })
        ));
        assert!(scene.push(
            lane,
            quad(
                // 与左 quad 留出 8 px 间隙：op 包围盒会外扩 1 px 吸收 AA，
                // 贴边的两个矩形仍会被判为相交（保守正确）。
                Rect::new(40.0, 0.0, 24.0, 64.0),
                Color::srgb(0.0, 0.0, 1.0, 1.0),
            )
        ));
        scene.set_order(&[lane]);
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut timings = FrameTimings::new();

        let mut first = full_plan((64, 64));
        let stats = renderer
            .submit_buffer(FrameIndex::from_raw(1), buffer, &mut first, &mut timings)
            .expect("full frame");
        assert_eq!(stats.draw_calls, 3, "两个实例 op + 一个几何 op");

        // 只 damage 左半边 ⇒ 右边那个实例 op 与 damage 不相交，必须跳过。
        let mut second = partial_plan(&[PhysicalRect::new(0, 0, 32, 64)], (64, 64));
        let stats = renderer
            .submit_buffer(FrameIndex::from_raw(2), buffer, &mut second, &mut timings)
            .expect("partial frame");
        assert_eq!(
            stats.draw_calls, 2,
            "只有与 damage 相交的 op 才产生 draw call（旧的『整份重放』实现会是 3）"
        );
    }

    #[test]
    fn damage_isolates_pixels_across_two_real_frames() {
        let _guard = lock();
        let Some(mut renderer) = warp_renderer((64, 64)) else {
            return;
        };
        renderer.set_background_color([1.0, 1.0, 1.0, 1.0]);
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
            .submit_buffer(FrameIndex::from_raw(1), buffer, &mut first, &mut timings)
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
            .submit_buffer(FrameIndex::from_raw(2), buffer, &mut second, &mut timings)
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
            color: Color::WHITE,
        }));
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut frame = full_plan((64, 64));
        let mut timings = FrameTimings::new();
        renderer
            .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
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
        renderer.queue_atlas_upload(AtlasKind::Image, page, pixels, rows);

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
            .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
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
        let Some(mut renderer) = super::GpuRenderer::new_warp_offscreen((64, 64)).ok() else {
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
        renderer.queue_atlas_upload(AtlasKind::Glyph, page, pixels, rows);
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
            .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
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
            .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
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
            .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
            .expect("first submit");

        renderer.resize((96, 64)).expect("resize");
        assert_eq!(renderer.surface(), (96, 64));
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut frame = full_plan((96, 64));
        renderer
            .submit_buffer(FrameIndex::from_raw(2), buffer, &mut frame, &mut timings)
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

    /// 全量帧的 copy 范围：`fresh`（交换链刚建好）整块 buffer，之后只拷内容子矩形。
    #[test]
    fn a_full_frame_copies_the_content_unless_the_swapchain_is_fresh() {
        let buffer = (1000, 700);
        let content = (800, 600);
        assert_eq!(
            copy_extent(FramePlan::new(Plan::Full, true), buffer, content),
            buffer,
            "第一次 present 之前 back buffer 的内容未定义"
        );
        assert_eq!(
            copy_extent(FramePlan::new(Plan::Full, false), buffer, content),
            content,
            "之后的全量帧只写可见的内容子矩形"
        );
        let fresh_partial = FramePlan::new(
            partial_plan(&[PhysicalRect::new(0, 0, 8, 8)], content).plan(),
            true,
        );
        assert_eq!(copy_extent(fresh_partial, buffer, content), buffer);
    }

    /// 内容子矩形：绘制与读回都只按内容尺寸，高水位 buffer 不参与（`design/DEVIATIONS.md` §36 F-199）。
    #[test]
    fn the_content_size_is_independent_of_the_buffer() {
        let _guard = lock();
        let Some(mut renderer) = warp_renderer((64, 64)) else {
            return;
        };
        assert_eq!(renderer.buffer(), (64, 64));
        renderer
            .set_content_size((32, 24))
            .expect("shrink the content");
        assert_eq!(renderer.surface(), (32, 24));
        assert_eq!(renderer.buffer(), (64, 64), "buffer 不随内容变化");

        // 铺满整个 buffer 的红块：内容子矩形之外的像素必须不参与本帧。
        let scene = one_lane_scene(quad(
            Rect::new(0.0, 0.0, 64.0, 64.0),
            Color::srgb(1.0, 0.0, 0.0, 1.0),
        ));
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&scene, Scale::ONE, &resolver());
        let mut frame = full_plan((32, 24));
        let mut timings = FrameTimings::new();
        renderer
            .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
            .expect("content frame");
        let image = renderer.readback().expect("readback");
        assert_eq!((image.width, image.height), (32, 24), "读回范围 = 内容尺寸");
        assert_eq!(pixel(&image, 32, 31, 23), [255, 0, 0, 255]);
    }

    /// 内容越过 grow-only 桶、但渲染器没有合成目标（离屏）：显式 `Composition` 错误，不静默
    /// 裁剪。真实窗口路径的 grow 需要 DComp + HWND，由 release demo 的运行时冒烟覆盖。
    #[test]
    fn growing_beyond_the_buffer_without_a_composition_target_is_an_error() {
        let _guard = lock();
        let Some(mut renderer) = warp_renderer((32, 32)) else {
            return;
        };
        match renderer.set_content_size((33, 32)) {
            Err(EngineError::Composition { detail }) => {
                assert!(detail.contains("composition target"), "{detail}");
            }
            other => panic!("unexpected result: {other:?}"),
        }
        assert!(
            renderer.set_content_size((32, 32)).is_ok(),
            "同值必须是 no-op"
        );
    }

    /// 内容超过 16384 硬上限：`buffer_size` 的 `Swapchain` 错误必须先于任何 GPU 工作。
    #[test]
    fn content_beyond_the_maximum_buffer_is_an_error() {
        let _guard = lock();
        let Some(mut renderer) = warp_renderer((32, 32)) else {
            return;
        };
        match renderer.set_content_size((16_385, 32)) {
            Err(EngineError::Swapchain { detail }) => {
                assert!(detail.contains("16385"), "{detail}");
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    /// P1-A：corner drag（一轴变大、另一轴变小）时，另一轴必须保留旧 buffer 的高水位。
    ///
    /// 视觉切换那一帧的旧 clip 仍可能指向旧内容，旧内容 ≤ 旧 buffer；只有新 buffer 逐轴 ≥
    /// 旧 buffer，中间帧才不可能采样到未定义像素。
    #[test]
    fn growth_keeps_the_other_axis_at_the_high_water_mark() {
        // buffer 2560×1792，新内容 2800×600：宽度增长，高度不得按内容缩回 768。
        let grown = grown_buffer((2560, 1792), (2800, 600)).expect("grows");
        assert_eq!(grown, (3072, 1792));
        assert!(
            grown.1 >= 1792,
            "高度必须保留旧 buffer（旧 clip 仍可能指向旧内容）: {grown:?}"
        );
        // 另一方向同理。
        assert_eq!(
            grown_buffer((2560, 1792), (1200, 2400)).expect("grows"),
            (2560, 2560)
        );
        // 当前 buffer 比按内容算出的桶大：新桶不得缩小。
        let covered = grown_buffer((3328, 2048), (2000, 600)).expect("fits");
        assert!(covered.0 >= 3328 && covered.1 >= 2048, "{covered:?}");
        // 就算内容完全落在当前 buffer 内，纯函数也不会把桶算小（防御性）。
        assert_eq!(
            grown_buffer((2560, 1792), (2000, 600)).expect("fits"),
            (2560, 1792)
        );
        // 上限检查在 max 之后仍然生效。
        assert!(matches!(
            grown_buffer((1536, 1280), (16_385, 100)),
            Err(EngineError::Swapchain { .. })
        ));
    }

    /// RTV 堆固定两个描述符：0 = 内容，1 = 1×1 背景（按 `GetDescriptorHandleIncrementSize`
    /// 偏移）。背景层 prime 只写第二个，主持久目标一个像素都不碰。
    #[test]
    fn the_rtv_heap_has_a_second_descriptor_for_the_background_layer() {
        let _guard = lock();
        let Some(renderer) = warp_renderer((8, 8)) else {
            return;
        };
        let main = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 0);
        let background = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 1);
        // SAFETY: 设备 / 堆句柄由渲染器持有，调用期间有效；查询无副作用。
        let size = unsafe {
            renderer
                .shared
                .raw()
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV)
        };
        assert_eq!(
            background.ptr,
            main.ptr + size as usize,
            "背景 RTV 必须是堆里的第二个描述符"
        );
        assert!(background.ptr > main.ptr);
    }

    /// 缩容决策：只在内容桶**严格小于**当前高水位、且渲染器有合成目标时返回新桶。
    #[test]
    fn the_shrink_decision_only_returns_a_smaller_bucket() {
        // 1200×925 内容 ⇒ 1280×1024；历史高水位 2560×1792（1900×1300 时开出来的桶）。
        assert_eq!(
            shrink_target((2560, 1792), (1200, 925), true).expect("fits"),
            Some((1280, 1024)),
            "静止缩小时回到当前内容的桶"
        );
        // 单轴缩小也算：宽度 2000+64 的桶是 2304，高度从 1792 缩到 768。
        assert_eq!(
            shrink_target((2560, 1792), (2000, 600), true).expect("fits"),
            Some((2304, 768))
        );
        // 内容桶 == 当前 buffer：已经是最小桶，不缩（否则会每 2 s 重建一次）。
        assert_eq!(
            shrink_target((2304, 1280), (2000, 1200), true).expect("fits"),
            None
        );
        // 没有合成目标（离屏 / 快照）：永不缩容（离屏由 `resize` 直接重建）。
        assert_eq!(
            shrink_target((2560, 1792), (1200, 925), false).expect("fits"),
            None
        );
        // 防御：内容比 buffer 还大（grow-only 不变量被破坏）时不得「缩」成更大的桶。
        assert_eq!(
            shrink_target((512, 512), (2000, 900), true).expect("fits"),
            None
        );
        // 上限检查仍然生效（内容 > 16384 是显式错误，不静默裁剪）。
        assert!(matches!(
            shrink_target((16384, 16384), (16_385, 100), true),
            Err(EngineError::Swapchain { .. })
        ));
    }

    /// 离屏目标没有合成树：`maybe_shrink` 是 no-op，高水位保持不变。
    #[test]
    fn an_offscreen_renderer_never_shrinks_the_high_water_bucket() {
        let _guard = lock();
        // 1024² 的 buffer 与 32² 的内容桶（768²）才有「可缩」的候选。
        let Some(mut renderer) = warp_renderer((1024, 1024)) else {
            return;
        };
        renderer
            .set_content_size((32, 32))
            .expect("content stays inside the buffer");
        assert_eq!(renderer.buffer(), (1024, 1024));
        assert!(
            renderer.shrink_in().is_none(),
            "离屏目标没有缩容候选，空闲等待必须保持无限等"
        );
        renderer
            .maybe_shrink()
            .expect("an offscreen renderer has no composition target to switch");
        assert_eq!(
            renderer.buffer(),
            (1024, 1024),
            "没有合成目标时不得重建持久目标"
        );
    }

    /// 背景层刷新只碰它自己的 1×1 持久目标：主持久目标里的内容一个像素都不能变。
    ///
    /// 旧实现借主 persist prime 1×1 背景层，每次改色都会把整块内容目标清成背景色；离屏目标
    /// 没有背景持久目标（`background_persist = None`），这条断言同时守住那个不变量与「标记必须
    /// 被消费、不能永真」的既有语义。
    #[test]
    fn a_background_refresh_never_touches_the_main_content_persist() {
        let _guard = lock();
        let Some(mut renderer) = warp_renderer((64, 64)) else {
            return;
        };
        let red = one_lane_scene(quad(
            Rect::new(0.0, 0.0, 64.0, 64.0),
            Color::srgb(1.0, 0.0, 0.0, 1.0),
        ));
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&red, Scale::ONE, &resolver());
        let mut frame = full_plan((64, 64));
        let mut timings = FrameTimings::new();
        renderer
            .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
            .expect("content frame");

        renderer.set_background_color([0.1, 0.2, 0.3, 1.0]);
        assert!(renderer.background_dirty);
        renderer
            .refresh_background()
            .expect("an offscreen renderer has no background layer to rebuild");
        assert!(!renderer.background_dirty, "标记必须被消费");
        assert!(
            renderer.background_persist.is_none(),
            "离屏目标不创建 1×1 背景持久目标"
        );

        let image = renderer.readback().expect("readback");
        assert_eq!(
            pixel(&image, 64, 32, 32),
            [255, 0, 0, 255],
            "改背景色不得清掉主持久目标"
        );
    }

    /// 缩容 prime 的内容保留：新目标先 clear，再把旧目标的 `(0, 0, content)` 拷到左上角；
    /// 内容之外保持背景色。WARP 离屏没有合成目标，所以这里直接测录制片段（真实 DComp 窗口
    /// 的整条 `maybe_shrink` 由 release demo 的运行时冒烟覆盖）。
    #[test]
    fn a_shrink_prime_copies_the_old_content_into_the_new_persist() {
        let _guard = lock();
        let Some(mut renderer) = warp_renderer((64, 64)) else {
            return;
        };
        // 旧内容：整块红。
        let red = one_lane_scene(quad(
            Rect::new(0.0, 0.0, 64.0, 64.0),
            Color::srgb(1.0, 0.0, 0.0, 1.0),
        ));
        let mut encoder = InstanceEncoder::new();
        let buffer = encoder.encode(&red, Scale::ONE, &resolver());
        let mut frame = full_plan((64, 64));
        let mut timings = FrameTimings::new();
        renderer
            .submit_buffer(FrameIndex::from_raw(1), buffer, &mut frame, &mut timings)
            .expect("old content");
        // 新目标 128×128：离屏路径不建背景层，第二个 RTV 描述符在这里空闲可用。
        let rtv = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 1);
        let destination = Persist::new(&renderer.shared, (128, 128), rtv).expect("new persist");
        let source = std::mem::replace(&mut renderer.persist, destination);
        {
            let destination = &renderer.persist;
            let source = &source;
            renderer
                .stream
                .submit_immediate(|list| {
                    // 缩容 prime 的固定顺序：先整块 clear，再从旧目标拷内容。
                    // SAFETY: 两个目标都处于 RENDER_TARGET；清整块不需要矩形。
                    unsafe {
                        list.ClearRenderTargetView(destination.rtv, &[1.0, 1.0, 1.0, 1.0], None)
                    };
                    copy_content(list, source, destination, (64, 64));
                    Ok(())
                })
                .expect("record the shrink copy");
        }
        renderer.surface = (128, 128);
        let image = renderer.readback().expect("readback");
        assert_eq!((image.width, image.height), (128, 128));
        assert_eq!(
            pixel(&image, 128, 8, 8),
            [255, 0, 0, 255],
            "旧内容原样保留在新目标的左上角"
        );
        assert_eq!(
            pixel(&image, 128, 100, 100),
            [255, 255, 255, 255],
            "内容之外是 clear 出来的背景色"
        );
    }
}

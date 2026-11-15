//! 帧编排：上传 → 逐 damage scissor 画进持久 offscreen → copy 到 back buffer → `Present1`
//! （`design/phase-08-d3d12.md` §5.6 / §6、`design/async-render-pipeline.md` §6.3）。
//!
//! 结构要点（DEVIATIONS F-71）：**只有持久 offscreen 是绘制目标**；swapchain 的 back buffer
//! 只作 `COPY_DEST`（不建 RTV、不挂 scissor 绘制）。
//!
//! **buffer / content 分离 + 内容取桶**（`design/DEVIATIONS.md` §36 F-199）：组合交换链在第一次 present
//! 之后不能 `ResizeBuffers`（probe `resize.log`），所以持久目标与交换链按
//! [`buffer_size`] 的桶尺寸创建（内容 + 512 余量、256 对齐），而 [`GpuRenderer::surface`] 是
//! **当前内容**尺寸 —— 绘制、viewport、scissor、root uniform、copy、present dirty 全部按内容
//! 子矩形，只有资源创建与 clear 用 buffer。窗口 resize 因此不需要任何 GPU 等待或资源重建；
//! 只有内容**越过** buffer 时才走 [`GpuRenderer::grow`]：等 idle → 新建持久目标 + 交换链 →
//! prime 新交换链一帧背景色（等 GPU fence）→ 把 visual 切过去并 `Commit` → 丢弃旧对象
//! （越过边界时会闪一帧背景，见 `grow` 的注释）。内容 visual 恒 1:1（不随 resize 缩放）；
//! UI 线程只更新内容裁剪与背景层缩放（`platform/windows/composition.rs`）。
//!
//! 窗口路径另有**一条 1×1 的组合交换链**作为背景层（`background_swapchain`）：它被 prime 成
//! 窗口背景色，由 DComp 的 scale 变换铺满客户区，所以窗口放大时露出的新区域是背景色，而不是
//! 桌面、也不是被拉伸的旧内容（Chrome 语义）。背景色变化时用
//! [`GpuRenderer::refresh_background`] 换一条新的 1×1 交换链（1×1，代价可忽略）。
//!
//! 所有权（`design/async-render-pipeline.md` §3.1）：本模块的 [`GpuRenderer`] 是**GPU 线程独占**
//! 的对象（device / queue / swapchain / persist / upload / retire / 计时），构造发生在窗口创建时
//! （UI 线程），构造完成后整体移交给 `kytrix-d3d12` 线程（[`super::renderer`]）。离屏 / 快照路径
//! 不走线程：`GpuRenderer` 由门面同步调用，语义与 golden 不变（§4.5 的下半段）。
//!
//! 提交是异步的（I2）：`ExecuteCommandLists` 后立即返回；节奏来自 DXGI 的帧延迟 waitable
//! （由 GPU 循环在每帧录制前等一次），完成通知只通过 fence 观察。

use std::{
    cell::Cell,
    sync::{Arc, Mutex},
    time::Instant,
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
        geometry::GeometryId,
        instance::{FrameOp, GpuClipEntry},
    },
    platform::windows::composition::{self, Composition, buffer_size},
    render::{
        RenderStats,
        compositor::{FramePlan, Plan},
        frame::{FramePayload, FrameRequest, FrameSlot, MAX_FRAMES_IN_FLIGHT},
        retire::GpuResourceHandle,
        timer::{FrameTimings, GpuTimerSample, Section},
        windows::{
            atlas::AtlasTextures,
            device::{Device, init_error_from},
            pipeline::{GeometryRegistry, Pipeline},
            queue::Queue,
            swapchain::Swapchain,
            timer::GpuTimer,
        },
    },
    window::FrameIndex,
};

/// 测试构建的图集页（2048² 在 WARP 上代价过高）。
#[cfg(test)]
const TEST_ATLAS_PAGE: u32 = 64;

/// `RootUniform` 的字节数（P07 §2.7：112 B = 28 DWORD）。
const ROOT_UNIFORM_DWORDS: usize = 28;

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
pub(crate) struct GpuRenderer {
    device: Device,
    queue: Queue,
    pipeline: Pipeline,
    atlas: AtlasTextures,
    persist: Persist,
    upload: UploadArena,
    geometry: GeometryRegistry,
    timer: Option<GpuTimer>,
    /// 组合交换链（窗口路径）；内容越过 buffer 时整条替换（`grow`）。
    swapchain: Option<Swapchain>,
    /// 背景层（窗口路径）：**1×1** 的组合交换链，内容恒为窗口背景色（`clear_color`）。
    ///
    /// DComp 把它按客户区缩放后铺满内容之外的区域；背景色变化时整条替换
    /// （[`GpuRenderer::refresh_background`]），内容交换链与内容尺寸都不受影响。
    background_swapchain: Option<Swapchain>,
    /// 背景色已改、背景层还没重建（由 [`GpuRenderer::set_background_color`] 置位，
    /// [`GpuRenderer::refresh_background`] 消费）。
    background_dirty: bool,
    /// 合成目标（窗口路径）：GPU 线程在 `grow` 时把内容 visual 切到新交换链。
    ///
    /// 与 `NativeWindow` 共享（`Arc<Mutex<_>>`）：所有 DComp 调用由互斥锁串行化，GPU 线程只在
    /// `grow` / `refresh_background` 里持锁做一次 `SetContent` + `Commit`（见
    /// `platform/windows/composition.rs`）。
    composition: Option<Arc<Mutex<Composition>>>,
    rtv_heap: ID3D12DescriptorHeap,
    clear_color: [f32; 4],
    /// 合成面尺寸（持久目标 + 组合交换链的 grow-only 高水位，见 `grown_buffer`）。
    ///
    /// 初值来自 [`buffer_size`]（内容 + 512 余量、256 对齐）；只在内容越过它时按
    /// [`GpuRenderer::grow`] 逐轴取大重建。缩小时不还 VRAM（组合交换链也不能
    /// `ResizeBuffers`）。
    buffer: (u32, u32),
    /// 当前**内容**尺寸（绘制 / copy / present 的范围；窗口客户区变化时只改这个字段）。
    surface: (u32, u32),
    /// 下一次 present 是否必须全量：组合交换链刚建好之后为 `true`，第一次 present 成功后清除
    /// （首次 present 与首次 copy 都必须覆盖整块 buffer，见 `copy_to_back` / `present1`）。
    ///
    /// 离屏目标没有 present，因此它恒为 `true` —— 与移交给 GPU 线程之前的离屏语义一致。
    fresh: bool,
    /// 最后一次**真正提交**的帧序号（重建资源入 retire 队列时的 tag，§4.3）。
    last_index: FrameIndex,
    #[cfg(any(test, feature = "test-support"))]
    readback: Readback,
}

// SAFETY: `GpuRenderer` 在构造完成后只整体移动一次到 `kytrix-d3d12` 线程，此后 UI 线程不再
// 访问其中任何对象；D3D12/DXGI 对象本身允许跨线程使用，这里保证的是「单线程独占」而不是共享。
// `Queue` 里的 fence 事件句柄（`HANDLE`）与 swapchain 的 waitable 句柄只在该线程上使用。
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
    },
}

impl GpuRenderer {
    /// 产品路径：硬件设备 + 离屏目标（P09 在此之后 attach 原生窗口）。
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Self::with_device(
            Device::new()?,
            size,
            crate::render::windows::atlas::ATLAS_PAGE,
        )
    }

    /// 测试用的 WARP 渲染器（图集页 = 测试页尺寸）。
    #[cfg(test)]
    pub(crate) fn new_warp_offscreen(size: (u32, u32)) -> Result<Self, EngineError> {
        Self::with_device(Device::new_warp()?, size, TEST_ATLAS_PAGE)
    }

    /// WARP + 指定图集页尺寸（`TestApp::snapshot` 必须与 `App` 的分配器同页尺寸，
    /// 否则字形页的整页上传会被尺寸断言拒绝）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_warp_offscreen_with_page(
        size: (u32, u32),
        atlas_page: u32,
    ) -> Result<Self, EngineError> {
        Self::with_device(Device::new_warp()?, size, atlas_page)
    }

    /// 组合交换链路径（Windows 窗口）。
    ///
    /// `content` 是当前客户区（物理像素）；`buffer` 由 [`buffer_size`] 给出（内容 + 余量、256
    /// 对齐），此后窗口 resize 只改内容尺寸（[`GpuRenderer::set_content_size`]），只有内容越过
    /// buffer 时才重建资源（[`GpuRenderer::grow`]）。
    ///
    /// 本方法在 UI 线程、GPU 线程启动之前运行。顺序（P1-B）：先创建主交换链与 **1×1 背景
    /// 交换链**并各 prime 一帧背景色（第一次 present，必须全量；客户区在第一次 present 之前是
    /// **完全透明**的，probe `dpi.log` 03/04），**再**把两者挂到 [`Composition`] 的 visual 上并
    /// `Commit`。已 attach 但从未 present 的交换链在合成器里是透明的，反过来做会让 `ShowWindow`
    /// 之前的一帧露出桌面。背景层先 prime、内容层后 prime：两者用同一个 clear color，顺序不影响
    /// 首次可见像素，这里只是保持「内容最后」的既有次序。
    pub(crate) fn new_for_composition(
        composition: Arc<Mutex<Composition>>,
        content: (u32, u32),
        background: [f32; 4],
    ) -> Result<Self, EngineError> {
        let content = (content.0.max(1), content.1.max(1));
        let buffer = buffer_size(content)?;
        let mut renderer = Self::with_device(
            Device::new()?,
            buffer,
            crate::render::windows::atlas::ATLAS_PAGE,
        )?;
        renderer.clear_color = background;
        let mut swapchain = Swapchain::for_composition(&renderer.queue, buffer)?;
        // 背景层是 1×1 的纯色交换链，由 UI 线程的 DComp scale 变换铺满客户区。
        let mut background_swapchain = Swapchain::for_composition(&renderer.queue, (1, 1))?;
        // prime 用显式的 swapchain / buffer / content；此刻两条 visual 都还没有内容。
        renderer.prime_detached(&mut background_swapchain, (1, 1), (1, 1))?;
        renderer.prime_detached(&mut swapchain, buffer, content)?;
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

    fn with_device(device: Device, size: (u32, u32), atlas_page: u32) -> Result<Self, EngineError> {
        let queue = Queue::new(&device)?;
        let pipeline = Pipeline::new(&device)?;
        let atlas = AtlasTextures::new(&device, atlas_page)?;
        pipeline.write_atlas(&device, AtlasKind::Glyph, atlas.texture(AtlasKind::Glyph));
        pipeline.write_atlas(&device, AtlasKind::Image, atlas.texture(AtlasKind::Image));

        // SAFETY: COM 创建调用：设备 / 工厂句柄有效，描述与输出指针在调用期间存活，失败按 HRESULT 返回。
        let rtv_heap: ID3D12DescriptorHeap = unsafe {
            device
                .raw()
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })
        }
        .map_err(|error| init_error_from("CreateDescriptorHeap(RTV)", error))?;
        // SAFETY: 描述符堆由 `Pipeline` / `GpuRenderer` 持有，句柄在堆生命周期内有效。
        let rtv = unsafe { rtv_heap.GetCPUDescriptorHandleForHeapStart() };

        let mut device = device;
        let persist = Persist::new(&mut device, size, rtv)?;
        let upload = UploadArena::new(&mut device)?;
        let timer = GpuTimer::new(&device, &queue)?;

        Ok(Self {
            device,
            queue,
            pipeline,
            atlas,
            persist,
            upload,
            geometry: GeometryRegistry::new(),
            timer,
            swapchain: None,
            background_swapchain: None,
            background_dirty: false,
            composition: None,
            rtv_heap,
            clear_color: [0.0, 0.0, 0.0, 0.0],
            buffer: (size.0.max(1), size.1.max(1)),
            surface: (size.0.max(1), size.1.max(1)),
            fresh: true,
            last_index: FrameIndex::default(),
            #[cfg(any(test, feature = "test-support"))]
            readback: Readback::new(),
        })
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
        self.queue.acquire_slot()
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
        self.queue
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
        self.queue
            .set_completion_event(signal_value, event)
            .map_err(|error| self.classify(error))
    }

    /// 队列 fence 的已完成值（非阻塞；完成通知的唯一判据）。
    pub(crate) fn completed_value(&self) -> u64 {
        debug_assert!(
            on_gpu_thread(),
            "the queue fence may only be observed on the kytrix-d3d12 thread"
        );
        self.queue.completed_value()
    }

    /// 内容尺寸变化（窗口路径）：内容仍在 buffer 内时是**纯 CPU 记账**；越过 buffer 时走
    /// [`GpuRenderer::grow`] 重建持久目标 + 组合交换链（grow-only 桶）。
    ///
    /// 窗口的 resize 已经把整帧标成全量 damage（`Window::set_surface`），下一帧的
    /// `FramePlan` 是 `Full`，不需要在这里碰 `fresh`。
    pub(crate) fn set_content_size(&mut self, content: (u32, u32)) -> Result<(), EngineError> {
        let content = (content.0.max(1), content.1.max(1));
        if content == self.surface {
            return Ok(());
        }
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
        self.queue
            .wait_idle()
            .map_err(|error| self.classify(error))?;
        self.queue
            .reset_in_flight_lists()
            .map_err(|error| self.classify(error))?;
        // 2. 新对象在旧对象销毁之前创建（失败时旧状态仍然完好）。
        // SAFETY: 描述符堆由本渲染器持有，句柄在堆生命周期内有效。
        let rtv = unsafe { self.rtv_heap.GetCPUDescriptorHandleForHeapStart() };
        let persist =
            Persist::new(&mut self.device, buffer, rtv).map_err(|error| self.classify(error))?;
        let mut swapchain = Swapchain::for_composition(&self.queue, buffer)?;
        // 3. **先**把新交换链 prime 一次（全量背景 + 等 GPU fence），**再**挂到 visual 上：
        //    已 attach 但从未 present 的交换链在合成器里是透明的（P1-B）。prime 用显式的新
        //    `swapchain` / `buffer` / `content` 记账，旧 visual 在这期间继续显示旧内容。
        //    旧 persist 先换下来、但保持存活到视觉切换之后；prime 用的是新 persist。
        let retired_index = self.last_index;
        let old_persist = std::mem::replace(&mut self.persist, persist);
        self.prime_detached(&mut swapchain, buffer, content)?;
        {
            let target = composition::lock(&composition);
            target.set_content(swapchain.raw())?;
            target.commit()?;
        }
        // 4. 视觉切换**之后**才替换 / 丢弃旧对象（旧 swapchain 的已 present 内容可能仍被
        //    DWM 引用到下一次 `Commit`，本地变量保证它活到 `Commit` 之后）。
        let _old_swapchain = self.swapchain.replace(swapchain);
        self.queue.retire(retired_index, old_persist.handle);
        self.buffer = buffer;
        self.surface = content;
        // prime 已经做过新交换链的第一次 present，后续帧不再是 `fresh`。
        self.fresh = false;
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
        self.queue
            .wait_idle()
            .map_err(|error| self.classify(error))?;
        // RTV 句柄由堆派生（堆是所有者；`Persist` 只缓存句柄本身）。
        // SAFETY: 描述符堆由本渲染器持有，句柄在堆生命周期内有效。
        let rtv = unsafe { self.rtv_heap.GetCPUDescriptorHandleForHeapStart() };
        let old = self
            .persist
            .recreate(&mut self.device, size, rtv)
            .map_err(|error| self.classify(error))?;
        self.queue.retire(self.last_index, old);
        self.buffer = size;
        self.surface = size;
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
    /// 背景层是单独一条交换链，已经挂到 visual 上的内容不会因为 `clear_color` 改变而变化，
    /// 所以换色必须：新建一条 1×1 组合交换链 → prime 新背景色（全量 + 等 GPU fence）→
    /// `SetContent` + `Commit` → 丢弃旧交换链。顺序与 `grow` 相同（prime 先于 attach，P1-B）。
    /// 1×1 纹理 + 一次 1×1 copy，代价可忽略；即使当前没有内容帧，背景色也会立刻生效。
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
        let mut swapchain = Swapchain::for_composition(&self.queue, (1, 1))?;
        self.prime_detached(&mut swapchain, (1, 1), (1, 1))?;
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
    /// 完全透明。`grow` / `refresh_background` 必须在 `SetContent` **之前**调用本方法：已 attach
    /// 但从未 present 的交换链在合成器里是透明的，若先 `SetContent` + `Commit` 再 prime，中间
    /// 一次 DWM 合成就会露出桌面（P1-B）。这里用显式的 `swapchain` / `buffer` / `content`，而不是
    /// `self.swapchain`（它可能还是旧的）。
    ///
    /// 背景层的 1×1 交换链也走这里（`buffer = content = (1, 1)`）：prime 会先把持久目标整块
    /// clear 成 `clear_color`，再 copy 左上角 1×1 —— 拷到的必然是背景色；持久目标本身会在下一个
    /// 全量帧重画（`Window::set_background_color` 已经标了全量 damage）。
    ///
    /// `sync = 0`：prime 不是用户可见帧，不值得等 vsync —— 但**必须**等这一帧的队列 fence
    /// （P2-1）：`Present1` 返回只是 CPU 侧入队，`ShowWindow` / 后续帧的 waitable 之前必须有
    /// 已完成的合成内容。
    fn prime_detached(
        &mut self,
        swapchain: &mut Swapchain,
        buffer: (u32, u32),
        content: (u32, u32),
    ) -> Result<(), EngineError> {
        let slot = self.queue.acquire_slot();
        self.queue
            .wait_slot_fence(slot.wait_value)
            .map_err(|error| self.classify(error))?;
        let payload = FramePayload::empty();
        let mut frame = FramePlan::new(Plan::Full, true);
        let mut timings = FrameTimings::new();
        // prime 不是内容帧：不要让它把 `last_index`（retire 的 tag）推回 `FrameIndex::default()`。
        // `grow` 已经在 prime 之前取好 `retired_index`；背景层刷新则发生在帧之间，两者都不该
        // 改变「最后一次内容帧」的记账。
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
            },
        )?;
        self.last_index = last_index;
        swapchain
            .present1(&mut frame, 0, content)
            .map_err(|error| self.classify(error))?;
        // 等到本帧的 GPU 工作完成（`FrameSlot::signal_value`），不是 `Present1` 的 CPU 返回。
        self.queue
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
        let slot = self.queue.acquire_slot();
        self.queue
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
        self.queue
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
        self.queue.release_completed(&mut self.device);
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
        self.queue.release_completed(&mut self.device);
    }

    /// 录制 + 提交一帧（不呈现、不等完成）。调用方先取槽并等该槽上一帧完成（§6.5）。
    ///
    /// `target` 决定录制的呈现目标：常规帧用自己的交换链；`prime_detached` 用**还没挂到
    /// visual 上**的新交换链（内容越过 buffer 时它必须先被 present，见 `grow`）。
    fn submit_recorded(
        &mut self,
        slot: FrameSlot,
        index: FrameIndex,
        payload: &FramePayload,
        frame: &mut FramePlan,
        timings: &mut FrameTimings,
        target: PresentTarget<'_>,
    ) -> Result<(RenderStats, FrameSlot), EngineError> {
        if let Some(error) = self.device.lost_error() {
            return Err(error);
        }
        timings.reset(index);
        debug_assert_eq!(
            timings.frame(),
            index,
            "timings must be reset for this frame"
        );
        let frame_started = Instant::now();

        self.queue
            .reset_slot(slot)
            .map_err(|error| self.classify(error))?;

        let stats = {
            let (swapchain, buffer, content, timer) = match target {
                PresentTarget::Own => (
                    self.swapchain.as_ref(),
                    self.buffer,
                    self.surface,
                    self.timer.as_ref(),
                ),
                PresentTarget::Detached {
                    swapchain,
                    buffer,
                    content,
                } => (Some(swapchain), buffer, content, None),
            };
            // 借用拆分：录制只碰这些字段，`buffer` / `surface` / `fresh` / `last_index` 留在外层。
            let mut recorder = FrameRecorder {
                device: &mut self.device,
                queue: &mut self.queue,
                pipeline: &self.pipeline,
                atlas: &mut self.atlas,
                persist: &self.persist,
                upload: &mut self.upload,
                geometry: &self.geometry,
                timer,
                swapchain,
                clear_color: self.clear_color,
                buffer,
                content,
            };
            recorder.record(slot, index, payload, frame, timings, frame_started)
        }
        .map_err(|error| self.classify(error))?;

        self.queue
            .submit(index, slot)
            .map_err(|error| self.classify(error))?;
        self.last_index = index;
        Ok((stats, slot))
    }

    /// 图集上传入口：P09 的字形页（整页）与 P15 的图片页共用（GPU 线程消费）。
    ///
    /// `pixels` 按**所有权**移动进来（`AtlasTextures` 的待上传槽直接持有它），避免槽 → 纹理
    /// 之间再复制一次整页（2048² 的图片页是 16 MiB）。
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        pixels: Vec<u8>,
        rows: crate::render::atlas::DirtyRows,
    ) {
        debug_assert!(
            self.swapchain.is_none() || on_gpu_thread(),
            "window atlas uploads may only be queued on the kytrix-d3d12 thread"
        );
        self.atlas.queue_upload(kind, pixels, rows);
    }

    /// 读回持久 offscreen（快照；阻塞到 GPU 完成）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn readback(&mut self) -> Result<ReadbackImage, EngineError> {
        self.queue.wait_idle()?;
        self.readback.capture(
            &self.device,
            &mut self.queue,
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

    /// 测试用的静态几何注册入口（P09 的 `GeometryStore` 接上后语义不变）。
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
        self.geometry.register(
            &mut self.device,
            id,
            vertices,
            indices,
            pipeline,
            topology,
            index_type,
        )
    }

    /// 把底层错误归类为设备丢失（粘性）或原样返回。
    fn classify(&mut self, error: EngineError) -> EngineError {
        self.device.note_failure().unwrap_or(error)
    }
}

impl Drop for GpuRenderer {
    fn drop(&mut self) {
        // 退出路径：等 GPU 完成所有在飞提交，并丢掉命令列表对 back buffer 的引用，
        // 之后才销毁 swapchain / 设备（§6.7 的关闭握手由门面负责线程 join）。
        let _ = self.queue.wait_idle();
        let _ = self.queue.reset_in_flight_lists();
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

/// 一帧录制所需的 GPU 线程独占对象（借用拆分：`GpuRenderer` 的字段在这里按用途分组）。
struct FrameRecorder<'a> {
    device: &'a mut Device,
    queue: &'a mut Queue,
    pipeline: &'a Pipeline,
    atlas: &'a mut AtlasTextures,
    persist: &'a Persist,
    upload: &'a mut UploadArena,
    geometry: &'a GeometryRegistry,
    timer: Option<&'a GpuTimer>,
    swapchain: Option<&'a Swapchain>,
    clear_color: [f32; 4],
    /// grow-only 的合成面尺寸（只有资源创建与 clear 用它）。
    buffer: (u32, u32),
    /// 当前内容尺寸（viewport / scissor / root uniform / copy / present 的范围）。
    content: (u32, u32),
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
            self.device,
            self.queue,
            index,
            slot.index,
            payload.instances(),
            payload.clips(),
        )?;
        self.pipeline.write_slot(
            self.device,
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

        let root = root_uniform(self.content.0 as f32, self.content.1 as f32);
        let full = frame.is_full();
        let scissor_whole = RECT {
            left: 0,
            top: 0,
            right: self.content.0 as i32,
            bottom: self.content.1 as i32,
        };
        // 几何绘制的下标表与 ops 同帧：`FrameOp::Geometry.index` 只有配合同一帧的表才有意义。
        let mut resolve = |op: u32| {
            payload
                .geometry()
                .get(op as usize)
                .map(|draw| draw.id)
                .ok_or_else(|| EngineError::GeometryRegister {
                    detail: smol_str::format_smolstr!("geometry op index {op} out of range"),
                })
        };

        let mut draw_calls = 0u32;
        {
            let list = self.queue.list(slot);
            if let Some(timer) = self.timer {
                timer.begin(list, slot.index);
            }
            if self.atlas.has_pending() {
                self.atlas.record_uploads(self.device, list, slot.index)?;
            }
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
            bind_frame(
                list,
                self.pipeline,
                self.persist,
                slot.index,
                &root,
                self.content,
            );
            match full {
                true => {
                    draw_calls = draw_ops(
                        list,
                        self.pipeline,
                        payload.ops(),
                        self.geometry,
                        &mut resolve,
                        &root,
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
                                &mut resolve,
                                &root,
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
        device: &mut Device,
        size: (u32, u32),
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Result<Self, EngineError> {
        let texture = create_target(device, size)?;
        let handle = device.allocate(texture.clone());
        device.create_render_target_view(&texture, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, rtv);
        Ok(Self {
            texture,
            handle,
            rtv,
        })
    }

    /// 重建；返回旧纹理的句柄（调用方决定 retire 还是立即释放，I4）。
    fn recreate(
        &mut self,
        device: &mut Device,
        size: (u32, u32),
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Result<GpuResourceHandle, EngineError> {
        let texture = create_target(device, size)?;
        let handle = device.allocate(texture.clone());
        device.create_render_target_view(&texture, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, rtv);
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
    clips: ID3D12Resource,
    clip_handle: GpuResourceHandle,
    clip_capacity: u32,
}

impl UploadArena {
    fn new(device: &mut Device) -> Result<Self, EngineError> {
        let mut slots = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let instances = create_upload(device, 256 * 128)?;
            let instance_handle = device.allocate(instances.clone());
            let clips = create_upload(device, 8 * 48)?;
            let clip_handle = device.allocate(clips.clone());
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

    fn slot(&self, slot: usize) -> &UploadSlot {
        &self.slots[slot]
    }

    /// 写入本帧的实例/裁剪字节；容量不够时扩容（旧缓冲交 retire 队列）。返回 `(实例数, 裁剪层数)`。
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
        let needs_instances = instance_count > self.slots[slot].instance_capacity;
        if needs_instances {
            let capacity = instance_count.next_power_of_two().max(256);
            let resource = create_upload(device, u64::from(capacity) * 128)?;
            let handle = device.allocate(resource.clone());
            let old = self.slots[slot].instance_handle;
            self.slots[slot].instances = resource;
            self.slots[slot].instance_handle = handle;
            self.slots[slot].instance_capacity = capacity;
            queue.retire(index, old);
        }
        let needs_clips = clip_count > self.slots[slot].clip_capacity;
        if needs_clips {
            let capacity = clip_count.next_power_of_two().max(8);
            let resource = create_upload(device, u64::from(capacity) * 48)?;
            let handle = device.allocate(resource.clone());
            let old = self.slots[slot].clip_handle;
            self.slots[slot].clips = resource;
            self.slots[slot].clip_handle = handle;
            self.slots[slot].clip_capacity = capacity;
            queue.retire(index, old);
        }

        write_upload(&self.slots[slot].instances, instances)?;
        write_upload(&self.slots[slot].clips, bytemuck::cast_slice(clips))?;
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

fn write_upload(resource: &ID3D12Resource, bytes: &[u8]) -> Result<(), EngineError> {
    if bytes.is_empty() {
        return Ok(());
    }
    let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: 映射区间由 `Map` 独占，直到同一函数的 `Unmap`；容量由该槽缓冲的创建尺寸保证。
    unsafe { resource.Map(0, None, Some(&mut pointer)) }
        .map_err(|error| init_error_from("Map(upload)", error))?;
    // SAFETY: 上传缓冲容量在 `UploadArena::write` 里已按 `bytes.len()` 扩容。
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len());
        resource.Unmap(0, None);
    }
    Ok(())
}

fn bind_frame(
    list: &ID3D12GraphicsCommandList,
    pipeline: &Pipeline,
    persist: &Persist,
    slot: usize,
    root: &[f32; ROOT_UNIFORM_DWORDS],
    content: (u32, u32),
) {
    // SAFETY: RTV / 视口 / 矩形由本帧持有，调用期间有效。
    unsafe {
        list.SetDescriptorHeaps(&[Some(pipeline.heap().clone())]);
        list.SetGraphicsRootSignature(pipeline.root_signature());
        list.SetPipelineState(pipeline.quad());
        list.SetGraphicsRoot32BitConstants(0, ROOT_UNIFORM_DWORDS as u32, root.as_ptr().cast(), 0);
        list.SetGraphicsRootDescriptorTable(1, pipeline.slot_table(slot));
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
    geometry: &GeometryRegistry,
    resolve: &mut impl FnMut(u32) -> Result<GeometryId, EngineError>,
    root: &[f32; ROOT_UNIFORM_DWORDS],
    scissor: RECT,
) -> Result<u32, EngineError> {
    let mut draw_calls = 0u32;
    let mut quad_bound = false;
    for op in ops {
        match *op {
            FrameOp::Instances { start, len } => {
                if len == 0 {
                    continue;
                }
                if !quad_bound {
                    // SAFETY: PSO / 根常量 / 拓扑 / scissor 都属于同一条已打开的 list。
                    unsafe {
                        list.SetPipelineState(pipeline.quad());
                        list.SetGraphicsRoot32BitConstants(
                            0,
                            ROOT_UNIFORM_DWORDS as u32,
                            root.as_ptr().cast(),
                            0,
                        );
                        list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
                        list.RSSetScissorRects(&[scissor]);
                    }
                    quad_bound = true;
                }
                // SAFETY: 4 顶点 TriangleStrip + 实例区间 [start, start+len) 已在编码期校验。
                unsafe { list.DrawInstanced(4, len, 0, start) };
                draw_calls = draw_calls.saturating_add(1);
            }
            FrameOp::Geometry {
                index,
                clip_index,
                clip_rect,
            } => {
                let draw = resolve(index)?;
                let entry = geometry
                    .lookup(draw)
                    .ok_or_else(|| EngineError::GeometryRegister {
                        detail: smol_str::format_smolstr!(
                            "geometry id {}:{} has no GPU buffers (GeometryStore lands in P09)",
                            draw.index(),
                            draw.generation()
                        ),
                    })?;
                let uniform = geometry_root(root, clip_index, clip_rect);
                // SAFETY: 顶点/索引缓冲由 `GeometryRegistry` 持有到本帧结束；
                // `entry.topology()` 与注册时的图元类型一致（P07 §2.7）。
                unsafe {
                    list.SetPipelineState(pipeline.geometry(entry.pipeline()));
                    list.SetGraphicsRoot32BitConstants(
                        0,
                        ROOT_UNIFORM_DWORDS as u32,
                        uniform.as_ptr().cast(),
                        0,
                    );
                    list.IASetPrimitiveTopology(entry.topology());
                    list.IASetVertexBuffers(0, Some(&[entry.vertex()]));
                    list.IASetIndexBuffer(Some(&entry.index()));
                    list.RSSetScissorRects(&[scissor]);
                    list.DrawIndexedInstanced(entry.index_count(), 1, 0, 0, 0);
                }
                draw_calls = draw_calls.saturating_add(1);
                quad_bound = false;
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
fn root_uniform(width: f32, height: f32) -> [f32; ROOT_UNIFORM_DWORDS] {
    let mut uniform = [0.0f32; ROOT_UNIFORM_DWORDS];
    uniform[0] = width;
    uniform[1] = height;
    uniform[2] = if width > 0.0 { 2.0 / width } else { 0.0 };
    uniform[3] = if height > 0.0 { 2.0 / height } else { 0.0 };
    uniform[4] = 1.0; // frame.params.scale（geometry 的逻辑→物理）
    uniform[8] = 1.0; // draw.basis.m11
    uniform[11] = 1.0; // draw.basis.m22
    uniform
}

/// 逐 geometry 的 draw uniform（`FrameOp::Geometry` 已经带 clip；颜色通道 P09 补齐）。
/// geometry 的 draw uniform（112 B；布局严格对照 `common.hlsl` 的 `GeometryDrawUniform`）：
/// basis @8、translation @12、color0 @16、clip_rect @20（x,y,w,h）、clip @24（`.x` = 索引）。
fn geometry_root(
    frame: &[f32; ROOT_UNIFORM_DWORDS],
    clip_index: u32,
    clip_rect: [u32; 4],
) -> [f32; ROOT_UNIFORM_DWORDS] {
    let mut uniform = [0.0f32; ROOT_UNIFORM_DWORDS];
    uniform[..8].copy_from_slice(&frame[..8]);
    // draw.basis @8 = 单位矩阵（P08 只有恒等变换，S-77）。
    uniform[8] = 1.0;
    uniform[11] = 1.0;
    // draw.translation @12 = (0, 0, 0, 0)。
    // draw.color0 @16 = 不透明白（预乘线性 RGBA；几何着色属 P09，S-77）。
    uniform[16] = 1.0;
    uniform[17] = 1.0;
    uniform[18] = 1.0;
    uniform[19] = 1.0;
    // draw.clip_rect @20 = (x, y, w, h) 物理像素。无裁剪时用视口，避免 inf 进根常量；
    // 有裁剪时把编码器给的 AABB `(x0, y0, x1, y1)` 换成 `(x, y, w, h)`。
    let (x, y, w, h) = if clip_index == 0 {
        (0.0, 0.0, frame[0], frame[1])
    } else {
        let x0 = f32::from_bits(clip_rect[0]);
        let y0 = f32::from_bits(clip_rect[1]);
        let x1 = f32::from_bits(clip_rect[2]);
        let y1 = f32::from_bits(clip_rect[3]);
        (x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
    };
    uniform[20] = x;
    uniform[21] = y;
    uniform[22] = w;
    uniform[23] = h;
    // draw.clip @24：`.x = clip_index`（uint，逐位写入，不能做 f32 数值转换）。
    uniform[24] = f32::from_bits(clip_index);
    uniform
}

fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn create_target(device: &mut Device, size: (u32, u32)) -> Result<ID3D12Resource, EngineError> {
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
        device.raw().CreateCommittedResource(
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

fn create_upload(device: &mut Device, bytes: u64) -> Result<ID3D12Resource, EngineError> {
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
        device.raw().CreateCommittedResource(
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
#[cfg(test)]
mod tests {
    use super::{GpuRenderer, copy_extent, copy_plan, geometry_root, grown_buffer, root_uniform};
    use crate::{
        error::EngineError,
        math::{PhysicalRect, Point, Rect, Scale, Size, Transform},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            color::{Color, ColorResolver, Interpolation},
            geometry::{GeometryId, GeometryPipeline, IndexType, Topology},
            instance::{FrameOp, InstanceEncoder},
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
        let uniform = root_uniform(800.0, 600.0);
        assert_eq!(uniform[0], 800.0);
        assert_eq!(uniform[1], 600.0);
        assert_eq!(uniform[2], 2.0 / 800.0);
        assert_eq!(uniform[3], 2.0 / 600.0);
        assert_eq!(uniform[4], 1.0);
        assert_eq!(uniform[8], 1.0);
        assert_eq!(uniform[11], 1.0);
        assert_eq!(uniform.len(), 28);
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

    /// 逐 dword 对照 `common.hlsl` 的 `GeometryDrawUniform`（B1）。
    #[test]
    fn geometry_root_matches_common_hlsl_layout() {
        let frame = root_uniform(800.0, 600.0);
        let clip_rect = [4.0f32, 5.0, 100.0, 120.0].map(f32::to_bits);
        let uniform = geometry_root(&frame, 3, clip_rect);
        assert_eq!(uniform[..8], frame[..8], "frame block is copied verbatim");
        // draw.basis @8
        assert_eq!(
            [uniform[8], uniform[9], uniform[10], uniform[11]],
            [1.0, 0.0, 0.0, 1.0]
        );
        // draw.translation @12
        assert_eq!(
            [uniform[12], uniform[13], uniform[14], uniform[15]],
            [0.0; 4]
        );
        // draw.color0 @16 = premultiplied white
        assert_eq!(
            [uniform[16], uniform[17], uniform[18], uniform[19]],
            [1.0, 1.0, 1.0, 1.0]
        );
        // draw.clip_rect @20 = (x, y, w, h)
        assert_eq!(
            [uniform[20], uniform[21], uniform[22], uniform[23]],
            [4.0, 5.0, 96.0, 115.0]
        );
        // draw.clip @24 = uint clip_index（逐位）
        assert_eq!(uniform[24].to_bits(), 3);
        assert_eq!(uniform[25..].iter().copied().sum::<f32>(), 0.0);
    }

    #[test]
    fn geometry_root_without_clip_uses_the_viewport() {
        let frame = root_uniform(64.0, 32.0);
        let uniform = geometry_root(&frame, 0, [0; 4]);
        assert_eq!(
            [uniform[20], uniform[21], uniform[22], uniform[23]],
            [0.0, 0.0, 64.0, 32.0]
        );
        assert_eq!(uniform[24].to_bits(), 0);
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
            FrameOp::Instances { start: 0, len: 1 }
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
            "each damage scissor repeats the draw op list"
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
        renderer.queue_atlas_upload(AtlasKind::Image, pixels, rows);

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
        let Some(mut renderer) =
            super::GpuRenderer::new_warp_offscreen_with_page((64, 64), page).ok()
        else {
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
        renderer.queue_atlas_upload(AtlasKind::Glyph, pixels, rows);
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

    /// 内容子矩形：绘制与读回都只按内容尺寸，grow-only buffer 不参与（`design/DEVIATIONS.md` §36 F-199）。
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
        // buffer 2560×1792，新内容 2800×600：宽度增长，高度不得按内容缩回 1280。
        let grown = grown_buffer((2560, 1792), (2800, 600)).expect("grows");
        assert_eq!(grown, (3328, 1792));
        assert!(
            grown.1 >= 1792,
            "高度必须保留旧 buffer（旧 clip 仍可能指向旧内容）: {grown:?}"
        );
        // 另一方向同理。
        assert_eq!(
            grown_buffer((2560, 1792), (1200, 2400)).expect("grows"),
            (2560, 3072)
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
}

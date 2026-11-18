//! 持久 offscreen：创建 / 重建 / 尺寸变化（`ResizeBuffers` 路径 + 回退开关）/ 背景层刷新。
//! 实现体由 `compositor.rs` 的薄委托调用。

use windows::Win32::Graphics::{
    Direct3D12::*,
    Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_SAMPLE_DESC},
};

use super::{GpuRenderer, Persist, on_gpu_thread};
use crate::{
    error::EngineError,
    platform::windows::composition::{self, buffer_size},
    render::windows::{
        device::{SharedGpu, init_error_from},
        hresult::resize_fallback_worthwhile,
        swapchain::Swapchain,
    },
};

/// 消费者：`GpuRenderer::set_content_size`（`compositor.rs` 的薄委托）。
///
/// 内容尺寸变化（窗口路径）分两种：
/// - **仍在同一个 buffer 桶内**：纯 CPU 记账（内容 visual 恒 1:1，UI 线程只改 DComp 的 clip /
///   背景 scale）。窗口 resize 因此不需要任何 GPU 等待或资源重建；
/// - **越过桶**：走 [`resize_target`]（`ResizeBuffers` + 重建持久目标）。
///
/// 窗口的 resize 已经把整帧标成全量 damage（`Window::set_surface`），下一帧的 `FramePlan` 是
/// `Full`，因此这里不需要碰 `TargetState`。
pub(super) fn set_content_size(
    renderer: &mut GpuRenderer,
    content: (u32, u32),
) -> Result<(), EngineError> {
    let content = (content.0.max(1), content.1.max(1));
    if content == renderer.surface {
        return Ok(());
    }
    // 上限校验对两条路径都生效：内容的任一边 > 16384 是显式错误（DXGI 纹理上限）。
    let buffer = buffer_size(content)?;
    if renderer.swapchain.is_none() {
        // 离屏 / 快照目标：没有交换链，持久目标按内容精确分配。
        return resize_offscreen(renderer, content);
    }
    if buffer == renderer.buffer {
        renderer.surface = content;
        return Ok(());
    }
    resize_target(renderer, content)?;
    Ok(())
}

/// 消费者：`GpuRenderer::resize_target`（`compositor.rs` 的薄委托）。
/// 把合成面（交换链 + 持久目标）调整到当前内容所需的桶。
///
/// **`ResizeBuffers` 的前置条件全部在这里满足**（R08-1）：
/// 1. `wait_idle`：本窗口的工作全部完成；
/// 2. `reset_in_flight_lists`：丢掉命令列表对 back buffer 的**间接引用**（唯一手段）；
/// 3. 不缓存 `GetBuffer` 引用（`Swapchain` 每帧重取）；
/// 4. 组合链的 `Width/Height` 永远 ≥ 1（`buffer_size` 保证）。
///
/// 之后重建持久目标（内容会丢 ⇒ `TargetState::Fresh` + `BufferSyncs::invalidate`）并
/// `ResizeBuffers`。**不再** prime / `SetContent` / `Commit` / 新建 factory —— 交换链对象没变，
/// 视觉树也不用动，所以这次 resize 不会闪一帧背景色。
///
/// `ResizeBuffers` 被驱动拒绝（`DXGI_ERROR_INVALID_CALL`）时走**回退开关**
/// （[`replace_swapchain`]，整链替换 + prime + `SetContent` + `Commit`，即旧 `grow` 的形状）。
pub(super) fn resize_target(
    renderer: &mut GpuRenderer,
    content: (u32, u32),
) -> Result<(), EngineError> {
    debug_assert!(
        on_gpu_thread(),
        "the composition target may only resize on the kytrix-d3d12 thread"
    );
    let content = (content.0.max(1), content.1.max(1));
    let buffer = buffer_size(content)?;
    if buffer == renderer.buffer {
        renderer.surface = content;
        return Ok(());
    }
    // 1. 等本窗口完成 + 丢掉命令列表对 back buffer 的间接引用。
    if !renderer
        .stream
        .wait_idle()
        .map_err(|error| renderer.classify(error))?
        .is_signaled()
    {
        // 设备健康但没等到（R08-3）：这一帧不改尺寸，下一帧重试。窗口继续显示旧内容，不致命。
        return Ok(());
    }
    renderer
        .stream
        .reset_in_flight_lists()
        .map_err(|error| renderer.classify(error))?;
    // 2. 新持久目标（旧句柄按最后一次提交序号 retire；命令列表刚被重置，引用已释放）。
    let rtv = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 0);
    let old = renderer
        .persist
        .recreate(&renderer.shared, buffer, rtv)
        .map_err(|error| renderer.classify(error))?;
    renderer.stream.retire(renderer.last_sequence, old);
    // 3. `ResizeBuffers`（回退开关见下）。
    let resized = {
        let device = renderer.shared.raw();
        match renderer.swapchain.as_mut() {
            Some(swapchain) => swapchain.resize(device, buffer),
            None => return Ok(()),
        }
    };
    match resized {
        Ok(()) => {}
        Err(error) if resize_fallback_worthwhile(error.code()) => {
            crate::render::windows::diagnostics::resize_fallback();
            replace_swapchain(renderer, buffer)?;
        }
        Err(error) => {
            return Err(EngineError::Swapchain {
                detail: smol_str::format_smolstr!(
                    "ResizeBuffers({}x{}): {:#010X} {}",
                    buffer.0,
                    buffer.1,
                    error.code().0 as u32,
                    error.code().message()
                ),
            });
        }
    }
    renderer.buffer = buffer;
    renderer.surface = content;
    // back buffer 与 persist 的内容都不可信：下一帧整帧直绘 + 整面 present。
    renderer.target.on_target_created();
    renderer.buffer_syncs.invalidate();
    Ok(())
}

/// 消费者：`GpuRenderer::rebuild_target`（`compositor.rs` 的薄委托）。
/// 显示 / 电源恢复的**整链重建**（R07-14）。
///
/// 为什么不是 `ResizeBuffers`：唤醒 / 换屏之后尺寸常常一点没变（同分辨率换屏、睡眠回来），
/// 但交换链的显示器关联、驱动侧的呈现状态与持久目标的内容都不可信。这里复用
/// [`replace_swapchain`] 那条已经存在的整链替换路径（新建 → prime → `SetContent` + `Commit`
/// → 丢弃旧链），因此恢复路径**没有**第二套「重建交换链」的实现。
///
/// 返回 `Ok(false)`：设备健康但本窗口还有在飞工作（`wait_idle` 没等到），这一轮先不重建 ——
/// 调用方保留代际，下一轮重试（同 `resize_target` 的处理）。
pub(super) fn rebuild_target(renderer: &mut GpuRenderer) -> Result<bool, EngineError> {
    // 离屏 / 快照目标没有交换链、也不 present：没有需要重建的东西（任何线程都可以判）。
    if renderer.swapchain.is_none() {
        return Ok(true);
    }
    debug_assert!(
        on_gpu_thread(),
        "the composition target may only be rebuilt on the kytrix-d3d12 thread"
    );
    // `ResizeBuffers` / 整链替换的前置条件（R08-1）：本窗口工作完成 + 丢掉命令列表对
    // back buffer 的间接引用（`replace_swapchain` 的 prime 会重新取 back buffer）。
    if !renderer
        .stream
        .wait_idle()
        .map_err(|error| renderer.classify(error))?
        .is_signaled()
    {
        return Ok(false);
    }
    renderer
        .stream
        .reset_in_flight_lists()
        .map_err(|error| renderer.classify(error))?;
    let buffer = buffer_size(renderer.surface)?;
    // 持久目标与 back buffer 一样从零开始（旧内容不可信 ⇒ `TargetState::Fresh`）。
    let rtv = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 0);
    let old = renderer
        .persist
        .recreate(&renderer.shared, buffer, rtv)
        .map_err(|error| renderer.classify(error))?;
    renderer.stream.retire(renderer.last_sequence, old);
    // 设备真的被移除时 `classify` 会把错误升级成 `DeviceLost`（⇒ GPU 线程走既有的
    // `GpuCell::recover`，R08-4）；设备健康时保留原始交换链错误（与 `resize_target` 同语义）。
    replace_swapchain(renderer, buffer).map_err(|error| renderer.classify(error))?;
    renderer.buffer = buffer;
    renderer.target.on_target_created();
    renderer.buffer_syncs.invalidate();
    Ok(true)
}

/// `ResizeBuffers` 失败时的**回退开关**（R08-1）：整条交换链替换 + prime + `SetContent`。
///
/// 这是旧 `grow` 路径的形状，保留它是为了「真机上某种驱动拒绝 `ResizeBuffers`」这种**尚未实测
/// 证伪**的场景 —— WARP 上无法创建组合交换链，这台机器上拿不到真机证据（见报告）。默认路径
/// 永远不经过这里；次数进 `Diagnostics::resize_fallbacks`。
fn replace_swapchain(renderer: &mut GpuRenderer, buffer: (u32, u32)) -> Result<(), EngineError> {
    let Some(composition) = renderer.composition.clone() else {
        return Err(EngineError::Composition {
            detail: smol_str::SmolStr::new_static(
                "content exceeds the buffer on a renderer without a composition target",
            ),
        });
    };
    let mut swapchain =
        Swapchain::for_composition(renderer.shared.raw(), renderer.shared.queue(), buffer)?;
    // 新链的第一次 present 必须先于 attach（已 attach 但从未 present 的交换链在合成器里透明）。
    renderer.prime_detached(&mut swapchain)?;
    {
        let target = composition::lock(&composition);
        target.set_content(swapchain.raw())?;
        target.commit()?;
    }
    // 视觉切换之后才丢弃旧链（DWM 可能仍引用它到本次 `Commit` 之后）。
    let _old = renderer.swapchain.replace(swapchain);
    Ok(())
}

/// 消费者：`GpuRenderer::resize`（`compositor.rs` 的薄委托）。
/// 离屏 / 快照路径的尺寸变化：重建持久目标（**没有** swapchain）。
pub(super) fn resize_offscreen(
    renderer: &mut GpuRenderer,
    size: (u32, u32),
) -> Result<(), EngineError> {
    debug_assert!(
        renderer.swapchain.is_none(),
        "offscreen targets never resize a composition swapchain"
    );
    let size = (size.0.max(1), size.1.max(1));
    if size == renderer.surface {
        return Ok(());
    }
    if !renderer
        .stream
        .wait_idle()
        .map_err(|error| renderer.classify(error))?
        .is_signaled()
    {
        return Ok(());
    }
    // RTV 句柄由堆派生（堆是所有者；`Persist` 只缓存句柄本身）。
    let rtv = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 0);
    let old = renderer
        .persist
        .recreate(&renderer.shared, size, rtv)
        .map_err(|error| renderer.classify(error))?;
    renderer.stream.retire(renderer.last_sequence, old);
    renderer.buffer = size;
    renderer.surface = size;
    renderer.target.on_target_created();
    renderer.buffer_syncs.invalidate();
    Ok(())
}

/// 消费者：`GpuRenderer::refresh_background`（`compositor.rs` 的薄委托）。
/// 把新的背景色落到独立的 **1×1 背景层**（GPU 线程；颜色没变是 no-op）。
///
/// 背景层是单独一条 1×1 组合交换链，已经挂到 visual 上的内容不会因为 `clear_color` 改变而
/// 变化，所以换色必须：新建一条 1×1 组合交换链 → 直接 clear 它的 back buffer 并 present
/// （R08-8 的直绘路径，不再需要 1×1 持久目标）→ `SetContent` + `Commit` → 丢弃旧交换链。
/// 顺序与旧实现相同（prime 先于 attach，P1-B）。1×1 纹理 + 一次 1×1 clear，代价可忽略。
pub(super) fn refresh_background(renderer: &mut GpuRenderer) -> Result<(), EngineError> {
    if !renderer.background_dirty {
        return Ok(());
    }
    let Some(composition) = renderer.composition.clone() else {
        renderer.background_dirty = false;
        return Ok(());
    };
    debug_assert!(
        on_gpu_thread(),
        "background layers may only be replaced on the kytrix-d3d12 thread"
    );
    let mut swapchain =
        Swapchain::for_composition(renderer.shared.raw(), renderer.shared.queue(), (1, 1))?;
    renderer.prime_detached(&mut swapchain)?;
    {
        let target = composition::lock(&composition);
        target.set_background_content(swapchain.raw())?;
        target.commit()?;
    }
    // 视觉切换之后才丢弃旧背景交换链（DWM 可能仍引用它到本次 `Commit` 之后）。
    let _old_background = renderer.background_swapchain.replace(swapchain);
    renderer.background_dirty = false;
    Ok(())
}

/// 消费者：`GpuRenderer` 的 RTV 堆查询。
/// RTV 堆里第 `index` 个描述符的 CPU 句柄（0 = 内容持久目标）。
pub(super) fn rtv_handle(
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

/// 消费者：`Persist::new`（`compositor.rs` 的薄委托）。
pub(super) fn create_persist(
    shared: &SharedGpu,
    size: (u32, u32),
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Result<Persist, EngineError> {
    let texture = create_target(shared.raw(), size)?;
    let handle = shared.allocate(texture.clone());
    create_render_target_view(shared.raw(), &texture, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, rtv);
    Ok(Persist {
        texture,
        handle,
        rtv,
        size: (size.0.max(1), size.1.max(1)),
    })
}

/// 消费者：`Persist::recreate`（`compositor.rs` 的薄委托）。
/// 重建；返回旧纹理的句柄（调用方决定 retire 还是立即释放，I4）。
pub(super) fn recreate_persist(
    persist: &mut Persist,
    shared: &SharedGpu,
    size: (u32, u32),
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Result<crate::render::retire::GpuResourceHandle, EngineError> {
    let texture = create_target(shared.raw(), size)?;
    let handle = shared.allocate(texture.clone());
    create_render_target_view(shared.raw(), &texture, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, rtv);
    persist.rtv = rtv;
    persist.size = (size.0.max(1), size.1.max(1));
    let old = persist.handle;
    persist.texture = texture;
    persist.handle = handle;
    Ok(old)
}

/// 消费者：`Persist::new` / `Persist::recreate`。
///
/// 资源格式是 `B8G8R8A8_UNORM`，RTV **视图**格式是 `_UNORM_SRGB`（DEVIATIONS F-80）：shader 在
/// 线性空间里算、混合，写回时由 sRGB 视图做唯一一次编码。这是 **relaxed format casting**
/// （RS2+，`CastingFullyTypedFormatSupported`），能力位进 D8 基线（R08-12）。
///
/// fast-clear 值按**资源**格式（UNORM）解释、`ClearRenderTargetView` 按**视图**格式（sRGB）
/// 解释：两者对 `[0,0,0,0]` 恰好一致，且 `[0,0,0,0]` 是 sRGB 与线性都映射到 0 的唯一取值。
/// 因此这里的 clear 值恒为全 0 —— **不要**把背景色写进 `D3D12_CLEAR_VALUE`（那会让首帧与
/// 后续 `ClearRenderTargetView` 的颜色不一致）。背景色只经显式的 clear 调用传入。
pub(super) fn create_target(
    device: &ID3D12Device,
    size: (u32, u32),
) -> Result<ID3D12Resource, EngineError> {
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
    // SAFETY: COM 调用：设备句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
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

/// 消费者：`Persist::new` / `Persist::recreate`。
/// 建一个 RTV（**视图**格式决定是否做 sRGB 编码，DEVIATIONS F-80）。
pub(super) fn create_render_target_view(
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

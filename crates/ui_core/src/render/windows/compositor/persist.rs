//! 持久 offscreen：创建 / 重建 / 尺寸（grow 与高水位缩容）、资源状态过渡（barrier）。
//! 实现体由 `compositor.rs` 的薄委托调用（零行为变更的结构拆分）。

use std::time::Instant;

use windows::Win32::Graphics::{
    Direct3D12::*,
    Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_SAMPLE_DESC},
};

use super::{GpuRenderer, Persist, PrimePersist, on_gpu_thread};
use crate::{
    error::EngineError,
    platform::windows::composition::{self, buffer_size},
    render::{
        retire::GpuResourceHandle,
        windows::{
            device::{SharedGpu, init_error_from},
            swapchain::Swapchain,
        },
    },
};

/// 消费者：`GpuRenderer::set_content_size`（`compositor.rs` 的薄委托）。
/// 内容尺寸变化（窗口路径）：内容仍在 buffer 内时是**纯 CPU 记账**；越过 buffer 时走
/// [`GpuRenderer::grow`] 重建持久目标 + 组合交换链（grow-only 桶）。
///
/// 窗口的 resize 已经把整帧标成全量 damage（`Window::set_surface`），下一帧的
/// `FramePlan` 是 `Full`，不需要在这里碰 `fresh`。每次真实变化都刷新 `content_since`：
/// 缩容策略只在内容静止 [`SHRINK_IDLE`] 之后才动手（拖动中永远不缩）。
pub(super) fn set_content_size(
    renderer: &mut GpuRenderer,
    content: (u32, u32),
) -> Result<(), EngineError> {
    let content = (content.0.max(1), content.1.max(1));
    if content == renderer.surface {
        return Ok(());
    }
    renderer.content_since = Instant::now();
    if content.0 <= renderer.buffer.0 && content.1 <= renderer.buffer.1 {
        renderer.surface = content;
        return Ok(());
    }
    renderer.grow(content)
}

/// 消费者：`GpuRenderer::grow`（`compositor.rs` 的薄委托）。
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
pub(super) fn grow(renderer: &mut GpuRenderer, content: (u32, u32)) -> Result<(), EngineError> {
    // grow-only 高水位：新桶 = 逐轴取「按内容算出的桶」与「当前 buffer」的最大值（见
    // [grown_buffer]）。上限检查在 max 之后：`buffer_size` 已经拒绝 > 16384 的内容，
    // 而当前 buffer 永远 ≤ 上限。
    let buffer = grown_buffer(renderer.buffer, content)?;
    let Some(composition) = renderer.composition.clone() else {
        return Err(EngineError::Composition {
            detail: smol_str::SmolStr::new_static(
                "content exceeds the buffer on a renderer without a composition target",
            ),
        });
    };
    // 1. GPU 完成 + 丢掉命令列表对旧 back buffer 的引用。
    renderer
        .stream
        .wait_idle()
        .map_err(|error| renderer.classify(error))?;
    renderer
        .stream
        .reset_in_flight_lists()
        .map_err(|error| renderer.classify(error))?;
    // 2. 新对象在旧对象销毁之前创建（失败时旧状态仍然完好）。
    let rtv = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 0);
    let persist =
        Persist::new(&renderer.shared, buffer, rtv).map_err(|error| renderer.classify(error))?;
    let mut swapchain = Swapchain::for_composition(renderer.shared.queue(), buffer)?;
    // 3. **先**把新交换链 prime 一次（全量背景 + 等 GPU fence），**再**挂到 visual 上：
    //    已 attach 但从未 present 的交换链在合成器里是透明的（P1-B）。prime 用显式的新
    //    `swapchain` / `buffer` / `content` 记账，旧 visual 在这期间继续显示旧内容。
    //    旧 persist 先换下来、但保持存活到视觉切换之后；prime 用的是新 persist。
    let retired_index = renderer.last_index;
    let old_persist = std::mem::replace(&mut renderer.persist, persist);
    renderer.prime_detached(&mut swapchain, buffer, content, PrimePersist::Main, None)?;
    {
        let target = composition::lock(&composition);
        target.set_content(swapchain.raw())?;
        target.commit()?;
    }
    // 4. 视觉切换**之后**才替换 / 丢弃旧对象（旧 swapchain 的已 present 内容可能仍被
    //    DWM 引用到下一次 `Commit`，本地变量保证它活到 `Commit` 之后）。
    let _old_swapchain = renderer.swapchain.replace(swapchain);
    renderer.stream.retire(retired_index, old_persist.handle);
    renderer.buffer = buffer;
    renderer.surface = content;
    // prime 已经做过新交换链的第一次 present，后续帧不再是 `fresh`。
    renderer.fresh = false;
    renderer.content_since = Instant::now();
    Ok(())
}

/// 消费者：`GpuRenderer::maybe_shrink`（`compositor.rs` 的薄委托）。
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
pub(super) fn maybe_shrink(renderer: &mut GpuRenderer) -> Result<(), EngineError> {
    let Some(buffer) = shrink_target(
        renderer.buffer,
        renderer.surface,
        renderer.composition.is_some(),
    )?
    else {
        // 已经是最小桶（或没有合成目标）：推迟下一次检查，避免空闲路径立刻重试。
        renderer.content_since = Instant::now();
        return Ok(());
    };
    let Some(composition) = renderer.composition.clone() else {
        // `shrink_target` 已经把「没有合成目标」判成 `None`；这里只是不构造一个假的 Arc。
        return Ok(());
    };
    debug_assert!(
        on_gpu_thread(),
        "the high-water bucket may only shrink on the kytrix-d3d12 thread"
    );
    // 1. GPU 完成 + 丢掉命令列表对旧 back buffer / 旧持久目标的引用。
    renderer
        .stream
        .wait_idle()
        .map_err(|error| renderer.classify(error))?;
    renderer
        .stream
        .reset_in_flight_lists()
        .map_err(|error| renderer.classify(error))?;
    // 2. 新对象在旧对象销毁之前创建（失败时旧状态仍然完好）。
    let rtv = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 0);
    let persist =
        Persist::new(&renderer.shared, buffer, rtv).map_err(|error| renderer.classify(error))?;
    let mut swapchain = Swapchain::for_composition(renderer.shared.queue(), buffer)?;
    // 3. prime 从旧持久目标拷内容，所以先把新目标换进来、旧目标留在栈上保持存活。
    let old_persist = std::mem::replace(&mut renderer.persist, persist);
    renderer.prime_detached(
        &mut swapchain,
        buffer,
        renderer.surface,
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
    let _old_swapchain = renderer.swapchain.replace(swapchain);
    // prime 的命令列表刚引用过旧持久目标：重置列表把引用丢掉，再把纹理直接交回 device。
    renderer
        .stream
        .reset_in_flight_lists()
        .map_err(|error| renderer.classify(error))?;
    let _ = renderer.shared.release(old_persist.handle);
    drop(old_persist);
    renderer.buffer = buffer;
    // prime 已经做过新交换链的第一次全量 present，后续帧按内容子矩形 copy。
    renderer.fresh = false;
    renderer.content_since = Instant::now();
    Ok(())
}

/// 消费者：`GpuRenderer::resize`（`compositor.rs` 的薄委托）。
/// 离屏 / 快照路径的尺寸变化：重建持久目标（**没有** swapchain）。
///
/// 窗口路径不经过这里：内容尺寸由 [`GpuRenderer::set_content_size`] 记账，buffer 由
/// [`GpuRenderer::grow`] 重建（组合交换链不能 `ResizeBuffers`）。
///
/// 每一步失败都走 [`GpuRenderer::classify`]（设备丢失优先归类）；调用方收到任何错误都会
/// 直接冒泡（§6.6：不重试）。
pub(super) fn resize(renderer: &mut GpuRenderer, size: (u32, u32)) -> Result<(), EngineError> {
    debug_assert!(
        renderer.swapchain.is_none(),
        "window targets never resize their composition swapchain"
    );
    let size = (size.0.max(1), size.1.max(1));
    if size == renderer.surface {
        return Ok(());
    }
    renderer
        .stream
        .wait_idle()
        .map_err(|error| renderer.classify(error))?;
    // RTV 句柄由堆派生（堆是所有者；`Persist` 只缓存句柄本身）。
    let rtv = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 0);
    let old = renderer
        .persist
        .recreate(&renderer.shared, size, rtv)
        .map_err(|error| renderer.classify(error))?;
    renderer.stream.retire(renderer.last_index, old);
    renderer.buffer = size;
    renderer.surface = size;
    renderer.content_since = Instant::now();
    renderer.fresh = true;
    Ok(())
}

/// 消费者：`GpuRenderer::refresh_background`（`compositor.rs` 的薄委托）。
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
    let mut swapchain = Swapchain::for_composition(renderer.shared.queue(), (1, 1))?;
    renderer.prime_detached(
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
    let _old_background = renderer.background_swapchain.replace(swapchain);
    renderer.background_dirty = false;
    Ok(())
}

/// 消费者：`GpuRenderer::grow`。
/// 增长后的 buffer：逐轴取「当前 buffer」与「按内容算出的桶」的较大值（grow-only 高水位）。
///
/// 只按内容算时，corner drag（一轴变大、另一轴变小）会把已经开好的另一轴换小；而视觉切换那一
/// 帧的旧 clip 仍指向旧内容，就会采样到新交换链之外（未定义像素 / 露出桌面）。逐轴取大保证
/// 「旧 presented 内容 ≤ 旧 buffer ≤ 新 buffer」，P1-B 的中间帧因此始终有定义。
pub(super) fn grown_buffer(
    current: (u32, u32),
    content: (u32, u32),
) -> Result<(u32, u32), EngineError> {
    let required = buffer_size(content)?;
    Ok((required.0.max(current.0), required.1.max(current.1)))
}

/// 消费者：`GpuRenderer::maybe_shrink` / `GpuRenderer::shrink_candidate`。
/// 缩容决策：返回新的 buffer，`None` = 保持当前高水位。
///
/// - 没有合成目标（离屏 / 快照）：永不缩容（离屏由 [`GpuRenderer::resize`] 直接重建）。
/// - 当前内容所需的桶已经覆盖当前 buffer（两轴都 ≥）：已经是最小桶。这也覆盖 `None` 的另一半
///   语义 —— 内容 > buffer 的 grow-only 不变量被破坏时不得「缩」成更大的桶。
/// - 否则返回 `buffer_size(content)`：内容 ≤ surface ≤ buffer，所以它逐轴 ≤ 当前 buffer。
pub(super) fn shrink_target(
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
    })
}

/// 消费者：`Persist::recreate`（`compositor.rs` 的薄委托）。
/// 重建；返回旧纹理的句柄（调用方决定 retire 还是立即释放，I4）。
pub(super) fn recreate_persist(
    persist: &mut Persist,
    shared: &SharedGpu,
    size: (u32, u32),
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Result<GpuResourceHandle, EngineError> {
    let texture = create_target(shared.raw(), size)?;
    let handle = shared.allocate(texture.clone());
    create_render_target_view(shared.raw(), &texture, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, rtv);
    persist.rtv = rtv;
    let old = persist.handle;
    persist.texture = texture;
    persist.handle = handle;
    Ok(old)
}

/// 消费者：`copy_to_back` / `copy_content`。
pub(super) fn transition(
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

/// 消费者：`Persist::new` / `Persist::recreate`。
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

/// 消费者：`GpuRenderer` 的 RTV 堆查询。
/// RTV 堆里第 `index` 个描述符的 CPU 句柄（0 = 内容，1 = 1×1 背景）。
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

/// 消费者：`Persist::new` / `Persist::recreate`。
/// 建一个 RTV（**视图格式**决定是否做 sRGB 编码，DEVIATIONS F-80）。
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

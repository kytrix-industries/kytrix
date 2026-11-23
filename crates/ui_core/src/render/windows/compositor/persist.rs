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

pub(super) fn set_content_size(
    renderer: &mut GpuRenderer,
    content: (u32, u32),
) -> Result<(), EngineError> {
    let content = (content.0.max(1), content.1.max(1));
    if content == renderer.surface {
        return Ok(());
    }
    let buffer = buffer_size(content)?;
    if renderer.swapchain.is_none() {
        return resize_offscreen(renderer, content);
    }
    if buffer == renderer.buffer {
        renderer.surface = content;
        return Ok(());
    }
    resize_target(renderer, content)?;
    Ok(())
}

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
    if !renderer
        .stream
        .wait_idle()
        .map_err(|error| renderer.classify(error))?
        .is_signaled()
    {
        return Ok(());
    }
    renderer
        .stream
        .reset_in_flight_lists()
        .map_err(|error| renderer.classify(error))?;
    let rtv = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 0);
    let old = renderer
        .persist
        .recreate(&renderer.shared, buffer, rtv)
        .map_err(|error| renderer.classify(error))?;
    renderer.stream.retire(renderer.last_sequence, old);
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
            replace_swapchain(renderer, buffer).map_err(|error| renderer.classify(error))?;
        }
        Err(error) => {
            return Err(renderer.classify(EngineError::Swapchain {
                detail: smol_str::format_smolstr!(
                    "ResizeBuffers({}x{}): {:#010X} {}",
                    buffer.0,
                    buffer.1,
                    error.code().0 as u32,
                    error.code().message()
                ),
            }));
        }
    }
    renderer.buffer = buffer;
    renderer.surface = content;
    renderer.target.on_target_created();
    renderer.buffer_syncs.invalidate();
    Ok(())
}

pub(super) fn rebuild_target(renderer: &mut GpuRenderer) -> Result<bool, EngineError> {
    if renderer.swapchain.is_none() {
        return Ok(true);
    }
    debug_assert!(
        on_gpu_thread(),
        "the composition target may only be rebuilt on the kytrix-d3d12 thread"
    );
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
    let rtv = rtv_handle(renderer.shared.raw(), &renderer.rtv_heap, 0);
    let old = renderer
        .persist
        .recreate(&renderer.shared, buffer, rtv)
        .map_err(|error| renderer.classify(error))?;
    renderer.stream.retire(renderer.last_sequence, old);
    replace_swapchain(renderer, buffer).map_err(|error| renderer.classify(error))?;
    renderer.buffer = buffer;
    renderer.target.on_target_created();
    renderer.buffer_syncs.invalidate();
    Ok(true)
}

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
    renderer.prime_detached(&mut swapchain)?;
    {
        let target = composition::lock(&composition);
        target.set_content(swapchain.raw())?;
        target.commit()?;
    }
    let _old = renderer.swapchain.replace(swapchain);
    Ok(())
}

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
    let _old_background = renderer.background_swapchain.replace(swapchain);
    renderer.background_dirty = false;
    Ok(())
}

pub(super) fn rtv_handle(
    device: &ID3D12Device,
    heap: &ID3D12DescriptorHeap,
    index: u32,
) -> D3D12_CPU_DESCRIPTOR_HANDLE {
    let size = unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) };
    let start = unsafe { heap.GetCPUDescriptorHandleForHeapStart() };
    D3D12_CPU_DESCRIPTOR_HANDLE {
        ptr: start.ptr + index as usize * size as usize,
    }
}

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
    unsafe { device.CreateRenderTargetView(resource, Some(&desc), handle) };
}

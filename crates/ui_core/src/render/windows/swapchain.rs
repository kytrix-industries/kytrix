use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, RECT},
        Graphics::{
            Direct3D12::*,
            Dxgi::{
                Common::{
                    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
                },
                *,
            },
        },
    },
    core::Interface,
};

use super::hresult::{DXGI_ERROR_INVALID_CALL, PresentOutcome, classify_present};
use crate::{
    error::EngineError,
    render::{compositor::DamageRects, damage::MAX_DAMAGE_RECTS, windows::device::init_error_from},
};

pub(crate) const BUFFER_COUNT: u32 = 3;

pub(crate) struct Swapchain {
    raw: IDXGISwapChain1,
    waitable: HANDLE,
    buffer: (u32, u32),
    rtv_heap: ID3D12DescriptorHeap,
    rtv_size: u32,
}

impl Swapchain {
    pub(crate) fn for_composition(
        device: &ID3D12Device,
        queue: &ID3D12CommandQueue,
        buffer: (u32, u32),
    ) -> Result<Self, EngineError> {
        let factory = create_factory()?;
        let desc = composition_desc(buffer);
        let raw: IDXGISwapChain1 =
            unsafe { factory.CreateSwapChainForComposition(queue, &desc, None) }.map_err(
                |error| EngineError::Swapchain {
                    detail: smol_str::format_smolstr!(
                        "CreateSwapChainForComposition({}x{}): {:#010X}",
                        desc.Width,
                        desc.Height,
                        error.code().0 as u32
                    ),
                },
            )?;
        let waitable = arm_frame_latency(&raw)?;
        apply_color_space(&raw)?;
        let rtv_heap = create_rtv_heap(device)?;
        let rtv_size =
            unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) };
        let chain = Self {
            raw,
            waitable,
            buffer: (desc.Width.max(1), desc.Height.max(1)),
            rtv_heap,
            rtv_size,
        };
        chain.write_back_buffer_rtvs(device)?;
        Ok(chain)
    }

    pub(crate) fn waitable(&self) -> HANDLE {
        self.waitable
    }

    pub(crate) fn frame_statistics(&self) -> Option<DXGI_FRAME_STATISTICS> {
        let mut stats = DXGI_FRAME_STATISTICS::default();
        unsafe { self.raw.GetFrameStatistics(&mut stats) }.ok()?;
        Some(stats)
    }

    pub(crate) fn raw(&self) -> &IDXGISwapChain1 {
        &self.raw
    }

    pub(crate) fn buffer(&self) -> (u32, u32) {
        self.buffer
    }

    pub(crate) fn back_buffer(&self) -> Result<(u32, ID3D12Resource), EngineError> {
        let chain: IDXGISwapChain3 = self
            .raw
            .cast()
            .map_err(|error| init_error_from("cast IDXGISwapChain3", error))?;
        let index = unsafe { chain.GetCurrentBackBufferIndex() };
        let resource =
            unsafe { self.raw.GetBuffer(index) }.map_err(|error| EngineError::Swapchain {
                detail: smol_str::format_smolstr!(
                    "GetBuffer({index}): {:#010X}",
                    error.code().0 as u32
                ),
            })?;
        Ok((index, resource))
    }

    pub(crate) fn rtv(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        debug_assert!(index < BUFFER_COUNT, "back buffer index out of range");
        let base = unsafe { self.rtv_heap.GetCPUDescriptorHandleForHeapStart() };
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: base.ptr + (u64::from(self.rtv_size) * u64::from(index)) as usize,
        }
    }

    pub(crate) fn resize(
        &mut self,
        device: &ID3D12Device,
        buffer: (u32, u32),
    ) -> Result<(), windows::core::Error> {
        let buffer = (buffer.0.max(1), buffer.1.max(1));
        if buffer == self.buffer {
            return Ok(());
        }
        unsafe {
            self.raw.ResizeBuffers(
                BUFFER_COUNT,
                buffer.0,
                buffer.1,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
            )
        }?;
        self.buffer = buffer;
        let waitable = arm_frame_latency(&self.raw).map_err(|error| match error {
            EngineError::Swapchain { detail } => windows::core::Error::new(
                DXGI_ERROR_INVALID_CALL,
                smol_str::format_smolstr!("frame latency waitable after resize: {detail}"),
            ),
            other => windows::core::Error::new(
                DXGI_ERROR_INVALID_CALL,
                smol_str::format_smolstr!("frame latency waitable after resize: {other}"),
            ),
        })?;
        if waitable != self.waitable {
            unsafe {
                let _ = CloseHandle(self.waitable);
            }
            self.waitable = waitable;
        }
        self.write_back_buffer_rtvs(device).map_err(|error| {
            windows::core::Error::new(
                DXGI_ERROR_INVALID_CALL,
                smol_str::format_smolstr!("back buffer RTVs after resize: {error}"),
            )
        })?;
        Ok(())
    }

    pub(crate) fn present1(&mut self, damage: Option<&DamageRects>, sync: u32) -> PresentOutcome {
        let mut rects = [RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        }; MAX_DAMAGE_RECTS];
        let count = match damage {
            Some(damage) if !damage.is_empty() => fill_rects(damage, &mut rects),
            _ => 0,
        };
        let params = DXGI_PRESENT_PARAMETERS {
            DirtyRectsCount: count,
            pDirtyRects: if count == 0 {
                std::ptr::null_mut()
            } else {
                rects.as_mut_ptr()
            },
            pScrollRect: std::ptr::null_mut(),
            pScrollOffset: std::ptr::null_mut(),
        };
        let hr = unsafe { self.raw.Present1(sync, DXGI_PRESENT(0), &params) };
        classify_present(hr)
    }

    fn write_back_buffer_rtvs(&self, device: &ID3D12Device) -> Result<(), EngineError> {
        let desc = D3D12_RENDER_TARGET_VIEW_DESC {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
            ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_RENDER_TARGET_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_RTV {
                    MipSlice: 0,
                    PlaneSlice: 0,
                },
            },
        };
        for index in 0..BUFFER_COUNT {
            let resource: ID3D12Resource =
                unsafe { self.raw.GetBuffer(index) }.map_err(|error| EngineError::Swapchain {
                    detail: smol_str::format_smolstr!(
                        "GetBuffer({index}) for RTVs: {:#010X}",
                        error.code().0 as u32
                    ),
                })?;
            unsafe { device.CreateRenderTargetView(&resource, Some(&desc), self.rtv(index)) };
            drop(resource);
        }
        Ok(())
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.waitable);
        }
    }
}

fn arm_frame_latency(raw: &IDXGISwapChain1) -> Result<HANDLE, EngineError> {
    let chain2: IDXGISwapChain2 = raw
        .cast()
        .map_err(|error| init_error_from("cast IDXGISwapChain2", error))?;
    unsafe { chain2.SetMaximumFrameLatency(1) }
        .map_err(|error| init_error_from("SetMaximumFrameLatency", error))?;
    let waitable = unsafe { chain2.GetFrameLatencyWaitableObject() };
    if waitable.is_invalid() {
        return Err(EngineError::Swapchain {
            detail: smol_str::SmolStr::new_static(
                "GetFrameLatencyWaitableObject returned a null handle",
            ),
        });
    }
    Ok(waitable)
}

fn apply_color_space(raw: &IDXGISwapChain1) -> Result<(), EngineError> {
    let chain3: IDXGISwapChain3 = raw
        .cast()
        .map_err(|error| init_error_from("cast IDXGISwapChain3", error))?;
    let color_space = crate::render::colorspace::dxgi_color_space();
    let support = unsafe { chain3.CheckColorSpaceSupport(color_space) }
        .map_err(|error| init_error_from("CheckColorSpaceSupport", error))?;
    if crate::render::colorspace::support_allows_present(support) {
        unsafe { chain3.SetColorSpace1(color_space) }.map_err(|error| EngineError::Swapchain {
            detail: smol_str::format_smolstr!("SetColorSpace1: {:#010X}", error.code().0 as u32),
        })?;
    }
    Ok(())
}

fn create_rtv_heap(device: &ID3D12Device) -> Result<ID3D12DescriptorHeap, EngineError> {
    unsafe {
        device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
            NumDescriptors: BUFFER_COUNT,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            NodeMask: 0,
        })
    }
    .map_err(|error| init_error_from("CreateDescriptorHeap(back buffer RTVs)", error))
}

fn composition_desc(size: (u32, u32)) -> DXGI_SWAP_CHAIN_DESC1 {
    DXGI_SWAP_CHAIN_DESC1 {
        Width: size.0.max(1),
        Height: size.1.max(1),
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: BUFFER_COUNT,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
    }
}

fn create_factory() -> Result<IDXGIFactory4, EngineError> {
    unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
        .map_err(|error| init_error_from("CreateDXGIFactory2", error))
}

fn fill_rects(damage: &DamageRects, out: &mut [RECT; MAX_DAMAGE_RECTS]) -> u32 {
    let mut count = 0;
    for rect in damage.as_slice() {
        out[count] = RECT {
            left: rect.x0,
            top: rect.y0,
            right: rect.x1,
            bottom: rect.y1,
        };
        count += 1;
    }
    count as u32
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::Dxgi::{
        Common::{DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM},
        DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
        DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    };

    use super::{BUFFER_COUNT, composition_desc, fill_rects};
    use crate::{
        math::PhysicalRect,
        render::{compositor::DamageRects, damage::MAX_DAMAGE_RECTS},
    };

    fn rects() -> [windows::Win32::Foundation::RECT; MAX_DAMAGE_RECTS] {
        [windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        }; MAX_DAMAGE_RECTS]
    }

    #[test]
    fn the_composition_desc_is_stretch_flip_sequential_with_three_buffers() {
        let desc = composition_desc((1000, 700));
        assert_eq!(desc.Scaling, DXGI_SCALING_STRETCH);
        assert_eq!(desc.SwapEffect, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL);
        assert_eq!(desc.BufferCount, BUFFER_COUNT);
        assert!(desc.BufferCount >= 2);
        assert_eq!(desc.Width, 1000);
        assert_eq!(desc.Height, 700);
        assert_eq!(desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM);
        assert_eq!(desc.SampleDesc.Count, 1);
        assert_eq!(
            desc.AlphaMode, DXGI_ALPHA_MODE_IGNORE,
            "组合交换链上没有可用的 SetBackgroundColor，窗口不透明"
        );
        assert_eq!(
            desc.Flags, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
            "waitable 是呈现节奏的唯一原语（SetMaximumFrameLatency 需要它）"
        );
    }

    #[test]
    fn the_desc_never_asks_for_a_zero_sized_buffer() {
        let desc = composition_desc((0, 0));
        assert_eq!((desc.Width, desc.Height), (1, 1));
    }

    #[test]
    fn dirty_rects_map_to_win32_rects_in_order() {
        let mut damage = DamageRects::new();
        assert!(damage.push(PhysicalRect::new(0, 0, 32, 16)));
        assert!(damage.push(PhysicalRect::new(32, 16, 64, 32)));
        let mut out = rects();
        assert_eq!(fill_rects(&damage, &mut out), 2);
        assert_eq!(
            (out[0].left, out[0].top, out[0].right, out[0].bottom),
            (0, 0, 32, 16)
        );
        assert_eq!(
            (out[1].left, out[1].top, out[1].right, out[1].bottom),
            (32, 16, 64, 32)
        );
    }

    #[test]
    fn the_swapchain_never_caches_a_back_buffer_reference() {
        let source = include_str!("swapchain.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default();
        assert!(!source.contains("back: ID3D12Resource"));
        assert!(!source.contains("buffers: [ID3D12Resource"));
        assert!(
            source.contains("self.raw.GetBuffer(index)"),
            "back buffer 必须在每次需要时向 DXGI 重新获取"
        );
        assert!(
            source.contains("ResizeBuffers"),
            "尺寸变化必须走 ResizeBuffers（R08-1）"
        );
    }
}

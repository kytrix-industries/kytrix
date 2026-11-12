//! offscreen → readback heap → CPU（`design/phase-08-d3d12.md` §5.7 / §7.7）。
//!
//! 只在 test / `test-support` 构建里存在（像素快照是 L3 的测试面；发布物不带读回路径）。
//! 读回的字节是**资源格式** `B8G8R8A8_UNORM` 里已编码好的 sRGB 值（DEVIATIONS F-80）。

use windows::Win32::Graphics::{
    Direct3D12::*,
    Dxgi::Common::{
        DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
    },
};

use crate::{
    error::EngineError,
    render::windows::{
        device::{Device, init_error_from},
        queue::Queue,
    },
};

/// 一次读回的 RGBA8 图像（BGRA→RGBA 已交换；alpha 是预乘值）。
pub(crate) struct ReadbackImage {
    pub width: u32,
    pub height: u32,
    pub rgba8: Vec<u8>,
}

/// 读回缓冲（按 surface 尺寸复用）。
pub(crate) struct Readback {
    buffer: Option<ID3D12Resource>,
    size: (u32, u32),
    row_pitch: u32,
    total: u64,
}

impl Readback {
    pub(crate) const fn new() -> Self {
        Self {
            buffer: None,
            size: (0, 0),
            row_pitch: 0,
            total: 0,
        }
    }

    /// 把 `source`（持久 offscreen，状态 `RENDER_TARGET`）读回 CPU。
    pub(crate) fn capture(
        &mut self,
        device: &Device,
        queue: &mut Queue,
        source: &ID3D12Resource,
        size: (u32, u32),
    ) -> Result<ReadbackImage, EngineError> {
        self.capture_from(
            device,
            queue,
            source,
            size,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        )
    }

    /// 指定源资源的出入状态（图集等非 RT 资源用）。
    pub(crate) fn capture_from(
        &mut self,
        device: &Device,
        queue: &mut Queue,
        source: &ID3D12Resource,
        size: (u32, u32),
        format: DXGI_FORMAT,
        resident: D3D12_RESOURCE_STATES,
    ) -> Result<ReadbackImage, EngineError> {
        if self.buffer.is_none() || self.size != size {
            self.recreate(device, size, format)?;
        }
        let buffer = self.buffer.as_ref().ok_or_else(|| EngineError::AppInit {
            detail: smol_str::SmolStr::new_static("readback buffer missing"),
        })?;
        let row_pitch = self.row_pitch;
        let placed = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: format,
                Width: size.0,
                Height: size.1,
                Depth: 1,
                RowPitch: row_pitch,
            },
        };

        queue.submit_immediate(|list| {
            transition(list, source, resident, D3D12_RESOURCE_STATE_COPY_SOURCE);
            let mut destination = D3D12_TEXTURE_COPY_LOCATION {
                pResource: core::mem::ManuallyDrop::new(Some(buffer.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: placed,
                },
            };
            let mut source_location = D3D12_TEXTURE_COPY_LOCATION {
                pResource: core::mem::ManuallyDrop::new(Some(source.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            // SAFETY: footprint 由 `GetCopyableFootprints` 计算，源纹理处于 COPY_SOURCE。
            unsafe { list.CopyTextureRegion(&destination, 0, 0, 0, &source_location, None) };
            // SAFETY: `pResource` 是 `ManuallyDrop`，结构体 drop 不释放 COM 引用（DEVIATIONS F-86）。
            unsafe {
                core::mem::ManuallyDrop::drop(&mut destination.pResource);
                core::mem::ManuallyDrop::drop(&mut source_location.pResource);
            }
            transition(list, source, D3D12_RESOURCE_STATE_COPY_SOURCE, resident);
            Ok(())
        })?;

        let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
        unsafe { buffer.Map(0, None, Some(&mut pointer)) }
            .map_err(|error| init_error_from("Map(readback)", error))?;
        let mut rgba8 = Vec::with_capacity((size.0 * size.1 * 4) as usize);
        let single_channel = matches!(
            format,
            windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8_UNORM
        );
        // SAFETY: buffer 容量 = row_pitch * height，读取范围在映射区内。
        unsafe {
            for y in 0..size.1 {
                let row = pointer.cast::<u8>().add((y * row_pitch) as usize);
                if single_channel {
                    for x in 0..size.0 {
                        let value = *row.add(x as usize);
                        rgba8.extend_from_slice(&[value, 0, 0, 255]);
                    }
                } else {
                    for x in 0..size.0 {
                        let pixel = row.add((x * 4) as usize);
                        rgba8.extend_from_slice(&[
                            *pixel.add(2),
                            *pixel.add(1),
                            *pixel,
                            *pixel.add(3),
                        ]);
                    }
                }
            }
        }
        unsafe { buffer.Unmap(0, None) };
        Ok(ReadbackImage {
            width: size.0,
            height: size.1,
            rgba8,
        })
    }

    fn recreate(
        &mut self,
        device: &Device,
        size: (u32, u32),
        format: DXGI_FORMAT,
    ) -> Result<(), EngineError> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(size.0),
            Height: size.1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            ..D3D12_RESOURCE_DESC::default()
        };

        let mut placed = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut rows = 0u32;
        let mut row_size = 0u64;
        let mut total = 0u64;
        unsafe {
            device.raw().GetCopyableFootprints(
                &desc,
                0,
                1,
                0,
                Some(&mut placed),
                Some(&mut rows),
                Some(&mut row_size),
                Some(&mut total),
            )
        };

        let buffer_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: total,
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
            Type: D3D12_HEAP_TYPE_READBACK,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
            MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
        };
        let mut out: Option<ID3D12Resource> = None;
        unsafe {
            device.raw().CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &buffer_desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut out,
            )
        }
        .map_err(|error| init_error_from("CreateCommittedResource(readback)", error))?;

        self.buffer = out;
        self.size = size;
        self.row_pitch = placed.Footprint.RowPitch;
        self.total = total;
        let _ = (rows, row_size);
        Ok(())
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
    // SAFETY: union 载荷与 `pResource` 都是 `ManuallyDrop`，结构体 drop 不释放 COM 引用（F-86）。
    unsafe {
        let transition: &mut D3D12_RESOURCE_TRANSITION_BARRIER = &mut barrier.Anonymous.Transition;
        core::mem::ManuallyDrop::drop(&mut transition.pResource);
    }
}

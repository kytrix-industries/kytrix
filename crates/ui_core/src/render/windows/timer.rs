use windows::Win32::Graphics::{
    Direct3D12::*,
    Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC},
};

use crate::{
    error::EngineError,
    render::{
        frame::MAX_FRAMES_IN_FLIGHT,
        timer::{GpuTimerSample, Section},
        windows::{
            device::{SharedGpu, init_error_from},
            queue::CommandStream,
        },
    },
};

const TIMESTAMPS_PER_FRAME: u32 = 2;

pub(crate) struct GpuTimer {
    heap: ID3D12QueryHeap,
    resolve: Vec<ID3D12Resource>,
    frequency: u64,
}

impl GpuTimer {
    pub(crate) fn new(
        shared: &SharedGpu,
        stream: &CommandStream,
    ) -> Result<Option<Self>, EngineError> {
        let Ok(frequency) = stream.timestamp_frequency() else {
            return Ok(None);
        };
        let mut heap_out: Option<ID3D12QueryHeap> = None;
        if unsafe {
            shared.raw().CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                    Count: TIMESTAMPS_PER_FRAME * MAX_FRAMES_IN_FLIGHT as u32,
                    NodeMask: 0,
                },
                &mut heap_out,
            )
        }
        .is_err()
        {
            return Ok(None);
        }
        let Some(heap) = heap_out else {
            return Ok(None);
        };

        let mut resolve = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            resolve.push(create_readback(shared.raw(), 16)?);
        }
        Ok(Some(Self {
            heap,
            resolve,
            frequency,
        }))
    }

    pub(crate) fn begin(&self, list: &ID3D12GraphicsCommandList, slot: usize) {
        unsafe {
            list.EndQuery(
                &self.heap,
                D3D12_QUERY_TYPE_TIMESTAMP,
                (slot as u32) * TIMESTAMPS_PER_FRAME,
            )
        };
    }

    pub(crate) fn end(&self, list: &ID3D12GraphicsCommandList, slot: usize) {
        let index = (slot as u32) * TIMESTAMPS_PER_FRAME + 1;
        let resolve = &self.resolve[slot];
        unsafe {
            list.EndQuery(&self.heap, D3D12_QUERY_TYPE_TIMESTAMP, index);
            list.ResolveQueryData(
                &self.heap,
                D3D12_QUERY_TYPE_TIMESTAMP,
                (slot as u32) * TIMESTAMPS_PER_FRAME,
                TIMESTAMPS_PER_FRAME,
                resolve,
                0,
            );
        }
    }

    pub(crate) fn resolve(&self, slot: usize) -> Option<GpuTimerSample> {
        let buffer = &self.resolve[slot];
        let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
        unsafe { buffer.Map(0, None, Some(&mut pointer)) }.ok()?;
        let (start, end) = unsafe {
            let base = pointer.cast::<u64>();
            (*base, *base.add(1))
        };
        unsafe { buffer.Unmap(0, None) };
        let ticks = end.wrapping_sub(start);
        Some(GpuTimerSample {
            section: Section::Gpu,
            cpu_ns: 0,
            gpu_ns: (ticks as u128 * 1_000_000_000u128 / u128::from(self.frequency)) as u64,
        })
    }
}

fn create_readback(device: &ID3D12Device, bytes: u64) -> Result<ID3D12Resource, EngineError> {
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
        Type: D3D12_HEAP_TYPE_READBACK,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    };
    let mut out: Option<ID3D12Resource> = None;
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut out,
        )
    }
    .map_err(|error| init_error_from("CreateCommittedResource(timestamp)", error))?;
    out.ok_or_else(|| EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("d3d12 timestamp readback buffer missing"),
    })
}

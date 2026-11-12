//! 图集纹理 + 脏行上传（`design/phase-08-d3d12.md` §5.5；格式与 UV 约定见 P02 §10）。
//!
//! 字形图集是 `R8_UNORM`（线性覆盖率），图片图集是 `B8G8R8A8_UNORM_SRGB`；两者都是 2048×2048、
//! 单 mip、无 MSAA。上传按**行区间**（`DirtyRows`）走每 in-flight 槽独立 staging 的
//! `CopyTextureRegion`。
//!
//! P08 没有 CPU 像素生产者（P06 的字形缓存 / P15 的图片缓存才有）：真正的入队入口是
//! test/CI 可见的 [`AtlasTextures::queue_upload`]；生产构建只保留录制路径
//! （[`AtlasTextures::record_uploads`]），像素源接上后 additive 打开。

use windows::Win32::Graphics::{
    Direct3D12::*,
    Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_UNKNOWN,
        DXGI_SAMPLE_DESC,
    },
};

use crate::{
    error::EngineError,
    paint::atlas::AtlasKind,
    render::{
        atlas::DirtyRows,
        frame::MAX_FRAMES_IN_FLIGHT,
        windows::device::{Device, init_error_from},
    },
};

/// 图集页尺寸（P02 §10.3：2048×2048）。
pub(crate) const ATLAS_PAGE: u32 = 2048;

/// staging 的**初始**行数（需要更大时按本次上传的行数扩容）。
///
/// 一次 `record_uploads` 里的多条 `CopyTextureRegion` 都在同一提交、同一个 staging 上，
/// **不能分块复用同一个缓冲**（后面的 CPU 写入会覆盖前面的内容，GPU 执行时读到的全是最后一块
/// —— V-64 的根因）。因此一次上传的每一行都必须在 staging 里各有位置。
const STAGING_ROWS: u32 = 256;

const PITCH_ALIGNMENT: u32 = 256;

/// 一张图集纹理 + 每 in-flight 槽的 staging。
struct AtlasTexture {
    resource: ID3D12Resource,
    /// 每槽一块 staging；容量按**本次上传的最大行数**增长（见 [`STAGING_ROWS`]）。
    staging: Vec<ID3D12Resource>,
    /// 当前 staging 能容纳的行数。
    staging_rows: u32,
    page: u32,
    row_pitch: u32,
    texel_bytes: u32,
}

/// 待上传的一页内容（整页 `pixels` + 脏行区间）。
struct PendingUpload {
    pixels: Vec<u8>,
    rows: DirtyRows,
}

/// 两种图集（字形 / 图片）。
pub(crate) struct AtlasTextures {
    glyph: AtlasTexture,
    image: AtlasTexture,
    pending: [Option<PendingUpload>; 2],
}

impl AtlasTextures {
    /// `page` 在产品路径恒为 [`ATLAS_PAGE`]；测试用更小的页（2048² 的纹理在 WARP 上代价过高）。
    pub(crate) fn new(device: &Device, page: u32) -> Result<Self, EngineError> {
        Ok(Self {
            glyph: AtlasTexture::new(device, AtlasKind::Glyph, page)?,
            image: AtlasTexture::new(device, AtlasKind::Image, page)?,
            pending: [None, None],
        })
    }

    pub(crate) fn texture(&self, kind: AtlasKind) -> &ID3D12Resource {
        match kind {
            AtlasKind::Glyph => &self.glyph.resource,
            AtlasKind::Image => &self.image.resource,
        }
    }

    /// 本帧是否有图集上传（诊断 / 测试）。
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.iter().any(Option::is_some)
    }

    /// 入队一页的脏行（整页 `pixels` 的布局 = `page * page * texel`）。
    pub(crate) fn queue_upload(&mut self, kind: AtlasKind, pixels: &[u8], rows: DirtyRows) {
        let page = match kind {
            AtlasKind::Glyph => self.glyph.page,
            AtlasKind::Image => self.image.page,
        };
        let expected = (page * page) as usize * texel_bytes(kind) as usize;
        debug_assert_eq!(pixels.len(), expected, "atlas page size mismatch");
        let slot = match kind {
            AtlasKind::Glyph => 0,
            AtlasKind::Image => 1,
        };
        self.pending[slot] = Some(PendingUpload {
            pixels: pixels.to_vec(),
            rows,
        });
    }

    /// 在本帧命令列表里录制「staging 写入 + 逐区间 `CopyTextureRegion`」。
    pub(crate) fn record_uploads(
        &mut self,
        device: &Device,
        list: &ID3D12GraphicsCommandList,
        slot: usize,
    ) -> Result<(), EngineError> {
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = match kind {
                AtlasKind::Glyph => 0,
                AtlasKind::Image => 1,
            };
            let Some(pending) = self.pending[index].take() else {
                continue;
            };
            let texture = match kind {
                AtlasKind::Glyph => &mut self.glyph,
                AtlasKind::Image => &mut self.image,
            };
            texture.upload(device, list, slot, kind, &pending)?;
        }
        Ok(())
    }
}

impl AtlasTexture {
    fn new(device: &Device, kind: AtlasKind, page: u32) -> Result<Self, EngineError> {
        let format = texture_format(kind);
        let texel = texel_bytes(kind);
        let row_pitch = align_up(page * texel, PITCH_ALIGNMENT);

        let mut staging = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            staging.push(create_buffer(
                device,
                u64::from(row_pitch) * u64::from(STAGING_ROWS),
                D3D12_HEAP_TYPE_UPLOAD,
            )?);
        }
        let resource = create_texture(device, page, page, format)?;
        Ok(Self {
            resource,
            staging,
            staging_rows: STAGING_ROWS,
            page,
            row_pitch,
            texel_bytes: texel,
        })
    }

    fn upload(
        &mut self,
        device: &Device,
        list: &ID3D12GraphicsCommandList,
        slot: usize,
        kind: AtlasKind,
        pending: &PendingUpload,
    ) -> Result<(), EngineError> {
        let resident = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
        barrier(
            list,
            &self.resource,
            resident,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );

        let full = 0..self.page;
        let spans: &[std::ops::Range<u32>] = if pending.rows.is_full() {
            std::slice::from_ref(&full)
        } else {
            pending.rows.spans()
        };
        // 每一段必须在 staging 里**各有位置**：同一 staging 上先写后画的多次拷贝会在 GPU 执行时
        // 读到最后一块的字节（V-64）。
        for span in spans {
            let rows = span.end - span.start;
            self.ensure_staging(device, slot, rows)?;
            self.upload_chunk(list, slot, kind, pending, span.start, rows)?;
        }

        barrier(
            list,
            &self.resource,
            D3D12_RESOURCE_STATE_COPY_DEST,
            resident,
        );
        Ok(())
    }

    /// 保证第 `slot` 个 staging 能容纳 `rows` 行（不够就重建；一次性扩容，之后复用）。
    fn ensure_staging(
        &mut self,
        device: &Device,
        slot: usize,
        rows: u32,
    ) -> Result<(), EngineError> {
        if rows <= self.staging_rows {
            return Ok(());
        }
        let bytes = u64::from(self.row_pitch) * u64::from(rows);
        let staging = create_buffer(device, bytes, D3D12_HEAP_TYPE_UPLOAD)?;
        self.staging[slot] = staging;
        self.staging_rows = rows;
        Ok(())
    }

    fn upload_chunk(
        &self,
        list: &ID3D12GraphicsCommandList,
        slot: usize,
        kind: AtlasKind,
        pending: &PendingUpload,
        start: u32,
        rows: u32,
    ) -> Result<(), EngineError> {
        let staging = &self.staging[slot];
        let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
        unsafe { staging.Map(0, None, Some(&mut pointer)) }
            .map_err(|error| init_error_from("Map(atlas staging)", error))?;
        // SAFETY: staging 容量 = row_pitch * STAGING_ROWS，`rows <= STAGING_ROWS`；
        // 源页由 `queue_upload` 的 debug_assert 保证尺寸。
        unsafe {
            for row in 0..rows {
                let source_row = start + row;
                let source = pending
                    .pixels
                    .as_ptr()
                    .add((source_row * self.page * self.texel_bytes) as usize);
                let destination = pointer.cast::<u8>().add((row * self.row_pitch) as usize);
                std::ptr::copy_nonoverlapping(
                    source,
                    destination,
                    (self.page * self.texel_bytes) as usize,
                );
            }
        }
        unsafe { staging.Unmap(0, None) };

        let placed = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: texture_format(kind),
                Width: self.page,
                Height: rows,
                Depth: 1,
                RowPitch: self.row_pitch,
            },
        };
        let mut destination = D3D12_TEXTURE_COPY_LOCATION {
            pResource: core::mem::ManuallyDrop::new(Some(self.resource.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        let mut source = D3D12_TEXTURE_COPY_LOCATION {
            pResource: core::mem::ManuallyDrop::new(Some(staging.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: placed,
            },
        };
        // SAFETY: 两个 location 与 `placed` 在调用期间有效，行区间已由 `DirtyRows` 保证在页内。
        unsafe { list.CopyTextureRegion(&destination, 0, start, 0, &source, None) };
        // SAFETY: `pResource` 是 `ManuallyDrop`，结构体 drop 不释放 COM 引用（DEVIATIONS F-86）。
        unsafe {
            core::mem::ManuallyDrop::drop(&mut destination.pResource);
            core::mem::ManuallyDrop::drop(&mut source.pResource);
        }
        Ok(())
    }
}

fn barrier(
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

fn texture_format(kind: AtlasKind) -> windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT {
    match kind {
        AtlasKind::Glyph => DXGI_FORMAT_R8_UNORM,
        AtlasKind::Image => DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
    }
}

fn texel_bytes(kind: AtlasKind) -> u32 {
    match kind {
        AtlasKind::Glyph => 1,
        AtlasKind::Image => 4,
    }
}

fn align_up(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment) * alignment
}

fn create_texture(
    device: &Device,
    width: u32,
    height: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> Result<ID3D12Resource, EngineError> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(width),
        Height: height,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_NONE,
        ..D3D12_RESOURCE_DESC::default()
    };
    create_resource(
        device,
        &desc,
        D3D12_HEAP_TYPE_DEFAULT,
        D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
    )
}

fn create_buffer(
    device: &Device,
    bytes: u64,
    heap_type: D3D12_HEAP_TYPE,
) -> Result<ID3D12Resource, EngineError> {
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
    create_resource(device, &desc, heap_type, D3D12_RESOURCE_STATE_GENERIC_READ)
}

fn create_resource(
    device: &Device,
    desc: &D3D12_RESOURCE_DESC,
    heap_type: D3D12_HEAP_TYPE,
    initial: D3D12_RESOURCE_STATES,
) -> Result<ID3D12Resource, EngineError> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: heap_type,
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
            desc,
            initial,
            None,
            &mut out,
        )
    }
    .map_err(|error| init_error_from("CreateCommittedResource", error))?;
    out.ok_or_else(|| EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("d3d12 CreateCommittedResource returned no resource"),
    })
}

#[cfg(test)]
mod tests {
    use super::{ATLAS_PAGE, AtlasTextures, align_up};

    /// 测试页：2048² 的纹理在 WARP 上代价过高（且会拖垮并行的 GPU 测试）。
    const TEST_PAGE: u32 = 64;
    use crate::{
        paint::atlas::AtlasKind,
        render::{atlas::DirtyRows, windows::device::Device},
    };

    #[test]
    fn row_pitch_is_256_aligned_for_both_texels() {
        assert_eq!(align_up(ATLAS_PAGE, 256), 2048);
        assert_eq!(align_up(ATLAS_PAGE * 4, 256), 8192);
    }

    #[test]
    fn dirty_rows_upload_only_touches_those_rows() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;

        use crate::render::windows::{queue::Queue, readback::Readback};

        let device = Device::new_warp().expect("WARP device");
        let mut queue = Queue::new(&device).expect("queue");
        let mut atlas = AtlasTextures::new(&device, TEST_PAGE).expect("atlas");

        let mut page = vec![0u8; (TEST_PAGE * TEST_PAGE) as usize];
        let row = TEST_PAGE as usize * 5;
        page[row..row + TEST_PAGE as usize].fill(0xAB);
        let mut rows = DirtyRows::default();
        rows.mark(5, 6);
        atlas.queue_upload(AtlasKind::Glyph, &page, rows);
        assert!(atlas.has_pending());

        queue
            .submit_immediate(|list| atlas.record_uploads(&device, list, 0))
            .expect("upload");
        assert!(
            !atlas.has_pending(),
            "pending must be consumed by the frame"
        );

        let mut readback = Readback::new();
        let image = readback
            .capture_from(
                &device,
                &mut queue,
                atlas.texture(AtlasKind::Glyph),
                (TEST_PAGE, TEST_PAGE),
                windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8_UNORM,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            )
            .expect("readback");
        // R8 覆盖率被读回成 RGBA（R = 覆盖率，A = 255）。
        assert_eq!(image.rgba8[(5 * TEST_PAGE * 4) as usize], 0xAB);
        assert_eq!(image.rgba8[(5 * TEST_PAGE * 4 + 3) as usize], 0xFF);
        assert_eq!(image.rgba8[(4 * TEST_PAGE * 4) as usize], 0);
        assert_eq!(image.rgba8[(6 * TEST_PAGE * 4) as usize], 0);
    }

    #[test]
    fn queueing_a_dirty_span_marks_pending() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let device = Device::new_warp().expect("WARP device");
        let mut atlas = AtlasTextures::new(&device, TEST_PAGE).expect("atlas");
        assert!(!atlas.has_pending());

        let mut rows = DirtyRows::default();
        rows.mark(4, 6);
        let page = vec![0x7Fu8; (TEST_PAGE * TEST_PAGE) as usize];
        atlas.queue_upload(AtlasKind::Glyph, &page, rows);
        assert!(atlas.has_pending());
    }
}

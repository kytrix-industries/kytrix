//! 图集纹理 + 脏行上传（`design/phase-08-d3d12.md` §5.5；格式与 UV 约定见 P02 §10）。
//!
//! 字形图集是 `R8_UNORM`（线性覆盖率），图片图集是 `B8G8R8A8_UNORM_SRGB`；两者都是**懒分配**：
//! 初始化只建 1×1 占位纹理（描述符始终有效），首次上传时才按页尺寸建真实纹理；页尺寸变化
//! （字形页换代扩容 / 首次图片页）会重建纹理并 retire 旧资源（DEVIATIONS F-208 / S-153）。
//! staging 有两档：**每槽常驻**一小块（≤ 1 MiB，增量上传复用）与**一次性**大块（整页上传，
//! 录制后按帧 retire）。常驻容量因此不随图集页尺寸放大（DEVIATIONS F-216）。
//!
//! 页尺寸随上传**显式过线**（S-153），不由 `pixels.len()` 反推：增量载荷只有脏行，长度
//! 不足以恢复整页尺寸。

use std::ops::Range;

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
        atlas::{AtlasRebuilt, DirtyRows, MAX_RESIDENT_STAGING_BYTES, texel_bytes},
        frame::MAX_FRAMES_IN_FLIGHT,
        retire::GpuResourceHandle,
        windows::{
            device::{Device, init_error_from},
            queue::Queue,
        },
    },
};

/// 初始化时的占位页（1×1）：让 SRV 在第一次上传之前也指向有效资源。
const PLACEHOLDER_PAGE: u32 = 1;

/// staging 的**初始**行数（需要更大时按本次上传的行数扩容）。
///
/// 一次 `record_uploads` 里的多条 `CopyTextureRegion` 都在同一提交、同一个 staging 上，
/// **不能分块复用同一个缓冲**（后面的 CPU 写入会覆盖前面的内容，GPU 执行时读到的全是最后一块
/// —— V-64 的根因）。因此一次上传的每一行都必须在 staging 里各有位置：容量按**本次上传的
/// 总行数**保证，各段按累计行偏移落位。
const STAGING_ROWS: u32 = 256;

const PITCH_ALIGNMENT: u32 = 256;

/// 每 in-flight 槽一块 staging（页变化时整组重建）。
struct StagingSlot {
    buffer: ID3D12Resource,
    handle: GpuResourceHandle,
    /// 该槽当前的 staging 行数容量。
    ///
    /// 必须是按槽的数组语义：只记一个全局值会让「槽 0 已扩容到 N 行、槽 1 仍是 256 行」时
    /// 槽 1 的写入越界（V-64 的第二个根因）。
    rows: u32,
    /// 持久映射的写入指针（DEVIATIONS F-210）：`Map` 一次，整个槽生命周期不再 `Unmap`。
    ptr: *mut u8,
}

// SAFETY: 指针只在 `kytrix-d3d12` 线程上写入（`AtlasTextures` 整体独占该线程）。
unsafe impl Send for StagingSlot {}

impl StagingSlot {
    fn new_with_rows(device: &mut Device, row_pitch: u32, rows: u32) -> Result<Self, EngineError> {
        let buffer = create_buffer(
            device,
            u64::from(row_pitch) * u64::from(rows),
            D3D12_HEAP_TYPE_UPLOAD,
        )?;
        let handle = device.allocate(buffer.clone());
        let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: UPLOAD 堆；本槽独占该映射，直到槽被替换 / 图集销毁。
        unsafe { buffer.Map(0, None, Some(&mut pointer)) }
            .map_err(|error| init_error_from("Map(atlas staging)", error))?;
        Ok(Self {
            buffer,
            handle,
            rows,
            ptr: pointer.cast::<u8>(),
        })
    }
}

/// 一张图集纹理 + 每 in-flight 槽的 staging。
struct AtlasTexture {
    kind: AtlasKind,
    resource: ID3D12Resource,
    /// 每槽一块 staging；容量按**该槽本次上传的总行数**增长（见 [`STAGING_ROWS`]）。
    staging: Vec<StagingSlot>,
    page: u32,
    row_pitch: u32,
    texel_bytes: u32,
}

/// 一次 staging 写入的行区间：源页起点、staging 起点、行数。
#[derive(Clone, Copy)]
struct Chunk {
    /// 目标纹理里的起始行（`pixels` 打包顺序之外的真实行号）。
    source_row: u32,
    /// staging 里的起始行。
    staging_row: u32,
    rows: u32,
    /// 打包缓冲里的累计行号（源数据只有脏行）。
    packed_row: u32,
}

/// 待上传的一页内容（整页 `pixels` + 页尺寸 + 脏行区间）。
struct PendingUpload {
    page: u32,
    pixels: Vec<u8>,
    rows: DirtyRows,
}

/// 两种图集（字形 / 图片）。
pub(crate) struct AtlasTextures {
    glyph: AtlasTexture,
    image: AtlasTexture,
    pending: [Option<PendingUpload>; 2],
    /// `prepare` 重建过纹理的页：本帧必须整页上传（新纹理内容未定义）。
    force_full: [bool; 2],
}

impl AtlasTextures {
    /// 建两张 1×1 占位页；真实页在第一次上传前由 [`AtlasTextures::prepare`] 按像素长度创建。
    pub(crate) fn new(device: &mut Device) -> Result<Self, EngineError> {
        Ok(Self {
            glyph: AtlasTexture::new(device, AtlasKind::Glyph)?,
            image: AtlasTexture::new(device, AtlasKind::Image)?,
            pending: [None, None],
            force_full: [false, false],
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

    /// 该图集是否有一笔上传已入队、还没被命令列表消费（离屏目标的空闲判据）。
    pub(crate) fn pending(&self, kind: AtlasKind) -> bool {
        self.pending[kind.index() as usize].is_some()
    }

    /// 入队一页的脏行（`pixels` 是 `rows` 顺序**紧密打包**的行，DEVIATIONS F-211）。
    ///
    /// `pixels` 按**所有权**移动进待上传槽，避免槽 → 纹理之间再复制一次。长度必须等于
    /// `行数 × page × texel`（release 也硬断言：越界读会读穿堆）。
    pub(crate) fn queue_upload(
        &mut self,
        kind: AtlasKind,
        page: u32,
        pixels: Vec<u8>,
        rows: DirtyRows,
    ) {
        let row_bytes = page as usize * texel_bytes(kind) as usize;
        let expected = rows.row_count(page) as usize * row_bytes;
        assert_eq!(
            pixels.len(),
            expected,
            "atlas payload size mismatch for {kind:?}: expected {expected} bytes for {} rows of a {page}² page, got {}",
            rows.row_count(page),
            pixels.len()
        );
        let slot = match kind {
            AtlasKind::Glyph => 0,
            AtlasKind::Image => 1,
        };
        assert!(
            self.pending[slot].is_none(),
            "atlas payload slot must be consumed before the next upload (kind {kind:?})"
        );
        self.pending[slot] = Some(PendingUpload { page, pixels, rows });
    }

    /// 本帧是否有一页的尺寸要变化（需要在**打开命令列表之前**重建纹理）。
    fn page_change(&self) -> Option<(AtlasKind, u32)> {
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = match kind {
                AtlasKind::Glyph => 0,
                AtlasKind::Image => 1,
            };
            let texture = match kind {
                AtlasKind::Glyph => &self.glyph,
                AtlasKind::Image => &self.image,
            };
            if let Some(pending) = &self.pending[index]
                && pending.page != texture.page
            {
                return Some((kind, pending.page));
            }
        }
        None
    }

    /// 在录制本帧**之前**把页尺寸变化落地（DEVIATIONS F-208）。
    ///
    /// 页变化会重建纹理并重写 SRV；SRV 堆可能仍被在飞命令列表引用，因此这里先 `wait_idle`
    /// 并重置 in-flight list，之后旧资源可以立即释放（不需要 retire）。页变化只发生在
    /// 首次使用与字形页换代（最多两档），等待代价可以接受。
    pub(crate) fn prepare(
        &mut self,
        device: &mut Device,
        queue: &mut Queue,
    ) -> Result<AtlasRebuilt, EngineError> {
        let mut rebuilt = AtlasRebuilt::default();
        if self.page_change().is_none() {
            return Ok(rebuilt);
        }
        queue.wait_idle()?;
        queue.reset_in_flight_lists()?;
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = match kind {
                AtlasKind::Glyph => 0,
                AtlasKind::Image => 1,
            };
            let Some(pending) = &self.pending[index] else {
                continue;
            };
            let page = pending.page;
            let texture = match kind {
                AtlasKind::Glyph => &mut self.glyph,
                AtlasKind::Image => &mut self.image,
            };
            if page == texture.page {
                continue;
            }
            texture.recreate(device, page)?;
            rebuilt.mark(kind);
            // 重建那一帧必须整页上传（新纹理内容未定义）。
            self.force_full[index] = true;
        }
        Ok(rebuilt)
    }

    /// 在本帧命令列表里录制「staging 写入 + 逐区间 `CopyTextureRegion`」。
    ///
    /// 返回因 staging 扩容换下的旧句柄（调用方在本帧录制结束后 retire；D3D12 命令列表
    /// 不持有资源引用，立即 drop 会让在飞帧读已释放的缓冲）。
    pub(crate) fn record_uploads(
        &mut self,
        device: &mut Device,
        list: &ID3D12GraphicsCommandList,
        slot: usize,
    ) -> Result<Vec<GpuResourceHandle>, EngineError> {
        let mut retired = Vec::new();
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
            debug_assert_eq!(
                pending.page, texture.page,
                "page changes must be applied by AtlasTextures::prepare before recording"
            );
            let force_full = std::mem::take(&mut self.force_full[index]);
            texture.upload(device, list, slot, kind, &pending, force_full, &mut retired)?;
        }
        Ok(retired)
    }
}

impl AtlasTexture {
    fn new(device: &mut Device, kind: AtlasKind) -> Result<Self, EngineError> {
        let texel = texel_bytes(kind);
        let row_pitch = align_up(PLACEHOLDER_PAGE * texel, PITCH_ALIGNMENT);
        let resource = create_texture(
            device,
            PLACEHOLDER_PAGE,
            PLACEHOLDER_PAGE,
            texture_format(kind),
        )?;
        let mut staging = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            staging.push(StagingSlot::new_with_rows(
                device,
                row_pitch,
                resident_rows(row_pitch),
            )?);
        }
        Ok(Self {
            kind,
            resource,
            staging,
            page: PLACEHOLDER_PAGE,
            row_pitch,
            texel_bytes: texel,
        })
    }

    /// 换页：按新页尺寸建纹理。调用方已 `wait_idle`，旧纹理 / 旧 staging 立即释放。
    fn recreate(&mut self, device: &mut Device, page: u32) -> Result<(), EngineError> {
        let row_pitch = align_up(page * self.texel_bytes, PITCH_ALIGNMENT);
        let resource = create_texture(device, page, page, texture_format(self.kind))?;
        // 旧纹理：GPU 已空闲（prepare 里 wait_idle），字段替换即释放。
        self.resource = resource;
        for slot in &mut self.staging {
            let old = std::mem::replace(
                slot,
                StagingSlot::new_with_rows(device, row_pitch, resident_rows(row_pitch))?,
            );
            // 旧 staging 的句柄表项也要一并释放（否则它会把 COM 引用留到设备销毁）。
            device.release(old.handle);
        }
        self.page = page;
        self.row_pitch = row_pitch;
        Ok(())
    }

    fn upload(
        &mut self,
        device: &mut Device,
        list: &ID3D12GraphicsCommandList,
        slot: usize,
        kind: AtlasKind,
        pending: &PendingUpload,
        force_full: bool,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(), EngineError> {
        let resident = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
        barrier(
            list,
            &self.resource,
            resident,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );

        let full = 0..self.page;
        // 重建过纹理的那一帧必须是整页上传（新纹理内容未定义）：App 在页尺寸变化时一定发整页。
        // 覆盖整页的脏行等价于整页上传（`mark(0, page)` 与 `mark(0, u32::MAX)` 都接受）。
        let covers_page =
            pending.rows.is_full() || pending.rows.spans() == std::slice::from_ref(&(0..self.page));
        assert!(
            !force_full || covers_page,
            "a rebuilt atlas page must be uploaded as a full page"
        );
        let spans: &[Range<u32>] = if pending.rows.is_full() {
            std::slice::from_ref(&full)
        } else {
            pending.rows.spans()
        };
        // 每一段必须在 staging 里**各有位置**：同一 staging 上先写后画的多次拷贝会在 GPU 执行时
        // 读到最后一块的字节（V-64）。因此先按本帧所有段的**总行数**保证容量，再按累计行偏移
        // 把各段顺序落位；`packed_row` 是打包缓冲里的累计行号（源数据只有脏行）。
        //
        // 行必须落在页内：`pack_rows` / `row_count` 已按页钳过长度，这里若还越页说明打包
        // 缓冲与行描述不是同一份数据，按契约错误立刻失败（否则下面的 `ptr::add` 会读穿堆）。
        let total_rows: u32 = spans
            .iter()
            .map(|span| {
                assert!(
                    span.start < span.end && span.end <= self.page,
                    "atlas dirty span {span:?} is outside the {}-row page",
                    self.page
                );
                span.end - span.start
            })
            .sum();
        // 小上传复用每槽常驻 staging；整页级的大上传用一次性缓冲（录制后 retire），
        // 常驻容量不随页尺寸放大（DEVIATIONS F-216）。
        let total_bytes = u64::from(total_rows) * u64::from(self.row_pitch);
        let mut transient = None;
        if total_bytes <= MAX_RESIDENT_STAGING_BYTES {
            self.ensure_staging(device, slot, total_rows, retired)?;
        } else {
            let temp = StagingSlot::new_with_rows(device, self.row_pitch, total_rows)?;
            retired.push(temp.handle);
            transient = Some(temp);
        }
        let staging = transient.as_ref().unwrap_or(&self.staging[slot]);
        let mut staging_row = 0;
        let mut packed_row = 0;
        for span in spans {
            let rows = span.end - span.start;
            self.upload_chunk(
                list,
                staging,
                kind,
                pending,
                Chunk {
                    source_row: span.start,
                    staging_row,
                    rows,
                    packed_row,
                },
            )?;
            staging_row += rows;
            packed_row += rows;
        }

        barrier(
            list,
            &self.resource,
            D3D12_RESOURCE_STATE_COPY_DEST,
            resident,
        );
        Ok(())
    }

    /// 保证第 `slot` 个 staging 能容纳 `rows` 行（不够就重建；扩容后该槽复用）。
    ///
    /// 只改**这一个槽**的容量记录：其它槽的缓冲没有变大，必须继续按自己的容量判断（V-64）。
    fn ensure_staging(
        &mut self,
        device: &mut Device,
        slot: usize,
        rows: u32,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(), EngineError> {
        if rows <= self.staging[slot].rows {
            return Ok(());
        }
        let staging = StagingSlot::new_with_rows(device, self.row_pitch, rows)?;
        let old = std::mem::replace(&mut self.staging[slot], staging);
        // 旧 staging 可能仍被在飞帧的命令列表读取（D3D12 命令列表不持有资源引用）：
        // 不能立即 drop，交给调用方按当前帧序号 retire。
        retired.push(old.handle);
        Ok(())
    }

    /// 把一段行写进 staging 的指定起点，并录制一次拷贝。
    fn upload_chunk(
        &self,
        list: &ID3D12GraphicsCommandList,
        staging: &StagingSlot,
        kind: AtlasKind,
        pending: &PendingUpload,
        chunk: Chunk,
    ) -> Result<(), EngineError> {
        // SAFETY: `staging` 的容量 ≥ 本帧总行数；指针是该槽的持久映射（DEVIATIONS F-210），
        // `staging_row + rows` 不超过容量，源页由 `queue_upload` 的整页尺寸断言保证。
        unsafe {
            for row in 0..chunk.rows {
                let source = pending
                    .pixels
                    .as_ptr()
                    .add(((chunk.packed_row + row) * self.page * self.texel_bytes) as usize);
                let destination = staging
                    .ptr
                    .add(((chunk.staging_row + row) * self.row_pitch) as usize);
                std::ptr::copy_nonoverlapping(
                    source,
                    destination,
                    (self.page * self.texel_bytes) as usize,
                );
            }
        }

        let placed = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            // 每段各自的行偏移（同一 staging 上多段不能再共用 offset 0，否则 GPU 只读到末段）。
            Offset: u64::from(chunk.staging_row) * u64::from(self.row_pitch),
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: texture_format(kind),
                Width: self.page,
                Height: chunk.rows,
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
            pResource: core::mem::ManuallyDrop::new(Some(staging.buffer.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: placed,
            },
        };
        // SAFETY: 两个 location 与 `placed` 在调用期间有效，行区间已由 `DirtyRows` 保证在页内。
        unsafe { list.CopyTextureRegion(&destination, 0, chunk.source_row, 0, &source, None) };
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

fn align_up(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment) * alignment
}

/// 每槽常驻 staging 的初始行数：受 1 MiB 上限与 [`STAGING_ROWS`] 双重约束。
fn resident_rows(row_pitch: u32) -> u32 {
    ((MAX_RESIDENT_STAGING_BYTES / u64::from(row_pitch)) as u32).clamp(1, STAGING_ROWS)
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
    // SAFETY: COM 调用：设备 / 工厂 / swapchain 句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
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
    use super::{AtlasTextures, STAGING_ROWS, align_up, resident_rows};
    use crate::render::atlas::{MAX_ATLAS_PAGE, MAX_RESIDENT_STAGING_BYTES};

    /// 测试页：2048² 的纹理在 WARP 上代价过高（且会拖垮并行的 GPU 测试）。
    const TEST_PAGE: u32 = 64;
    use crate::{
        paint::atlas::AtlasKind,
        render::{atlas::DirtyRows, windows::device::Device},
    };

    #[test]
    fn row_pitch_is_256_aligned_for_both_texels() {
        assert_eq!(align_up(MAX_ATLAS_PAGE, 256), 2048);
        assert_eq!(align_up(MAX_ATLAS_PAGE * 4, 256), 8192);
    }

    /// 常驻 staging 永远不超过 1 MiB：整页级上传走一次性缓冲（F-216）。
    #[test]
    fn resident_staging_stays_within_the_cap() {
        for pitch in [256u32, 512, 1024, 2048, 4096, 8192] {
            let rows = resident_rows(pitch);
            assert!(
                (1..=STAGING_ROWS).contains(&rows),
                "pitch {pitch}: {rows} rows"
            );
            assert!(
                u64::from(rows) * u64::from(pitch) <= MAX_RESIDENT_STAGING_BYTES,
                "pitch {pitch}: {} bytes",
                u64::from(rows) * u64::from(pitch)
            );
        }
        assert_eq!(resident_rows(8192), 128, "2048² 图片页的行宽");
        assert_eq!(resident_rows(256), STAGING_ROWS);
    }

    /// 整页上传（> 1 MiB）必须走一次性缓冲：常驻槽不得按整页扩容（F-216）。
    #[test]
    fn a_full_page_upload_does_not_grow_the_resident_slot() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        use crate::render::windows::queue::Queue;

        let mut device = Device::new_warp().expect("WARP device");
        let mut queue = Queue::new(&device).expect("queue");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");
        // 1024² 的图片整页 = 4 MiB，超过常驻上限。
        const PAGE: u32 = 1024;
        let mut rows = DirtyRows::default();
        rows.mark(0, u32::MAX);
        atlas.queue_upload(
            AtlasKind::Image,
            PAGE,
            vec![0u8; (PAGE * PAGE * 4) as usize],
            rows,
        );
        atlas.prepare(&mut device, &mut queue).expect("prepare");
        let mut retired = Vec::new();
        queue
            .submit_immediate(|list| {
                retired = atlas.record_uploads(&mut device, list, 0)?;
                Ok(())
            })
            .expect("upload");

        assert!(
            !retired.is_empty(),
            "整页上传必须把一次性 staging 交给 retire"
        );
        let row_pitch = align_up(PAGE * 4, 256);
        let resident = atlas.image.staging[0].rows;
        assert!(
            u64::from(resident) * u64::from(row_pitch) <= MAX_RESIDENT_STAGING_BYTES,
            "常驻槽被整页上传撑大：{resident} 行 × {row_pitch} B"
        );
        assert!(resident < PAGE, "整页行数不得常驻（{resident}）");
    }

    #[test]
    fn dirty_rows_upload_only_touches_those_rows() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;

        use crate::render::windows::{queue::Queue, readback::Readback};

        let mut device = Device::new_warp().expect("WARP device");
        let mut queue = Queue::new(&device).expect("queue");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");

        // 第一次必须先整页上传（真实入口：新窗口 / 页尺寸变化都走整页），之后才是增量。
        let mut rows = DirtyRows::default();
        rows.mark(0, u32::MAX);
        atlas.queue_upload(
            AtlasKind::Glyph,
            TEST_PAGE,
            vec![0u8; (TEST_PAGE * TEST_PAGE) as usize],
            rows,
        );
        atlas.prepare(&mut device, &mut queue).expect("prepare");
        queue
            .submit_immediate(|list| atlas.record_uploads(&mut device, list, 0).map(|_| ()))
            .expect("full upload");

        // 增量：只有 1 行（= 页宽字节）的载荷。
        let page = vec![0xABu8; TEST_PAGE as usize];
        let mut rows = DirtyRows::default();
        rows.mark(5, 6);
        atlas.queue_upload(AtlasKind::Glyph, TEST_PAGE, page, rows);
        assert!(atlas.has_pending());

        atlas.prepare(&mut device, &mut queue).expect("prepare");
        queue
            .submit_immediate(|list| atlas.record_uploads(&mut device, list, 0).map(|_| ()))
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
        let mut device = Device::new_warp().expect("WARP device");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");
        assert!(!atlas.has_pending());

        let mut rows = DirtyRows::default();
        rows.mark(4, 6);
        let page = vec![0x7Fu8; 2 * TEST_PAGE as usize];
        atlas.queue_upload(AtlasKind::Glyph, TEST_PAGE, page, rows);
        assert!(atlas.has_pending());
    }
    /// staging 容量必须**按 in-flight 槽**记录（V-64）。
    ///
    /// 场景：槽 0 先做「字形整页」（512 行 > `STAGING_ROWS`）把该槽扩容到 512 行；随后槽 1 做
    /// 「图片整页」（同样 512 行）。若容量是全局单值，槽 1 会按 512 行的记录跳过扩容，进而把
    /// 512×2048 B 写进只有 256×2048 B 的缓冲 —— 堆越界写。
    const BIG_PAGE: u32 = 512;

    #[test]
    fn cross_slot_staging_capacity_is_per_slot() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        use windows::Win32::Graphics::{
            Direct3D12::D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8_UNORM},
        };

        use crate::render::windows::{queue::Queue, readback::Readback};

        let mut device = Device::new_warp().expect("WARP device");
        let mut queue = Queue::new(&device).expect("queue");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");

        // 槽 0：字形整页（512 行）。
        let glyph = vec![0x11u8; (BIG_PAGE * BIG_PAGE) as usize];
        let mut rows = DirtyRows::default();
        rows.mark(0, BIG_PAGE);
        atlas.queue_upload(AtlasKind::Glyph, BIG_PAGE, glyph, rows);
        atlas.prepare(&mut device, &mut queue).expect("prepare");
        queue
            .submit_immediate(|list| atlas.record_uploads(&mut device, list, 0).map(|_| ()))
            .expect("glyph upload");

        // 槽 1：图片整页（512 行 × 4 B；行 pitch 2048）。
        let mut image = vec![0u8; (BIG_PAGE * BIG_PAGE * 4) as usize];
        for texel in image.as_chunks_mut::<4>().0 {
            *texel = [0x33, 0x44, 0x55, 0xFF];
        }
        let mut rows = DirtyRows::default();
        rows.mark(0, BIG_PAGE);
        atlas.queue_upload(AtlasKind::Image, BIG_PAGE, image, rows);
        atlas.prepare(&mut device, &mut queue).expect("prepare");
        queue
            .submit_immediate(|list| atlas.record_uploads(&mut device, list, 1).map(|_| ()))
            .expect("image upload");

        // 两个独立的读回器：R8 与 BGRA8 的行 pitch 不同（`Readback` 按尺寸/格式缓存）。
        let mut glyph_readback = Readback::new();
        let glyph_image = glyph_readback
            .capture_from(
                &device,
                &mut queue,
                atlas.texture(AtlasKind::Glyph),
                (BIG_PAGE, BIG_PAGE),
                DXGI_FORMAT_R8_UNORM,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            )
            .expect("glyph readback");
        let glyph_row = |row: u32| glyph_image.rgba8[(row * BIG_PAGE * 4) as usize];
        assert_eq!(glyph_row(0), 0x11, "glyph row 0");
        assert_eq!(glyph_row(BIG_PAGE - 1), 0x11, "glyph last row");

        let mut image_readback = Readback::new();
        let image_image = image_readback
            .capture_from(
                &device,
                &mut queue,
                atlas.texture(AtlasKind::Image),
                (BIG_PAGE, BIG_PAGE),
                DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            )
            .expect("image readback");
        let image_row = |row: u32| {
            let index = (row * BIG_PAGE * 4) as usize;
            [
                image_image.rgba8[index],
                image_image.rgba8[index + 1],
                image_image.rgba8[index + 2],
                image_image.rgba8[index + 3],
            ]
        };
        assert_eq!(image_row(0), [0x55, 0x44, 0x33, 0xFF], "image row 0");
        assert_eq!(
            image_row(BIG_PAGE - 1),
            [0x55, 0x44, 0x33, 0xFF],
            "image last row"
        );
    }

    /// 多脏段必须各自落位：每段写到 staging 自己的行偏移，`CopyTextureRegion` 的
    /// `PlacedFootprint.Offset` 也必须跟着走（V-64；旧的「全部写 offset 0」会让两段都读到末段）。
    #[test]
    fn multi_span_upload_places_each_span_at_its_own_offset() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        use windows::Win32::Graphics::{
            Direct3D12::D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            Dxgi::Common::DXGI_FORMAT_R8_UNORM,
        };

        use crate::render::windows::{queue::Queue, readback::Readback};

        let mut device = Device::new_warp().expect("WARP device");
        let mut queue = Queue::new(&device).expect("queue");
        let mut atlas = AtlasTextures::new(&mut device).expect("atlas");

        // 第一次整页清零（真实入口：新窗口 / 页尺寸变化都走整页）。
        let mut rows = DirtyRows::default();
        rows.mark(0, u32::MAX);
        atlas.queue_upload(
            AtlasKind::Glyph,
            BIG_PAGE,
            vec![0u8; (BIG_PAGE * BIG_PAGE) as usize],
            rows,
        );
        atlas.prepare(&mut device, &mut queue).expect("prepare");
        queue
            .submit_immediate(|list| atlas.record_uploads(&mut device, list, 0).map(|_| ()))
            .expect("full upload");

        // 打包布局：段按行号顺序紧密排列（10 行 0x44 + 20 行 0x55）。
        let mut page = vec![0x44u8; (10 * BIG_PAGE) as usize];
        page.extend(std::iter::repeat_n(0x55u8, (20 * BIG_PAGE) as usize));
        let mut rows = DirtyRows::default();
        rows.mark(10, 20);
        rows.mark(300, 320);
        atlas.queue_upload(AtlasKind::Glyph, BIG_PAGE, page, rows);
        atlas.prepare(&mut device, &mut queue).expect("prepare");
        queue
            .submit_immediate(|list| atlas.record_uploads(&mut device, list, 0).map(|_| ()))
            .expect("upload");

        let mut readback = Readback::new();
        let image = readback
            .capture_from(
                &device,
                &mut queue,
                atlas.texture(AtlasKind::Glyph),
                (BIG_PAGE, BIG_PAGE),
                DXGI_FORMAT_R8_UNORM,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            )
            .expect("readback");
        let row = |row: u32| image.rgba8[(row * BIG_PAGE * 4) as usize];
        assert_eq!(row(0), 0, "untouched row before the first span");
        assert_eq!(row(15), 0x44, "first span content");
        assert_eq!(row(100), 0, "gap between the spans stays untouched");
        assert_eq!(row(299), 0, "row before the second span");
        assert_eq!(row(305), 0x55, "second span content");
        assert_eq!(row(511), 0, "untouched row after the last span");
    }
}

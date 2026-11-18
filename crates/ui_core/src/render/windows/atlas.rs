//! 共享图集纹理（`SharedAtlas`）+ 每窗口上传器（`AtlasUploads`）
//! （`design/phase-08-d3d12.md` §5.5；格式与 UV 约定见 P02 §10）。
//!
//! 字形图集是 `R8_UNORM`（线性覆盖率），图片图集是 `B8G8R8A8_UNORM_SRGB`。两张纹理是**进程级
//! 共享资源**，由 `device::SharedGpu` 持有；每窗口只有自己的**上传路径**：
//!
//! - [`SharedAtlas`]：纹理 + 当前页尺寸 + 行 pitch + **CPU 影子页**。懒分配：初始化只建 1×1
//!   占位纹理（SRV 始终指向有效资源），第一次整页上传或页档换代时由
//!   `SharedGpu::rebuild_atlas_pages` 重建。
//! - [`AtlasUploads`]（每窗口）：待上传页 + 每 in-flight 槽的 staging。staging 必须是每窗口的：
//!   同一份共享纹理会被多个窗口的 GPU 线程并发上传，各窗口的 staging 不能互相覆盖；每个槽的
//!   staging 又只在本窗口该槽的 fence 完成前复用（与 `UploadArena` 同一套 in-flight 语义）。
//!
//! 并发模型：窗口的 GPU 线程在 `SharedGpu` 的图集**读锁**内录制并提交（页在录制期间不会变），
//! 页重建走**写锁**。写锁期间不可能出现新的提交（录制与提交都在读锁内），因此页换代只需要等
//! 各窗口登记的**提交水位**（`queue::StreamWatermark`）而不必等整条共享队列 idle（R08-6）。
//!
//! **staging 的释放必须由 fence 保护**（R08-2）：D3D12 **不**替应用保活命令列表引用过的资源
//! （MSDN：应用必须保证 GPU 完成前不释放被引用资源），所以被本帧列表读过的 staging 句柄一律
//! 交给 `CommandStream::retire`（按帧序号延迟释放），**只有**调用方已经 `wait_idle` 的路径
//! （teardown / 回退重建）才允许立即 `shared.release`。
//!
//! staging 有两档：**每槽常驻**一小块（≤ 1 MiB，增量上传复用）与**一次性**大块（整页上传，
//! 录制后按帧 retire）。常驻容量因此不随图集页尺寸放大（DEVIATIONS F-216）。页尺寸随上传
//! **显式过线**（S-153），不由 `pixels.len()` 反推：增量载荷只有脏矩形，长度不足以恢复整页尺寸。
//!
//! **CPU 影子页**（R08-4）：每张图集的当前页在系统内存里留一份 1:1 拷贝（字形 4 MiB / 图片
//! 16 MiB）。设备丢失后进程内恢复靠它把图集内容原样重传 —— 否则恢复出来的窗口只有占位纹理，
//! 所有字形与图片都会消失。写入点就是 [`AtlasUploads::record_uploads`]（CPU 已经有一份字节，
//! 增量行落位成本与 staging 拷贝同量级）。

use std::sync::{Mutex, MutexGuard, PoisonError};

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
        atlas::{DirtyRects, MAX_RESIDENT_STAGING_BYTES, RectChunk, texel_bytes},
        frame::MAX_FRAMES_IN_FLIGHT,
        retire::GpuResourceHandle,
        windows::device::{SharedGpu, init_error_from},
    },
};

/// 初始化时的占位页（1×1）：让 SRV 在第一次上传之前也指向有效资源。
const PLACEHOLDER_PAGE: u32 = 1;

/// 常驻 staging 的**初始容量**上界（行数口径：`STAGING_ROWS × 页行宽`，再夹到 1 MiB）。
///
/// 一次 `record_uploads` 里的多条 `CopyTextureRegion` 都在同一提交、同一个 staging 上，
/// **不能分块复用同一个缓冲**（后面的 CPU 写入会覆盖前面的内容，GPU 执行时读到的全是最后一块
/// —— V-64 的根因）。因此一次上传的每个 AOI 矩形都必须在 staging 里各有位置：容量按**本次
/// 上传的总字节数**保证，各矩形按 512 对齐的偏移落位。
const STAGING_ROWS: u32 = 256;

/// `D3D12_PLACED_SUBRESOURCE_FOOTPRINT.RowPitch` 的对齐（D3D12 要求 256）。
const PITCH_ALIGNMENT: u32 = 256;

/// `D3D12_PLACED_SUBRESOURCE_FOOTPRINT.Offset` 的对齐（D3D12 要求 512）。
const PLACED_OFFSET_ALIGNMENT: u32 = 512;

/// 一张共享图集页（纹理 + 当前页尺寸 + 行 pitch）。
struct SharedPage {
    resource: ID3D12Resource,
    page: u32,
    row_pitch: u32,
}

impl SharedPage {
    fn new(device: &ID3D12Device, kind: AtlasKind, page: u32) -> Result<Self, EngineError> {
        let row_pitch = row_pitch_for(kind, page);
        let resource = create_texture(device, page, page, texture_format(kind))?;
        Ok(Self {
            resource,
            page,
            row_pitch,
        })
    }
}

/// 进程级共享的两张图集纹理（由 `SharedGpu` 的图集读写锁保护）。
pub(crate) struct SharedAtlas {
    glyph: SharedPage,
    image: SharedPage,
    /// 每张图集当前页的 CPU 影子（`R08-4`：设备丢失后按它重传；见模块文档）。
    ///
    /// 放在 `Mutex` 里而不是复用读写锁：影子只在**录制期**（已经持有图集读锁）被更新，
    /// 两个窗口并发上传同一页时写的是同一份内容，锁只保证 `Vec` 不被并发破坏。
    shadow: [Mutex<Option<ShadowPage>>; 2],
}

/// 一张图集页的 CPU 侧 1:1 拷贝（紧密行，行宽 = `page × texel`）。
struct ShadowPage {
    page: u32,
    pixels: Vec<u8>,
}

impl SharedAtlas {
    /// 建两张 1×1 占位页；真实页在第一次整页上传时由 `SharedGpu::rebuild_atlas_pages` 创建。
    pub(crate) fn new(device: &ID3D12Device) -> Result<Self, EngineError> {
        Ok(Self {
            glyph: SharedPage::new(device, AtlasKind::Glyph, PLACEHOLDER_PAGE)?,
            image: SharedPage::new(device, AtlasKind::Image, PLACEHOLDER_PAGE)?,
            shadow: [Mutex::new(None), Mutex::new(None)],
        })
    }

    pub(crate) fn texture(&self, kind: AtlasKind) -> &ID3D12Resource {
        &self.page_ref(kind).resource
    }

    /// 该图集当前的页尺寸（纹素，边长）。
    pub(crate) fn page(&self, kind: AtlasKind) -> u32 {
        self.page_ref(kind).page
    }

    /// 该图集当前的 staging 行 pitch（字节，256 对齐）。
    pub(crate) fn row_pitch(&self, kind: AtlasKind) -> u32 {
        self.page_ref(kind).row_pitch
    }

    /// 换页：按新页尺寸建纹理并替换本页；返回被换下来的旧纹理（调用方决定立即释放还是挂进
    /// 延迟销毁表，R08-6 的慢路径）。
    ///
    /// 正常路径的调用方（`SharedGpu::rebuild_atlas_pages`）已经等到全部窗口的提交水位，因此
    /// 旧纹理不再被任何在飞命令列表引用，可以立即 drop。
    pub(crate) fn recreate(
        &mut self,
        device: &ID3D12Device,
        kind: AtlasKind,
        page: u32,
    ) -> Result<ID3D12Resource, EngineError> {
        let next = SharedPage::new(device, kind, page)?;
        let old = std::mem::replace(self.page_mut(kind), next);
        Ok(old.resource)
    }

    /// 把一次上传的字节写进 CPU 影子（R08-4）。
    ///
    /// `pixels` 是 `rects` 顺序**逐矩形紧密打包**的字节（与打包缓冲的布局同源）；页尺寸变化
    /// 说明 App 换代，影子按新页整页重建（整页上传是换代之后的唯一合法载荷，见
    /// [`AtlasUploads::reconcile_pages`]）。
    fn update_shadow(&self, kind: AtlasKind, page: u32, pixels: &[u8], rects: &DirtyRects) {
        let texel = texel_bytes(kind) as usize;
        let row_bytes = page as usize * texel;
        if row_bytes == 0 || pixels.len() != rects.byte_count(page, texel_bytes(kind)) {
            return;
        }
        let mut shadow = shadow_lock(&self.shadow[kind.index() as usize]);
        if shadow.as_ref().is_none_or(|shadow| shadow.page != page) {
            *shadow = Some(ShadowPage {
                page,
                pixels: vec![0; row_bytes * page as usize],
            });
        }
        let Some(shadow) = shadow.as_mut() else {
            return;
        };
        for chunk in rects.chunks(page, texel_bytes(kind)) {
            let width_bytes = chunk.width as usize * texel;
            for row in 0..chunk.height as usize {
                let source = chunk.offset + row * width_bytes;
                let destination = (chunk.y as usize + row) * row_bytes + chunk.x as usize * texel;
                if source + width_bytes <= pixels.len()
                    && destination + width_bytes <= shadow.pixels.len()
                {
                    shadow.pixels[destination..destination + width_bytes]
                        .copy_from_slice(&pixels[source..source + width_bytes]);
                }
            }
        }
    }

    /// 设备丢失恢复用：两张图集当前页的 `(页尺寸, 整页字节)`（没有影子页的图集是 `None`）。
    pub(crate) fn shadow_snapshot(&self) -> [Option<(u32, Vec<u8>)>; 2] {
        std::array::from_fn(|index| {
            shadow_lock(&self.shadow[index])
                .as_ref()
                .map(|shadow| (shadow.page, shadow.pixels.clone()))
        })
    }

    fn page_ref(&self, kind: AtlasKind) -> &SharedPage {
        match kind {
            AtlasKind::Glyph => &self.glyph,
            AtlasKind::Image => &self.image,
        }
    }

    fn page_mut(&mut self, kind: AtlasKind) -> &mut SharedPage {
        match kind {
            AtlasKind::Glyph => &mut self.glyph,
            AtlasKind::Image => &mut self.image,
        }
    }
}

fn shadow_lock(shadow: &Mutex<Option<ShadowPage>>) -> MutexGuard<'_, Option<ShadowPage>> {
    shadow.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 每 in-flight 槽一块 staging（页变化时整组重建）。
struct StagingSlot {
    buffer: ID3D12Resource,
    handle: GpuResourceHandle,
    /// 该槽当前的 staging **字节**容量。
    ///
    /// 必须是按槽的数组语义：只记一个全局值会让「槽 0 已扩容到 N 字节、槽 1 仍是初始容量」时
    /// 槽 1 的写入越界（V-64 的第二个根因）。
    capacity: u32,
    /// 持久映射的写入指针（DEVIATIONS F-210）：`Map` 一次，整个槽生命周期不再 `Unmap`。
    ptr: *mut u8,
}

// SAFETY: 指针只在 `kytrix-d3d12` 线程上写入（`AtlasUploads` 整体独占该线程）。
unsafe impl Send for StagingSlot {}

impl StagingSlot {
    fn new_with_bytes(shared: &SharedGpu, bytes: u32) -> Result<Self, EngineError> {
        let buffer = create_buffer(
            shared.raw(),
            u64::from(bytes.max(1)),
            D3D12_HEAP_TYPE_UPLOAD,
        )?;
        let handle = shared.allocate(buffer.clone());
        let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: UPLOAD 堆；本槽独占该映射，直到槽被替换 / 图集销毁。
        unsafe { buffer.Map(0, None, Some(&mut pointer)) }
            .map_err(|error| init_error_from("Map(atlas staging)", error))?;
        Ok(Self {
            buffer,
            handle,
            capacity: bytes,
            ptr: pointer.cast::<u8>(),
        })
    }
}

/// 一个 AOI 矩形在 staging 里的落点（D3D12 placed footprint 的约束：行 pitch 256 对齐、
/// 偏移 512 对齐）。
#[derive(Clone, Copy)]
struct StagingRect {
    chunk: RectChunk,
    /// staging 里该矩形的行 pitch（字节，[`PITCH_ALIGNMENT`] 对齐）。
    row_pitch: u32,
    /// staging 里的起始字节偏移（[`PLACED_OFFSET_ALIGNMENT`] 对齐）。
    offset: u32,
}

/// 一个图集（kind）的每窗口 staging：每 in-flight 槽一块缓冲，页换代时整组重建。
struct AtlasStaging {
    kind: AtlasKind,
    page: u32,
    slots: Vec<StagingSlot>,
}

impl AtlasStaging {
    fn new(
        shared: &SharedGpu,
        kind: AtlasKind,
        page: u32,
        row_pitch: u32,
    ) -> Result<Self, EngineError> {
        let mut slots = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            slots.push(StagingSlot::new_with_bytes(
                shared,
                resident_bytes(row_pitch),
            )?);
        }
        Ok(Self { kind, page, slots })
    }

    /// 把整组 staging 的句柄交给调用方 retire（R08-2）。
    ///
    /// D3D12 **不**替应用保活命令列表引用过的资源（MSDN：应用必须保证 GPU 完成前不释放被引用
    /// 的资源）。这些缓冲可能仍被本窗口其它 in-flight 槽的命令列表在读，立即
    /// `shared.release` 会把它从设备句柄表里摘掉并由 COM 当场销毁 —— 那正是「in-flight 帧
    /// 引用已释放 staging」的 use-after-free。所以这里只交出句柄，由调用方按**帧序号**交给
    /// `CommandStream::retire`。只有调用方已经 `wait_idle` 的路径（teardown）才允许立即释放。
    fn retire(self, retired: &mut Vec<GpuResourceHandle>) {
        for slot in self.slots {
            retired.push(slot.handle);
        }
    }

    /// 保证第 `slot` 个 staging 至少能容纳 `bytes` 字节（不够就重建；扩容后该槽复用）。
    ///
    /// 只改**这一个槽**的容量记录：其它槽的缓冲没有变大，必须继续按自己的容量判断（V-64）。
    fn ensure_bytes(
        &mut self,
        shared: &SharedGpu,
        slot: usize,
        bytes: u32,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(), EngineError> {
        if bytes <= self.slots[slot].capacity {
            return Ok(());
        }
        let staging = StagingSlot::new_with_bytes(shared, bytes)?;
        let old = std::mem::replace(&mut self.slots[slot], staging);
        // 旧 staging 可能仍被在飞帧的命令列表读取（D3D12 命令列表不持有资源引用）：
        // 不能立即 drop，交给调用方按当前帧序号 retire。
        retired.push(old.handle);
        Ok(())
    }

    /// 本帧每个 AOI 矩形在 staging 里的落点：行 pitch 按**矩形宽**（不是页宽）对齐 256，
    /// 偏移按 512 对齐；返回布局与总字节数。空集合返回 `(vec![], 0)`（零上传）。
    fn layout(&self, rects: &DirtyRects) -> (Vec<StagingRect>, u32) {
        let texel = texel_bytes(self.kind);
        let mut layout = Vec::new();
        let mut end = 0u32;
        for chunk in rects.chunks(self.page, texel) {
            debug_assert!(
                chunk.x + chunk.width <= self.page && chunk.y + chunk.height <= self.page,
                "atlas AOI chunk {chunk:?} is outside the {}-texel page",
                self.page
            );
            let row_pitch = align_up(chunk.width * texel, PITCH_ALIGNMENT);
            let offset = align_up(end, PLACED_OFFSET_ALIGNMENT);
            end = offset + row_pitch * chunk.height;
            layout.push(StagingRect {
                chunk,
                row_pitch,
                offset,
            });
        }
        (layout, end)
    }

    /// 在本帧命令列表里录制「staging 写入 + 逐矩形 `CopyTextureRegion`」。
    ///
    /// AOI（R06-3）：每个脏矩形单独一次拷贝，`PlacedFootprint.Width/Height` 与 `destination`
    /// 的 `(DstX, DstY)` 都取矩形自己的 `(x, y, w, h)` —— 页宽只出现在页缓冲的行 pitch 里，
    /// staging 的行 pitch 是**矩形宽**（单字形不再上传整页宽的行）。
    fn upload(
        &mut self,
        shared: &SharedGpu,
        atlas: &SharedAtlas,
        list: &ID3D12GraphicsCommandList,
        slot: usize,
        pending: &PendingUpload,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(), EngineError> {
        let (layout, total_bytes) = self.layout(&pending.rects);
        if layout.is_empty() {
            // 空集合 = 零上传：不分配 staging、不录 copy（调用方本就不该入队空集合）。
            return Ok(());
        }
        let resource = atlas.texture(self.kind);
        transition(
            list,
            resource,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );

        // 小上传复用每槽常驻 staging；整页级的大上传用一次性缓冲（录制后 retire），
        // 常驻容量不随页尺寸放大（DEVIATIONS F-216）。
        let mut transient = None;
        if u64::from(total_bytes) <= MAX_RESIDENT_STAGING_BYTES {
            self.ensure_bytes(shared, slot, total_bytes, retired)?;
        } else {
            let temp = StagingSlot::new_with_bytes(shared, total_bytes)?;
            retired.push(temp.handle);
            transient = Some(temp);
        }
        let staging = transient.as_ref().unwrap_or(&self.slots[slot]);
        for rect in &layout {
            self.upload_rect(list, resource, staging, pending, *rect);
        }

        transition(
            list,
            resource,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        );
        Ok(())
    }

    /// 把一个 AOI 矩形写进 staging 的指定偏移，并录制一次 `CopyTextureRegion`。
    fn upload_rect(
        &self,
        list: &ID3D12GraphicsCommandList,
        resource: &ID3D12Resource,
        staging: &StagingSlot,
        pending: &PendingUpload,
        rect: StagingRect,
    ) {
        let texel = texel_bytes(self.kind) as usize;
        let width_bytes = rect.chunk.width as usize * texel;
        // SAFETY: `staging` 的容量 ≥ 本帧总字节数；指针是该槽的持久映射（DEVIATIONS F-210），
        // `offset + (height-1) × row_pitch + width_bytes` 不超过容量；源缓冲长度由
        // `queue_upload` 的 `byte_count` 断言保证，`chunk.offset` 是打包布局里的真实偏移。
        unsafe {
            for row in 0..rect.chunk.height as usize {
                let source = pending
                    .pixels
                    .as_ptr()
                    .add(rect.chunk.offset + row * width_bytes);
                let destination = staging
                    .ptr
                    .add(rect.offset as usize + row * rect.row_pitch as usize);
                std::ptr::copy_nonoverlapping(source, destination, width_bytes);
            }
        }

        let placed = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            // 每个矩形各自的行偏移（同一 staging 上多块不能再共用 offset 0，否则 GPU 只读到末块）。
            Offset: u64::from(rect.offset),
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: texture_format(self.kind),
                // AOI：footprint 就是矩形本身，不是整页宽。
                Width: rect.chunk.width,
                Height: rect.chunk.height,
                Depth: 1,
                RowPitch: rect.row_pitch,
            },
        };
        let mut destination = D3D12_TEXTURE_COPY_LOCATION {
            pResource: core::mem::ManuallyDrop::new(Some(resource.clone())),
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
        // SAFETY: 两个 location 与 `placed` 在调用期间有效；目标坐标 + footprint 尺寸由
        // `layout` 的页内断言保证在纹理边界内。
        unsafe {
            list.CopyTextureRegion(&destination, rect.chunk.x, rect.chunk.y, 0, &source, None)
        };
        // SAFETY: `pResource` 是 `ManuallyDrop`，结构体 drop 不释放 COM 引用（DEVIATIONS F-86）。
        unsafe {
            core::mem::ManuallyDrop::drop(&mut destination.pResource);
            core::mem::ManuallyDrop::drop(&mut source.pResource);
        }
    }
}

/// 待上传的一页内容（打包 `pixels` + 页尺寸 + 脏矩形）。
struct PendingUpload {
    page: u32,
    /// 入队时**共享图集**的页尺寸。
    ///
    /// 用来区分「新页」（App 换代后要重建共享纹理，`page != queued_page`）与「旧页」
    /// （入队后别的窗口已经换代，`page == queued_page` 且与当前共享页不同：上传作废）。
    queued_page: u32,
    pixels: Vec<u8>,
    rects: DirtyRects,
}

impl PendingUpload {
    /// 是否覆盖整页（首次上传 / 页换代之后的唯一合法载荷，见 `AtlasUploads::reconcile_pages`）。
    fn covers_full_page(&self) -> bool {
        self.rects.covers_page(self.page)
    }
}

/// 每窗口的图集上传路径：待上传页 + 每 in-flight 槽 staging。
pub(crate) struct AtlasUploads {
    pending: [Option<PendingUpload>; 2],
    staging: [Option<AtlasStaging>; 2],
}

impl AtlasUploads {
    pub(crate) const fn new() -> Self {
        Self {
            pending: [None, None],
            staging: [None, None],
        }
    }

    /// 该图集是否有一笔上传已入队、还没被命令列表消费（离屏目标的空闲判据）。
    pub(crate) fn pending(&self, kind: AtlasKind) -> bool {
        self.pending[kind.index() as usize].is_some()
    }

    /// 测试：某图集的 staging（还没上传过时为 `None`）。
    #[cfg(test)]
    fn staging(&self, kind: AtlasKind) -> Option<&AtlasStaging> {
        self.staging[kind.index() as usize].as_ref()
    }

    /// 入队一页的脏矩形（`pixels` 是 `rects` 顺序**逐矩形紧密打包**的字节，R06-3）。
    ///
    /// `pixels` 按**所有权**移动进待上传槽，避免槽 → 纹理之间再复制一次。长度必须等于
    /// [`DirtyRects::byte_count`]（release 也硬断言：越界读会读穿堆）。
    pub(crate) fn queue_upload(
        &mut self,
        kind: AtlasKind,
        page: u32,
        queued_page: u32,
        pixels: Vec<u8>,
        rects: DirtyRects,
    ) {
        let expected = rects.byte_count(page, texel_bytes(kind));
        assert_eq!(
            pixels.len(),
            expected,
            "atlas payload size mismatch for {kind:?}: expected {expected} bytes for the dirty rects of a {page}² page, got {}",
            pixels.len()
        );
        let slot = kind.index() as usize;
        assert!(
            self.pending[slot].is_none(),
            "atlas payload slot must be consumed before the next upload (kind {kind:?})"
        );
        self.pending[slot] = Some(PendingUpload {
            page,
            queued_page,
            pixels,
            rects,
        });
    }

    /// 帧前对齐**共享图集页**：丢弃已经不可能再被上传的过期增量，返回必须重建的页。
    ///
    /// 页尺寸由 App 的图集分配器决定，是进程级的：别的窗口一旦把共享页换成新尺寸，本窗口
    /// 排队中的旧页上传就不再成立：**新页**（整页 + 入队时共享页还不是它 ⇒ App 换代）要求
    /// 重建共享纹理；**旧页**（入队时共享页就是它，之后被别的窗口换代）无论整页还是增量都
    /// 直接丢弃 —— 重建回旧页会让所有窗口的 tile 对一个尺寸错误的纹理采样，且丢弃的行会被
    /// 下一次整页上传（页内容是进程级的）覆盖。
    ///
    /// 调用方在 `SharedGpu` 的图集读锁内调用；返回非空时先释放读锁，`SharedGpu::rebuild_atlas_pages`
    /// 在写锁内落地新页，然后重试。
    pub(crate) fn reconcile_pages(&mut self, pages: [u32; 2]) -> Vec<(AtlasKind, u32)> {
        let mut rebuild = Vec::new();
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = kind.index() as usize;
            let Some(pending) = &self.pending[index] else {
                continue;
            };
            if pending.page == pages[index] {
                continue;
            }
            // 整页 + 「入队时共享页还不是它」= App 换代的**新页**：重建共享纹理。
            // 其余（旧增量 / 入队时页就是它的旧整页）都是别的窗口已经换代之后留下的过期上传：
            // 丢弃即可 —— 丢弃的行会在下一次整页上传时被整页内容覆盖（页内容是进程级的）。
            if pending.covers_full_page() && pending.queued_page != pending.page {
                rebuild.push((kind, pending.page));
            } else {
                self.pending[index] = None;
            }
        }
        rebuild
    }

    /// teardown：把本窗口 staging 的句柄交回共享设备（GPU 线程已 idle，列表已重置）。
    ///
    /// 进程级设备不会随窗口销毁，所以每窗口的句柄必须在窗口销毁时主动注销，否则它们的
    /// COM 引用会一直留在共享设备的句柄表里。
    pub(crate) fn release(&self, shared: &SharedGpu) {
        for staging in self.staging.iter().flatten() {
            for slot in &staging.slots {
                let _ = shared.release(slot.handle);
            }
        }
    }

    /// 把 staging 对齐到共享页：页尺寸变化（或第一次上传）时整组重建本窗口的 staging。
    ///
    /// 必须在 `reconcile_pages` 之后、录制之前调用：此时每个待上传页都等于共享页。被换下来的
    /// 旧 staging 句柄推进 `retired`（调用方按本帧序号 retire，R08-2）。
    pub(crate) fn sync_staging(
        &mut self,
        shared: &SharedGpu,
        atlas: &SharedAtlas,
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(), EngineError> {
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = kind.index() as usize;
            let Some(pending) = &self.pending[index] else {
                continue;
            };
            if self.staging[index]
                .as_ref()
                .is_some_and(|staging| staging.page == pending.page)
            {
                continue;
            }
            let page = pending.page;
            let staging = AtlasStaging::new(shared, kind, page, atlas.row_pitch(kind))?;
            if let Some(old) = self.staging[index].replace(staging) {
                old.retire(retired);
            }
            debug_assert_eq!(atlas.page(kind), page, "图集页与待上传页必须已经对齐");
        }
        Ok(())
    }

    /// 在本帧命令列表里录制全部待上传页（每页 = staging 写入 + 逐区间 `CopyTextureRegion`），
    /// 同时把字节落进 CPU 影子页（R08-4 的恢复来源）。
    ///
    /// 返回因 staging 扩容换下的旧句柄（调用方在本帧录制结束后 retire；D3D12 命令列表
    /// 不持有资源引用，立即 drop 会让在飞帧读已释放的缓冲）。
    pub(crate) fn record_uploads(
        &mut self,
        shared: &SharedGpu,
        atlas: &SharedAtlas,
        list: &ID3D12GraphicsCommandList,
        slot: usize,
    ) -> Result<Vec<GpuResourceHandle>, EngineError> {
        let mut retired = Vec::new();
        for kind in [AtlasKind::Glyph, AtlasKind::Image] {
            let index = kind.index() as usize;
            let Some(pending) = self.pending[index].take() else {
                continue;
            };
            debug_assert_eq!(
                pending.page,
                atlas.page(kind),
                "page changes must be applied before recording"
            );
            if pending.page != atlas.page(kind) {
                // 防御：页在录制期间被改（理论上被图集读锁排除）。丢掉而不是画错内容。
                continue;
            }
            // 影子页与 GPU 上传用同一份字节：设备丢失后按它重传整页（R08-4）。
            atlas.update_shadow(kind, pending.page, &pending.pixels, &pending.rects);
            let Some(staging) = self.staging[index].as_mut() else {
                return Err(EngineError::AppInit {
                    detail: smol_str::format_smolstr!(
                        "atlas staging missing for {kind:?} page {}",
                        pending.page
                    ),
                });
            };
            debug_assert_eq!(staging.page, pending.page);
            staging.upload(shared, atlas, list, slot, &pending, &mut retired)?;
        }
        Ok(retired)
    }
}

pub(crate) fn transition(
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

/// 该图集在 `page` 尺寸下的 staging 行 pitch（字节，256 对齐）。
///
/// 共享纹理、每窗口 staging、设备恢复时的一次性 staging 都用同一个行 pitch：D3D12 的
/// `PlacedFootprint.RowPitch` 必须 256 对齐，三者不一致会让整页拷贝错行。
pub(crate) fn row_pitch_for(kind: AtlasKind, page: u32) -> u32 {
    align_up(page * texel_bytes(kind), PITCH_ALIGNMENT)
}

/// 把一块 UPLOAD staging 的整页内容拷进图集纹理（设备恢复用，R08-4）。
///
/// 调用方负责前后各一次状态过渡（`PIXEL_SHADER_RESOURCE → COPY_DEST → PIXEL_SHADER_RESOURCE`），
/// 并保证 `staging.pixels.len() == row_pitch × page`。
pub(crate) fn copy_page(
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    staging: &ID3D12Resource,
    kind: AtlasKind,
    page: u32,
    row_pitch: u32,
) {
    let placed = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        Offset: 0,
        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
            Format: texture_format(kind),
            Width: page,
            Height: page,
            Depth: 1,
            RowPitch: row_pitch,
        },
    };
    let mut destination = D3D12_TEXTURE_COPY_LOCATION {
        pResource: core::mem::ManuallyDrop::new(Some(texture.clone())),
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
    // SAFETY: 两个 location 与 `placed` 在调用期间有效；整页区间落在纹理与 staging 内
    // （staging 按 `row_pitch × page` 分配）。
    unsafe { list.CopyTextureRegion(&destination, 0, 0, 0, &source, None) };
    // SAFETY: `pResource` 是 `ManuallyDrop`，结构体 drop 不释放 COM 引用（DEVIATIONS F-86）。
    unsafe {
        core::mem::ManuallyDrop::drop(&mut destination.pResource);
        core::mem::ManuallyDrop::drop(&mut source.pResource);
    }
}

/// 每槽常驻 staging 的初始容量（字节）：受 1 MiB 上限与 [`STAGING_ROWS`] 行双重约束。
fn resident_bytes(row_pitch: u32) -> u32 {
    (u64::from(row_pitch) * u64::from(STAGING_ROWS)).min(MAX_RESIDENT_STAGING_BYTES) as u32
}

fn create_texture(
    device: &ID3D12Device,
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
    device: &ID3D12Device,
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
    device: &ID3D12Device,
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
    // SAFETY: COM 调用：设备句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
    unsafe {
        device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, desc, initial, None, &mut out)
    }
    .map_err(|error| init_error_from("CreateCommittedResource", error))?;
    out.ok_or_else(|| EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("d3d12 CreateCommittedResource returned no resource"),
    })
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::{
        Direct3D12::D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8_UNORM},
    };

    use super::{AtlasUploads, STAGING_ROWS, SharedAtlas, align_up, resident_bytes};
    use crate::{
        math::PhysicalRect,
        paint::atlas::AtlasKind,
        render::{
            atlas::{DirtyRects, MAX_ATLAS_PAGE, MAX_RESIDENT_STAGING_BYTES},
            frame::MAX_FRAMES_IN_FLIGHT,
            windows::{device::SharedGpu, queue::CommandStream, readback::Readback},
        },
    };

    /// 测试页：2048² 的纹理在 WARP 上代价过高（且会拖垮并行的 GPU 测试）。
    const TEST_PAGE: u32 = 64;

    /// 整页哨兵（新窗口 / 页尺寸变化的载荷）。
    fn full_rects() -> DirtyRects {
        DirtyRects::full()
    }

    /// 整页宽的脏行矩形（等价于旧的 `mark(y0, y1)` 语义，用于验证行落位）。
    fn rows(y0: u32, y1: u32) -> DirtyRects {
        let mut rects = DirtyRects::default();
        rects.mark(PhysicalRect::new(0, y0 as i32, i32::MAX, y1 as i32));
        rects
    }

    /// 单个 AOI 矩形（x/y/w/h 全部保留，R06-3）。
    fn rect(x0: u32, y0: u32, x1: u32, y1: u32) -> DirtyRects {
        let mut rects = DirtyRects::default();
        rects.mark(PhysicalRect::new(
            x0 as i32, y0 as i32, x1 as i32, y1 as i32,
        ));
        rects
    }

    /// 一个独立的 WARP 共享 GPU + 独立共享图集（不碰进程级单例；图集状态可预测）。
    struct Harness {
        shared: std::sync::Arc<SharedGpu>,
        stream: CommandStream,
        atlas: SharedAtlas,
        uploads: AtlasUploads,
    }

    impl Harness {
        fn new() -> Self {
            let shared = SharedGpu::new_warp().expect("WARP shared gpu");
            let stream = CommandStream::new(&shared).expect("command stream");
            let atlas = SharedAtlas::new(shared.raw()).expect("shared atlas");
            Self {
                shared,
                stream,
                atlas,
                uploads: AtlasUploads::new(),
            }
        }

        /// 入队 + 页对齐（重建到待上传页）+ staging 对齐 + 录进一次立即提交。
        fn upload(
            &mut self,
            kind: AtlasKind,
            page: u32,
            pixels: Vec<u8>,
            dirty: DirtyRects,
        ) -> Vec<crate::render::retire::GpuResourceHandle> {
            let queued_page = self.atlas.page(kind);
            self.uploads
                .queue_upload(kind, page, queued_page, pixels, dirty);
            let mut rebuild = self.uploads.reconcile_pages([
                self.atlas.page(AtlasKind::Glyph),
                self.atlas.page(AtlasKind::Image),
            ]);
            for (kind, page) in rebuild.drain(..) {
                let _old = self
                    .atlas
                    .recreate(self.shared.raw(), kind, page)
                    .expect("recreate atlas page");
            }
            let mut retired = Vec::new();
            self.uploads
                .sync_staging(&self.shared, &self.atlas, &mut retired)
                .expect("sync staging");
            let shared = &self.shared;
            let atlas = &self.atlas;
            let uploads = &mut self.uploads;
            self.stream
                .submit_immediate(|list| {
                    retired.extend(uploads.record_uploads(shared, atlas, list, 0)?);
                    Ok(())
                })
                .expect("upload");
            retired
        }

        /// 把一张图集的整页读回 CPU（RGBA8；R8 覆盖率读回为 R = 覆盖率、A = 255）。
        fn readback(&mut self, kind: AtlasKind, page: u32) -> ReadbackImage {
            let format = match kind {
                AtlasKind::Glyph => DXGI_FORMAT_R8_UNORM,
                AtlasKind::Image => DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
            };
            let mut readback = Readback::new();
            readback
                .capture_from(
                    &self.shared,
                    &mut self.stream,
                    self.atlas.texture(kind),
                    (page, page),
                    format,
                    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                )
                .expect("readback")
        }
    }

    type ReadbackImage = crate::render::windows::readback::ReadbackImage;

    #[test]
    fn row_pitch_is_256_aligned_for_both_texels() {
        assert_eq!(align_up(MAX_ATLAS_PAGE, 256), 2048);
        assert_eq!(align_up(MAX_ATLAS_PAGE * 4, 256), 8192);
    }

    /// 常驻 staging 永远不超过 1 MiB：整页级上传走一次性缓冲（F-216）。
    #[test]
    fn resident_staging_stays_within_the_cap() {
        for pitch in [256u32, 512, 1024, 2048, 4096, 8192] {
            let bytes = resident_bytes(pitch);
            assert!(bytes > 0, "pitch {pitch}: 常驻容量不得为 0");
            assert!(
                u64::from(bytes) <= MAX_RESIDENT_STAGING_BYTES,
                "pitch {pitch}: {bytes} bytes"
            );
        }
        // 2048² 图片页的行宽：256 行 × 8192 B = 2 MiB，夹到 1 MiB。
        assert_eq!(resident_bytes(8192), MAX_RESIDENT_STAGING_BYTES as u32);
        // 小页：256 行 × 256 B = 64 KiB。
        assert_eq!(resident_bytes(256), STAGING_ROWS * 256);
    }

    /// 页对齐决策（不建 GPU 设备）：整页上传要求重建；过期增量直接丢弃。
    #[test]
    fn reconcile_pages_keeps_full_page_rebuilds_and_drops_stale_increments() {
        let mut uploads = AtlasUploads::new();
        uploads.queue_upload(
            AtlasKind::Glyph,
            TEST_PAGE,
            TEST_PAGE,
            vec![0xAB; 2 * TEST_PAGE as usize],
            rows(4, 6),
        );
        assert!(
            uploads.reconcile_pages([TEST_PAGE * 8, 1]).is_empty(),
            "非整页的旧页增量不应触发重建"
        );
        assert!(
            !uploads.pending(AtlasKind::Glyph),
            "过期增量必须在页对齐时被丢弃"
        );

        // 占位页是 1×1：整页上传代表 App 换代（入队页 1 ≠ 新页），必须要求重建。
        uploads.queue_upload(
            AtlasKind::Glyph,
            TEST_PAGE,
            1,
            vec![0u8; (TEST_PAGE * TEST_PAGE) as usize],
            full_rects(),
        );
        assert_eq!(
            uploads.reconcile_pages([TEST_PAGE * 8, 1]),
            vec![(AtlasKind::Glyph, TEST_PAGE)],
            "整页上传必须要求把共享页重建到它自己的页尺寸"
        );
        assert!(
            uploads.reconcile_pages([TEST_PAGE, 1]).is_empty(),
            "页已对齐时不再要求重建"
        );
        assert!(uploads.pending(AtlasKind::Glyph));
    }

    /// 旧页整页上传（入队时共享页就是它，之后被别的窗口换代）不得把共享纹理重建回旧页：
    /// 否则所有窗口的 tile 都会对一个尺寸错误的纹理采样。
    #[test]
    fn reconcile_pages_discards_a_full_page_upload_of_a_superseded_page() {
        let mut uploads = AtlasUploads::new();
        uploads.queue_upload(
            AtlasKind::Glyph,
            512,
            512,
            vec![0u8; 512 * 512],
            full_rects(),
        );
        assert!(
            uploads.reconcile_pages([1024, 1]).is_empty(),
            "旧页整页上传不得触发重建"
        );
        assert!(!uploads.pending(AtlasKind::Glyph), "旧页上传必须被丢弃");
    }

    /// 整页上传（> 1 MiB）必须走一次性缓冲：常驻槽不得按整页扩容（F-216）。
    #[test]
    fn a_full_page_upload_does_not_grow_the_resident_slot() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        // 1024² 的图片整页 = 4 MiB，超过常驻上限。
        const PAGE: u32 = 1024;
        let retired = harness.upload(
            AtlasKind::Image,
            PAGE,
            vec![0u8; (PAGE * PAGE * 4) as usize],
            full_rects(),
        );
        assert!(
            !retired.is_empty(),
            "整页上传必须把一次性 staging 交给 retire"
        );
        let staging = harness
            .uploads
            .staging(AtlasKind::Image)
            .expect("image staging");
        assert_eq!(staging.page, PAGE);
        let resident = staging.slots[0].capacity;
        assert!(
            u64::from(resident) <= MAX_RESIDENT_STAGING_BYTES,
            "常驻槽被整页上传撑大：{resident} B"
        );
        assert!(
            u64::from(resident) < u64::from(PAGE) * u64::from(PAGE) * 4,
            "整页字节数不得常驻（{resident} B）"
        );
    }

    #[test]
    fn a_dirty_row_upload_only_touches_that_row() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        // 第一次必须先整页上传（真实入口：新窗口 / 页尺寸变化都走整页），之后才是增量。
        harness.upload(
            AtlasKind::Glyph,
            TEST_PAGE,
            vec![0u8; (TEST_PAGE * TEST_PAGE) as usize],
            full_rects(),
        );
        // 增量：只有 1 行（= 页宽字节）的载荷。
        let page = vec![0xABu8; TEST_PAGE as usize];
        harness.upload(AtlasKind::Glyph, TEST_PAGE, page, rows(5, 6));

        let image = harness.readback(AtlasKind::Glyph, TEST_PAGE);
        assert_eq!(image.rgba8[(5 * TEST_PAGE * 4) as usize], 0xAB);
        assert_eq!(image.rgba8[(5 * TEST_PAGE * 4 + 3) as usize], 0xFF);
        assert_eq!(image.rgba8[(4 * TEST_PAGE * 4) as usize], 0);
        assert_eq!(image.rgba8[(6 * TEST_PAGE * 4) as usize], 0);
    }

    /// R06-3 AOI 回归：8×11 字形只上传 88 B，且只有该矩形落进纹理。
    ///
    /// 旧「整页宽 × 行区间」语义在同一页上要传 11 × 64 = 704 B；AOI 是 88 B（8 倍）。
    #[test]
    fn an_eight_by_eleven_glyph_uploads_only_its_aoi_rect() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        harness.upload(
            AtlasKind::Glyph,
            TEST_PAGE,
            vec![0u8; (TEST_PAGE * TEST_PAGE) as usize],
            full_rects(),
        );
        // 8×11 字形落在 (20, 30)。
        const X: u32 = 20;
        const Y: u32 = 30;
        const W: u32 = 8;
        const H: u32 = 11;
        let glyph = rect(X, Y, X + W, Y + H);
        assert_eq!(glyph.byte_count(TEST_PAGE, 1), 88, "载荷必须只有 88 B");
        assert_eq!(11 * TEST_PAGE as usize, 704, "旧语义要传 11 整行 × 页宽 64");
        harness.upload(AtlasKind::Glyph, TEST_PAGE, vec![0xCDu8; 88], glyph);

        let image = harness.readback(AtlasKind::Glyph, TEST_PAGE);
        let texel = |x: u32, y: u32| image.rgba8[((y * TEST_PAGE + x) * 4) as usize];
        assert_eq!(texel(X, Y), 0xCD, "AOI 左上角必须被写");
        assert_eq!(
            texel(X + W - 1, Y + H - 1),
            0xCD,
            "AOI 右下角必须被写（行内只有 width × texel 字节）"
        );
        assert_eq!(texel(X - 1, Y), 0, "AOI 左侧一列不得被写");
        assert_eq!(texel(X + W, Y), 0, "AOI 右侧一列不得被写");
        assert_eq!(texel(X, Y - 1), 0, "AOI 上一行不得被写");
        assert_eq!(texel(X, Y + H), 0, "AOI 下一行不得被写");
    }

    #[test]
    fn queueing_a_dirty_span_marks_pending() {
        let mut uploads = AtlasUploads::new();
        assert!(!uploads.pending(AtlasKind::Glyph));
        uploads.queue_upload(
            AtlasKind::Glyph,
            TEST_PAGE,
            TEST_PAGE,
            vec![0x7Fu8; 2 * TEST_PAGE as usize],
            rows(4, 6),
        );
        assert!(uploads.pending(AtlasKind::Glyph));
        assert!(!uploads.pending(AtlasKind::Image));
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
        let mut harness = Harness::new();

        // 槽 0：字形整页（512 行）。
        let glyph = vec![0x11u8; (BIG_PAGE * BIG_PAGE) as usize];
        harness.upload(AtlasKind::Glyph, BIG_PAGE, glyph, full_rects());

        // 槽 1：图片整页（512 行 × 4 B；行 pitch 2048）。
        let mut image = vec![0u8; (BIG_PAGE * BIG_PAGE * 4) as usize];
        for texel in image.as_chunks_mut::<4>().0 {
            *texel = [0x33, 0x44, 0x55, 0xFF];
        }
        harness.upload(AtlasKind::Image, BIG_PAGE, image, full_rects());

        // 两个独立的读回器：R8 与 BGRA8 的行 pitch 不同（`Readback` 按尺寸/格式缓存）。
        let glyph_image = harness.readback(AtlasKind::Glyph, BIG_PAGE);
        let glyph_row = |row: u32| glyph_image.rgba8[(row * BIG_PAGE * 4) as usize];
        assert_eq!(glyph_row(0), 0x11, "glyph row 0");
        assert_eq!(glyph_row(BIG_PAGE - 1), 0x11, "glyph last row");

        let image_image = harness.readback(AtlasKind::Image, BIG_PAGE);
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

    /// R08-2：页换代交出来的旧 staging 句柄必须走 retire，不能当场从共享设备表里摘掉。
    ///
    /// D3D12 **不**替应用保活命令列表引用过的资源；旧实现（注释写「COM 引用由那些列表持有」）
    /// 在页换代时立即 `release`，两个 in-flight 槽仍可能在读这些 UPLOAD 缓冲。
    #[test]
    fn a_page_change_retires_the_old_staging_instead_of_releasing_it() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        // 页 64：建 3 槽常驻 staging（64² 字形整页 4 KiB ≪ 1 MiB）。
        let _ = harness.upload(
            AtlasKind::Glyph,
            64,
            vec![0u8; (64 * 64) as usize],
            full_rects(),
        );
        let live = harness.shared.live_resources();
        assert_eq!(live, MAX_FRAMES_IN_FLIGHT, "页 64 只注册 3 个常驻 staging");

        // 页 128：整页上传 = App 换代 ⇒ 重建共享页 + 整组 staging 换代。
        let retired = harness.upload(
            AtlasKind::Glyph,
            128,
            vec![0u8; (128 * 128) as usize],
            full_rects(),
        );
        assert_eq!(
            retired.len(),
            MAX_FRAMES_IN_FLIGHT,
            "整组 3 个旧 staging 必须交给 retire（R08-2）"
        );
        assert_eq!(
            harness.shared.live_resources(),
            live + MAX_FRAMES_IN_FLIGHT,
            "新增 3 个 staging，而旧的 3 个必须**还活着**（尚未 retire）"
        );

        // 交给提交流之后仍在 retire 队列里（fence 未到，不能释放）。
        // 键是 fence 的 u64 提交序号（R06-1）。
        for handle in retired {
            harness.stream.retire(1, handle);
        }
        assert!(
            harness.stream.pending_retire() >= MAX_FRAMES_IN_FLIGHT,
            "retire 队列必须持有这些句柄直到该帧 fence 完成"
        );
    }

    /// R08-4：CPU 影子页必须与上传字节一致 —— 设备丢失后靠它把图集内容重传回新设备。
    #[test]
    fn the_atlas_shadow_mirrors_full_and_incremental_uploads() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();
        let row_bytes = TEST_PAGE as usize;

        // 整页上传：影子页 = 整页字节（紧密行）。
        let mut page = vec![0u8; row_bytes * TEST_PAGE as usize];
        page[5 * row_bytes + 3] = 0xAB;
        harness.upload(AtlasKind::Glyph, TEST_PAGE, page.clone(), full_rects());
        let snapshot = harness.atlas.shadow_snapshot();
        let (size, bytes) = snapshot[0]
            .as_ref()
            .expect("glyph shadow after full upload");
        assert_eq!(*size, TEST_PAGE);
        assert_eq!(bytes, &page, "影子页必须与整页字节 1:1");

        // 增量上传：只有脏行覆盖，其余像素保持（打包缓冲里只有脏行）。
        let dirty_row = vec![0x77u8; row_bytes];
        harness.upload(AtlasKind::Glyph, TEST_PAGE, dirty_row, rows(5, 6));
        let snapshot = harness.atlas.shadow_snapshot();
        let bytes = &snapshot[0].as_ref().expect("glyph shadow").1;
        assert_eq!(bytes[5 * row_bytes + 3], 0x77, "脏行被覆盖");
        assert_eq!(bytes[4 * row_bytes], 0, "脏行之外保持不变");

        // AOI 增量（R06-3）：4×2 的矩形只覆盖自己的列/行，影子页的其它字节保持原样。
        let aoi = rect(3, 7, 7, 9);
        assert_eq!(aoi.byte_count(TEST_PAGE, 1), 8);
        harness.upload(AtlasKind::Glyph, TEST_PAGE, vec![0x99u8; 8], aoi);
        let snapshot = harness.atlas.shadow_snapshot();
        let bytes = &snapshot[0].as_ref().expect("glyph shadow").1;
        assert_eq!(bytes[7 * row_bytes + 3], 0x99, "AOI 左上角");
        assert_eq!(bytes[8 * row_bytes + 6], 0x99, "AOI 右下角");
        assert_eq!(bytes[7 * row_bytes + 2], 0, "AOI 左侧一列保持原样");
        assert_eq!(bytes[7 * row_bytes + 7], 0, "AOI 右侧一列保持原样");
        assert_eq!(bytes[6 * row_bytes + 3], 0, "AOI 上一行保持原样");
        assert_eq!(bytes[9 * row_bytes + 3], 0, "AOI 下一行保持原样");

        // 页换代：影子页按新页整页重建。
        harness.upload(AtlasKind::Glyph, 128, vec![0x11u8; 128 * 128], full_rects());
        let snapshot = harness.atlas.shadow_snapshot();
        let (size, bytes) = snapshot[0]
            .as_ref()
            .expect("glyph shadow after page change");
        assert_eq!(*size, 128);
        assert!(bytes.len() == 128 * 128);
        assert!(bytes.iter().all(|byte| *byte == 0x11));
    }

    /// 多脏矩形必须各自落位：每块写到 staging 自己的偏移，`CopyTextureRegion` 的
    /// `PlacedFootprint.Offset` 也必须跟着走（V-64；旧的「全部写 offset 0」会让两块都读到末块）。
    #[test]
    fn multi_rect_upload_places_each_rect_at_its_own_offset() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut harness = Harness::new();

        // 第一次整页清零（真实入口：新窗口 / 页尺寸变化都走整页）。
        harness.upload(
            AtlasKind::Glyph,
            BIG_PAGE,
            vec![0u8; (BIG_PAGE * BIG_PAGE) as usize],
            full_rects(),
        );

        // 打包布局：矩形按行号顺序紧密排列（10 行 0x44 + 20 行 0x55，都是整页宽）。
        let mut page = vec![0x44u8; (10 * BIG_PAGE) as usize];
        page.extend(std::iter::repeat_n(0x55u8, (20 * BIG_PAGE) as usize));
        let mut dirty = DirtyRects::default();
        dirty.mark(PhysicalRect::new(0, 10, i32::MAX, 20));
        dirty.mark(PhysicalRect::new(0, 300, i32::MAX, 320));
        harness.upload(AtlasKind::Glyph, BIG_PAGE, page, dirty);

        let image = harness.readback(AtlasKind::Glyph, BIG_PAGE);
        let row = |row: u32| image.rgba8[(row * BIG_PAGE * 4) as usize];
        assert_eq!(row(0), 0, "untouched row before the first rect");
        assert_eq!(row(15), 0x44, "first rect content");
        assert_eq!(row(100), 0, "gap between the rects stays untouched");
        assert_eq!(row(299), 0, "row before the second rect");
        assert_eq!(row(305), 0x55, "second rect content");
        assert_eq!(row(511), 0, "untouched row after the last rect");
    }
}
